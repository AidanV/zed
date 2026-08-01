//! Handing the terminal to a child process (SPEC §7.1).
//!
//! `ted` runs other terminal programs by giving up the tty, waiting, and taking
//! it back. It is the primitive behind `:!`, and the one `:Explore` and any
//! later `:Git`-style integration are bindings on top of.
//!
//! Two things make it more than "restore the mode, spawn, enter again". The
//! thread reading stdin has to stop, *provably*, before the child starts, or
//! both processes read the same file descriptor and the user's keystrokes are
//! split between them at random. And nothing about the terminal survives a
//! child that may have pushed its own keyboard flags, changed the cursor shape
//! or painted over every cell, so resume re-asserts the terminal rather than
//! assuming any of it came back as it was left.

use std::ffi::OsString;
use std::io::{Stdout, Write as _, stdout};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use crossterm::event::{Event, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::channel::mpsc::UnboundedSender;
use gpui::AsyncApp;
use parking_lot::{Condvar, Mutex};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use util::ResultExt as _;
use util::command::Stdio;

use crate::frame::{enter_terminal_mode, force_full_repaint, restore_terminal_mode};

/// How long the reader waits for input before looking at whether it has been
/// asked to park, and so the worst-case latency of the handoff. 50ms is
/// invisible to a user.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long [`Reader::park`] waits for the acknowledgement. A reader that has
/// not answered by then is presumed stuck inside `read` — parsing a partial
/// escape sequence, say — and starting a child while that is true is exactly
/// the input-stealing race the handshake exists to prevent, so the suspension
/// is abandoned instead.
const ACKNOWLEDGEMENT_TIMEOUT: Duration = Duration::from_secs(2);

/// The thread that reads terminal events, and the handshake that stops it.
pub struct Reader {
    control: Arc<Control>,
}

#[derive(Default)]
struct Control {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Set by the frame loop when it wants stdin to itself.
    paused: bool,
    /// Set by the reader from the one point it is provably outside `read`.
    parked: bool,
    stopped: bool,
}

impl Reader {
    /// A plain OS thread rather than an event source wired into three different
    /// run loops. The channel send is what wakes the foreground executor —
    /// `examples/wake_probe.rs` measures that path.
    pub fn spawn(events: UnboundedSender<Event>) -> Self {
        let control = Arc::new(Control::default());
        std::thread::spawn({
            let control = control.clone();
            move || {
                read_events(&control, events);
                let mut state = control.state.lock();
                state.stopped = true;
                control.changed.notify_all();
            }
        });
        Self { control }
    }

    /// Stops the reader consuming stdin, and waits until it says that it has.
    ///
    /// The wait is the mechanism; the flag alone is not. `crossterm::event::read`
    /// blocks and cannot be cancelled, so a reader that has not acknowledged may
    /// still be inside one, and would go on swallowing input the child is
    /// waiting for (SPEC §7.1).
    pub fn park(&self) -> Result<Parked<'_>> {
        let deadline = Instant::now() + ACKNOWLEDGEMENT_TIMEOUT;
        let mut state = self.control.state.lock();
        state.paused = true;

        // A reader that has stopped altogether is not reading stdin either,
        // which is all the handshake is really asking about.
        while !state.parked && !state.stopped {
            if self
                .control
                .changed
                .wait_until(&mut state, deadline)
                .timed_out()
                && !state.parked
                && !state.stopped
            {
                state.paused = false;
                self.control.changed.notify_all();
                anyhow::bail!("the terminal reader did not stop reading stdin");
            }
        }
        drop(state);

        Ok(Parked {
            control: &self.control,
        })
    }
}

/// Restarts the reader when dropped, so no path out of a suspension — including
/// an early `?` — can leave `ted` deaf to its own terminal.
pub struct Parked<'a> {
    control: &'a Control,
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        let mut state = self.control.state.lock();
        state.paused = false;
        self.control.changed.notify_all();
    }
}

impl Control {
    fn park_while_paused(&self) {
        let mut state = self.state.lock();
        if !state.paused {
            return;
        }
        state.parked = true;
        self.changed.notify_all();
        while state.paused {
            self.changed.wait(&mut state);
        }
        state.parked = false;
    }
}

fn read_events(control: &Control, events: UnboundedSender<Event>) {
    loop {
        match crossterm::event::poll(POLL_INTERVAL) {
            // Reading only once `poll` reports input is what makes parking
            // possible at all: a blocking `read` cannot be cancelled, so a
            // reader that entered one is committed to consuming whatever
            // arrives next, whoever it was meant for.
            Ok(true) => match crossterm::event::read() {
                Ok(event) => {
                    // `REPORT_EVENT_TYPES` makes the terminal report releases,
                    // and nothing downstream acts on one: each would cost a
                    // draw and a projection, and would clear a notification the
                    // press that raised it had only just put on screen.
                    if matches!(&event, Event::Key(key) if matches!(key.kind, KeyEventKind::Release))
                    {
                        continue;
                    }
                    if events.unbounded_send(event).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    log::error!("terminal reader stopped: {error}");
                    return;
                }
            },
            // The one point in the loop where the thread is provably outside
            // `read`, and so the only place it can promise to stay out of one.
            Ok(false) => control.park_while_paused(),
            Err(error) => {
                log::error!("terminal reader stopped: {error}");
                return;
            }
        }
    }
}

/// A program to hand the terminal to.
pub struct Child {
    /// What to call it in messages: what the user asked for, rather than the
    /// shell `ted` may be running it through.
    pub label: String,
    /// `OsString` rather than `String` because `:Explore` substitutes paths
    /// into its arguments (SPEC §13.4), and a path is not required to be UTF-8.
    pub program: OsString,
    pub arguments: Vec<OsString>,
    /// Whether to wait for a keypress before taking the screen back. A command
    /// that printed to the screen needs it or its output vanishes in the time
    /// it takes to draw one frame; a full-screen program that has already had
    /// the user's attention does not.
    pub wait_for_key: bool,
}

impl Child {
    /// A command line run through the user's shell, so pipes, redirection,
    /// quoting and `&&` mean at `:!` what they mean at a prompt.
    pub fn shell(command: &str) -> Self {
        Self {
            label: format!("!{command}"),
            program: util::shell::get_system_shell().into(),
            arguments: vec!["-c".into(), command.into()],
            wait_for_key: true,
        }
    }
}

/// Runs `child` with the terminal to itself, then takes the terminal back.
///
/// The returned message is what the user should be told, if anything: a child
/// that fails to start or exits non-zero is reported rather than swallowed
/// (SPEC §7.1). An `Err`, by contrast, means the terminal could not be set up
/// again and there is nothing left to paint into.
pub async fn run(
    child: Child,
    reader: &Reader,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    cx: &mut AsyncApp,
) -> Result<Option<String>> {
    let parked = match reader.park() {
        Ok(parked) => parked,
        // Nothing has been handed over yet, so the terminal is still `ted`'s
        // and this is a report rather than a failure.
        Err(error) => return Ok(Some(format!("{}: {error}", child.label))),
    };

    // The child inherits the cursor as `ted` left it, and `ted` hides it
    // whenever the editor has no cursor to place.
    terminal.show_cursor().log_err();
    restore_terminal_mode();
    // Raw mode is what makes ctrl-C a keystroke; for as long as `ted` is out of
    // it, the terminal turns ctrl-C back into a signal.
    let signals = TerminalSignals::handle();

    let message = match spawn_and_wait(&child).await {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("{}: {status}", child.label)),
        Err(error) => Some(format!("{}: {error}", child.label)),
    };

    if child.wait_for_key {
        acknowledge(message.as_deref(), cx).await.log_err();
    }

    enter_terminal_mode()?;
    drop(signals);
    // With the terminal raw again, anything it is still holding was typed at
    // the child; the reader would not see it until the next keystroke anyway.
    discard_typeahead();
    // Ratatui's diff buffer describes the screen as it was before the child
    // painted over it, so it no longer describes anything.
    force_full_repaint(terminal);
    drop(parked);

    Ok(message)
}

/// Awaiting the child rather than waiting on it is what keeps GPUI running for
/// its whole lifetime: language servers, file watching and the rest should not
/// stall just because something else owns the screen (SPEC §7.1).
async fn spawn_and_wait(child: &Child) -> Result<ExitStatus> {
    let mut command = util::command::new_command(&child.program);
    command
        .args(&child.arguments)
        // The point of the whole exercise: the child gets `ted`'s terminal
        // rather than a pipe. Spelt out because the default differs by platform.
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    command
        .status()
        .await
        .with_context(|| format!("could not run {}", child.program.display()))
}

/// Waits for a keypress before the screen is taken back. Vim prompts here for
/// the same reason: whatever the child printed is still on the screen `ted` is
/// about to paint over.
async fn acknowledge(message: Option<&str>, cx: &mut AsyncApp) -> Result<()> {
    // Raw mode first, and before the prompt is printed, so that "any key" means
    // any key rather than a line the terminal buffers until enter — and so that
    // the key the prompt asks for is typed into a terminal that is already
    // delivering keys one at a time (see `discard_typeahead`).
    enable_raw_mode()?;
    discard_typeahead();

    let mut out = stdout();
    match message {
        Some(message) => write!(out, "\r\n[ted] {message} — press any key")?,
        None => write!(out, "\r\n[ted] press any key")?,
    }
    out.flush()?;

    let waited = wait_for_key(cx).await;
    disable_raw_mode().log_err();
    waited
}

/// Waits until the terminal has a byte for `ted`, without asking crossterm.
///
/// Any byte will do here, so nothing needs parsing — and asking crossterm would
/// be worse than unnecessary. `crossterm::event::poll` is edge-triggered
/// underneath (mio's epoll), and its event source reports a pending SIGWINCH
/// without draining the terminal, so a keypress that arrives while a resize is
/// waiting to be reported has its readiness consumed by the resize and is never
/// reported again. `poll(2)` is level-triggered: it answers "is there a byte",
/// not "has one just arrived".
async fn wait_for_key(cx: &mut AsyncApp) -> Result<()> {
    loop {
        if has_terminal_input() {
            return Ok(());
        }
        // Awaited on the executor's own timer, so a prompt nobody is looking at
        // still doesn't stop GPUI.
        cx.background_executor().timer(POLL_INTERVAL).await;
    }
}

/// Drops whatever the user typed while the child owned the terminal.
///
/// It is not `ted`'s input — it was typed at the child — and after a suspension
/// it is worse than stale, it is *invisible*: input that arrives while the
/// terminal is in canonical mode is held by the line discipline, which reports
/// nothing readable until the line is complete, and the edge-triggered poll
/// described above never fires for it again once raw mode makes it readable. It
/// would surface only when the next key arrived, an unattributable keystroke
/// behind every one the user typed.
fn discard_typeahead() {
    // The events crossterm has already parsed, as opposed to the bytes the
    // terminal itself is still holding. A poll error means the terminal is not
    // answering at all, which the reader thread reports on its own; either way
    // there is nothing left here to drain.
    while crossterm::event::poll(Duration::ZERO).unwrap_or(false) {
        if crossterm::event::read().is_err() {
            break;
        }
    }
    discard_terminal_input();
}

#[cfg(unix)]
fn has_terminal_input() -> bool {
    let mut watch = libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    };
    // safety: `watch` is a single valid `pollfd` and the call does not block.
    unsafe { libc::poll(&mut watch, 1, 0) > 0 }
}

#[cfg(unix)]
fn discard_terminal_input() {
    // safety: `tcflush` only touches the terminal's own input queue.
    unsafe {
        libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
    }
}

/// Windows is out of scope (SPEC §23); crossterm's own poll is the fallback
/// there, edge triggering and all.
#[cfg(not(unix))]
fn has_terminal_input() -> bool {
    crossterm::event::poll(Duration::ZERO).unwrap_or(false)
}

#[cfg(not(unix))]
fn discard_terminal_input() {}

/// The terminal's own ctrl-C and ctrl-\ reach every process in the foreground
/// process group, `ted` included, and `ted`'s default disposition would
/// terminate it — taking the user's unsaved buffers with it.
///
/// The replacement is a handler that does nothing rather than `SIG_IGN`, and
/// the difference is load-bearing: `exec` resets a *handled* signal to its
/// default in the child but preserves an *ignored* one, so ignoring here would
/// leave ctrl-C doing nothing to the child either.
#[cfg(unix)]
struct TerminalSignals {
    interrupt: libc::sighandler_t,
    quit: libc::sighandler_t,
}

#[cfg(unix)]
extern "C" fn discard_signal(_signal: libc::c_int) {}

#[cfg(unix)]
impl TerminalSignals {
    fn handle() -> Self {
        // safety: the handler does nothing, which is async-signal-safe, and
        // both dispositions are put back when this value is dropped.
        unsafe {
            let discard = discard_signal as *const () as libc::sighandler_t;
            Self {
                interrupt: libc::signal(libc::SIGINT, discard),
                quit: libc::signal(libc::SIGQUIT, discard),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for TerminalSignals {
    fn drop(&mut self) {
        // safety: as above; these are the dispositions `handle` replaced.
        unsafe {
            libc::signal(libc::SIGINT, self.interrupt);
            libc::signal(libc::SIGQUIT, self.quit);
        }
    }
}

/// Windows is out of scope (SPEC §23), and has no foreground process group for
/// a terminal signal to reach in the first place.
#[cfg(not(unix))]
struct TerminalSignals;

#[cfg(not(unix))]
impl TerminalSignals {
    fn handle() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// The property the whole handshake exists for: `park` does not return
    /// until the reader has said it is outside `read`.
    ///
    /// A stand-in thread plays the reader, because the real one needs a
    /// terminal to poll and the race it guards against is only visible from the
    /// pty harness (SPEC §20.3).
    #[test]
    fn parking_waits_for_the_acknowledgement() {
        let control = Arc::new(Control::default());
        let reader = Reader {
            control: control.clone(),
        };
        let reading = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = std::thread::spawn({
            let reading = reading.clone();
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::SeqCst) {
                    // Stands in for `read`: the stretch of the loop where the
                    // reader is committed to consuming whatever arrives.
                    std::thread::sleep(Duration::from_millis(10));
                    reading.store(false, Ordering::SeqCst);
                    control.park_while_paused();
                    reading.store(true, Ordering::SeqCst);
                }
            }
        });

        let parked = reader.park().expect("the reader never acknowledged");
        assert!(
            !reading.load(Ordering::SeqCst),
            "park returned while the reader could still be inside a read"
        );

        drop(parked);
        stop.store(true, Ordering::SeqCst);
        thread.join().ok();
    }

    #[test]
    fn a_stopped_reader_needs_no_acknowledgement() {
        let control = Arc::new(Control::default());
        let reader = Reader {
            control: control.clone(),
        };
        control.state.lock().stopped = true;

        reader
            .park()
            .expect("a reader that has stopped is not reading stdin");
    }

    #[test]
    fn a_shell_child_is_labelled_by_what_the_user_typed() {
        let child = Child::shell("git commit -v");
        assert_eq!(child.label, "!git commit -v");
        assert_eq!(
            child.arguments,
            vec![OsString::from("-c"), OsString::from("git commit -v")]
        );
        assert!(child.wait_for_key);
    }
}
