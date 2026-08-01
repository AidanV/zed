//! The frame loop (SPEC §7) and the terminal's mode.
//!
//! GPUI's redraw model is pull: the platform invokes the window's
//! `on_request_frame` callback and GPUI decides inside it whether to redraw.
//! `ted` owns that pull, driving it from a foreground task fed by a plain
//! reader thread.

use std::io::{Write, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{execute, queue};
use editor::Editor;
use editor::display_map::DisplayRow;
use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui::{App, AppContext as _, AsyncApp, WindowHandle};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::bootstrap;
use crate::cell::{CELL_HEIGHT, CELL_WIDTH};
use crate::input::keystroke_for;
use crate::platform::{TerminalPlatform, TerminalWindowState};
use crate::render::render;
use crate::snapshot::{CellPoint, CellRect, EditorView, RowView, StatusView, ViewSnapshot};

/// Rows `ted` paints itself and therefore withholds from the GPUI window. The
/// window is sized to the grid *minus* these, so the editor's reported rect can
/// never overlap a row `ted` owns (SPEC §10.2).
const RESERVED_ROWS: u16 = 1;

/// Below this the editor's width arithmetic goes negative, which is not a case
/// Zed is expected to handle (SPEC §10.2).
const MINIMUM_COLUMNS: u16 = 20;
const MINIMUM_ROWS: u16 = 5;

const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(100);

/// Set once the terminal has been put into raw mode, so the panic hook and the
/// normal shutdown path can both restore it and neither does so twice.
static TERMINAL_IS_RAW: AtomicBool = AtomicBool::new(false);

pub struct Options {
    pub path: Option<PathBuf>,
}

pub fn run(options: Options) -> Result<()> {
    let (columns, rows) = crossterm::terminal::size().context("could not read terminal size")?;

    enter_terminal_mode()?;
    install_panic_hook();

    let platform = std::rc::Rc::new(TerminalPlatform::new(columns, rows));
    let result = std::rc::Rc::new(std::cell::RefCell::new(Ok(())));

    // `Assets::load_fonts` reads through `cx.asset_source()` and panics if it
    // is the default empty one, so the source must be installed even though
    // `CellTextSystem` ignores the fonts it loads.
    gpui::Application::with_platform(platform.clone())
        .with_assets(assets::Assets)
        .run({
            let result = result.clone();
            move |cx| {
                if let Err(error) = start(options.path.clone(), columns, rows, &platform, cx) {
                    *result.borrow_mut() = Err(error);
                    cx.quit();
                }
            }
        });

    restore_terminal_mode();
    result.replace(Ok(()))
}

fn start(
    path: Option<PathBuf>,
    columns: u16,
    rows: u16,
    platform: &TerminalPlatform,
    cx: &mut App,
) -> Result<()> {
    bootstrap::init(cx)?;

    let editor_rows = rows.saturating_sub(RESERVED_ROWS);
    let opened = bootstrap::open_editor(path.as_deref(), columns, editor_rows, cx)?;
    let window_state = platform
        .window()
        .context("TerminalPlatform did not record the window it opened")?;
    window_state.activate_once();

    let (sender, receiver) = futures::channel::mpsc::unbounded();
    spawn_reader_thread(sender);

    let path_label = opened
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "[No Name]".to_owned());

    cx.spawn(async move |cx| {
        let loop_result = drive(
            receiver,
            opened.window,
            window_state,
            path_label,
            columns,
            rows,
            cx,
        )
        .await;
        if let Err(error) = loop_result {
            log::error!("ted's frame loop stopped: {error}");
        }
        cx.update(|cx| cx.quit());
    })
    .detach();

    Ok(())
}

/// A plain OS thread rather than an event source wired into three different run
/// loops. `crossterm::event::read` blocks, and the channel send is what wakes
/// the foreground executor — `examples/wake_probe.rs` measures that path.
fn spawn_reader_thread(sender: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            match crossterm::event::read() {
                Ok(event) => {
                    if sender.unbounded_send(event).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    log::error!("terminal reader stopped: {error}");
                    return;
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    mut events: UnboundedReceiver<Event>,
    window: WindowHandle<Editor>,
    window_state: std::rc::Rc<TerminalWindowState>,
    path_label: String,
    mut columns: u16,
    mut rows: u16,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .context("could not initialize the terminal backend")?;
    terminal.hide_cursor().ok();

    let mut painted: Option<ViewSnapshot> = None;

    loop {
        // Never call this from inside a `cx.update(..)` closure: GPUI's
        // registered frame callback does its own work through
        // `handle.update(&mut cx, ..)` on an `AsyncApp`, which cannot run while
        // the `App` is already borrowed.
        window_state.request_frame();

        let snapshot = build_snapshot(&window, &path_label, columns, rows, cx)?;
        if painted.as_ref() != Some(&snapshot) {
            paint(&mut terminal, &snapshot)?;
            painted = Some(snapshot);
        }

        let timer = cx.background_executor().timer(IDLE_FRAME_INTERVAL);
        let event = futures::select_biased! {
            event = events.next() => event,
            _ = futures::FutureExt::fuse(timer) => continue,
        };

        let Some(event) = event else {
            return Ok(());
        };

        match event {
            Event::Key(key) if is_quit(&key) => return Ok(()),
            Event::Key(key) => {
                if let Some(keystroke) = keystroke_for(&key) {
                    // Deliberately `App::update_window` rather than
                    // `WindowHandle::update`: the latter holds a mutable borrow
                    // of the root view for the duration of the closure, and
                    // dispatch routes into the editor's own action handlers,
                    // which update that same entity.
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_keystroke(keystroke, cx);
                    })
                    .ok();
                }
            }
            Event::Paste(text) => {
                // One edit, not replayed keystrokes: replaying would run vim
                // motions over the pasted text (SPEC §8.2).
                window
                    .update(cx, |editor, window, cx| {
                        editor.handle_input(&text, window, cx);
                    })
                    .ok();
            }
            Event::Resize(new_columns, new_rows) => {
                columns = new_columns;
                rows = new_rows;
                if fits(columns, rows) {
                    window_state.resize_to_cells(columns, rows.saturating_sub(RESERVED_ROWS));
                }
                terminal.clear().ok();
                painted = None;
            }
            Event::FocusGained | Event::FocusLost | Event::Mouse(_) => {}
        }
    }
}

fn is_quit(key: &crossterm::event::KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c'))
}

fn fits(columns: u16, rows: u16) -> bool {
    columns >= MINIMUM_COLUMNS && rows >= MINIMUM_ROWS
}

fn paint(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    snapshot: &ViewSnapshot,
) -> Result<()> {
    terminal.draw(|frame| {
        render(snapshot, frame.buffer_mut());
        if let Some(cursor) = snapshot.cursor {
            frame.set_cursor_position((cursor.column, cursor.row));
        }
    })?;
    if snapshot.cursor.is_some() {
        terminal.show_cursor().ok();
    } else {
        terminal.hide_cursor().ok();
    }
    Ok(())
}

/// Reads the projection out of the entities *after* the draw: `Editor::style`
/// and `last_bounds` are populated during element layout, so a snapshot taken
/// before the first frame has neither (SPEC §10.2, "Ordering").
fn build_snapshot(
    window: &WindowHandle<Editor>,
    path_label: &str,
    columns: u16,
    rows: u16,
    cx: &mut AsyncApp,
) -> Result<ViewSnapshot> {
    if !fits(columns, rows) {
        return Ok(ViewSnapshot {
            columns,
            rows,
            editor: None,
            status: StatusView {
                path: Some(format!(
                    "terminal too small (need {MINIMUM_COLUMNS}x{MINIMUM_ROWS})"
                )),
                ..Default::default()
            },
            cursor: None,
        });
    }

    // The window's root view *is* the editor, so `WindowHandle::update` already
    // hands us `&mut Editor`. Going through the entity handle again here would
    // nest an update inside an update and panic.
    window
        .update(cx, |editor, window, cx| {
            {
                let snapshot = editor.snapshot(window, cx);
                let display = &snapshot.display_snapshot;

                let text_rect = editor
                    .last_bounds()
                    .map(|bounds| CellRect {
                        x: (f32::from(bounds.origin.x) / f32::from(CELL_WIDTH)) as u16,
                        y: (f32::from(bounds.origin.y) / f32::from(CELL_HEIGHT)) as u16,
                        width: (f32::from(bounds.size.width) / f32::from(CELL_WIDTH)) as u16,
                        height: (f32::from(bounds.size.height) / f32::from(CELL_HEIGHT)) as u16,
                    })
                    .unwrap_or(CellRect::new(0, 0, columns, rows - RESERVED_ROWS));

                let scroll = editor.scroll_position(cx);
                let first_row = scroll.y.max(0.0) as u32;
                // Fractional visible counts mean a partial bottom row; render
                // the whole ones and drop the partial (SPEC §5.3).
                let visible = editor.visible_line_count().unwrap_or(0.0).floor() as u32;
                let last_display_row = display.max_point().row().0;

                let rows_view = (first_row..first_row.saturating_add(visible))
                    .take_while(|row| *row <= last_display_row)
                    .map(|row| RowView::new(row, display.line(DisplayRow(row))))
                    .collect::<Vec<_>>();

                let cursor = cursor_cell(editor, &snapshot, &rows_view, &text_rect, first_row);

                let dirty = editor.buffer().read(cx).is_dirty(cx);

                Ok(ViewSnapshot {
                    columns,
                    rows,
                    editor: Some(EditorView {
                        text_rect,
                        scroll_columns: scroll.x.max(0.0) as u16,
                        rows: rows_view,
                        max_display_row: last_display_row,
                        soft_wrapped: true,
                    }),
                    status: StatusView {
                        path: Some(path_label.to_owned()),
                        dirty,
                        mode: None,
                        message: None,
                    },
                    cursor,
                })
            }
        })
        .context("window closed while building a frame")?
}

/// The primary cursor's cell, converted through the row's `byte_to_cell` table
/// — `DisplayPoint::column()` is a byte offset, and that table is the only
/// permitted conversion (SPEC §5.4).
fn cursor_cell(
    editor: &mut Editor,
    snapshot: &editor::EditorSnapshot,
    rows: &[RowView],
    text_rect: &CellRect,
    first_row: u32,
) -> Option<CellPoint> {
    let head = editor
        .selections
        .newest_display(&snapshot.display_snapshot)
        .head();
    let display_row = head.row().0;

    let row = rows.iter().find(|row| row.display_row == display_row)?;
    let column = row.cell_for_byte(head.column() as usize);

    Some(CellPoint {
        column: text_rect.x + column.min(text_rect.width.saturating_sub(1)),
        row: text_rect.y + (display_row - first_row) as u16,
    })
}

fn enter_terminal_mode() -> Result<()> {
    enable_raw_mode().context("could not enter raw mode")?;
    TERMINAL_IS_RAW.store(true, Ordering::SeqCst);

    let mut out = stdout();
    // The clear is not redundant with `EnterAlternateScreen`: terminals without
    // alternate-screen support ignore the switch and leave the shell's output
    // on screen, and Ratatui's first `draw` diffs against a buffer it assumes
    // is blank, so it emits only the non-space cells and the old contents show
    // through everywhere `ted` paints a space.
    execute!(
        out,
        EnterAlternateScreen,
        Clear(ClearType::All),
        EnableBracketedPaste
    )
    .context("could not set up the terminal")?;

    // Disambiguates ctrl-i from tab, ctrl-m from enter and ctrl-[ from escape,
    // which vim's binding set depends on (SPEC §8.2). Terminals without the
    // protocol simply ignore it, so a failure here is not fatal.
    queue!(
        out,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    )
    .ok();
    out.flush().ok();
    Ok(())
}

/// Idempotent, because it runs from both the panic hook and normal shutdown.
/// `ted` owns the terminal's mode, so leaving it raw would hand the user a
/// broken shell (SPEC §3.1, §7).
fn restore_terminal_mode() {
    if !TERMINAL_IS_RAW.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = stdout();
    queue!(out, PopKeyboardEnhancementFlags).ok();
    execute!(out, DisableBracketedPaste, LeaveAlternateScreen).ok();
    out.flush().ok();
    disable_raw_mode().ok();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_mode();
        previous(info);
    }));
}
