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
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{execute, queue};
use editor::Editor;
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedSender;
use gpui::{App, AppContext as _, AsyncApp, Entity};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use theme::ActiveTheme as _;

use crate::bootstrap::{self, Backend};
use crate::command_line::{CommandLine, Update};
use crate::input::keystroke_for;
use crate::palette::{ColorDepth, Palette};
use crate::platform::{TerminalPlatform, TerminalWindowState, set_window_grid};
use crate::render::{render, reserved_rows};
use crate::snapshot::{CommandLineView, CursorShape, StatusView, ViewSnapshot};

/// Below this the editor's width arithmetic goes negative, which is not a case
/// Zed is expected to handle (SPEC §10.2).
const MINIMUM_COLUMNS: u16 = 20;
const MINIMUM_ROWS: u16 = 5;

/// SPEC §7: ~8ms while input is arriving or something is still settling, backing
/// off when idle. Calling `request_frame` on a clean window is cheap — GPUI
/// early-returns without laying out — but the terminal write is not, so the idle
/// tier is what keeps a terminal editor at ~0% CPU.
const BUSY_FRAME_INTERVAL: Duration = Duration::from_millis(8);
const IDLE_FRAME_INTERVAL: Duration = Duration::from_millis(100);
/// Frames to stay on the busy cadence after the last input, long enough for an
/// asynchronous rewrap or a language server's first diagnostics to land.
const BUSY_FRAMES_AFTER_INPUT: u32 = 24;

/// Set once the terminal has been put into raw mode, so the panic hook and the
/// normal shutdown path can both restore it and neither does so twice.
static TERMINAL_IS_RAW: AtomicBool = AtomicBool::new(false);

pub struct Options {
    pub paths: Vec<PathBuf>,
    pub vim: bool,
    pub opaque_background: bool,
}

pub fn run(options: Options) -> Result<()> {
    let (columns, rows) = crossterm::terminal::size().context("could not read terminal size")?;

    enter_terminal_mode()?;
    install_panic_hook();

    set_window_grid(columns, rows.saturating_sub(reserved_rows(false, 0)));
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
                if let Err(error) = start(&options, columns, rows, platform.clone(), cx) {
                    *result.borrow_mut() = Err(error);
                    cx.quit();
                }
            }
        });

    restore_terminal_mode();
    result.replace(Ok(()))
}

fn start(
    options: &Options,
    columns: u16,
    rows: u16,
    platform: std::rc::Rc<TerminalPlatform>,
    cx: &mut App,
) -> Result<()> {
    let app_state = bootstrap::init(options.vim, cx)?;

    let opening = bootstrap::open(options.paths.clone(), app_state, cx);
    let background = cx.theme().colors().editor_background;
    let palette = Palette::new(ColorDepth::detect(), background, options.opaque_background);

    cx.spawn(async move |cx| {
        let loop_result = async {
            let backend = opening.await?;
            // Only recorded once `open_window` has run, which happens inside
            // `bootstrap::open`.
            let window_state = platform
                .window()
                .context("TerminalPlatform did not record the window it opened")?;
            window_state.activate_once();
            drive(backend, window_state, palette, columns, rows, cx).await
        }
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
                    // `REPORT_EVENT_TYPES` makes the terminal report releases,
                    // and nothing downstream acts on one: each would cost a
                    // draw and a projection, and would clear a notification the
                    // press that raised it had only just put on screen.
                    if matches!(&event, Event::Key(key) if matches!(key.kind, KeyEventKind::Release))
                    {
                        continue;
                    }
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

/// The mutable state the loop carries between frames, kept together so the
/// reserved-row count and the lines that justify it cannot drift apart.
struct Session {
    backend: Backend,
    palette: Palette,
    columns: u16,
    rows: u16,
    command_line: Option<CommandLine>,
    /// Messages `ted` itself raised, cleared on the next keystroke so they stay
    /// transient (SPEC §13.3). Notifications the *backend* raised are read fresh
    /// each frame by `snapshot::backend_notifications`.
    messages: Vec<String>,
    busy_frames: u32,
}

impl Session {
    fn fits(&self) -> bool {
        self.columns >= MINIMUM_COLUMNS && self.rows >= MINIMUM_ROWS
    }
}

async fn drive(
    backend: Backend,
    window_state: std::rc::Rc<TerminalWindowState>,
    palette: Palette,
    columns: u16,
    rows: u16,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))
        .context("could not initialize the terminal backend")?;
    terminal.hide_cursor().ok();

    let (sender, mut events) = futures::channel::mpsc::unbounded();
    spawn_reader_thread(sender);

    let mut session = Session {
        backend,
        palette,
        columns,
        rows,
        command_line: None,
        messages: Vec::new(),
        busy_frames: BUSY_FRAMES_AFTER_INPUT,
    };
    let mut painted: Option<ViewSnapshot> = None;
    let mut reserved = u16::MAX;

    loop {
        // `:q` closes the active item rather than the window, so an empty
        // workspace is what "quit" looks like from here (SPEC §14.3).
        if cx.update(|cx| session.backend.is_empty(cx)) {
            return Ok(());
        }

        let command_line = session
            .command_line
            .as_ref()
            .map(|command_line| command_line.view())
            .or_else(|| search_line(&session, cx));

        let mut notifications = session.messages.clone();
        notifications.extend(backend_notifications(&session, cx));

        // A line appearing or disappearing changes how much of the grid the
        // window may use, and that is a resize like any other (SPEC §10.2).
        let wanted = reserved_rows(command_line.is_some(), notifications.len());
        if wanted != reserved {
            reserved = wanted;
            resize_window(&session, reserved, &window_state);
        }

        // Never call this from inside a `cx.update(..)` closure: GPUI's
        // registered frame callback does its own work through
        // `handle.update(&mut cx, ..)` on an `AsyncApp`, which cannot run while
        // the `App` is already borrowed.
        window_state.request_frame();

        // A closed window is how a `workspace::CloseWindow` reaches this loop,
        // which is a normal way to quit rather than a failure.
        let Ok(snapshot) = build_snapshot(&session, reserved, command_line, notifications, cx)
        else {
            return Ok(());
        };
        if painted.as_ref() != Some(&snapshot) {
            paint(&mut terminal, &snapshot, &session.palette)?;
            painted = Some(snapshot);
            session.busy_frames = session.busy_frames.max(1);
        }

        let interval = if session.busy_frames > 0 {
            session.busy_frames -= 1;
            BUSY_FRAME_INTERVAL
        } else {
            IDLE_FRAME_INTERVAL
        };
        let timer = cx.background_executor().timer(interval);
        let event = futures::select_biased! {
            event = events.next() => event,
            _ = futures::FutureExt::fuse(timer) => continue,
        };

        let Some(event) = event else {
            return Ok(());
        };
        session.busy_frames = BUSY_FRAMES_AFTER_INPUT;
        session.messages.clear();

        // One event per frame, deliberately. Dispatching a queued burst together
        // would let a held key's backlog drain in fewer frames, but a keystroke
        // is not safe to dispatch until the one before it has been drawn *and*
        // whatever it spawned has run: GPUI rebuilds the dispatch tree during
        // paint (SPEC §4.2.1), and deploying the search bar or a modal finishes
        // in a task. Batching routes the rest of the burst against the tree the
        // previous frame built — `/beta` types `beta` as vim motions rather than
        // into the query. Keeping up with auto-repeat is a matter of making the
        // frame cheap, not of dispatching more per frame.
        match event {
            // The escape hatch, and the only way out without vim. With a
            // command line open it cancels that instead, which is both what
            // vim does and what stops a stray `:` from stranding the user.
            Event::Key(key) if is_quit(&key) && session.command_line.is_none() => {
                return Ok(());
            }
            Event::Key(key) => handle_key(&mut session, key, cx).await?,
            Event::Paste(text) => {
                // One edit, not replayed keystrokes: replaying would run vim
                // motions over the pasted text (SPEC §8.2).
                if let Some(editor) = active_editor(&session, cx) {
                    session
                        .backend
                        .window
                        .update(cx, |_, window, cx| {
                            editor.update(cx, |editor, cx| {
                                editor.handle_input(&text, window, cx);
                            });
                        })
                        .ok();
                }
            }
            Event::Resize(new_columns, new_rows) => {
                session.columns = new_columns;
                session.rows = new_rows;
                resize_window(&session, reserved, &window_state);
                terminal.clear().ok();
                painted = None;
            }
            Event::FocusGained | Event::FocusLost | Event::Mouse(_) => {}
        }
    }
}

/// Routes a keystroke either into `ted`'s own `:` line or into GPUI's dispatch
/// tree. The `:` line is the only thing `ted` handles itself; everything else,
/// including `/` search, belongs to the backend.
async fn handle_key(session: &mut Session, key: KeyEvent, cx: &mut AsyncApp) -> Result<()> {
    if session.command_line.is_some() {
        return handle_command_line_key(session, key, cx).await;
    }

    if opens_command_line(session, &key, cx) {
        let Some(editor) = active_editor(session, cx) else {
            return Ok(());
        };
        let prefix = cx.update(|cx| crate::command_line::prefix_for(&editor, cx));
        let mut command_line = CommandLine::new(prefix);
        command_line
            .refresh(session.backend.workspace.downgrade(), cx)
            .await;
        session.command_line = Some(command_line);
        return Ok(());
    }

    let Some(keystroke) = keystroke_for(&key) else {
        return Ok(());
    };
    // Deliberately `App::update_window` rather than `WindowHandle::update`: the
    // latter holds a mutable borrow of the root view for the duration of the
    // closure, and dispatch routes into handlers that update that same entity.
    cx.update_window(session.backend.window.into(), |_, window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    })
    .ok();
    Ok(())
}

async fn handle_command_line_key(
    session: &mut Session,
    key: KeyEvent,
    cx: &mut AsyncApp,
) -> Result<()> {
    let Some(command_line) = session.command_line.as_mut() else {
        return Ok(());
    };

    match command_line.handle_key(&key) {
        Update::Unchanged => {}
        Update::QueryChanged => {
            command_line
                .refresh(session.backend.workspace.downgrade(), cx)
                .await;
        }
        Update::Cancel => session.command_line = None,
        Update::Submit => {
            let action = command_line.selected_action();
            let query = command_line.query().to_owned();
            session.command_line = None;
            match action {
                Some(action) => {
                    cx.update_window(session.backend.window.into(), |_, window, cx| {
                        window.dispatch_action(action, cx);
                    })
                    .ok();
                }
                None => session.messages.push(format!("not a command: :{query}")),
            }
        }
    }
    Ok(())
}

/// `:` opens `ted`'s command line whenever vim would have opened Zed's palette:
/// in a mode where `:` is a command rather than text. Without vim there is no
/// `:` line at all, and `:` is just a character.
fn opens_command_line(session: &Session, key: &KeyEvent, cx: &mut AsyncApp) -> bool {
    if key.code != KeyCode::Char(':')
        || matches!(key.kind, KeyEventKind::Release)
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return false;
    }
    let Some(editor) = active_editor(session, cx) else {
        return false;
    };

    cx.update_window(session.backend.window.into(), |_, window, cx| {
        use gpui::Focusable as _;
        // Vim's mode is the editor's even while something else holds focus —
        // the `/` search bar's own query editor, most of all — and a `:` typed
        // there is text, not a command.
        let focused = editor
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx);
        focused
            && vim::mode(editor.read(cx), cx)
                .is_some_and(|mode| !matches!(mode, vim::Mode::Insert | vim::Mode::Replace))
    })
    .unwrap_or(false)
}

fn active_editor(session: &Session, cx: &mut AsyncApp) -> Option<Entity<Editor>> {
    cx.update(|cx| session.backend.active_editor(cx))
}

fn is_quit(key: &KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c'))
}

/// Sizes the GPUI window to the grid *minus* the rows `ted` paints itself, so
/// the editor's reported rect can never overlap one of them (SPEC §10.2).
/// `resize_to_cells` invokes GPUI's own `on_resize` callback, which is what
/// triggers relayout.
fn resize_window(session: &Session, reserved: u16, window_state: &TerminalWindowState) {
    if !session.fits() {
        return;
    }
    let rows = session.rows.saturating_sub(reserved);
    set_window_grid(session.columns, rows);
    window_state.resize_to_cells(session.columns, rows);
}

fn paint(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    snapshot: &ViewSnapshot,
    palette: &Palette,
) -> Result<()> {
    terminal.draw(|frame| {
        render(snapshot, palette, frame.buffer_mut());
        if let Some(cursor) = snapshot.cursor {
            frame.set_cursor_position((cursor.column, cursor.row));
        }
    })?;
    if snapshot.cursor.is_some() {
        terminal.show_cursor().ok();
    } else {
        terminal.hide_cursor().ok();
    }
    set_cursor_shape(snapshot.cursor_shape);
    Ok(())
}

/// The terminal blinks its own cursor, so its *shape* is how `ted` shows vim's
/// mode without drawing a cell — and it is also what screen readers and
/// terminal cursor-shape escapes expect (SPEC §7).
fn set_cursor_shape(shape: CursorShape) {
    use crossterm::cursor::SetCursorStyle;
    let style = match shape {
        CursorShape::Block => SetCursorStyle::SteadyBlock,
        CursorShape::Bar => SetCursorStyle::SteadyBar,
        CursorShape::Underline => SetCursorStyle::SteadyUnderScore,
    };
    let mut out = stdout();
    queue!(out, style).ok();
    out.flush().ok();
}

fn backend_notifications(session: &Session, cx: &mut AsyncApp) -> Vec<String> {
    cx.update_window(session.backend.window.into(), |_, window, cx| {
        crate::snapshot::backend_notifications(&session.backend.workspace, window, cx)
    })
    .unwrap_or_default()
}

fn build_snapshot(
    session: &Session,
    reserved: u16,
    command_line: Option<CommandLineView>,
    notifications: Vec<String>,
    cx: &mut AsyncApp,
) -> Result<ViewSnapshot> {
    if !session.fits() {
        return Ok(ViewSnapshot {
            columns: session.columns,
            rows: session.rows,
            status: StatusView {
                path: Some(format!(
                    "terminal too small (need {MINIMUM_COLUMNS}x{MINIMUM_ROWS})"
                )),
                ..Default::default()
            },
            ..Default::default()
        });
    }

    cx.update_window(session.backend.window.into(), |_, window, cx| {
        crate::snapshot::build(
            crate::snapshot::Frame {
                columns: session.columns,
                rows: session.rows,
                reserved_rows: reserved,
                command_line,
                notifications,
                workspace: &session.backend.workspace,
            },
            window,
            cx,
        )
    })
    .context("window closed while building a frame")
}

/// Projects the pane's `BufferSearchBar` into `ted`'s bottom line. Vim's `/`
/// and `?` dispatch into that bar and type into its own editor, so `ted` only
/// mirrors what is already there (SPEC §14.2).
fn search_line(session: &Session, cx: &mut AsyncApp) -> Option<CommandLineView> {
    cx.update(|cx| {
        let search_bar = session.backend.active_pane_search_bar(cx)?;
        let search_bar = search_bar.read(cx);
        if search_bar.is_dismissed() {
            return None;
        }
        let (index, total) = search_bar.match_summary().unwrap_or((0, 0));
        let query = search_bar.query(cx);
        Some(CommandLineView {
            prefix: '/',
            cursor: query.len(),
            query,
            completions: Vec::new(),
            selected_completion: None,
            message: Some(format!("{index}/{total}")),
        })
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
    execute!(
        out,
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
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
