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
            rows: ROWS,
            child,
            _master: pty.master,
        })
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
                    self.screen.feed(&chunk);
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
        self.writer.write_all(keys.as_bytes()).ok();
        self.writer.flush().ok();
        self.settle(AFTER_KEYS);
    }

    /// Resizes the pty, which delivers SIGWINCH to `ted` exactly as a terminal
    /// emulator would.
    fn resize(&mut self, columns: u16, rows: u16) {
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
        self.settle(AFTER_KEYS);
    }

    fn row(&self, index: u16) -> String {
        self.screen.row(index)
    }

    /// The row `ted` reserves for its status line, which is always the last one.
    fn status(&self) -> String {
        self.screen.row(self.rows - 1)
    }

    fn exited(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => {
                    // Keep draining, or the child can block writing its final
                    // frame into a full pty buffer and never reach exit.
                    self.output.recv_timeout(Duration::from_millis(100)).ok();
                }
                Err(_) => return false,
            }
        }
        false
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

    fn data_dir(&self) -> std::path::PathBuf {
        self.directory.join("data")
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
    assert!(
        terminal.exited(Duration::from_secs(10)),
        "ted is still running after :q"
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
    Ok(())
}
