//! `ted`'s own lists, painted over the editor (SPEC §24.1).
//!
//! The file finder (§24.4) and the buffer switcher (§24.6) are the same widget
//! with different content: one searches, the other goes back. Both are `ted`'s
//! own lists over Zed's own data rather than projections of Zed's `Picker`
//! views, which is what SPEC §13.1 decided for M2 and what the Mirror hook
//! replaces in M3 — `FileFinderDelegate` is a public struct whose constructor
//! and every field are private, so there is nothing to read even before the
//! rendering question arises.
//!
//! Nothing here reserves a row. The list floats over the editor's cells, so a
//! box that grows and shrinks with every keystroke of a query costs one more row
//! of painting rather than a window resize, a relayout and an asynchronous
//! rewrap of the whole buffer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Bias;
use gpui::{App, AppContext as _, AsyncApp, EntityId, Task};
use project::{Candidates, PathMatchCandidateSet, ProjectPath};
use util::paths::PathStyle;
use util::rel_path::RelPath;
use workspace::{SplitDirection, Workspace};

use crate::actions::Surface;
use crate::bootstrap::Backend;
use crate::snapshot::{MatchedText, OverlayPlacement, OverlayRow, OverlayView, QueryView};

/// As many matches as Zed's own finder asks for. Past this the list is scrolling
/// far beyond anything a user reads, and the footer says how many files were
/// searched either way.
const MAX_MATCHES: usize = 100;

/// What a keystroke did to an open overlay, as far as the frame loop is
/// concerned.
#[derive(Debug, PartialEq)]
pub enum Update {
    Unchanged,
    /// The query changed; the search has to be restarted.
    QueryChanged,
    Cancel,
    /// Open what is selected, optionally into a fresh split (M4 renders those).
    Confirm(Option<SplitDirection>),
}

/// Where a row leads.
enum Target {
    Path(ProjectPath),
    /// An item already open in the active pane, identified rather than indexed
    /// because the list is ordered by activation and the pane's order is not.
    Item(EntityId),
}

struct Row {
    view: OverlayRow,
    target: Target,
}

/// The results of one search, held until the frame loop picks them up.
struct Landed {
    query: String,
    rows: Vec<Row>,
    searched: usize,
}

enum Kind {
    Files,
    Buffers,
}

pub struct Overlay {
    kind: Kind,
    query: String,
    /// Byte offset of the insertion point within `query`.
    cursor: usize,
    rows: Vec<Row>,
    selected: usize,
    /// How many files the last search looked at, which is the right-hand half of
    /// the finder's count.
    searched: usize,
    /// Results a running search has produced. Read on the next frame rather than
    /// awaited, so the list keeps showing the previous result set instead of
    /// blanking between keystrokes (SPEC §24.4).
    landed: Rc<RefCell<Option<Landed>>>,
    /// Set when a newer search starts. `match_path_sets` checks it as it goes,
    /// so an obsolete scan stops rather than finishing into a list nobody wants.
    cancel: Arc<AtomicBool>,
    _search: Option<Task<()>>,
}

impl Overlay {
    /// The surface `surface` opens, or `None` for one that is not a list.
    pub fn open(surface: Surface, backend: &Backend, cx: &mut App) -> Option<Self> {
        match surface {
            Surface::Finder => Some(Self::finder(backend, cx)),
            Surface::Switcher => Some(Self::switcher(backend, cx)),
            Surface::GoToLine | Surface::Hover => None,
        }
    }

    /// Opens with an empty query, always: nothing is recalled from the last time
    /// it was open, so the first character typed is the first character of the
    /// search rather than an edit to a query the user has to notice and clear
    /// (SPEC §24.2).
    fn finder(backend: &Backend, cx: &mut App) -> Self {
        let mut overlay = Self::empty(Kind::Files);
        overlay.rows = recent_rows(backend, cx);
        overlay.searched = overlay.rows.len();
        overlay
    }

    fn switcher(backend: &Backend, cx: &App) -> Self {
        let mut overlay = Self::empty(Kind::Buffers);
        overlay.rows = open_item_rows(backend, cx);
        // The second row, because the first is the buffer you are already in
        // (SPEC §24.6).
        overlay.selected = usize::from(overlay.rows.len() > 1);
        overlay
    }

    fn empty(kind: Kind) -> Self {
        Self {
            kind,
            query: String::new(),
            cursor: 0,
            rows: Vec::new(),
            selected: 0,
            searched: 0,
            landed: Rc::new(RefCell::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            _search: None,
        }
    }

    pub fn view(&self) -> OverlayView {
        let (title, placement) = match self.kind {
            Kind::Files => ("files", OverlayPlacement::Grid),
            Kind::Buffers => ("buffers", OverlayPlacement::TopCentre),
        };
        OverlayView {
            title: Some(title.to_owned()),
            query: matches!(self.kind, Kind::Files).then(|| QueryView {
                text: self.query.clone(),
                cursor: self.cursor,
            }),
            rows: self.rows.iter().map(|row| row.view.clone()).collect(),
            selected: (!self.rows.is_empty()).then_some(self.selected),
            footer: self.footer(),
            placement,
        }
    }

    fn footer(&self) -> Option<String> {
        match self.kind {
            // The switcher is the list of what is open; there is no second
            // number to compare it against (SPEC §24.6).
            Kind::Buffers => None,
            Kind::Files if self.rows.is_empty() => Some("no matches".to_owned()),
            Kind::Files => Some(format!("{}/{}", self.rows.len(), self.searched)),
        }
    }

    /// One vocabulary across every consumer, so nothing has to be learnt twice
    /// (SPEC §24.2). `ctrl-n` / `ctrl-p` move the selection and *only* those:
    /// neither `ctrl-j`/`ctrl-k` nor the arrows claim that job anywhere in `ted`.
    pub fn handle_key(&mut self, event: &KeyEvent) -> Update {
        if matches!(event.kind, KeyEventKind::Release) {
            return Update::Unchanged;
        }
        let control = event.modifiers.contains(KeyModifiers::CONTROL);
        let filtered = matches!(self.kind, Kind::Files);

        match event.code {
            KeyCode::Esc => Update::Cancel,
            KeyCode::Char('c') if control => Update::Cancel,
            KeyCode::Enter => Update::Confirm(None),
            KeyCode::Char('s') if control => Update::Confirm(Some(SplitDirection::Down)),
            KeyCode::Char('v') if control => Update::Confirm(Some(SplitDirection::Right)),
            KeyCode::Char('n') if control => {
                self.move_selection(1);
                Update::Unchanged
            }
            KeyCode::Char('p') if control => {
                self.move_selection(-1);
                Update::Unchanged
            }
            // While the switcher is up, `ctrl-tab` walks it. The hold-and-release
            // interaction a GUI uses needs key-release events, which arrive only
            // under the Kitty protocol, and a switcher that behaved differently
            // on Terminal.app than on Ghostty would be worse than one that always
            // confirms on `enter` (SPEC §24.6).
            KeyCode::Tab if !filtered => {
                self.move_selection(1);
                Update::Unchanged
            }
            KeyCode::BackTab if !filtered => {
                self.move_selection(-1);
                Update::Unchanged
            }
            _ if !filtered => Update::Unchanged,
            KeyCode::Backspace => {
                let Some(previous) = self.query[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                else {
                    // Backspacing an empty query dismisses, the way it closes the
                    // `:` line rather than leaving an empty one open.
                    return Update::Cancel;
                };
                self.query.replace_range(previous..self.cursor, "");
                self.cursor = previous;
                Update::QueryChanged
            }
            KeyCode::Delete => {
                if self.cursor >= self.query.len() {
                    return Update::Unchanged;
                }
                let next = self.query[self.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(index, _)| self.cursor + index)
                    .unwrap_or(self.query.len());
                self.query.replace_range(self.cursor..next, "");
                Update::QueryChanged
            }
            KeyCode::Left => {
                self.cursor = self.query[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                Update::Unchanged
            }
            KeyCode::Right => {
                self.cursor = self.query[self.cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(index, _)| self.cursor + index)
                    .unwrap_or(self.query.len());
                Update::Unchanged
            }
            KeyCode::Home => {
                self.cursor = 0;
                Update::Unchanged
            }
            KeyCode::End => {
                self.cursor = self.query.len();
                Update::Unchanged
            }
            KeyCode::Char(character) if !control => {
                self.query.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                Update::QueryChanged
            }
            _ => Update::Unchanged,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let count = self.rows.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(count);
        self.selected = next as usize;
    }

    /// Takes whatever the running search finished with. Called every frame; a
    /// result for a query the user has since edited is dropped rather than shown.
    pub fn poll(&mut self) {
        let Some(landed) = self.landed.borrow_mut().take() else {
            return;
        };
        if landed.query != self.query {
            return;
        }
        self.rows = landed.rows;
        self.searched = landed.searched;
        self.selected = 0;
    }

    /// Starts the search for the current query, cancelling whatever was running.
    ///
    /// An empty query has nothing to score, so it lists recents; the moment a
    /// character is typed, the matcher's score decides and recency stops
    /// mattering (SPEC §24.4).
    pub fn refresh(&mut self, backend: &Backend, cx: &mut App) {
        if !matches!(self.kind, Kind::Files) {
            return;
        }
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));

        if self.query.is_empty() {
            self._search = None;
            self.rows = recent_rows(backend, cx);
            self.searched = self.rows.len();
            self.selected = 0;
            return;
        }

        let workspace = backend.workspace.read(cx);
        let project = workspace.project().read(cx);
        let path_style = project.path_style(cx);
        let worktrees = project
            .worktree_store()
            .read(cx)
            .visible_worktrees_and_single_files(cx)
            .collect::<Vec<_>>();
        // With one worktree the root name is noise on every row; with several it
        // is the only thing telling two `src/main.rs` apart.
        let include_root_name = worktrees.len() > 1;
        let candidate_sets = worktrees
            .into_iter()
            .map(|worktree| PathMatchCandidateSet {
                snapshot: worktree.read(cx).snapshot(),
                // SPEC §24.4: ignored files stay out, and there is no `ctrl-h`
                // to let them back in.
                include_ignored: false,
                include_root_name,
                candidates: Candidates::Files,
            })
            .collect::<Vec<_>>();

        let query = self.query.clone();
        let cancel = self.cancel.clone();
        let landed = self.landed.clone();
        let executor = cx.background_executor().clone();
        self._search = Some(cx.spawn(async move |_| {
            let searched = candidate_sets
                .iter()
                .map(fuzzy_nucleo::PathMatchCandidateSet::len)
                .sum();
            let matches = fuzzy_nucleo::match_path_sets(
                candidate_sets.as_slice(),
                &query,
                &None,
                fuzzy_nucleo::Case::Ignore,
                MAX_MATCHES,
                &cancel,
                executor,
            )
            .await;
            if cancel.load(Ordering::Acquire) {
                return;
            }

            let rows = matches
                .into_iter()
                .map(|found| {
                    let full = found.path_prefix.join(&found.path);
                    Row {
                        view: path_row(&full, &found.positions, path_style),
                        target: Target::Path(ProjectPath {
                            worktree_id: project::WorktreeId::from_usize(found.worktree_id),
                            path: found.path,
                        }),
                    }
                })
                .collect();
            *landed.borrow_mut() = Some(Landed {
                query,
                rows,
                searched,
            });
        }));
    }

    /// Opens what is selected. The finder's row is a path and the switcher's is
    /// an item that is already open, which is the whole difference between the
    /// two surfaces.
    pub fn confirm(
        &self,
        split: Option<SplitDirection>,
        backend: &Backend,
        cx: &mut AsyncApp,
    ) -> Task<Result<()>> {
        let Some(row) = self.rows.get(self.selected) else {
            return Task::ready(Ok(()));
        };

        let workspace = backend.workspace.clone();
        let opening = cx.update_window(backend.window.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| match &row.target {
                Target::Path(path) => {
                    let pane = split.map(|direction| {
                        workspace
                            .split_pane(workspace.active_pane().clone(), direction, window, cx)
                            .downgrade()
                    });
                    let opening = workspace.open_path(path.clone(), pane, true, window, cx);
                    // Foreground: what it resolves to is an item handle, and an
                    // entity handle belongs on the thread that owns it.
                    cx.spawn(async move |_, _| opening.await.map(|_| ()))
                }
                Target::Item(item) => {
                    activate_item(workspace, *item, window, cx);
                    Task::ready(Ok(()))
                }
            })
        });

        match opening {
            Ok(opening) => opening,
            // A closed window is how a quit reaches this, and there is no longer
            // anywhere for the file to open into.
            Err(_) => Task::ready(Ok(())),
        }
    }
}

fn activate_item(
    workspace: &mut Workspace,
    item: EntityId,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let pane = workspace.active_pane().clone();
    pane.update(cx, |pane, cx| {
        let Some(index) = pane
            .items()
            .position(|candidate| candidate.item_id() == item)
        else {
            return;
        };
        pane.activate_item(index, true, true, window, cx);
    });
}

/// The rows an empty query shows: where the user has been, most recent first
/// (SPEC §24.4).
fn recent_rows(backend: &Backend, cx: &App) -> Vec<Row> {
    let workspace = backend.workspace.read(cx);
    let path_style = workspace.path_style(cx);
    workspace
        .recent_navigation_history(Some(MAX_MATCHES), cx)
        .into_iter()
        .map(|(path, _)| Row {
            view: path_row(&path.path, &[], path_style),
            target: Target::Path(path),
        })
        .collect()
}

/// What is open in the active pane, most recently used first — the same two
/// sources Zed's own switcher sorts by, so "the second entry is where I just
/// came from" holds in both (SPEC §24.6).
fn open_item_rows(backend: &Backend, cx: &App) -> Vec<Row> {
    let workspace = backend.workspace.read(cx);
    let path_style = workspace.path_style(cx);
    let pane = workspace.active_pane().read(cx);
    let history = pane.activation_history();

    let mut items = pane.items().cloned().collect::<Vec<_>>();
    items.sort_by_key(|item| {
        // Ascending timestamps, so the most recently activated is last; an item
        // never activated has no entry and belongs at the end of the list rather
        // than the start of it.
        std::cmp::Reverse(
            history
                .iter()
                .position(|entry| entry.entity_id == item.item_id())
                .map(|position| position as i64)
                .unwrap_or(-1),
        )
    });

    items
        .into_iter()
        .map(|item| {
            let label = item.tab_content_text(0, cx).to_string();
            let directory = item
                .project_path(cx)
                .and_then(|path| directory_of(&path.path, path_style));
            Row {
                view: OverlayRow {
                    label: MatchedText::plain(label),
                    detail: directory.map(MatchedText::plain),
                    modified: item.is_dirty(cx),
                },
                target: Target::Item(item.item_id()),
            }
        })
        .collect()
}

/// A path as two columns: the file name, and the directory that tells two files
/// of the same name apart (SPEC §24.4).
///
/// `positions` are byte offsets into the path as one string, which is how both
/// `fuzzy_nucleo` and Zed's own finder report them, so they are split at the
/// same boundary the columns are.
fn path_row(path: &RelPath, positions: &[usize], path_style: PathStyle) -> OverlayRow {
    let file_name = path.file_name().unwrap_or_default();
    let name_start = path.as_unix_str().len().saturating_sub(file_name.len());
    let label = MatchedText {
        text: file_name.to_owned(),
        matched: positions
            .iter()
            .filter_map(|position| position.checked_sub(name_start))
            .collect(),
    };

    let detail = directory_of(path, path_style).map(|directory| MatchedText {
        matched: positions
            .iter()
            .copied()
            .filter(|position| *position < directory.len())
            .collect(),
        text: directory,
    });

    OverlayRow {
        label,
        detail,
        modified: false,
    }
}

fn directory_of(path: &RelPath, path_style: PathStyle) -> Option<String> {
    let parent = path.parent()?;
    let directory = parent.display(path_style).into_owned();
    (!directory.is_empty()).then_some(directory)
}

/// A number typed at a bare prompt, which is the only go-to-line code `ted` has
/// (SPEC §24.5).
///
/// With vim, `:42` parses in vim's own interceptor and dispatches like any other
/// command, so there is nothing here to do. Without it there is no `:` line at
/// all, and `go_to_line::Toggle` would open a modal `ted` cannot paint.
pub fn go_to_line(backend: &Backend, line: u32, cx: &mut AsyncApp) -> Result<()> {
    let editor = cx
        .update(|cx| backend.active_editor(cx))
        .context("nothing is open to jump in")?;
    cx.update_window(backend.window.into(), |_, window, cx| {
        editor.update(cx, |editor, cx| {
            let point = editor.buffer().read(cx).snapshot(cx).clip_point(
                multi_buffer::MultiBufferPoint::new(line.saturating_sub(1), 0),
                Bias::Left,
            );
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([point..point]);
            });
        });
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn control(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn row(label: &str) -> Row {
        Row {
            view: OverlayRow {
                label: MatchedText::plain(label),
                detail: None,
                modified: false,
            },
            target: Target::Path(ProjectPath {
                worktree_id: project::WorktreeId::from_usize(0),
                path: RelPath::empty_arc(),
            }),
        }
    }

    fn with_rows(kind: Kind, count: usize) -> Overlay {
        let mut overlay = Overlay::empty(kind);
        overlay.rows = (0..count)
            .map(|index| row(&format!("row {index}")))
            .collect();
        overlay
    }

    #[test]
    fn typing_filters_and_moving_does_not() {
        let mut overlay = with_rows(Kind::Files, 3);
        assert_eq!(
            overlay.handle_key(&key(KeyCode::Char('a'))),
            Update::QueryChanged
        );
        assert_eq!(
            overlay.view().query.map(|query| query.text).as_deref(),
            Some("a")
        );
        assert_eq!(
            overlay.handle_key(&control(KeyCode::Char('n'))),
            Update::Unchanged
        );
        assert_eq!(overlay.selected, 1);
    }

    #[test]
    fn only_ctrl_n_and_ctrl_p_move_the_selection() {
        let mut overlay = with_rows(Kind::Files, 3);
        for code in [KeyCode::Down, KeyCode::Up] {
            overlay.handle_key(&key(code));
            assert_eq!(overlay.selected, 0, "{code:?} moved the selection");
        }
        for code in ['j', 'k'] {
            overlay.handle_key(&control(KeyCode::Char(code)));
            assert_eq!(overlay.selected, 0, "ctrl-{code} moved the selection");
        }

        overlay.handle_key(&control(KeyCode::Char('p')));
        assert_eq!(overlay.selected, 2, "the selection should wrap backwards");
    }

    #[test]
    fn the_switcher_starts_on_the_second_row_and_takes_no_query() {
        let mut overlay = with_rows(Kind::Buffers, 4);
        overlay.selected = 1;
        assert_eq!(overlay.view().query, None);

        // A printable key is not a filter here — there is nothing to filter.
        assert_eq!(
            overlay.handle_key(&key(KeyCode::Char('a'))),
            Update::Unchanged
        );
        assert_eq!(overlay.handle_key(&key(KeyCode::Tab)), Update::Unchanged);
        assert_eq!(overlay.selected, 2);
    }

    #[test]
    fn escape_and_enter_are_distinct_outcomes() {
        let mut overlay = with_rows(Kind::Files, 1);
        assert_eq!(overlay.handle_key(&key(KeyCode::Esc)), Update::Cancel);
        assert_eq!(
            overlay.handle_key(&key(KeyCode::Enter)),
            Update::Confirm(None)
        );
        assert_eq!(
            overlay.handle_key(&control(KeyCode::Char('v'))),
            Update::Confirm(Some(SplitDirection::Right))
        );
    }

    #[test]
    fn backspacing_an_empty_query_dismisses() {
        let mut overlay = with_rows(Kind::Files, 1);
        overlay.handle_key(&key(KeyCode::Char('a')));
        assert_eq!(
            overlay.handle_key(&key(KeyCode::Backspace)),
            Update::QueryChanged
        );
        assert_eq!(overlay.handle_key(&key(KeyCode::Backspace)), Update::Cancel);
    }

    #[test]
    fn a_result_for_an_edited_query_is_dropped() {
        let mut overlay = with_rows(Kind::Files, 0);
        overlay.query = "beta".to_owned();
        *overlay.landed.borrow_mut() = Some(Landed {
            query: "bet".to_owned(),
            rows: vec![row("stale.rs")],
            searched: 1,
        });

        overlay.poll();
        assert!(overlay.rows.is_empty(), "a stale result reached the list");
    }

    #[test]
    fn a_path_becomes_a_name_and_a_directory() {
        let path = RelPath::from_unix_str("crates/ted/src/snapshot.rs").expect("a relative path");
        // "ted/sna": the first three characters land in the directory column and
        // the rest in the file name, at offsets counted from each column's start.
        let positions = vec![7, 8, 9, 15, 16, 17];
        let row = path_row(path, &positions, PathStyle::Unix);
        assert_eq!(row.label.text, "snapshot.rs");
        assert_eq!(row.label.matched, vec![0, 1, 2]);
        assert_eq!(
            row.detail.as_ref().map(|detail| detail.text.as_str()),
            Some("crates/ted/src")
        );
        assert_eq!(row.detail.expect("a directory").matched, positions[..3]);
    }

    #[test]
    fn a_file_at_the_root_has_no_directory_column() {
        let path = RelPath::from_unix_str("main.rs").expect("a relative path");
        let row = path_row(path, &[], PathStyle::Unix);
        assert_eq!(row.label.text, "main.rs");
        assert_eq!(row.detail, None);
    }
}
