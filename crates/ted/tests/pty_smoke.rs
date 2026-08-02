//! The end-to-end half of M1's acceptance (SPEC §20.3, §21): run the real `ted`
//! binary inside a pty, type at it, and assert the grid it draws.
//!
//! This is the only test that exercises the terminal's modes and escape
//! sequences, the reader-thread-to-run-loop bridge, and the workspace-level
//! features — `:w` through vim's interceptor, `/` through `BufferSearchBar` —
//! since all of those need a real `Project` and a real run loop. In-process
//! behaviour is covered by `tests/editing_session.rs`.
//!
//! Every run gets its own `--user-data-dir`, so the test never reads or writes
//! the settings, keymap or workspace database a real Zed shares with `ted`.

use std::io::{Read as _, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Context as _;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

const COLUMNS: u16 = 80;
const ROWS: u16 = 24;

/// Caps on waiting for the screen to stop changing. A cold start loads a real
/// `Project`, scans a worktree and parses with tree-sitter, so it is much slower
/// than anything that follows a keystroke.
const STARTUP: Duration = Duration::from_secs(45);
const AFTER_KEYS: Duration = Duration::from_secs(10);
/// How long the screen must stay unchanged before it counts as settled.
const QUIET: Duration = Duration::from_millis(750);

struct Terminal {
    writer: Box<dyn Write + Send>,
    output: mpsc::Receiver<Vec<u8>>,
    screen: Screen,
    /// Every byte `ted` has written, which is the only way to assert on the
    /// terminal *modes* it set: they leave no trace on the screen.
    transcript: Vec<u8>,
    rows: u16,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Terminal {
    fn open(
        file: &std::path::Path,
        data_dir: &std::path::Path,
        extra: &[&str],
    ) -> anyhow::Result<Self> {
        let pty = native_pty_system().openpty(PtySize {
            rows: ROWS,
            cols: COLUMNS,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ted"));
        command.arg(file);
        command.arg("--user-data-dir");
        command.arg(data_dir);
        // A child `ted` suspends to inherits this, so a scripted one can record
        // what it did next to the file under test using a relative path.
        if let Some(parent) = file.parent() {
            command.cwd(parent);
        }
        for argument in extra {
            command.arg(argument);
        }
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // A real `Client` would otherwise try to reach the production server.
        command.env("ZED_SERVER_URL", "http://127.0.0.1:1");
        let child = pty.slave.spawn_command(command)?;
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 || sender.send(chunk[..read].to_vec()).is_err() {
                    return;
                }
            }
        });

        Ok(Self {
            writer,
            output,
            screen: Screen::new(COLUMNS, ROWS),
            transcript: Vec::new(),
            rows: ROWS,
            child,
            _master: pty.master,
        })
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.transcript.extend_from_slice(chunk);
        self.screen.feed(chunk);
    }

    /// Feeds output into the screen model until it has been quiet for [`QUIET`],
    /// giving up after `timeout`. Waiting for quiet rather than a fixed sleep is
    /// what keeps this test at a few seconds a case instead of a few minutes,
    /// and it is also more honest: `ted` repaints when something changed.
    fn settle(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        // Quiet before anything is on screen is just a slow start, not a
        // settled screen. `ted` emits its terminal-mode escapes immediately and
        // then takes seconds to reach its first frame, so "some bytes arrived"
        // is not the signal — "some cell has content" is.
        let mut quiet_since = Instant::now();
        while Instant::now() < deadline {
            match self.output.recv_timeout(QUIET) {
                Ok(chunk) => {
                    self.feed(&chunk);
                    quiet_since = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !self.screen.is_blank() && quiet_since.elapsed() >= QUIET {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn send(&mut self, keys: &str) {
        self.type_keys(keys);
        self.settle(AFTER_KEYS);
    }

    /// Sends `keys` and keeps settling until `ready` holds.
    ///
    /// One settle is enough for anything the editor answers synchronously, but
    /// not for a picker that matches on a background thread: the first quiet
    /// screen after the keystroke is the query with an empty list under it,
    /// which is a settled frame and the wrong one to assert on.
    fn send_until(&mut self, keys: &str, ready: impl Fn(&Screen) -> bool) {
        self.type_keys(keys);
        let deadline = Instant::now() + AFTER_KEYS;
        while Instant::now() < deadline {
            self.settle(AFTER_KEYS);
            if ready(&self.screen) {
                return;
            }
        }
    }

    /// Types without waiting for the screen to settle. While a child owns the
    /// terminal `ted` paints nothing, so there is no frame to wait for and the
    /// keys are not `ted`'s to answer.
    fn type_keys(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).ok();
        self.writer.flush().ok();
    }

    /// Waits for a scripted child to record that it reached a point, still
    /// draining output so a full pty buffer can never be what blocks it.
    fn wait_for_file(&mut self, path: &std::path::Path, timeout: Duration) -> anyhow::Result<()> {
        self.wait_until(path, |_| true, timeout)
    }

    /// Waits for a scripted child to record *what* it did. The shell creates
    /// the file when it opens the redirect and writes to it afterwards, so
    /// existence alone does not mean the contents are there yet.
    fn wait_for_contents(
        &mut self,
        path: &std::path::Path,
        wanted: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        self.wait_until(path, |contents| contents.trim() == wanted, timeout)
            .map_err(|_| {
                let contents = std::fs::read_to_string(path).unwrap_or_default();
                anyhow::anyhow!("{} holds {contents:?}, not {wanted:?}", path.display())
            })
    }

    fn wait_until(
        &mut self,
        path: &std::path::Path,
        ready: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Ok(contents) = std::fs::read_to_string(path)
                && ready(&contents)
            {
                return Ok(());
            }
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => self.feed(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        anyhow::bail!("{} never appeared", path.display())
    }

    fn transcript_mark(&self) -> usize {
        self.transcript.len()
    }

    /// Waits for `ted` to write something in particular.
    ///
    /// This is how a suspended `ted` is observed at all: it is not painting, so
    /// the screen model has nothing to say, and it is out of raw mode, so a key
    /// typed before it asks for one sits in the terminal's line buffer where no
    /// reader can see it until the next byte arrives.
    fn wait_for_output(&mut self, needle: &str, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.transcript_since(0).contains(needle) {
                return Ok(());
            }
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => self.feed(&chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        anyhow::bail!("ted never wrote {needle:?}")
    }

    fn transcript_since(&self, mark: usize) -> String {
        String::from_utf8_lossy(self.transcript.get(mark..).unwrap_or_default()).into_owned()
    }

    /// Resizes the pty, which delivers SIGWINCH to `ted` exactly as a terminal
    /// emulator would.
    fn resize(&mut self, columns: u16, rows: u16) {
        self.resize_quietly(columns, rows);
        self.settle(AFTER_KEYS);
    }

    /// Resizes without waiting for a frame. While `ted` is suspended nothing
    /// repaints, so waiting would only burn the timeout.
    fn resize_quietly(&mut self, columns: u16, rows: u16) {
        self._master
            .resize(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .ok();
        self.screen = Screen::new(columns, rows);
        self.rows = rows;
    }

    fn row(&self, index: u16) -> String {
        self.screen.row(index)
    }

    /// The row `ted` reserves for its status line, which is always the last one.
    fn status(&self) -> String {
        self.screen.row(self.rows - 1)
    }

    fn exited(&mut self, timeout: Duration) -> bool {
        self.exit_status(timeout).is_some()
    }

    /// How `ted` exited, which is not the same question as whether it did: a
    /// shutdown that panics — on leaked entity handles, say — still exits.
    fn exit_status(&mut self, timeout: Duration) -> Option<portable_pty::ExitStatus> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    // Keep draining, or the child can block writing its final
                    // frame into a full pty buffer and never reach exit.
                    self.output.recv_timeout(Duration::from_millis(100)).ok();
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Just enough of a terminal to reconstruct what is on screen: absolute cursor
/// positioning, erases, and newline handling. Colours and other SGR attributes
/// are parsed only so they do not land in the grid as text.
struct Screen {
    columns: u16,
    rows: u16,
    cells: Vec<char>,
    cursor: (u16, u16),
    pending: Vec<u8>,
}

impl Screen {
    fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            cells: vec![' '; usize::from(columns) * usize::from(rows)],
            cursor: (0, 0),
            pending: Vec::new(),
        }
    }

    fn row(&self, index: u16) -> String {
        let start = usize::from(index) * usize::from(self.columns);
        let end = start + usize::from(self.columns);
        self.cells
            .get(start..end)
            .map(|row| row.iter().collect::<String>())
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }

    fn put(&mut self, character: char) {
        let (column, row) = self.cursor;
        if row < self.rows && column < self.columns {
            let index = usize::from(row) * usize::from(self.columns) + usize::from(column);
            if let Some(cell) = self.cells.get_mut(index) {
                *cell = character;
            }
        }
        self.cursor.0 = self.cursor.0.saturating_add(1).min(self.columns);
    }

    fn erase_all(&mut self) {
        self.cells.fill(' ');
    }

    fn is_blank(&self) -> bool {
        self.cells.iter().all(|cell| *cell == ' ')
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let buffered = std::mem::take(&mut self.pending);
        // A chunk can end mid-sequence or mid-codepoint; whatever is left over
        // is put back for the next chunk to complete.
        let text = String::from_utf8_lossy(&buffered).into_owned();
        let mut characters = text.chars().peekable();

        while let Some(character) = characters.next() {
            match character {
                '\u{1b}' => {
                    let Some(&next) = characters.peek() else {
                        self.pending.extend_from_slice("\u{1b}".as_bytes());
                        return;
                    };
                    characters.next();
                    match next {
                        '[' => {
                            let mut sequence = String::new();
                            loop {
                                let Some(byte) = characters.next() else {
                                    return;
                                };
                                if byte.is_ascii_alphabetic() {
                                    self.control(&sequence, byte);
                                    break;
                                }
                                sequence.push(byte);
                            }
                        }
                        // OSC: everything up to BEL or ST is not screen content.
                        ']' => {
                            for byte in characters.by_ref() {
                                if byte == '\u{7}' || byte == '\u{1b}' {
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                '\r' => self.cursor.0 = 0,
                '\n' => self.cursor.1 = self.cursor.1.saturating_add(1).min(self.rows),
                '\u{7}' => {}
                character => self.put(character),
            }
        }
    }

    fn control(&mut self, parameters: &str, final_byte: char) {
        let numbers: Vec<u16> = parameters
            .trim_start_matches(['?', '>', '<'])
            .split(';')
            .map(|value| value.parse().unwrap_or(0))
            .collect();
        let first = numbers.first().copied().unwrap_or(0);

        match final_byte {
            'H' | 'f' => {
                let row = numbers.first().copied().unwrap_or(1).max(1) - 1;
                let column = numbers.get(1).copied().unwrap_or(1).max(1) - 1;
                self.cursor = (column.min(self.columns), row.min(self.rows));
            }
            'J' => {
                // Only "erase everything" matters here; `ted` clears the screen
                // on entry and on resize and otherwise repaints cell by cell.
                if first == 2 || first == 3 {
                    self.erase_all();
                }
            }
            'K' => {
                let (column, row) = self.cursor;
                let start = usize::from(row) * usize::from(self.columns);
                let range = match first {
                    1 => start..start + usize::from(column) + 1,
                    2 => start..start + usize::from(self.columns),
                    _ => start + usize::from(column)..start + usize::from(self.columns),
                };
                if let Some(cells) = self.cells.get_mut(range) {
                    cells.fill(' ');
                }
            }
            'C' => self.cursor.0 = (self.cursor.0 + first.max(1)).min(self.columns),
            'D' => self.cursor.0 = self.cursor.0.saturating_sub(first.max(1)),
            'A' => self.cursor.1 = self.cursor.1.saturating_sub(first.max(1)),
            'B' => self.cursor.1 = (self.cursor.1 + first.max(1)).min(self.rows),
            _ => {}
        }
    }
}

struct Fixture {
    directory: std::path::PathBuf,
}

impl Fixture {
    fn new(name: &str, contents: &str) -> anyhow::Result<Self> {
        let directory =
            std::env::temp_dir().join(format!("ted-pty-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(directory.join("data"))?;
        std::fs::write(directory.join("main.rs"), contents)?;
        Ok(Self { directory })
    }

    fn file(&self) -> std::path::PathBuf {
        self.directory.join("main.rs")
    }

    /// Somewhere for a scripted child to record what it did, next to the file
    /// under test and cleaned up with it.
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.directory.join(name)
    }

    fn data_dir(&self) -> std::path::PathBuf {
        self.directory.join("data")
    }

    /// `ted`'s own settings file, beside the `settings.json` it shares with GUI
    /// Zed (SPEC §9).
    fn write_config(&self, contents: &str) -> anyhow::Result<()> {
        let config = self.data_dir().join("config");
        std::fs::create_dir_all(&config)?;
        std::fs::write(config.join("ted.json"), contents)?;
        Ok(())
    }

    /// Sets the `file_manager` argv `:Explore` runs, so the command can be
    /// driven without Yazi or anything else installed.
    fn set_file_manager(&self, argv: &str) -> anyhow::Result<()> {
        self.write_config(&format!(r#"{{ "file_manager": {argv} }}"#))
    }

    fn contents(&self) -> String {
        std::fs::read_to_string(self.file()).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.directory).ok();
    }
}

fn open(name: &str, contents: &str) -> anyhow::Result<(Fixture, Terminal)> {
    open_with(name, contents, &[])
}

fn open_with(name: &str, contents: &str, extra: &[&str]) -> anyhow::Result<(Fixture, Terminal)> {
    let fixture = Fixture::new(name, contents)?;
    let mut terminal = Terminal::open(&fixture.file(), &fixture.data_dir(), extra)?;
    terminal.settle(STARTUP);
    Ok((fixture, terminal))
}

/// Opens `main.rs` *and* the directory around it, so the finder has a real
/// worktree to search: a lone file path makes a single-file worktree containing
/// nothing else.
fn open_project(name: &str, files: &[(&str, &str)]) -> anyhow::Result<(Fixture, Terminal)> {
    let fixture = Fixture::new(name, "fn main() {}\n")?;
    for (path, contents) in files {
        std::fs::write(fixture.path(path), contents)?;
    }
    let mut terminal = Terminal::open(&fixture.file(), &fixture.data_dir(), &["."])?;
    terminal.settle(STARTUP);
    Ok((fixture, terminal))
}

/// `ctrl-p`, which the keymap binds to `file_finder::Toggle` — Zed's own modal,
/// projected into `ted`'s list by SPEC §13.1's Mirror strategy.
const CTRL_P: &str = "\u{10}";

/// `f1`, which the keymap binds to `command_palette::Toggle` alongside
/// `ctrl-shift-p` — and unlike that one, a terminal can express it without the
/// Kitty protocol's disambiguation (SPEC §8.2).
const F1: &str = "\u{1b}OP";

/// SPEC §21/M3: the command palette is Zed's own modal, projected rather than
/// replaced — so the actions in it are the ones Zed really has, filtered by the
/// filter `ted` really installed.
#[test]
fn the_command_palette_is_projected_rather_than_replaced() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("palette", "fn main() {}\n")?;

    terminal.send_until(F1, |screen| screen.row(0).contains("commands"));
    assert!(
        terminal.row(0).contains("commands"),
        "the command palette did not open: {:?}",
        terminal.row(0)
    );

    // Every action Zed has, fuzzy-matched on a background thread — much more
    // work than the finder's scan of one small worktree, so the list arrives a
    // frame or more after the query does.
    let has_undo = |screen: &Screen| (2..6).any(|row| screen.row(row).contains("undo"));
    terminal.send_until("undo", has_undo);
    assert!(
        has_undo(&terminal.screen),
        "the palette's own matches are not in the list: {:?}",
        (0..6).map(|row| terminal.row(row)).collect::<Vec<_>>()
    );

    terminal.send("\u{1b}");
    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "dismissing the palette quit ted"
    );
    assert!(
        terminal.row(0).ends_with("fn main() {}"),
        "the buffer was not repainted under the box: {:?}",
        terminal.row(0)
    );
    Ok(())
}

/// SPEC §24.4, and the first half of §24.10's acceptance: find a file by name
/// and open it.
#[test]
fn the_finder_opens_a_file_by_name() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open_project("finder", &[("beta.rs", "fn beta() {}\n")])?;

    terminal.send(CTRL_P);
    assert!(
        terminal.row(0).contains("files"),
        "the finder did not open: {:?}",
        terminal.row(0)
    );

    terminal.send("beta");
    assert!(
        terminal.row(3).contains("beta.rs"),
        "the match is not in the list: {:?}",
        terminal.row(3)
    );

    terminal.send("\r");
    assert!(
        terminal.status().contains("beta.rs"),
        "the chosen file did not open: {:?}",
        terminal.status()
    );
    Ok(())
}

/// SPEC §24.2: `esc` dismisses the list and leaves the editor exactly as it was,
/// rather than quitting `ted` or opening anything.
#[test]
fn escape_dismisses_the_finder_and_changes_nothing() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open_project("finder-escape", &[("beta.rs", "fn beta() {}\n")])?;

    terminal.send(CTRL_P);
    terminal.send("beta");
    terminal.send("\u{1b}");

    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "dismissing the finder quit ted"
    );
    assert!(
        terminal.status().contains("main.rs"),
        "the editor did not come back: {:?}",
        terminal.status()
    );
    assert!(
        terminal.row(0).ends_with("fn main() {}"),
        "the buffer was not repainted under the box: {:?}",
        terminal.row(0)
    );
    Ok(())
}

/// SPEC §24.6 and §24.7: with more than one item open, the strip says what is
/// there and the switcher says which of them you were in last. `:ls` is the case
/// that forces the surface table to be consulted at dispatch as well as at
/// keymap load — it arrives as an action no keymap pass ever saw.
#[test]
fn the_tab_strip_and_the_switcher_show_what_else_is_open() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open_project("switcher", &[("beta.rs", "fn beta() {}\n")])?;

    terminal.send(CTRL_P);
    terminal.send("beta");
    terminal.send("\r");

    let strip = terminal.row(0);
    assert!(
        strip.contains("main.rs") && strip.contains("beta.rs"),
        "the tab strip does not show both items: {strip:?}"
    );
    assert!(
        terminal.row(1).ends_with("fn beta() {}"),
        "the editor was not shifted down by the strip: {:?}",
        terminal.row(1)
    );

    terminal.send(":ls\r");
    assert!(
        terminal.row(2).contains("beta.rs"),
        "the switcher does not lead with the buffer you are in: {:?}",
        terminal.row(2)
    );
    assert!(
        terminal.row(3).contains("main.rs"),
        "the switcher does not offer the one you came from: {:?}",
        terminal.row(3)
    );

    // The selection starts on the second row, so `enter` goes back.
    terminal.send("\r");
    assert!(
        terminal.status().contains("main.rs"),
        "the switcher did not switch: {:?}",
        terminal.status()
    );

    // `:q` closes a tab and the rest take its place (SPEC §24.7); with one item
    // left there is nothing for a strip to say.
    terminal.send(":q\r");
    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "closing one of two items quit ted"
    );
    assert!(
        terminal.row(0).ends_with("fn beta() {}"),
        "the strip did not go when the second item did: {:?}",
        terminal.row(0)
    );
    Ok(())
}

/// SPEC §24.8: `shift-k` is `editor::Hover`, retargeted onto `ted`'s own panel
/// because the editor's popover is a view `ted` cannot read. With nothing to say
/// about the position, the panel never appears — an empty box over the code
/// would be worse than no answer — and the editor keeps the keyboard.
#[test]
fn shift_k_with_nothing_to_say_puts_nothing_on_screen() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("hover", "alpha\nbeta\n")?;
    let before = terminal.row(0);

    terminal.send("K");
    assert_eq!(
        terminal.row(0),
        before,
        "something was painted over the code"
    );
    assert!(
        terminal.row(1).ends_with("beta"),
        "the row under the cursor was covered: {:?}",
        terminal.row(1)
    );

    terminal.send("j");
    assert!(
        terminal.status().contains("2:1"),
        "the editor did not keep the keyboard: {:?}",
        terminal.status()
    );
    Ok(())
}

/// SPEC §24.5: the only go-to-line code `ted` has. With vim, `:42` parses in
/// vim's own interceptor and there is nothing to build; without it there is no
/// `:` line at all, and `ctrl-g` would otherwise open a modal `ted` cannot
/// paint.
#[test]
fn ctrl_g_jumps_to_a_line_without_vim() -> anyhow::Result<()> {
    let contents = (1..=40)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let (_fixture, mut terminal) = open_with("go-to-line", &contents, &["--no-vim"])?;

    terminal.send("\u{7}");
    terminal.send("20");
    assert!(
        terminal.row(ROWS - 2).starts_with(":20"),
        "no number prompt: {:?}",
        terminal.row(ROWS - 2)
    );

    terminal.send("\r");
    assert!(
        terminal.status().trim_end().ends_with("20:1"),
        "the cursor did not jump: {:?}",
        terminal.status()
    );
    Ok(())
}

/// SPEC §24.3: the rest of the best-matching command, dimmed after the cursor,
/// with `right` accepting it. vim's interceptor reports `:w` as `:write`, so the
/// tail is the part the user did not type.
#[test]
fn the_colon_line_completes_with_ghost_text() -> anyhow::Result<()> {
    let (fixture, mut terminal) = open("ghost", "alpha\nbeta\n")?;

    terminal.send("x");
    terminal.send(":w");
    assert!(
        terminal.row(ROWS - 2).starts_with(":write"),
        "no ghost text on the command line: {:?}",
        terminal.row(ROWS - 2)
    );

    // `right` accepts the ghost into the query, and the query is what runs.
    terminal.send("\u{1b}[C");
    terminal.send("\r");
    assert_eq!(fixture.contents(), "lpha\nbeta\n");
    Ok(())
}

#[test]
fn a_file_is_drawn_with_a_gutter_and_a_status_line() -> anyhow::Result<()> {
    let (_fixture, terminal) = open("draw", "fn main() {\n    let x = 1;\n}\n")?;

    assert!(
        terminal.row(0).ends_with("fn main() {"),
        "first row was {:?}",
        terminal.row(0)
    );
    assert!(
        terminal.row(0).trim_start().starts_with('1'),
        "no line number in the gutter: {:?}",
        terminal.row(0)
    );
    let status = terminal.status();
    assert!(
        status.starts_with("NORMAL main.rs"),
        "status was {status:?}"
    );
    assert!(status.trim_end().ends_with("1:1"), "status was {status:?}");
    Ok(())
}

#[test]
fn vim_motions_move_the_cursor_position_in_the_status_line() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("motions", "alpha\nbeta\ngamma\n")?;

    terminal.send("jjll");
    assert!(
        terminal.status().trim_end().ends_with("3:3"),
        "status was {:?}",
        terminal.status()
    );

    terminal.send("i");
    assert!(
        terminal.status().starts_with("INSERT"),
        "status was {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn a_burst_of_repeats_lands_exactly_where_it_was_typed() -> anyhow::Result<()> {
    let contents = (1..=60)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    let (_fixture, mut terminal) = open("repeat", &contents)?;

    // A held key arrives faster than the loop retires a frame, so the events
    // queue up behind it. Every one still has to land: the failure this guards
    // is the queue being coalesced or truncated to catch up, which would settle
    // the cursor short of where it was typed.
    terminal.send(&"j".repeat(20));
    assert!(
        terminal.status().trim_end().ends_with("21:1"),
        "status was {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn colon_w_saves_through_vims_interceptor() -> anyhow::Result<()> {
    let (fixture, mut terminal) = open("save", "alpha\nbeta\n")?;

    // `x` deletes a character, so the buffer is dirty and the write is visible
    // in the file rather than a no-op.
    terminal.send("x");
    assert!(
        terminal.status().contains("[+]"),
        "the buffer is not marked dirty: {:?}",
        terminal.status()
    );

    terminal.send(":");
    let command_line = terminal.row(ROWS - 2);
    assert!(
        command_line.starts_with(':'),
        "no command line opened: {command_line:?}"
    );

    terminal.send("w\r");
    assert_eq!(fixture.contents(), "lpha\nbeta\n");
    assert!(
        !terminal.status().contains("[+]"),
        "the buffer is still dirty after :w: {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn slash_search_projects_the_buffer_search_bar() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("search", "alpha\nbeta\ngamma\nbeta\n")?;

    terminal.send("/beta");
    let search_line = terminal.row(ROWS - 2);
    assert!(
        search_line.starts_with("/beta"),
        "no search line: {search_line:?}"
    );

    // Submitting is what makes the search bar pick an active match, and so what
    // gives `match_summary` an index to project.
    terminal.send("\r");
    let search_line = terminal.row(ROWS - 2);
    assert!(
        search_line.contains("/2"),
        "no match count in {search_line:?}"
    );
    // Two `beta`s, and the cursor landed on one of them.
    assert!(
        terminal.status().trim_end().ends_with(":1"),
        "the cursor did not move to a match: {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn a_colon_typed_into_the_search_bar_is_text_not_a_command() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("search-colon", "a:b\nplain\n")?;

    terminal.send("/a:");
    let line = terminal.row(ROWS - 2);
    assert!(
        line.starts_with("/a:"),
        "the colon opened a command line instead of extending the query: {line:?}"
    );
    Ok(())
}

#[test]
fn colon_q_quits() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("quit", "alpha\n")?;

    // `:q` maps to `workspace::CloseActiveItem`, which in GUI Zed would leave an
    // empty window; in a terminal the empty workspace *is* the exit condition.
    terminal.send(":q\r");
    let status = terminal
        .exit_status(Duration::from_secs(10))
        .context("ted is still running after :q")?;
    assert!(status.success(), "ted exited badly: {status:?}");
    Ok(())
}

#[test]
fn colon_q_on_a_dirty_buffer_asks_before_quitting() -> anyhow::Result<()> {
    let (fixture, mut terminal) = open("quit_dirty", "alpha\nbeta\n")?;

    // `x` deletes a character, which is what makes the close prompt.
    terminal.send("x");
    assert!(
        terminal.status().contains("[+]"),
        "the buffer is not marked dirty: {:?}",
        terminal.status()
    );

    // `:q` closes a dirty item through a `window.prompt`. Without a projection
    // of it, GPUI renders the prompt into the window instead — where nothing
    // paints it and it holds focus — and `ted` hangs with no way to answer
    // (SPEC §13.3).
    terminal.send(":q\r");
    let answers = terminal.row(ROWS - 2);
    assert!(
        answers.contains("[1] Save") && answers.contains("[2] Don't Save"),
        "the save prompt was not projected: {answers:?}"
    );
    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "ted quit without waiting for an answer"
    );

    // Answer 2, "Don't Save": the edit is discarded, the item closes, and the
    // empty workspace is the exit condition (SPEC §14.3).
    terminal.send("2");
    let status = terminal
        .exit_status(Duration::from_secs(10))
        .context("ted is still running after answering the save prompt")?;
    // Not merely "it exited": an answered prompt leaves nothing awaiting it, so
    // the shutdown has no abandoned task holding entity handles and gpui's leak
    // detector has nothing to panic about.
    assert!(status.success(), "ted exited badly: {status:?}");
    assert_eq!(fixture.contents(), "alpha\nbeta\n");
    Ok(())
}

#[test]
fn escape_cancels_the_save_prompt_and_leaves_the_buffer_alone() -> anyhow::Result<()> {
    let (fixture, mut terminal) = open("quit_cancel", "alpha\nbeta\n")?;

    terminal.send("x");
    terminal.send(":q\r");
    assert!(
        terminal.row(ROWS - 2).contains("[3] Cancel"),
        "the save prompt was not projected: {:?}",
        terminal.row(ROWS - 2)
    );

    // Escape takes the last answer, which on every prompt Zed raises here is
    // `Cancel`: the editor comes back, still dirty, with the file untouched.
    terminal.send("\x1b");
    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "cancelling the prompt quit anyway"
    );
    assert!(
        terminal.status().contains("[+]"),
        "the buffer stopped being dirty after cancelling: {:?}",
        terminal.status()
    );
    assert_eq!(fixture.contents(), "alpha\nbeta\n");

    // And the editor has the keyboard again, which is the part a prompt that
    // was never answered would have kept.
    terminal.send("j");
    assert!(
        terminal.status().contains("2:1"),
        "the editor did not take the keyboard back: {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn resizing_the_terminal_relays_out_the_editor() -> anyhow::Result<()> {
    // 60 columns of text: it fits on one row at 80 columns and cannot at 30.
    let (_fixture, mut terminal) = open("resize", &format!("{}\n", "ab ".repeat(20)))?;
    // Row 1 is buffer line 2's gutter, so the test for "did it wrap" is whether
    // row 1 carries any of the *text*, not whether it is blank.
    assert!(
        !terminal.row(1).contains("ab"),
        "the line wrapped before the resize: {:?}",
        terminal.row(1)
    );

    terminal.resize(30, ROWS);
    assert!(
        terminal.row(1).contains("ab"),
        "the line did not rewrap after the resize: {:?}",
        terminal.row(1)
    );
    // The status line follows the grid's new last row, not the old one.
    assert!(
        terminal.status().starts_with("NORMAL main.rs"),
        "status was {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn no_vim_types_printable_characters_straight_into_the_buffer() -> anyhow::Result<()> {
    // Without vim there is no normal mode, so `h` is a character rather than a
    // motion and `:` is a character rather than a command line (SPEC §21/M1).
    let (fixture, mut terminal) = open_with("no-vim", "alpha\n", &["--no-vim"])?;

    terminal.send("h:");
    assert!(
        terminal.status().starts_with("main.rs"),
        "a mode is being reported without vim: {:?}",
        terminal.status()
    );
    assert!(
        terminal.row(ROWS - 2).is_empty(),
        "a command line opened without vim: {:?}",
        terminal.row(ROWS - 2)
    );
    assert!(
        terminal.row(0).ends_with("h:alpha"),
        "the characters were not inserted: {:?}",
        terminal.row(0)
    );
    // The file itself is untouched until something saves it.
    assert_eq!(fixture.contents(), "alpha\n");
    Ok(())
}

#[test]
fn a_suspended_child_owns_the_input_and_ted_takes_the_terminal_back() -> anyhow::Result<()> {
    let (fixture, mut terminal) = open("suspend", "alpha\nbeta\n")?;
    let started = fixture.path("started");
    let captured = fixture.path("captured");
    let mark = terminal.transcript_mark();

    // A scripted child rather than a real file manager, so the test depends on
    // nothing being installed (SPEC §20.3). It blocks on stdin, which is
    // exactly where a reader thread that had not parked would steal the input.
    terminal.send(":!echo yes > started; read line; echo $line > captured");
    terminal.type_keys("\r");
    terminal.wait_for_file(&started, Duration::from_secs(10))?;

    terminal.type_keys("hello\r");
    terminal.wait_for_contents(&captured, "hello", Duration::from_secs(10))?;

    // The child has exited and `ted` is holding the screen so its output can be
    // read; any key hands the screen back.
    terminal.wait_for_output("press any key", AFTER_KEYS)?;
    terminal.send(" ");

    // Ratatui's diff buffer describes the screen as it was before the child
    // painted over it, so a resume that did not force a full repaint would
    // leave the grid blank.
    assert!(
        terminal.row(0).ends_with("alpha"),
        "the buffer was not repainted after the child: {:?}",
        terminal.row(0)
    );
    // `hello` as vim motions ends in `o`, which opens a line and enters insert
    // mode, so a leaked keystroke cannot hide here.
    let status = terminal.status();
    assert!(
        status.starts_with("NORMAL main.rs"),
        "the child's input reached ted: {status:?}"
    );
    assert!(
        !status.contains("[+]"),
        "the child's input edited the buffer: {status:?}"
    );

    let transcript = terminal.transcript_since(mark);
    assert!(
        transcript.contains("\u{1b}[?1049l"),
        "ted kept the alternate screen while the child ran"
    );
    assert!(
        transcript.contains("\u{1b}[?1049h"),
        "ted did not take the alternate screen back"
    );
    assert!(
        transcript.contains("\u{1b}[>"),
        "the keyboard enhancement flags were not pushed again on resume"
    );
    Ok(())
}

#[test]
fn a_resize_while_suspended_is_recovered_and_a_failure_is_reported() -> anyhow::Result<()> {
    // 60 columns of text: it fits on one row at 80 columns and cannot at 50.
    let (fixture, mut terminal) = open("suspend-resize", &format!("{}\n", "ab ".repeat(20)))?;
    let suspended = fixture.path("suspended");

    terminal.send(":!touch suspended; exit 3");
    terminal.type_keys("\r");
    // The child running is what proves the terminal has been handed over; the
    // screen says nothing, because a suspended `ted` paints nothing.
    terminal.wait_for_file(&suspended, Duration::from_secs(10))?;

    // The reader is parked, so no `Event::Resize` is delivered, and the one the
    // terminal queues is consumed by the prompt `ted` is holding the screen
    // with. Re-querying the grid on resume is the only thing that can recover
    // this (SPEC §7.1).
    terminal.resize_quietly(50, 20);
    terminal.wait_for_output("press any key", AFTER_KEYS)?;
    let mark = terminal.transcript_mark();
    terminal.send(" ");
    if terminal.status().is_empty() {
        let rows: Vec<String> = (0..20).map(|r| terminal.row(r)).collect();
        panic!(
            "ROWS: {rows:#?}\nAFTER KEY: {:?}",
            terminal.transcript_since(mark)
        );
    }

    assert!(
        terminal.status().starts_with("NORMAL main.rs"),
        "the status line is not on the new last row: {:?}",
        terminal.status()
    );
    assert!(
        terminal.row(1).contains("ab"),
        "the editor was not relaid out at the new size: {:?}",
        terminal.row(1)
    );
    // A non-zero exit is reported rather than swallowed; the notification line
    // sits directly above the status line.
    let notification = terminal.row(terminal.rows - 2);
    assert!(
        notification.contains("exit status: 3"),
        "the child's failure was not reported: {notification:?}"
    );
    Ok(())
}

#[test]
fn interrupting_the_child_does_not_take_ted_with_it() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("suspend-interrupt", "alpha\n")?;

    terminal.send(":!sleep 30\r");
    // Out of raw mode the terminal turns ctrl-C back into a signal, and sends
    // it to every process in the foreground process group — `ted` included
    // (SPEC §7.1).
    terminal.type_keys("\u{3}");
    terminal.wait_for_output("press any key", AFTER_KEYS)?;
    terminal.send(" ");

    assert!(
        !terminal.exited(Duration::from_millis(500)),
        "ted did not survive a ctrl-C aimed at its child"
    );
    // A frame `ted` painted after the suspension, rather than the one left on
    // screen from before it.
    let notification = terminal.row(ROWS - 2);
    assert!(
        notification.contains("signal"),
        "the interrupted child was not reported: {notification:?}"
    );
    Ok(())
}

/// SPEC §13.4: `:Explore` suspends to a file manager and opens whatever it
/// wrote to the chooser file. A scripted stand-in rather than Yazi, so the test
/// depends on nothing being installed — and, since `ted` reads only the chooser
/// file, on none of the real thing's behaviour either.
#[test]
fn explore_opens_what_the_file_manager_chose() -> anyhow::Result<()> {
    let fixture = Fixture::new("explore", "alpha\n")?;
    let first = fixture.path("first.rs");
    let second = fixture.path("second.rs");
    std::fs::write(&first, "fn first() {}\n")?;
    std::fs::write(&second, "fn second() {}\n")?;
    let explored = fixture.path("explored");

    // Two paths, to show that a multiple selection opens as multiple buffers in
    // one call. `{chooser}` is what `ted` substitutes; the rest is the script's.
    fixture.set_file_manager(&format!(
        r#"["sh", "-c", "echo '{}' > '{{chooser}}'; echo '{}' >> '{{chooser}}'; touch '{}'"]"#,
        first.display(),
        second.display(),
        explored.display()
    ))?;

    let mut terminal = Terminal::open(&fixture.file(), &fixture.data_dir(), &[])?;
    terminal.settle(STARTUP);

    terminal.send(":Explore");
    terminal.type_keys("\r");
    terminal.wait_for_file(&explored, Duration::from_secs(10))?;
    // A file manager is full-screen and has already had the user's attention,
    // so `ted` takes the screen back without a prompt (SPEC §7.1).
    terminal.settle(AFTER_KEYS);

    // Which of the two ends up active is not something one call to `open_paths`
    // promises, so cycle the pane and assert on what is in it. `main.rs` is
    // still there too, so three steps see everything.
    //
    // Row 1, not row 0: three items in the pane means a tab strip, and the
    // editor's rows start under it (SPEC §24.7).
    let mut opened = Vec::new();
    for _ in 0..3 {
        opened.push((terminal.status(), terminal.row(1)));
        terminal.send(":bnext\r");
    }

    assert!(
        opened.iter().any(|(status, _)| status.contains("first.rs")),
        "the first chosen file was not opened: {opened:#?}"
    );
    // A chosen file is named by its name, wherever it came from. `ted` was
    // given `main.rs`, so neither of these is inside a worktree yet and each
    // gets one of its own — and an *invisible* worktree is one Zed answers for
    // with an absolute path, which is not what belongs in a tab.
    let directory = first
        .parent()
        .expect("the fixture has a directory")
        .display()
        .to_string();
    for (status, _) in &opened {
        assert!(
            !status.contains(&directory),
            "a chosen file was named by its absolute path: {status:?}"
        );
    }
    let Some((_, drawn)) = opened
        .iter()
        .find(|(status, _)| status.contains("second.rs"))
    else {
        panic!("the second chosen file was not opened: {opened:#?}");
    };
    assert!(
        drawn.contains("fn second()"),
        "the chosen file's contents were not drawn: {drawn:?}"
    );
    Ok(())
}

/// SPEC §13.4: a missing binary surfaces on the notification line rather than
/// failing silently — and, because it is found before anything is handed over,
/// without the terminal ever leaving `ted`'s hands.
#[test]
fn explore_reports_a_file_manager_that_is_not_installed() -> anyhow::Result<()> {
    let fixture = Fixture::new("explore-missing", "alpha\n")?;
    fixture.set_file_manager(r#"["ted-no-such-file-manager"]"#)?;

    let mut terminal = Terminal::open(&fixture.file(), &fixture.data_dir(), &[])?;
    terminal.settle(STARTUP);
    let mark = terminal.transcript_mark();

    terminal.send(":Explore\r");

    let notification = terminal.row(ROWS - 2);
    assert!(
        notification.contains("ted-no-such-file-manager"),
        "the missing file manager was not reported: {notification:?}"
    );
    assert!(
        !terminal.transcript_since(mark).contains("\u{1b}[?1049l"),
        "ted gave up the alternate screen for a child it could not run"
    );
    assert!(
        terminal.status().starts_with("NORMAL main.rs"),
        "the buffer was not still on screen: {:?}",
        terminal.status()
    );
    Ok(())
}

/// SPEC §9: a `ted.json` that cannot be parsed falls back to the defaults, and
/// says so. Silence here is the worst outcome — the user would find out when
/// `:Explore` ran a program they had configured away from.
#[test]
fn a_malformed_ted_json_is_reported_rather_than_ignored() -> anyhow::Result<()> {
    let fixture = Fixture::new("ted-json", "alpha\n")?;
    fixture.write_config(r#"{ "file_manager": ["yazi" }"#)?;

    let mut terminal = Terminal::open(&fixture.file(), &fixture.data_dir(), &[])?;
    terminal.settle(STARTUP);

    let notification = terminal.row(ROWS - 2);
    assert!(
        notification.contains("ted.json"),
        "the unusable config was not reported: {notification:?}"
    );
    // Reported, not fatal: there is still an editor under the notification.
    assert!(
        terminal.status().starts_with("NORMAL main.rs"),
        "ted did not start with an unusable config: {:?}",
        terminal.status()
    );
    Ok(())
}

#[test]
fn the_terminal_is_restored_when_ted_exits() -> anyhow::Result<()> {
    let (_fixture, mut terminal) = open("restore", "alpha\n")?;

    // Ctrl-C is `ted`'s own quit, and the shutdown path must leave the
    // alternate screen and pop the keyboard enhancement flags (SPEC §7).
    let mut trailing = Vec::new();
    terminal.writer.write_all(b"\x03").ok();
    terminal.writer.flush().ok();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match terminal.output.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => trailing.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let trailing = String::from_utf8_lossy(&trailing);
    assert!(
        trailing.contains("\u{1b}[?1049l"),
        "the alternate screen was never left"
    );
    assert!(
        trailing.contains("\u{1b}[<"),
        "the keyboard enhancement flags were never popped"
    );
    let status = terminal
        .exit_status(Duration::from_secs(10))
        .context("ted is still running after ctrl-c")?;
    assert!(status.success(), "ted exited badly: {status:?}");
    Ok(())
}
