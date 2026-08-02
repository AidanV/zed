//! The message, on demand (SPEC §24.8).
//!
//! The buffer shows severity and nothing else — a coloured underline where the
//! code is wrong, carried through `snapshot::span_style`. The *message* appears
//! only when asked for, because nothing a language server says is allowed to
//! move the code the user is reading.
//!
//! Neither half is read where Zed keeps it. `Editor::activate_diagnostics`
//! renders a group into block rows behind a `RenderBlock` closure returning an
//! opaque element, and the hover popover keeps its text as an `Entity<Markdown>`
//! — so `ted` asks the buffer and the project instead, both of which are public
//! end to end and are the same sources the editor privately caches.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{App, Hsla, Task, Window};
use language::DiagnosticEntryRef;
use lsp::DiagnosticSeverity;
use multi_buffer::{MultiBufferPoint, MultiBufferRow};
use theme::ActiveTheme as _;

use crate::bootstrap::Backend;
use crate::snapshot::HoverContent;

/// An open panel: what is already known about the cursor's position, and the
/// request for the rest.
///
/// The two halves arrive at different times and the panel does not wait for the
/// slower one. Diagnostics are in the buffer and are read outright; the
/// language server's documentation is a request that may take as long as the
/// server does, and awaiting it in the frame loop would stop `ted` painting and
/// answering keys until it landed.
pub struct Panel {
    contents: Vec<HoverContent>,
    landed: Rc<RefCell<Option<Vec<HoverContent>>>>,
    _request: Option<Task<()>>,
}

impl Panel {
    pub fn open(backend: &Backend, window: &mut Window, cx: &mut App) -> Option<Self> {
        let editor = backend.active_editor(cx)?;
        let accent = cx.theme().colors().text_accent;

        editor.update(cx, |editor, cx| {
            let snapshot = editor.snapshot(window, cx);
            let buffer_snapshot = snapshot.display_snapshot.buffer_snapshot();
            let cursor = editor
                .selections
                .newest_display(&snapshot.display_snapshot)
                .head()
                .to_point(&snapshot.display_snapshot);

            let status = editor.style(cx).status.clone();
            let row = cursor.row;
            let line_end =
                MultiBufferPoint::new(row, buffer_snapshot.line_len(MultiBufferRow(row)));
            let contents = buffer_snapshot
                .diagnostics_in_range(MultiBufferPoint::new(row, 0)..line_end)
                .map(|entry| describe(&entry, &status))
                .collect::<Vec<_>>();

            let landed = Rc::new(RefCell::new(None));
            // The multibuffer's own position, not the language buffer's: the
            // project takes the buffer and a position inside *it*.
            let request = editor
                .buffer()
                .read(cx)
                .text_anchor_for_position(cursor, cx)
                .zip(editor.project().cloned())
                .map(|((buffer, position), project)| {
                    let hover =
                        project.update(cx, |project, cx| project.hover(&buffer, position, cx));
                    cx.spawn({
                        let landed = landed.clone();
                        async move |_, _| {
                            let documentation = hover
                                .await
                                .unwrap_or_default()
                                .iter()
                                .filter_map(|hover| documentation(hover, accent))
                                .collect();
                            *landed.borrow_mut() = Some(documentation);
                        }
                    })
                });

            Some(Self {
                contents,
                landed,
                _request: request,
            })
        })
    }

    /// Takes whatever the language server answered with, once it has.
    pub fn poll(&mut self) {
        let Some(documentation) = self.landed.borrow_mut().take() else {
            return;
        };
        self.contents.extend(documentation);
    }

    pub fn contents(&self) -> &[HoverContent] {
        &self.contents
    }

    /// Whether the panel has nothing to say and nothing on the way. A position
    /// with no diagnostic and no language server behind it is one the user asked
    /// about and got no answer for, which is a panel that should never appear
    /// rather than an empty one.
    pub fn is_empty(&self) -> bool {
        self.contents.is_empty() && self._request.is_none()
    }
}

fn documentation(hover: &project::Hover, accent: Hsla) -> Option<HoverContent> {
    let text = hover
        .contents
        .iter()
        .map(|content| flatten_markdown(&content.text))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| HoverContent {
        rail: Some(accent),
        text,
    })
}

/// One diagnostic as the three lines SPEC §24.8 paints: what it is, what it
/// says, and who said it.
fn describe(
    entry: &DiagnosticEntryRef<'_, MultiBufferPoint>,
    status: &theme::StatusColors,
) -> HoverContent {
    let diagnostic = &entry.diagnostic;
    let mut heading = severity_name(diagnostic.severity).to_owned();
    if let Some(code) = &diagnostic.code {
        heading.push(' ');
        heading.push_str(&match code {
            lsp::NumberOrString::Number(number) => number.to_string(),
            lsp::NumberOrString::String(code) => code.clone(),
        });
    }

    let mut text = format!("{heading}\n{}", diagnostic.message);
    if let Some(source) = &diagnostic.source {
        text.push('\n');
        text.push_str(source);
    }

    HoverContent {
        rail: Some(severity_color(diagnostic.severity, status)),
        text,
    }
}

fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::ERROR => "error",
        DiagnosticSeverity::WARNING => "warning",
        DiagnosticSeverity::INFORMATION => "info",
        DiagnosticSeverity::HINT => "hint",
        _ => "diagnostic",
    }
}

/// The same mapping the buffer's underline uses, so the rail and the underline
/// under the code cannot disagree about what an error looks like.
pub fn severity_color(severity: DiagnosticSeverity, status: &theme::StatusColors) -> Hsla {
    match severity {
        DiagnosticSeverity::ERROR => status.error,
        DiagnosticSeverity::WARNING => status.warning,
        DiagnosticSeverity::INFORMATION => status.info,
        DiagnosticSeverity::HINT => status.hint,
        _ => status.ignored,
    }
}

/// Markdown as plain text: enough to read a type signature and a doc comment.
///
/// Turning fences, emphasis and lists into terminal styling is a small renderer
/// of its own and travels with the other deferred work (SPEC §21/M3.5) — this is
/// not a placeholder that gets thrown away, it is the same panel with a better
/// text pass behind it.
fn flatten_markdown(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue;
        }
        let stripped = trimmed.trim_start_matches('#');
        let stripped = if stripped.len() != trimmed.len() {
            stripped.trim_start()
        } else {
            trimmed
        };
        lines.push(strip_emphasis(stripped));
    }

    // A doc comment that started or ended with a fence leaves blank lines at the
    // edges; the ones between paragraphs are the author's and stay.
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn strip_emphasis(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(index) = rest.find(['*', '_', '`']) {
        result.push_str(&rest[..index]);
        let marker = rest.as_bytes()[index] as char;
        rest = &rest[index..];
        let run = rest
            .chars()
            .take_while(|character| *character == marker)
            .count();
        rest = &rest[run..];
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_diagnostic_reads_as_what_it_is_what_it_says_and_who_said_it() {
        let status = theme::StatusColors::dark();
        let diagnostic = language::Diagnostic {
            severity: DiagnosticSeverity::ERROR,
            code: Some(lsp::NumberOrString::String("E0061".to_owned())),
            message: "this function takes 5 arguments but 2 were supplied".to_owned(),
            source: Some("rust-analyzer".to_owned()),
            ..Default::default()
        };
        let described = describe(
            &DiagnosticEntryRef {
                range: MultiBufferPoint::new(0, 0)..MultiBufferPoint::new(0, 4),
                diagnostic: &diagnostic,
            },
            &status,
        );

        assert_eq!(
            described.text,
            "error E0061\nthis function takes 5 arguments but 2 were supplied\nrust-analyzer"
        );
        // The rail's colour is the severity's, so it cannot disagree with the
        // underline the same severity put under the code.
        assert_eq!(described.rail, Some(status.error));
    }

    #[test]
    fn severity_colours_are_the_ones_the_buffer_underlines_with() {
        let status = theme::StatusColors::dark();
        assert_eq!(
            severity_color(DiagnosticSeverity::ERROR, &status),
            status.error
        );
        assert_eq!(
            severity_color(DiagnosticSeverity::WARNING, &status),
            status.warning
        );
        assert_eq!(
            severity_color(DiagnosticSeverity::INFORMATION, &status),
            status.info
        );
        assert_eq!(
            severity_color(DiagnosticSeverity::HINT, &status),
            status.hint
        );
    }

    #[test]
    fn fences_go_and_the_code_inside_them_stays() {
        let flattened = flatten_markdown("```rust\nfn build(x: u16) -> Row\n```\n\nBuilds a row.");
        assert_eq!(flattened, "fn build(x: u16) -> Row\n\nBuilds a row.");
    }

    #[test]
    fn emphasis_and_headings_lose_their_markers() {
        assert_eq!(flatten_markdown("# Heading"), "Heading");
        assert_eq!(
            flatten_markdown("a **bold** and `code` word"),
            "a bold and code word"
        );
        assert_eq!(flatten_markdown("__underlined__"), "underlined");
    }

    #[test]
    fn a_blank_paragraph_break_survives() {
        assert_eq!(flatten_markdown("one\n\ntwo"), "one\n\ntwo");
    }
}
