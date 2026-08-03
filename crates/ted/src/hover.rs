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
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Hsla, Task, Window};
use language::{DiagnosticEntryRef, LanguageRegistry, Rope};
use lsp::DiagnosticSeverity;
use multi_buffer::{MultiBufferPoint, MultiBufferRow};
use theme::{ActiveTheme as _, SyntaxTheme};

use crate::bootstrap::Backend;
use crate::markdown;
use crate::snapshot::{HoverContent, HoverLine, SpanStyle, StyledSpan, StyledText};

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

            let styles = markdown_styles(cx);
            let syntax = cx.theme().syntax().clone();

            let landed = Rc::new(RefCell::new(None));
            // The multibuffer's own position, not the language buffer's: the
            // project takes the buffer and a position inside *it*.
            let request = editor
                .buffer()
                .read(cx)
                .text_anchor_for_position(cursor, cx)
                .zip(editor.project().cloned())
                .map(|((buffer, position), project)| {
                    let languages = project.read(cx).languages().clone();
                    let hover =
                        project.update(cx, |project, cx| project.hover(&buffer, position, cx));
                    cx.spawn({
                        let landed = landed.clone();
                        async move |_, _| {
                            let mut contents = Vec::new();
                            for hover in hover.await.unwrap_or_default().iter() {
                                let documentation =
                                    documentation(hover, accent, &styles, &languages, &syntax)
                                        .await;
                                contents.extend(documentation);
                            }
                            *landed.borrow_mut() = Some(contents);
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

fn markdown_styles(cx: &App) -> markdown::Styles {
    let colors = cx.theme().colors();
    markdown::Styles {
        code_background: colors.editor_background,
        code_foreground: colors.text,
        accent: colors.text_accent,
        muted: colors.text_muted,
    }
}

/// The language server's documentation, rendered rather than flattened
/// (SPEC §21/M3.5).
///
/// Asynchronous because the fences are: colouring one needs its `Language`, and
/// resolving a language may have to load a grammar. The renderer itself stays
/// pure and says only *where* the fences are; this is what goes and gets them.
async fn documentation(
    hover: &project::Hover,
    accent: Hsla,
    styles: &markdown::Styles,
    languages: &Arc<LanguageRegistry>,
    syntax: &Arc<SyntaxTheme>,
) -> Option<HoverContent> {
    let mut lines = Vec::new();
    for block in &hover.contents {
        let mut rendered = match &block.kind {
            project::HoverBlockKind::Markdown => markdown::render(&block.text, styles),
            // A block the server already told us is code, without a fence
            // around it: rendering it as markdown would eat its `*`s and `_`s.
            project::HoverBlockKind::Code { language } => {
                markdown::render(&fence(&block.text, language), styles)
            }
            project::HoverBlockKind::PlainText => markdown::Rendered {
                lines: block.text.lines().map(plain_line).collect(),
                fences: Vec::new(),
            },
        };

        for fence in std::mem::take(&mut rendered.fences) {
            let Ok(language) = languages.language_for_name(&fence.language).await else {
                continue;
            };
            highlight_fence(&mut rendered.lines, &fence.lines, &language, syntax);
        }
        lines.extend(rendered.lines);
    }

    // One rail for the whole answer rather than one per block: the blocks are
    // one thing the server said, and railing them apart would read as several.
    let lines = trim_blank_edges(lines);
    (!lines.is_empty()).then(|| HoverContent {
        rail: Some(accent),
        lines,
    })
}

fn fence(text: &str, language: &str) -> String {
    format!("```{language}\n{text}\n```")
}

fn plain_line(text: &str) -> HoverLine {
    HoverLine::prose(StyledText::plain(text))
}

/// Colours one fenced block in place, keeping the ground the markdown renderer
/// put under it.
///
/// The whole block is parsed as one document rather than a line at a time,
/// because a grammar that only ever saw one line of a function body would find
/// almost nothing in it.
fn highlight_fence(
    lines: &mut [HoverLine],
    range: &Range<usize>,
    language: &Arc<language::Language>,
    syntax: &SyntaxTheme,
) {
    let Some(block) = lines.get(range.clone()) else {
        return;
    };
    let source = block
        .iter()
        .map(|line| line.text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let highlights = language.highlight_text(&Rope::from(source.as_str()), 0..source.len());

    let mut start = 0usize;
    for line in lines.get_mut(range.clone()).into_iter().flatten() {
        // The ground the markdown renderer laid down, which every syntax colour
        // is painted on top of rather than instead of.
        let ground = line
            .text
            .spans
            .first()
            .map(|span| span.style)
            .unwrap_or_default();
        let end = start + line.text.text.len();
        let mut spans = vec![StyledSpan {
            range: 0..line.text.text.len(),
            style: ground,
        }];
        for (highlight, id) in &highlights {
            let from = highlight.start.max(start);
            let to = highlight.end.min(end);
            if from >= to {
                continue;
            }
            let Some(style) = syntax.get(*id) else {
                continue;
            };
            spans.push(StyledSpan {
                range: from - start..to - start,
                style: SpanStyle {
                    foreground: style.color.or(ground.foreground),
                    background: ground.background,
                    ..Default::default()
                },
            });
        }
        line.text.spans = spans;
        // The newline the join put between the lines.
        start = end + 1;
    }
}

/// Blank lines at either edge are the fences' and the server's, not the
/// author's; the ones between paragraphs are the author's and stay.
fn trim_blank_edges(mut lines: Vec<HoverLine>) -> Vec<HoverLine> {
    while lines.first().is_some_and(|line| line.text.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.text.is_empty()) {
        lines.pop();
    }
    lines
}

/// One diagnostic as the three lines SPEC §24.8 paints: what it is, what it
/// says, and who said it.
///
/// Not markdown: a diagnostic's message is prose a compiler wrote, and the
/// backticks in `expected `u32`, found `u8`` are the message rather than markup.
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

    let mut lines = vec![HoverLine::prose(StyledText {
        spans: vec![StyledSpan {
            range: 0..heading.len(),
            style: SpanStyle {
                bold: true,
                ..Default::default()
            },
        }],
        text: heading,
    })];
    lines.extend(diagnostic.message.lines().map(plain_line));
    if let Some(source) = &diagnostic.source {
        lines.push(plain_line(source));
    }

    HoverContent {
        rail: Some(severity_color(diagnostic.severity, status)),
        lines,
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

        let lines = described
            .lines
            .iter()
            .map(|line| line.text.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            lines,
            [
                "error E0061",
                "this function takes 5 arguments but 2 were supplied",
                "rust-analyzer"
            ]
        );
        // The heading is the only thing in a diagnostic that is not prose.
        assert!(described.lines[0].text.spans[0].style.bold);
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
}
