//! `:Explore` — browsing files by handing the terminal to a file manager
//! (SPEC §13.4).
//!
//! `ted` does not build a project panel. It suspends (SPEC §7.1) into a program
//! that is better at file *management* than anything `ted` would write, and the
//! integration is nearly free: the `Project` underneath is real and watches its
//! worktrees, so whatever the file manager does on disk propagates back into
//! open buffers with no coordination on either side. All that crosses the
//! boundary is a file of chosen paths.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{App, AppContext as _, AsyncApp, Entity};
use workspace::{OpenOptions, OpenVisible, Workspace};

use crate::bootstrap::Backend;
use crate::config::Config;
use crate::suspend::Child;

const CHOOSER_PLACEHOLDER: &str = "{chooser}";
const DIRECTORY_PLACEHOLDER: &str = "{directory}";

/// A `:Explore` whose file manager is running or has run: everything needed to
/// collect what it chose, once the terminal is `ted`'s again.
pub struct Explore {
    chooser: PathBuf,
    /// Held only so the chooser file outlives the child and is removed when
    /// this is dropped, on every path out of a suspension.
    _directory: tempfile::TempDir,
}

/// Resolves the file manager and where it should start, before the terminal is
/// handed over.
///
/// Every reason `:Explore` cannot run is an `Err` carrying what the user should
/// be told (SPEC §13.3), and all of them are found here rather than after the
/// screen has already been given away.
pub fn prepare(config: &Config, backend: &Backend, cx: &App) -> Result<(Child, Explore)> {
    let project = backend.workspace.read(cx).project().read(cx);
    // A joined project's files exist only over Zed's collab protocol, with no
    // filesystem locally and no SSH access to the host by design, so there is
    // nothing on this machine for a file manager to browse. An SSH-remote
    // project has files, but on the other end of the connection; mapping a
    // selection back onto that connection is SPEC §21/M5.
    anyhow::ensure!(
        !project.is_via_collab(),
        "a joined project has no files on this machine — use the file finder"
    );
    anyhow::ensure!(
        !project.is_via_remote_server(),
        "a remote project's files are not on this machine — use the file finder"
    );

    let (program, arguments) = config
        .file_manager
        .split_first()
        .context("no file manager is configured")?;
    // Reported now rather than as a spawn failure after the terminal has been
    // handed over, when the message would flash past between two full repaints.
    let program =
        which::which(program).with_context(|| format!("{program} is not on this machine"))?;

    let directory =
        tempfile::tempdir().context("could not create a directory for the selection")?;
    let chooser = directory.path().join("selection");
    let start = start_directory(&backend.workspace, cx)?;

    let child = Child {
        label: "Explore".to_owned(),
        program: program.into(),
        arguments: arguments
            .iter()
            .map(|argument| expand(argument, &chooser, &start))
            .collect(),
        // A full-screen program has already had the user's attention and has
        // nothing left on screen worth reading (SPEC §7.1).
        wait_for_key: false,
    };

    Ok((
        child,
        Explore {
            chooser,
            _directory: directory,
        },
    ))
}

impl Explore {
    /// Opens whatever the file manager chose, all of it in one call, and returns
    /// the message the user should see if anything went wrong.
    pub async fn open_selection(self, backend: &Backend, cx: &mut AsyncApp) -> Option<String> {
        let paths = match self.selection() {
            Ok(paths) => paths,
            Err(error) => return Some(format!("Explore: {error}")),
        };
        if paths.is_empty() {
            return None;
        }

        let opening = cx.update_window(backend.window.into(), |_, window, cx| {
            backend.workspace.update(cx, |workspace, cx| {
                workspace.open_paths(
                    paths.clone(),
                    OpenOptions {
                        // What the user picked in a file manager is a file to
                        // edit, not a directory to add to the project.
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    None,
                    window,
                    cx,
                )
            })
        });
        let opened = match opening {
            Ok(opening) => opening.await,
            // A closed window is how a quit reaches this, and there is no
            // longer anywhere to report anything to.
            Err(_) => return None,
        };

        // `open_paths` answers in the order it was asked, and answers `None` for
        // a path that named a directory rather than something to open.
        paths
            .iter()
            .zip(opened)
            .find_map(|(path, outcome)| match outcome {
                Some(Err(error)) => Some(format!(
                    "Explore: could not open {}: {error}",
                    path.display()
                )),
                Some(Ok(_)) | None => None,
            })
    }

    fn selection(&self) -> Result<Vec<PathBuf>> {
        match std::fs::read_to_string(&self.chooser) {
            Ok(contents) => Ok(chosen_paths(&contents)),
            // A file manager quit without choosing anything never writes the
            // file at all, which is the ordinary outcome rather than a failure.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => {
                Err(error).with_context(|| format!("could not read {}", self.chooser.display()))
            }
        }
    }
}

/// One path per line. Nothing is trimmed but the line ending, because a leading
/// or trailing space is a legal part of a file name.
fn chosen_paths(contents: &str) -> Vec<PathBuf> {
    contents
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// SPEC §13.4: the active buffer's parent, falling back to the first worktree
/// root. A workspace with neither is not a state `ted` reaches with a file
/// open, but it is one an empty `ted` starts in.
fn start_directory(workspace: &Entity<Workspace>, cx: &App) -> Result<PathBuf> {
    let workspace = workspace.read(cx);
    let project = workspace.project().read(cx);

    let active_buffer_parent = workspace
        .active_item(cx)
        .and_then(|item| item.project_path(cx))
        .and_then(|path| project.absolute_path(&path, cx))
        .and_then(|path| path.parent().map(Path::to_path_buf));

    let worktree_root = || {
        project
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    };

    active_buffer_parent
        .or_else(worktree_root)
        .map(Ok)
        .unwrap_or_else(|| {
            std::env::current_dir().context("this workspace has no directory to browse")
        })
}

/// Substitutes the two placeholders into one argument.
///
/// Kept in `OsString` throughout: the setting is UTF-8 because it came from
/// JSON, but the paths substituted into it are not required to be, and a lossy
/// conversion here would hand the file manager a path that silently is not the
/// one it was given.
fn expand(argument: &str, chooser: &Path, directory: &Path) -> OsString {
    let mut expanded = OsString::new();
    let mut rest = argument;

    while let Some(index) = rest.find('{') {
        expanded.push(&rest[..index]);
        let placeholder = &rest[index..];
        if let Some(tail) = placeholder.strip_prefix(CHOOSER_PLACEHOLDER) {
            expanded.push(chooser);
            rest = tail;
        } else if let Some(tail) = placeholder.strip_prefix(DIRECTORY_PLACEHOLDER) {
            expanded.push(directory);
            rest = tail;
        } else {
            expanded.push("{");
            rest = &placeholder[1..];
        }
    }
    expanded.push(rest);

    expanded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_substituted_wherever_they_appear() {
        let chooser = Path::new("/tmp/ted/selection");
        let directory = Path::new("/home/user/project");

        assert_eq!(
            expand("--chooser-file={chooser}", chooser, directory),
            OsString::from("--chooser-file=/tmp/ted/selection")
        );
        assert_eq!(
            expand("{directory}", chooser, directory),
            OsString::from("/home/user/project")
        );
        assert_eq!(
            expand("{chooser}:{directory}", chooser, directory),
            OsString::from("/tmp/ted/selection:/home/user/project")
        );
    }

    /// A file manager's own syntax is full of braces — `nnn`'s `-p` argument and
    /// any shell wrapper's `${VAR}` among them — and only the two `ted` defines
    /// mean anything here.
    #[test]
    fn braces_that_are_not_placeholders_survive() {
        let chooser = Path::new("/tmp/selection");
        let directory = Path::new("/project");

        assert_eq!(
            expand("${EDITOR} {chooser} {unknown}", chooser, directory),
            OsString::from("${EDITOR} /tmp/selection {unknown}")
        );
        assert_eq!(
            expand("{", chooser, directory),
            OsString::from("{"),
            "a lone brace should not be dropped"
        );
    }

    #[test]
    fn a_path_that_is_not_utf8_survives_substitution() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;

            let directory = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff"));
            let expanded = expand("{directory}/x", Path::new("/tmp/selection"), directory);
            assert_eq!(expanded.as_os_str().as_bytes(), b"/tmp/\xff/x");
        }
    }

    #[test]
    fn every_chosen_line_is_a_path() {
        assert_eq!(
            chosen_paths("/a/one.rs\n/a/two.rs\n"),
            vec![PathBuf::from("/a/one.rs"), PathBuf::from("/a/two.rs")]
        );
        // Yazi writes CRLF on Windows and a trailing newline everywhere.
        assert_eq!(
            chosen_paths("/a/one.rs\r\n\r\n/a/two.rs"),
            vec![PathBuf::from("/a/one.rs"), PathBuf::from("/a/two.rs")]
        );
        assert!(chosen_paths("").is_empty());
        assert!(chosen_paths("\n").is_empty());
    }

    #[test]
    fn a_space_in_a_name_is_part_of_the_name() {
        assert_eq!(
            chosen_paths("/a/two words.rs\n"),
            vec![PathBuf::from("/a/two words.rs")]
        );
    }
}
