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
use gpui::{App, AppContext as _, AsyncApp, Entity};
use ratatui::Terminal;
use ratatui::backend::{Backend as _, CrosstermBackend};
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::actions::Surface;
use crate::bootstrap::{self, Backend};
use crate::command_line::{CommandLine, Effect, HostCommand, Update};
use crate::config::{self, Config};
use crate::explore;
use crate::hover;
use crate::input::keystroke_for;
use crate::overlay::{self, Overlay};
use crate::palette::{ColorDepth, Palette};
use crate::platform::{TerminalPlatform, TerminalWindowState, set_window_grid};
use crate::render::{render, reserved_rows};
use crate::snapshot::{CommandLineView, CursorShape, PromptView, StatusView, ViewSnapshot};
use crate::suspend::{self, Reader};

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

    set_window_grid(
        columns,
        rows.saturating_sub(reserved_rows(false, false, 0, 0)),
    );
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

/// The terminal itself: what `ted` draws on, what it last drew there, and the
/// thread reading from it. Grouped because a suspension gives up all three at
/// once and has to re-assert all three on the way back (SPEC §7.1).
struct Tty {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    /// The snapshot currently on screen, or `None` when the next frame must be
    /// painted whatever it contains.
    painted: Option<ViewSnapshot>,
    reader: Reader,
}

/// The mutable state the loop carries between frames, kept together so the
/// reserved-row count and the lines that justify it cannot drift apart.
struct Session {
    backend: Backend,
    config: Config,
    palette: Palette,
    columns: u16,
    rows: u16,
    command_line: Option<CommandLine>,
    /// `ted`'s own list, while one is open. It owns the keyboard, so a key that
    /// reaches it never reaches GPUI's dispatch tree (SPEC §24.2).
    overlay: Option<Overlay>,
    /// What `shift-k` asked for, until the next key dismisses it (SPEC §24.8).
    hover: Option<hover::Panel>,
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
    let mut tty = Tty {
        terminal,
        painted: None,
        reader: Reader::spawn(sender),
    };

    // Read here rather than during bootstrap so a malformed `ted.json` has a
    // notification line to be reported on (SPEC §9).
    let (config, complaint) = config::load();
    let mut session = Session {
        backend,
        config,
        palette,
        columns,
        rows,
        command_line: None,
        overlay: None,
        hover: None,
        messages: complaint.into_iter().collect(),
        busy_frames: BUSY_FRAMES_AFTER_INPUT,
    };
    let mut reserved = u16::MAX;

    loop {
        // `:q` closes the active item rather than the window, so an empty
        // workspace is what "quit" looks like from here (SPEC §14.3).
        if cx.update(|cx| session.backend.is_empty(cx)) {
            return Ok(());
        }

        // What a surface asked for while the last frame's keystroke was being
        // dispatched: a global action listener recorded it, and this is where it
        // becomes a surface (SPEC §24.2).
        if let Some(surface) = cx.update(crate::actions::take_request) {
            open_surface(&mut session, surface, cx).await;
        }
        if let Some(overlay) = session.overlay.as_mut() {
            overlay.poll();
        }
        if let Some(hover) = session.hover.as_mut() {
            hover.poll();
        }

        let command_line = command_line_view(&session, cx).or_else(|| search_line(&session, cx));
        let prompt = window_state.pending_prompt();
        let overlay = session.overlay.as_ref().map(Overlay::view);

        let mut notifications = session.messages.clone();
        notifications.extend(backend_notifications(&session, cx));

        // A line appearing or disappearing changes how much of the grid the
        // window may use, and that is a resize like any other (SPEC §10.2). An
        // open overlay is not one of them: it floats over the editor's cells
        // (SPEC §24.1).
        let top_rows = cx.update(|cx| session.backend.tab_rows(cx));
        let wanted = reserved_rows(
            command_line.is_some(),
            prompt.is_some(),
            notifications.len(),
            top_rows,
        );
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
        let Ok(snapshot) = build_snapshot(
            &session,
            Reserved {
                rows: reserved,
                top_rows,
            },
            command_line,
            overlay,
            prompt,
            notifications,
            cx,
        ) else {
            return Ok(());
        };
        if tty.painted.as_ref() != Some(&snapshot) {
            paint(&mut tty.terminal, &snapshot, &session.palette)?;
            tty.painted = Some(snapshot);
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
            // The escape hatch, and the only way out without vim. With a command
            // line or a list open it dismisses that instead, which is both what
            // vim does and what stops a stray `:` or `ctrl-p` from stranding the
            // user.
            Event::Key(key)
                if is_quit(&key) && session.command_line.is_none() && session.overlay.is_none() =>
            {
                return Ok(());
            }
            Event::Key(key) => {
                // `ted`'s own messages are transient: the next keystroke is the
                // user acknowledging them. A resize is not — it may not even be
                // something the user did.
                session.messages.clear();

                // Read again rather than reuse the frame's copy: the task the
                // last keystroke started may have raised a prompt while this
                // one was still on its way, and the answer belongs to whichever
                // question is open *now*.
                //
                // An unanswered prompt owns the keyboard, because what asked it
                // is waiting on the answer and everything the key would
                // otherwise reach is downstream of that (SPEC §13.3). Ctrl-C
                // above still gets out, and dropping the sender on the way is a
                // cancel.
                if let Some(prompt) = window_state.pending_prompt() {
                    if let Some(answer) = answer_for(&key, &prompt) {
                        window_state.answer_prompt(answer);
                    }
                } else if let Some(command) = handle_key(&mut session, key, cx).await? {
                    suspend_to(&mut session, &mut tty, command, &window_state, reserved, cx)
                        .await?;
                }
            }
            Event::Paste(text) => {
                session.messages.clear();
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
                force_full_repaint(&mut tty.terminal);
                tty.painted = None;
            }
            Event::FocusGained | Event::FocusLost | Event::Mouse(_) => {}
        }
    }
}

/// Hands the terminal to a child process and takes it back (SPEC §7.1).
///
/// Nothing about the terminal is assumed on the way back: the child may have
/// pushed its own keyboard flags or changed the cursor shape, painted over
/// every cell Ratatui believes it knows, and been resized without `ted` ever
/// hearing the `Event::Resize` its parked reader was not there to receive.
async fn suspend_to(
    session: &mut Session,
    tty: &mut Tty,
    command: HostCommand,
    window_state: &TerminalWindowState,
    reserved: u16,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (child, explore) = match command {
        HostCommand::Shell(command) => (suspend::Child::shell(&command), None),
        HostCommand::Explore => {
            match cx.update(|cx| explore::prepare(&session.config, &session.backend, cx)) {
                Ok((child, explore)) => (child, Some(explore)),
                // Nothing has been handed over yet, so there is still a screen to
                // say so on.
                Err(error) => {
                    session.messages.push(format!("Explore: {error}"));
                    return Ok(());
                }
            }
        }
    };

    let message = suspend::run(child, &tty.reader, &mut tty.terminal, cx).await?;
    session.messages.extend(message);
    tty.painted = None;

    // After the terminal is back, so a failure to open has somewhere to be
    // reported and the buffers it opens are drawn on the next frame.
    if let Some(explore) = explore {
        session
            .messages
            .extend(explore.open_selection(&session.backend, cx).await);
    }

    if let Some((columns, rows)) = crossterm::terminal::size().log_err() {
        session.columns = columns;
        session.rows = rows;
    }
    resize_window(session, reserved, window_state);
    session.busy_frames = BUSY_FRAMES_AFTER_INPUT;
    Ok(())
}

/// Routes a keystroke either into `ted`'s own `:` line or into GPUI's dispatch
/// tree. The `:` line is the only thing `ted` handles itself; everything else,
/// including `/` search, belongs to the backend.
///
/// Returns a host command when the keystroke asked for one, since running it
/// needs the terminal itself rather than anything reachable from here.
async fn handle_key(
    session: &mut Session,
    key: KeyEvent,
    cx: &mut AsyncApp,
) -> Result<Option<HostCommand>> {
    // The panel dismisses on the next key, whatever that key goes on to do
    // (SPEC §24.8).
    session.hover = None;

    // Exactly one owner per keystroke, and `ted`'s own surfaces sit in front of
    // GPUI: a key that reaches an overlay never reaches the dispatch tree
    // (SPEC §24.2).
    if session.overlay.is_some() {
        handle_overlay_key(session, key, cx).await;
        return Ok(None);
    }
    if session.command_line.is_some() {
        return handle_command_line_key(session, key, cx).await;
    }

    if opens_command_line(session, &key, cx) {
        let Some(editor) = active_editor(session, cx) else {
            return Ok(None);
        };
        let prefix = cx.update(|cx| crate::command_line::prefix_for(&editor, cx));
        let mut command_line = CommandLine::new(prefix);
        command_line
            .refresh(session.backend.workspace.downgrade(), cx)
            .await;
        session.command_line = Some(command_line);
        return Ok(None);
    }

    let Some(keystroke) = keystroke_for(&key) else {
        return Ok(None);
    };
    // Deliberately `App::update_window` rather than `WindowHandle::update`: the
    // latter holds a mutable borrow of the root view for the duration of the
    // closure, and dispatch routes into handlers that update that same entity.
    cx.update_window(session.backend.window.into(), |_, window, cx| {
        window.dispatch_keystroke(keystroke, cx);
    })
    .ok();
    Ok(None)
}

/// Everything an open list answers to (SPEC §24.2). Nothing here reaches GPUI:
/// the overlay is in front of it for as long as it is open.
async fn handle_overlay_key(session: &mut Session, key: KeyEvent, cx: &mut AsyncApp) {
    let Some(overlay) = session.overlay.as_mut() else {
        return;
    };

    match overlay.handle_key(&key) {
        overlay::Update::Unchanged => {}
        overlay::Update::QueryChanged => {
            cx.update(|cx| overlay.refresh(&session.backend, cx));
        }
        // Leaving the editor exactly as it was, which is what makes `esc` a
        // no-op rather than a state change (SPEC §24.6).
        overlay::Update::Cancel => session.overlay = None,
        overlay::Update::Confirm(split) => {
            let opening = overlay.confirm(split, &session.backend, cx);
            session.overlay = None;
            if let Err(error) = opening.await {
                session.messages.push(format!("could not open: {error}"));
            }
        }
    }
}

async fn handle_command_line_key(
    session: &mut Session,
    key: KeyEvent,
    cx: &mut AsyncApp,
) -> Result<Option<HostCommand>> {
    let Some(command_line) = session.command_line.as_mut() else {
        return Ok(None);
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
            // The bare number prompt has no interceptor behind it: it is the
            // only go-to-line code `ted` has, and it exists only for a session
            // with no `:` line at all (SPEC §24.5).
            if let Some(line) = command_line.line_number() {
                session.command_line = None;
                if let Err(error) = crate::overlay::go_to_line(&session.backend, line, cx) {
                    session.messages.push(format!("go to line: {error}"));
                }
                return Ok(None);
            }

            let effect = command_line.selected_effect();
            let query = command_line.query().to_owned();
            session.command_line = None;
            match effect {
                // The second place SPEC §24.2's table is consulted: `:ls`
                // resolves inside vim's interceptor to `tab_switcher::ToggleAll`,
                // an action no keymap pass ever saw, and dispatching it would
                // open a modal `ted` cannot paint.
                Some(Effect::Dispatch(action)) => {
                    match crate::actions::surface_for(action.as_ref()) {
                        Some(surface) => open_surface(session, surface, cx).await,
                        None => {
                            cx.update_window(session.backend.window.into(), |_, window, cx| {
                                window.dispatch_action(action, cx);
                            })
                            .ok();
                        }
                    }
                }
                Some(Effect::Host(command)) => return Ok(Some(command)),
                None => session.messages.push(format!("not a command: :{query}")),
            }
        }
    }
    Ok(None)
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

/// Which answer a keystroke picks from an open prompt, or `None` when it picks
/// none and the prompt stays up (SPEC §13.3).
///
/// The digits are the answers as they are painted, numbered from 1. `enter`
/// takes the first, which is the answer GPUI treats as the default and the one
/// a platform dialog would have focused. `esc` takes the last: every prompt
/// Zed raises on this path ends in `Cancel`, and a terminal user reaching for
/// escape means to back out, not to save.
fn answer_for(key: &KeyEvent, prompt: &PromptView) -> Option<usize> {
    let answers = prompt.answers.len();
    if matches!(key.kind, KeyEventKind::Release) || answers == 0 {
        return None;
    }

    match key.code {
        KeyCode::Enter => Some(0),
        KeyCode::Esc => Some(answers - 1),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            let chosen = character.to_digit(10)?.checked_sub(1)? as usize;
            (chosen < answers).then_some(chosen)
        }
        _ => None,
    }
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
    // Not `snapshot.cursor` directly: while one of `ted`'s own surfaces owns the
    // keyboard the cursor belongs in the field being typed in, and the renderer
    // is what knows where that row is (SPEC §24.1).
    let (position, shape) = crate::render::cursor(snapshot);
    terminal.draw(|frame| {
        render(snapshot, palette, frame.buffer_mut());
        if let Some(cursor) = position {
            frame.set_cursor_position((cursor.column, cursor.row));
        }
    })?;
    if position.is_some() {
        terminal.show_cursor().ok();
    } else {
        terminal.hide_cursor().ok();
    }
    set_cursor_shape(shape);
    Ok(())
}

/// Clears the screen and throws away Ratatui's record of what was on it, so the
/// next draw emits every cell rather than the handful that changed.
///
/// `Terminal::clear` would do both, but only after asking the terminal where
/// its cursor is and waiting on stdin for the answer — and it skips the reset
/// when no answer comes, which is exactly the case that needs it most.
pub(crate) fn force_full_repaint(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>) {
    terminal.backend_mut().clear().log_err();
    terminal.swap_buffers();
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

/// How much of the grid `ted` kept for itself this frame, and how much of that
/// sits above the editor — which the projection needs separately, because rows
/// withheld at the top also shift every rect the editor reports (SPEC §24.7).
#[derive(Clone, Copy)]
struct Reserved {
    rows: u16,
    top_rows: u16,
}

fn build_snapshot(
    session: &Session,
    reserved: Reserved,
    command_line: Option<CommandLineView>,
    overlay: Option<crate::snapshot::OverlayView>,
    prompt: Option<PromptView>,
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
                reserved_rows: reserved.rows,
                top_rows: reserved.top_rows,
                command_line,
                overlay,
                hover: session
                    .hover
                    .as_ref()
                    .map(|hover| hover.contents().to_vec())
                    .unwrap_or_default(),
                prompt,
                notifications,
                workspace: &session.backend.workspace,
            },
            window,
            cx,
        )
    })
    .context("window closed while building a frame")
}

/// The `:` line as it is painted, with the keybinding for whatever `enter` would
/// dispatch: `Window::keystroke_text_for` needs a window, and the selected
/// candidate changes without a refresh (SPEC §24.3).
fn command_line_view(session: &Session, cx: &mut AsyncApp) -> Option<CommandLineView> {
    let command_line = session.command_line.as_ref()?;
    let mut view = command_line.view();
    if let Some(action) = command_line.selected_action()
        && let Ok(Some(keystrokes)) =
            cx.update_window(session.backend.window.into(), |_, window, _| {
                window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .map(|binding| {
                        binding
                            .keystrokes()
                            .iter()
                            .map(|keystroke| keystroke.inner().unparse())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            })
    {
        // Ahead of the count, because showing it is what teaches you that
        // `ctrl-s` was faster than typing `:save` out.
        view.trailing.insert(0, keystrokes);
    }
    Some(view)
}

/// Opens whichever of `ted`'s own surfaces an action asked for (SPEC §24.2).
async fn open_surface(session: &mut Session, surface: Surface, cx: &mut AsyncApp) {
    match surface {
        Surface::Finder | Surface::Switcher => {
            session.overlay = cx.update(|cx| Overlay::open(surface, &session.backend, cx));
        }
        Surface::GoToLine => session.command_line = Some(CommandLine::go_to_line()),
        Surface::Hover => {
            let window = session.backend.window.into();
            session.hover = cx
                .update_window(window, |_, window, cx| {
                    hover::Panel::open(&session.backend, window, cx)
                })
                .ok()
                .flatten()
                .filter(|panel| !panel.is_empty());
        }
    }
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
            message: Some(format!("{index}/{total}")),
            ..Default::default()
        })
    })
}

/// Idempotent, because a suspension leaves and re-enters terminal mode around
/// the child, and re-emits all of this unconditionally rather than trusting
/// that the child left the flag stack as it found it (SPEC §7.1).
pub(crate) fn enter_terminal_mode() -> Result<()> {
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

/// Idempotent, because it runs from the panic hook, from normal shutdown and
/// from a suspension. `ted` owns the terminal's mode, so leaving it raw would
/// hand the user a broken shell (SPEC §3.1, §7).
pub(crate) fn restore_terminal_mode() {
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
