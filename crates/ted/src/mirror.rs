//! Projecting Zed's own modals into `ted`'s list (SPEC §13.1, the Mirror
//! strategy).
//!
//! A `Picker<D>` produces its rows as GPUI elements, which is nothing a terminal
//! can paint. `PickerDelegate::text_for_match` is the seam: the delegate answers
//! in plain text, and everything else about the modal — its filtering, its
//! ordering, its keybindings, what `enter` does — stays where it already is.
//! Nothing here handles a key. The modal holds focus, so its keystrokes reach it
//! down GPUI's own dispatch path, and `ted` only reads what to draw.
//!
//! The registry below is the whole of `ted`'s knowledge of Zed's modals, and a
//! modal missing from it degrades visibly rather than swallowing keys into a
//! surface nobody can see.

use command_palette::CommandPalette;
use file_finder::FileFinder;
use gpui::{App, Entity, Window};
use outline::OutlineView;
use picker::{Picker, PickerDelegate, PickerRowText};
use project_symbols::ProjectSymbolsDelegate;
use workspace::Workspace;

use crate::snapshot::{MatchedText, OverlayPlacement, OverlayRow, OverlayView, QueryView};

/// How many matches around the selection are turned into rows.
///
/// Every projected row costs the delegate real work — the command palette
/// resolves a keybinding per row — and this runs on every frame, while the list
/// on screen is at most half the grid tall. The footer reports the true position
/// in the full match list, so the number of rows projected is a drawing decision
/// and never a claim about how many matches there are.
const PROJECTED_ROWS: usize = 64;

/// The list to paint for whatever modal the workspace has open, or `None` when
/// it has none.
pub fn view(workspace: &Entity<Workspace>, window: &Window, cx: &App) -> Option<OverlayView> {
    let workspace = workspace.read(cx);
    let type_name = workspace.active_modal_type_name(cx)?;

    if let Some(palette) = workspace.active_modal::<CommandPalette>(cx) {
        return Some(project(palette.read(cx).picker(), "commands", window, cx));
    }
    if let Some(finder) = workspace.active_modal::<FileFinder>(cx) {
        return Some(project(finder.read(cx).picker(), "files", window, cx));
    }
    if let Some(outline) = workspace.active_modal::<OutlineView>(cx) {
        return Some(project(outline.read(cx).picker(), "outline", window, cx));
    }
    if let Some(symbols) = workspace.active_modal::<Picker<ProjectSymbolsDelegate>>(cx) {
        return Some(project(&symbols, "symbols", window, cx));
    }

    Some(unsupported(type_name))
}

fn project<D: PickerDelegate>(
    picker: &Entity<Picker<D>>,
    title: &str,
    window: &Window,
    cx: &App,
) -> OverlayView {
    let query = picker.read(cx).query(cx);
    let picker = picker.read(cx);
    let count = picker.delegate.match_count();
    let selected = picker
        .delegate
        .selected_index()
        .min(count.saturating_sub(1));

    // Centred on the selection, and clamped so a short list is never padded with
    // rows that are not there.
    let first = selected
        .saturating_sub(PROJECTED_ROWS / 2)
        .min(count.saturating_sub(PROJECTED_ROWS.min(count)));
    let last = first.saturating_add(PROJECTED_ROWS).min(count);

    let mut rows = Vec::with_capacity(last - first);
    for index in first..last {
        // A delegate that answers for one match answers for all of them, so the
        // first `None` is this picker saying it has no textual projection at all
        // rather than this row failing.
        let Some(text) = picker.delegate.text_for_match(index, window, cx) else {
            return unprojectable(title);
        };
        rows.push(row_for(text));
    }

    OverlayView {
        title: Some(title.to_owned()),
        query: Some(QueryView {
            // The picker's query editor exposes its text and not its selection,
            // so the caret sits where typing puts it.
            cursor: query.len(),
            text: query,
        }),
        selected: (!rows.is_empty()).then_some(selected - first),
        footer: Some(match count {
            0 => "no matches".to_owned(),
            count => format!("{}/{count}", selected + 1),
        }),
        rows,
        placement: OverlayPlacement::Grid,
    }
}

fn row_for(text: PickerRowText) -> OverlayRow {
    OverlayRow {
        label: MatchedText {
            text: text.label,
            matched: text.label_positions,
        },
        detail: text.detail.map(|detail| MatchedText {
            text: detail,
            matched: text.detail_positions,
        }),
        modified: false,
    }
}

/// A modal `ted` has no adapter for. It still holds focus and still answers keys
/// — saying so is the difference between a surface that is unfinished and one
/// that looks broken.
fn unsupported(type_name: &str) -> OverlayView {
    // The leaf of the path and nothing of the generic arguments: a fully
    // qualified `picker::Picker<some_crate::SomeDelegate>` is longer than most
    // grids are wide, and every informative part of it is in the head.
    let name = type_name
        .split('<')
        .next()
        .unwrap_or(type_name)
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(type_name);
    notice(format!("modal: {name} — not yet supported in ted"))
}

fn unprojectable(title: &str) -> OverlayView {
    notice(format!(
        "{title} — this picker has no text for ted to paint"
    ))
}

fn notice(message: String) -> OverlayView {
    OverlayView {
        title: None,
        query: None,
        rows: vec![OverlayRow {
            label: MatchedText::plain(message),
            detail: None,
            modified: false,
        }],
        selected: None,
        footer: None,
        placement: OverlayPlacement::Grid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only_row(view: &OverlayView) -> &str {
        match view.rows.as_slice() {
            [row] => &row.label.text,
            rows => panic!("expected one row, got {}", rows.len()),
        }
    }

    #[test]
    fn a_modal_with_no_adapter_says_so_by_name() {
        let view = unsupported("some_crate::submodule::KeymapEditorModal");
        assert_eq!(
            only_row(&view),
            "modal: KeymapEditorModal — not yet supported in ted"
        );
        assert_eq!(view.selected, None, "there is nothing to choose");
        assert_eq!(view.query, None, "there is nothing to type");
    }

    /// A type with no path is as much of a name as there is, and it is still
    /// better than saying nothing.
    #[test]
    fn a_bare_type_name_is_used_as_it_is() {
        assert_eq!(
            only_row(&unsupported("Anonymous")),
            "modal: Anonymous — not yet supported in ted"
        );
    }

    /// A picker used as its own modal arrives as a generic, whose arguments are
    /// most of its length and none of its meaning.
    #[test]
    fn a_generic_modal_is_named_by_its_head() {
        assert_eq!(
            only_row(&unsupported("picker::Picker<some_crate::SomeDelegate>")),
            "modal: Picker — not yet supported in ted"
        );
    }

    #[test]
    fn a_picker_with_no_text_is_named_by_the_registry() {
        assert_eq!(
            only_row(&unprojectable("symbols")),
            "symbols — this picker has no text for ted to paint"
        );
    }
}
