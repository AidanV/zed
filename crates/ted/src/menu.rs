//! The completions popup and the code-action menu (SPEC §24.9).
//!
//! One module, because upstream they are one field: `Editor::context_menu` holds
//! either a `CompletionsMenu` or a `CodeActionsMenu`, and the reader that makes
//! one readable makes the other readable too. Reading it is the whole of what
//! `ted` does here — the menu is Zed's, its keys reach it down the dispatch tree
//! like any other, and nothing in this file is routed to.
//!
//! The GUI menu is an element tree `ted` cannot execute (SPEC §4.2.1), so
//! `Editor::context_menu_contents` hands over the same content as plain data and
//! this turns it into cells.

use std::ops::Range;

use editor::Editor;
use editor::code_context_menus::{ContextMenuEntry, ContextMenuOrigin};
use gpui::{App, Entity, Hsla};
use theme::ActiveTheme as _;

use crate::snapshot::{MenuRow, StyledSpan, StyledText, style_from_highlight};

/// What the open menu contains, before the projection decides where it goes.
pub struct Contents {
    pub rows: Vec<MenuRow>,
    /// `None` when the selected index is a header or a rule, which the menu can
    /// report between two groups and which nothing may land on.
    pub selected: Option<usize>,
    pub anchor: Anchor,
}

/// What the box opens against. Completions deploy at the cursor; code actions
/// deploy from the gutter indicator, which is a row of its own and usually not
/// the row the cursor is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Cursor,
    GutterRow(u32),
}

pub fn read(editor: &Entity<Editor>, cx: &App) -> Option<Contents> {
    let contents = editor.read(cx).context_menu_contents()?;
    // The theme's, not the editor's: the box is a surface the theme owns, and a
    // run that named no colour of its own belongs to that surface rather than to
    // the code underneath it (SPEC §24.1).
    let foreground = cx.theme().colors().text;

    let rows = contents
        .entries
        .iter()
        .map(|entry| row_for(entry, foreground))
        .collect::<Vec<_>>();
    let selected = rows
        .get(contents.selected)
        .filter(|row| row.is_selectable())
        .map(|_| contents.selected);

    Some(Contents {
        rows,
        selected,
        anchor: match contents.origin {
            ContextMenuOrigin::GutterIndicator(row) => Anchor::GutterRow(row.0),
            // A terminal has no quick action bar to hang a menu off, so the
            // cursor is the only honest anchor left.
            ContextMenuOrigin::Cursor | ContextMenuOrigin::QuickActionBar => Anchor::Cursor,
        },
    })
}

fn row_for(entry: &ContextMenuEntry, foreground: Hsla) -> MenuRow {
    match entry {
        ContextMenuEntry::Completion(completion) => {
            // `styled_runs_for_code_label` fades everything past the filter
            // range, which is exactly the signature — so splitting the label
            // there gives the signature column its dimmer colour for free.
            let split = completion.filter_range.end.min(completion.label.len());
            MenuRow::Entry {
                label: styled(&completion.label, 0..split, &completion.runs, foreground),
                matched: completion
                    .matched
                    .iter()
                    // `StringMatch::positions` are offsets into the *filter*
                    // text, so they land on the label only once shifted by
                    // where the filter range starts.
                    .filter_map(|offset| offset.checked_add(completion.filter_range.start))
                    .filter(|offset| *offset < split)
                    .collect(),
                kind: completion.kind.map(kind_word).map(str::to_owned),
                signature: styled(
                    &completion.label,
                    split..completion.label.len(),
                    &completion.runs,
                    foreground,
                ),
            }
        }
        ContextMenuEntry::CodeAction(action) => MenuRow::Entry {
            label: StyledText::plain(format!(
                "{} {}",
                action_glyph(action.kind.as_deref()),
                action.title
            )),
            matched: Vec::new(),
            // An action has neither a kind column nor a signature: the glyph
            // says what kind it is, and there is nothing else to say about it.
            kind: None,
            signature: StyledText::default(),
        },
        ContextMenuEntry::GroupHeader(label) => MenuRow::Header(label.to_string()),
        ContextMenuEntry::Divider => MenuRow::Divider,
    }
}

/// A slice of a label with the runs that cover it, rebased on the slice.
fn styled(
    label: &str,
    range: Range<usize>,
    runs: &[(Range<usize>, gpui::HighlightStyle)],
    foreground: Hsla,
) -> StyledText {
    let text = label.get(range.clone()).unwrap_or_default().to_owned();
    let spans = runs
        .iter()
        .filter_map(|(run, highlight)| {
            let start = run.start.max(range.start);
            let end = run.end.min(range.end);
            (start < end).then(|| StyledSpan {
                range: start - range.start..end - range.start,
                style: style_from_highlight(Some(*highlight), foreground),
            })
        })
        .collect();
    StyledText { text, spans }
}

/// The kind as a word rather than a glyph or a colour, because a word needs no
/// legend and the column is narrow either way (SPEC §24.9).
///
/// Short enough to keep the column narrow, and the word the language itself
/// uses where there is one — `fn` rather than "function", since the box is read
/// beside code that says `fn`.
fn kind_word(kind: lsp::CompletionItemKind) -> &'static str {
    use lsp::CompletionItemKind as Kind;
    match kind {
        Kind::FUNCTION => "fn",
        Kind::METHOD => "method",
        Kind::CONSTRUCTOR => "new",
        Kind::FIELD => "field",
        Kind::VARIABLE => "let",
        Kind::CLASS => "class",
        Kind::INTERFACE => "trait",
        Kind::MODULE => "mod",
        Kind::PROPERTY => "prop",
        Kind::UNIT => "unit",
        Kind::VALUE => "value",
        Kind::ENUM => "enum",
        Kind::KEYWORD => "keyword",
        Kind::SNIPPET => "snippet",
        Kind::COLOR => "colour",
        Kind::FILE => "file",
        Kind::REFERENCE => "ref",
        Kind::FOLDER => "dir",
        Kind::ENUM_MEMBER => "variant",
        Kind::CONSTANT => "const",
        Kind::STRUCT => "struct",
        Kind::EVENT => "event",
        Kind::OPERATOR => "op",
        Kind::TYPE_PARAMETER => "type",
        _ => "",
    }
}

/// A leading glyph per kind, which is all an action's row carries besides its
/// title (SPEC §24.9).
///
/// The LSP kind is a dotted hierarchy, so the first segment is what decides it
/// and `refactor.extract` reads as a refactor. Plain BMP symbols rather than a
/// Nerd Font's private-use area: SPEC §24.1 assumes a complete font for box
/// drawing, which is a much smaller ask than an icon patch.
fn action_glyph(kind: Option<&str>) -> char {
    match kind.and_then(|kind| kind.split('.').next()) {
        Some("quickfix") => '✚',
        Some("refactor") => '⇄',
        Some("source") => '⚑',
        Some("task") => '▶',
        Some("debug") => '◆',
        _ => '·',
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_labels_syntax_runs_are_cut_at_the_filter_range() {
        let bold = gpui::HighlightStyle {
            font_weight: Some(gpui::FontWeight::BOLD),
            ..Default::default()
        };
        let runs = vec![(0..3, bold), (3..9, gpui::HighlightStyle::default())];
        let default = gpui::hsla(0.0, 0.0, 1.0, 1.0);

        let label = styled("foo(x: u8)", 0..3, &runs, default);
        assert_eq!(label.text, "foo");
        assert_eq!(label.spans.len(), 1);
        assert_eq!(label.spans[0].range, 0..3);
        assert!(label.spans[0].style.bold);

        let signature = styled("foo(x: u8)", 3..10, &runs, default);
        assert_eq!(signature.text, "(x: u8)");
        // Rebased on the slice, not left pointing into the original label.
        assert_eq!(signature.spans[0].range, 0..6);
    }

    #[test]
    fn a_dotted_action_kind_reads_as_its_first_segment() {
        assert_eq!(
            action_glyph(Some("refactor.extract")),
            action_glyph(Some("refactor"))
        );
        assert_ne!(action_glyph(Some("quickfix")), action_glyph(Some("source")));
        assert_eq!(action_glyph(None), action_glyph(Some("something.else")));
    }
}
