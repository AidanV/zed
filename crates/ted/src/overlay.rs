//! The buffer switcher, painted over the editor (SPEC §24.6).
//!
//! It is `ted`'s own rather than a projection of Zed's `tab_switcher`, because
//! the two surfaces answer different questions: Zed's is a filterable list, and
//! this one is cycling and nothing else — one verb, no query row, no preview, no
//! closing from the list. Every *other* modal is projected as it is by
//! [`crate::mirror`], which is what SPEC §13.1 settled on from M3 onward.
//!
//! Nothing here reserves a row. The list floats over the editor's cells, so a
//! box that appears and disappears costs one more row of painting rather than a
//! window resize, a relayout and an asynchronous rewrap of the whole buffer.

use anyhow::{Context as _, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Bias;
use gpui::{App, AppContext as _, AsyncApp, EntityId};
use util::paths::PathStyle;
use util::rel_path::RelPath;
use workspace::Workspace;

use crate::actions::Surface;
use crate::bootstrap::Backend;
use crate::snapshot::{MatchedText, OverlayPlacement, OverlayRow, OverlayView};

/// What a keystroke did to an open overlay, as far as the frame loop is
/// concerned.
#[derive(Debug, PartialEq)]
pub enum Update {
    Unchanged,
    Cancel,
    /// Go to what is selected.
    Confirm,
}

/// An item already open in the active pane, identified rather than indexed
/// because the list is ordered by activation and the pane's order is not.
struct Row {
    view: OverlayRow,
    item: EntityId,
}

pub struct Overlay {
    rows: Vec<Row>,
    selected: usize,
}

impl Overlay {
    /// The surface `surface` opens, or `None` for one that is not a list.
    pub fn open(surface: Surface, backend: &Backend, cx: &App) -> Option<Self> {
        match surface {
            Surface::Switcher => Some(Self::switcher(backend, cx)),
            Surface::GoToLine | Surface::Hover => None,
        }
    }

    fn switcher(backend: &Backend, cx: &App) -> Self {
        let rows = open_item_rows(backend, cx);
        // The second row, because the first is the buffer you are already in
        // (SPEC §24.6).
        let selected = usize::from(rows.len() > 1);
        Self { rows, selected }
    }

    pub fn view(&self) -> OverlayView {
        OverlayView {
            title: Some("buffers".to_owned()),
            // The switcher is the list of what is open; there is nothing to
            // filter and no second number to compare it against (SPEC §24.6).
            query: None,
            rows: self.rows.iter().map(|row| row.view.clone()).collect(),
            selected: (!self.rows.is_empty()).then_some(self.selected),
            footer: None,
            placement: OverlayPlacement::TopCentre,
        }
    }

    /// One vocabulary across every consumer, so nothing has to be learnt twice
    /// (SPEC §24.2). `ctrl-n` / `ctrl-p` move the selection and *only* those:
    /// neither `ctrl-j`/`ctrl-k` nor the arrows claim that job anywhere `ted`
    /// owns the keyboard.
    pub fn handle_key(&mut self, event: &KeyEvent) -> Update {
        if matches!(event.kind, KeyEventKind::Release) {
            return Update::Unchanged;
        }
        let control = event.modifiers.contains(KeyModifiers::CONTROL);

        match event.code {
            KeyCode::Esc => Update::Cancel,
            KeyCode::Char('c') if control => Update::Cancel,
            KeyCode::Enter => Update::Confirm,
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
            KeyCode::Tab => {
                self.move_selection(1);
                Update::Unchanged
            }
            KeyCode::BackTab => {
                self.move_selection(-1);
                Update::Unchanged
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

    /// Goes to what is selected.
    pub fn confirm(&self, backend: &Backend, cx: &mut AsyncApp) -> Result<()> {
        let Some(row) = self.rows.get(self.selected) else {
            return Ok(());
        };

        let workspace = backend.workspace.clone();
        let item = row.item;
        cx.update_window(backend.window.into(), |_, window, cx| {
            workspace.update(cx, |workspace, cx| {
                activate_item(workspace, item, window, cx);
            });
        })
        // A closed window is how a quit reaches this, and there is no longer
        // anywhere for the item to be activated in.
        .ok();
        Ok(())
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
                item: item.item_id(),
            }
        })
        .collect()
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

    fn with_rows(count: usize) -> Overlay {
        let rows = (0..count)
            .map(|index| Row {
                view: OverlayRow {
                    label: MatchedText::plain(format!("row {index}")),
                    detail: None,
                    modified: false,
                },
                item: EntityId::default(),
            })
            .collect();
        Overlay { rows, selected: 0 }
    }

    #[test]
    fn only_ctrl_n_and_ctrl_p_move_the_selection() {
        let mut overlay = with_rows(3);
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
    fn the_switcher_takes_no_query_and_cycles_on_tab() {
        let mut overlay = with_rows(4);
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
        let mut overlay = with_rows(1);
        assert_eq!(overlay.handle_key(&key(KeyCode::Esc)), Update::Cancel);
        assert_eq!(overlay.handle_key(&key(KeyCode::Enter)), Update::Confirm);
    }
}
