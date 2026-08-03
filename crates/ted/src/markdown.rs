//! A hand-rolled renderer for the markdown a language server's hover answer or
//! a doc comment is written in (SPEC §24.8, M3.5).
//!
//! `ted`'s dependency set is fixed and a doc comment's markdown is a small,
//! well-behaved subset of the language, so this is a small line-oriented parser
//! rather than a dependency on `pulldown-cmark`. It turns emphasis and inline
//! code into terminal attributes, lays out fenced code and pipe tables as fixed
//! text the renderer clips instead of wraps, and leaves syntax-colouring a fence
//! to the caller: that is asynchronous (SPEC §21) and does not belong in a pure
//! function of a string.

use std::ops::Range;

use gpui::Hsla;

use crate::cell::text_cells;
use crate::snapshot::{HoverLine, SpanStyle, StyledSpan, StyledText, Underline};

/// The theme colours a rendered document needs. Passed in rather than read from
/// the theme here, so this module interprets no GPUI types beyond `Hsla`.
#[derive(Clone, Copy, Debug)]
pub struct Styles {
    /// Inline code and fenced blocks.
    pub code_background: Hsla,
    /// Fenced-block text before any syntax colouring lands on it.
    pub code_foreground: Hsla,
    /// Links, and the rule characters a table is drawn with.
    pub accent: Hsla,
    /// A link's URL, and anything else the reader is not meant to read first.
    pub muted: Hsla,
}

/// A document as lines, and where the fenced code in it is.
#[derive(Debug, Default)]
pub struct Rendered {
    pub lines: Vec<HoverLine>,
    pub fences: Vec<Fence>,
}

/// A fenced block, so the caller can syntax-colour it once it has resolved the
/// language — which is asynchronous and does not belong in a pure renderer.
#[derive(Debug)]
pub struct Fence {
    /// The fence's info string, verbatim and lowercased ("rust", "sh"). Empty
    /// when the fence named no language.
    pub language: String,
    /// Which of `Rendered::lines` the block's code occupies, excluding the
    /// fence markers themselves (which are not emitted as lines).
    pub lines: Range<usize>,
}

/// Renders `source` line by line, in the order of precedence a doc comment's
/// markdown needs resolved: a fenced block first (so nothing inside one is
/// read as a heading or a list), then a pipe table, then headings, lists,
/// block quotes, rules, and finally ordinary prose.
pub fn render(source: &str, styles: &Styles) -> Rendered {
    let source_lines: Vec<&str> = source.lines().collect();
    let mut lines = Vec::new();
    let mut fences = Vec::new();
    let mut index = 0;

    while index < source_lines.len() {
        let line = source_lines[index];

        if let Some((marker_character, run_length, language)) = parse_fence_open(line) {
            index = render_fence(
                &source_lines,
                index,
                marker_character,
                run_length,
                language,
                styles,
                &mut lines,
                &mut fences,
            );
            continue;
        }

        if is_table_header(&source_lines, index) {
            index = render_table(&source_lines, index, styles, &mut lines);
            continue;
        }

        if let Some((_, content)) = heading_level(line) {
            lines.push(render_heading(content, styles));
            index += 1;
            continue;
        }

        if let Some((leading_spaces, marker, content)) = list_item(line) {
            lines.push(render_list_item(leading_spaces, &marker, content, styles));
            index += 1;
            continue;
        }

        if let Some(content) = blockquote_content(line) {
            lines.push(render_blockquote(content, styles));
            index += 1;
            continue;
        }

        if is_horizontal_rule(line) {
            lines.push(render_horizontal_rule(styles));
            index += 1;
            continue;
        }

        // Everything else is prose. A blank line is the author's paragraph
        // break and stays, but a run of more than one collapses to a single
        // one, and the very first and last lines of the document are trimmed
        // by the pass below — so a doc comment that opened or closed with a
        // fence does not leave the panel with dead space at either end.
        if line.trim().is_empty() {
            let previous_is_blank = lines
                .last()
                .map(|line: &HoverLine| line.text.is_empty())
                .unwrap_or(true);
            if !previous_is_blank {
                lines.push(HoverLine::prose(StyledText::default()));
            }
        } else {
            lines.push(HoverLine::prose(inline_styled(line, styles)));
        }
        index += 1;
    }

    while lines.last().is_some_and(|line| line.text.is_empty()) {
        lines.pop();
    }

    Rendered { lines, fences }
}

fn parse_fence_open(line: &str) -> Option<(char, usize, String)> {
    let trimmed = line.trim_start();
    let marker_character = trimmed.chars().next()?;
    if marker_character != '`' && marker_character != '~' {
        return None;
    }
    let run_length = trimmed
        .chars()
        .take_while(|&character| character == marker_character)
        .count();
    if run_length < 3 {
        return None;
    }
    let info = trimmed.get(run_length..)?.trim().to_lowercase();
    Some((marker_character, run_length, info))
}

fn is_fence_close(line: &str, marker_character: char, min_length: usize) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| character == marker_character)
        && trimmed.chars().count() >= min_length
}

/// Consumes the fence body starting just after its opening marker, emitting
/// every line verbatim with the code surface's style. An unclosed fence simply
/// runs until `lines` is exhausted, which is what "runs to the end of the
/// document" means in practice.
fn render_fence(
    lines: &[&str],
    index: usize,
    marker_character: char,
    min_length: usize,
    language: String,
    styles: &Styles,
    output: &mut Vec<HoverLine>,
    fences: &mut Vec<Fence>,
) -> usize {
    let code_style = SpanStyle {
        background: Some(styles.code_background),
        foreground: Some(styles.code_foreground),
        ..Default::default()
    };
    let start = output.len();
    let mut cursor = index + 1;
    while let Some(&line) = lines.get(cursor) {
        cursor += 1;
        if is_fence_close(line, marker_character, min_length) {
            break;
        }
        let text = line.to_owned();
        let text_length = text.len();
        output.push(HoverLine {
            text: StyledText {
                text,
                spans: vec![StyledSpan {
                    range: 0..text_length,
                    style: code_style,
                }],
            },
            wrap: false,
            indent: 0,
        });
    }
    fences.push(Fence {
        language,
        lines: start..output.len(),
    });
    cursor
}

fn is_table_header(lines: &[&str], index: usize) -> bool {
    let Some(&header) = lines.get(index) else {
        return false;
    };
    let Some(&delimiter) = lines.get(index + 1) else {
        return false;
    };
    header.contains('|') && is_delimiter_row(delimiter)
}

/// Whether a line is a valid GitHub-table delimiter row: every cell between the
/// pipes is dashes, optionally bracketed by a leading or trailing `:` for
/// alignment. A row that fails this is not a table, even if the line above it
/// looks like a header — the header just falls through to prose.
fn is_delimiter_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let trimmed = trimmed.trim_matches('|');
    if trimmed.is_empty() {
        return false;
    }
    trimmed.split('|').all(|cell| {
        let cell = cell.trim();
        let cell = cell.strip_prefix(':').unwrap_or(cell);
        let cell = cell.strip_suffix(':').unwrap_or(cell);
        !cell.is_empty() && cell.chars().all(|character| character == '-')
    })
}

fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

#[derive(Clone, Copy)]
enum Alignment {
    Left,
    Center,
    Right,
}

fn parse_alignment(cell: &str) -> Alignment {
    let cell = cell.trim();
    match (cell.starts_with(':'), cell.ends_with(':')) {
        (true, true) => Alignment::Center,
        (false, true) => Alignment::Right,
        _ => Alignment::Left,
    }
}

fn pad_cell(text: &str, width: u32, alignment: Alignment) -> String {
    let padding = width.saturating_sub(text_cells(text)) as usize;
    match alignment {
        Alignment::Left => format!("{text}{}", " ".repeat(padding)),
        Alignment::Right => format!("{}{text}", " ".repeat(padding)),
        Alignment::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
        }
    }
}

fn box_rule_line(
    widths: &[u32],
    left: char,
    middle: char,
    right: char,
    style: SpanStyle,
) -> HoverLine {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        if index > 0 {
            text.push(middle);
        }
        for _ in 0..width.saturating_add(2) {
            text.push('─');
        }
    }
    text.push(right);
    let text_length = text.len();
    HoverLine {
        text: StyledText {
            text,
            spans: vec![StyledSpan {
                range: 0..text_length,
                style,
            }],
        },
        wrap: false,
        indent: 0,
    }
}

fn push_rule_character(
    text: &mut String,
    spans: &mut Vec<StyledSpan>,
    character: char,
    style: SpanStyle,
) {
    let start = text.len();
    text.push(character);
    spans.push(StyledSpan {
        range: start..text.len(),
        style,
    });
}

fn box_row_line(
    row: &[String],
    widths: &[u32],
    alignments: &[Alignment],
    style: SpanStyle,
) -> HoverLine {
    let mut text = String::new();
    let mut spans = Vec::new();
    push_rule_character(&mut text, &mut spans, '│', style);
    for column in 0..widths.len() {
        text.push(' ');
        let cell_text = row.get(column).map(String::as_str).unwrap_or("");
        let alignment = alignments.get(column).copied().unwrap_or(Alignment::Left);
        text.push_str(&pad_cell(cell_text, widths[column], alignment));
        text.push(' ');
        push_rule_character(&mut text, &mut spans, '│', style);
    }
    HoverLine {
        text: StyledText { text, spans },
        wrap: false,
        indent: 0,
    }
}

/// Consumes a header row, its delimiter row, and every body row after it that
/// still looks like a row of the same table (a non-blank line naming a pipe),
/// then emits the whole thing box-drawn with each column as wide as its widest
/// cell.
fn render_table(
    lines: &[&str],
    index: usize,
    styles: &Styles,
    output: &mut Vec<HoverLine>,
) -> usize {
    let header_line = lines.get(index).copied().unwrap_or_default();
    let delimiter_line = lines.get(index + 1).copied().unwrap_or_default();
    let header_cells = split_row(header_line);
    let delimiter_cells = split_row(delimiter_line);
    let column_count = header_cells.len();
    let alignments: Vec<Alignment> = (0..column_count)
        .map(|column| {
            delimiter_cells
                .get(column)
                .map(|cell| parse_alignment(cell))
                .unwrap_or(Alignment::Left)
        })
        .collect();

    let mut rows = vec![header_cells];
    let mut cursor = index + 2;
    while let Some(&line) = lines.get(cursor) {
        if line.trim().is_empty() || !line.contains('|') {
            break;
        }
        rows.push(split_row(line));
        cursor += 1;
    }

    let mut widths = vec![0u32; column_count];
    for row in &rows {
        for (column, width) in widths.iter_mut().enumerate() {
            let cell_text = row.get(column).map(String::as_str).unwrap_or("");
            *width = (*width).max(text_cells(cell_text));
        }
    }

    let accent_style = SpanStyle {
        foreground: Some(styles.accent),
        ..Default::default()
    };

    output.push(box_rule_line(&widths, '┌', '┬', '┐', accent_style));
    if let Some(header_row) = rows.first() {
        output.push(box_row_line(header_row, &widths, &alignments, accent_style));
    }
    output.push(box_rule_line(&widths, '├', '┼', '┤', accent_style));
    for row in rows.get(1..).unwrap_or_default() {
        output.push(box_row_line(row, &widths, &alignments, accent_style));
    }
    output.push(box_rule_line(&widths, '└', '┴', '┘', accent_style));

    cursor
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed
        .chars()
        .take_while(|&character| character == '#')
        .count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = trimmed.get(hashes..)?;
    if rest.is_empty() {
        return Some((hashes, ""));
    }
    if !rest.starts_with(' ') {
        return None;
    }
    Some((hashes, rest.trim_start()))
}

/// A heading's whole line is bold, on top of whatever inline styling its text
/// already carries — so a heading with inline code in it stays code-coloured
/// as well as bold, rather than one replacing the other.
fn render_heading(content: &str, styles: &Styles) -> HoverLine {
    HoverLine::prose(apply_bold(inline_styled(content, styles)))
}

fn apply_bold(styled: StyledText) -> StyledText {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for span in styled.spans {
        if span.range.start > cursor {
            spans.push(StyledSpan {
                range: cursor..span.range.start,
                style: SpanStyle {
                    bold: true,
                    ..Default::default()
                },
            });
        }
        cursor = span.range.end;
        spans.push(StyledSpan {
            range: span.range,
            style: with_bold(span.style),
        });
    }
    if cursor < styled.text.len() {
        spans.push(StyledSpan {
            range: cursor..styled.text.len(),
            style: SpanStyle {
                bold: true,
                ..Default::default()
            },
        });
    }
    StyledText {
        text: styled.text,
        spans,
    }
}

enum ListMarker {
    Bullet,
    Ordered(String),
}

fn list_item(line: &str) -> Option<(usize, ListMarker, &str)> {
    let leading_spaces = line.len() - line.trim_start_matches(' ').len();
    let rest = line.get(leading_spaces..)?;

    for bullet in ["- ", "* ", "+ "] {
        if let Some(content) = rest.strip_prefix(bullet) {
            return Some((leading_spaces, ListMarker::Bullet, content));
        }
    }

    let digit_count = rest
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digit_count == 0 {
        return None;
    }
    let number = rest.get(..digit_count)?;
    let content = rest.get(digit_count..)?.strip_prefix(". ")?;
    Some((
        leading_spaces,
        ListMarker::Ordered(number.to_owned()),
        content,
    ))
}

/// A prefix (a list item's bullet, or a block quote's bar) followed by
/// inline-styled content, with `indent` set to exactly the prefix's own width
/// so a wrapped continuation lines up under the text rather than the prefix.
fn prefixed_line(
    prefix: &str,
    prefix_style: SpanStyle,
    content: &str,
    styles: &Styles,
) -> HoverLine {
    let indent = text_cells(prefix).min(u32::from(u16::MAX)) as u16;
    let mut spans = Vec::new();
    if prefix_style != SpanStyle::default() {
        spans.push(StyledSpan {
            range: 0..prefix.len(),
            style: prefix_style,
        });
    }
    let inline = inline_styled(content, styles);
    let offset = prefix.len();
    let mut text = prefix.to_owned();
    text.push_str(&inline.text);
    spans.extend(inline.spans.into_iter().map(|span| StyledSpan {
        range: span.range.start + offset..span.range.end + offset,
        style: span.style,
    }));
    HoverLine {
        text: StyledText { text, spans },
        wrap: true,
        indent,
    }
}

/// A nesting level is two source spaces, and each level indents the line by
/// two more cells, so a nested list reads as nested rather than at the same
/// depth as its parent.
fn render_list_item(
    leading_spaces: usize,
    marker: &ListMarker,
    content: &str,
    styles: &Styles,
) -> HoverLine {
    let level = leading_spaces / 2;
    let mut prefix = "  ".repeat(level);
    match marker {
        ListMarker::Bullet => prefix.push_str("• "),
        ListMarker::Ordered(number) => {
            prefix.push_str(number);
            prefix.push_str(". ");
        }
    }
    prefixed_line(&prefix, SpanStyle::default(), content, styles)
}

fn blockquote_content(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if let Some(content) = trimmed.strip_prefix("> ") {
        return Some(content);
    }
    (trimmed == ">").then_some("")
}

fn render_blockquote(content: &str, styles: &Styles) -> HoverLine {
    let prefix_style = SpanStyle {
        foreground: Some(styles.muted),
        ..Default::default()
    };
    prefixed_line("▌ ", prefix_style, content, styles)
}

fn is_horizontal_rule(line: &str) -> bool {
    let characters: Vec<char> = line
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    let Some(&first) = characters.first() else {
        return false;
    };
    matches!(first, '-' | '*' | '_')
        && characters.len() >= 3
        && characters.iter().all(|&character| character == first)
}

/// The panel's width is not known to this module — wrapping is the caller's
/// job — so the rule is a modest fixed length rather than one that pretends to
/// span a width it cannot see.
fn render_horizontal_rule(styles: &Styles) -> HoverLine {
    let text = "─".repeat(8);
    let text_length = text.len();
    HoverLine {
        text: StyledText {
            text,
            spans: vec![StyledSpan {
                range: 0..text_length,
                style: SpanStyle {
                    foreground: Some(styles.muted),
                    ..Default::default()
                },
            }],
        },
        wrap: false,
        indent: 0,
    }
}

/// Finds a `marker`-delimited pair starting exactly at `position`: the opening
/// marker must be followed by a non-space character, and a valid closer is one
/// preceded by a non-space character — an invalid candidate is skipped in
/// favour of a later one, so `**a** b **c**` does not close at the first `**`
/// it merely could. Returns the inner byte range and the index just past the
/// closing marker.
fn matched_span(source: &str, position: usize, marker: &str) -> Option<(Range<usize>, usize)> {
    if !source.get(position..)?.starts_with(marker) {
        return None;
    }
    let after_marker = position + marker.len();
    let first_character = source.get(after_marker..)?.chars().next()?;
    if first_character.is_whitespace() {
        return None;
    }

    let mut search_from = after_marker;
    loop {
        let relative = source.get(search_from..)?.find(marker)?;
        let close_start = search_from + relative;
        if close_start <= after_marker {
            search_from = close_start + marker.len();
            continue;
        }
        let previous_character = source.get(..close_start)?.chars().next_back()?;
        if previous_character.is_whitespace() {
            search_from = close_start + marker.len();
            continue;
        }
        return Some((after_marker..close_start, close_start + marker.len()));
    }
}

fn find_code_span(source: &str, position: usize) -> Option<(Range<usize>, usize)> {
    if !source.get(position..)?.starts_with('`') {
        return None;
    }
    let after = position + 1;
    let relative = source.get(after..)?.find('`')?;
    let close = after + relative;
    Some((after..close, close + 1))
}

fn find_link(source: &str, position: usize) -> Option<(Range<usize>, Range<usize>, usize)> {
    if !source.get(position..)?.starts_with('[') {
        return None;
    }
    let after_bracket = position + 1;
    let relative_close_bracket = source.get(after_bracket..)?.find(']')?;
    let close_bracket = after_bracket + relative_close_bracket;
    if !source.get(close_bracket + 1..)?.starts_with('(') {
        return None;
    }
    let url_start = close_bracket + 2;
    let relative_close_paren = source.get(url_start..)?.find(')')?;
    let url_end = url_start + relative_close_paren;
    Some((
        after_bracket..close_bracket,
        url_start..url_end,
        url_end + 1,
    ))
}

fn find_autolink(source: &str, position: usize) -> Option<(Range<usize>, usize)> {
    if !source.get(position..)?.starts_with('<') {
        return None;
    }
    let after = position + 1;
    let relative = source.get(after..)?.find('>')?;
    let close = after + relative;
    let inner = source.get(after..close)?;
    (inner.contains("://") && !inner.contains(' ')).then_some((after..close, close + 1))
}

fn with_bold(style: SpanStyle) -> SpanStyle {
    SpanStyle {
        bold: true,
        ..style
    }
}

fn with_italic(style: SpanStyle) -> SpanStyle {
    SpanStyle {
        italic: true,
        ..style
    }
}

fn with_strikethrough(style: SpanStyle) -> SpanStyle {
    SpanStyle {
        strikethrough: true,
        ..style
    }
}

fn with_code(style: SpanStyle, styles: &Styles) -> SpanStyle {
    SpanStyle {
        background: Some(styles.code_background),
        foreground: Some(styles.code_foreground),
        ..style
    }
}

fn with_link(style: SpanStyle, styles: &Styles) -> SpanStyle {
    SpanStyle {
        foreground: Some(styles.accent),
        underline: Some(Underline { color: None }),
        ..style
    }
}

fn with_muted(style: SpanStyle, styles: &Styles) -> SpanStyle {
    SpanStyle {
        foreground: Some(styles.muted),
        ..style
    }
}

fn append_run(
    out_text: &mut String,
    out_spans: &mut Vec<StyledSpan>,
    content: &str,
    style: SpanStyle,
) {
    if content.is_empty() {
        return;
    }
    let start = out_text.len();
    out_text.push_str(content);
    // A run with no attributes at all costs no span; the gap already reads as
    // the surface's own style.
    if style != SpanStyle::default() {
        out_spans.push(StyledSpan {
            range: start..out_text.len(),
            style,
        });
    }
}

fn flush_literal(
    source: &str,
    range: Range<usize>,
    style: SpanStyle,
    out_text: &mut String,
    out_spans: &mut Vec<StyledSpan>,
) {
    if range.start >= range.end {
        return;
    }
    append_run(
        out_text,
        out_spans,
        source.get(range).unwrap_or_default(),
        style,
    );
}

/// The inline scanner. `base_style` is whatever the enclosing markup already
/// decided — a link's accent, a heading's future bold, the code inside a
/// bolded run — so that nesting combines attributes instead of one replacing
/// another. Inline code is the one construct that does not recurse: its
/// content is taken verbatim, because backticks are markdown's own escape from
/// the rest of the syntax.
fn scan_inline(
    source: &str,
    base_style: SpanStyle,
    out_text: &mut String,
    out_spans: &mut Vec<StyledSpan>,
    styles: &Styles,
) {
    let mut index = 0;
    let mut literal_start = 0;

    while index < source.len() {
        if let Some((inner, end)) =
            matched_span(source, index, "**").or_else(|| matched_span(source, index, "__"))
        {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            scan_inline(
                source.get(inner).unwrap_or_default(),
                with_bold(base_style),
                out_text,
                out_spans,
                styles,
            );
            index = end;
            literal_start = index;
            continue;
        }
        if let Some((inner, end)) = matched_span(source, index, "~~") {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            scan_inline(
                source.get(inner).unwrap_or_default(),
                with_strikethrough(base_style),
                out_text,
                out_spans,
                styles,
            );
            index = end;
            literal_start = index;
            continue;
        }
        if let Some((inner, end)) =
            matched_span(source, index, "*").or_else(|| matched_span(source, index, "_"))
        {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            scan_inline(
                source.get(inner).unwrap_or_default(),
                with_italic(base_style),
                out_text,
                out_spans,
                styles,
            );
            index = end;
            literal_start = index;
            continue;
        }
        if let Some((inner, end)) = find_code_span(source, index) {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            append_run(
                out_text,
                out_spans,
                source.get(inner).unwrap_or_default(),
                with_code(base_style, styles),
            );
            index = end;
            literal_start = index;
            continue;
        }
        if let Some((text_range, url_range, end)) = find_link(source, index) {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            scan_inline(
                source.get(text_range).unwrap_or_default(),
                with_link(base_style, styles),
                out_text,
                out_spans,
                styles,
            );
            out_text.push(' ');
            append_run(
                out_text,
                out_spans,
                source.get(url_range).unwrap_or_default(),
                with_muted(base_style, styles),
            );
            index = end;
            literal_start = index;
            continue;
        }
        if let Some((url_range, end)) = find_autolink(source, index) {
            flush_literal(
                source,
                literal_start..index,
                base_style,
                out_text,
                out_spans,
            );
            append_run(
                out_text,
                out_spans,
                source.get(url_range).unwrap_or_default(),
                with_link(base_style, styles),
            );
            index = end;
            literal_start = index;
            continue;
        }

        let character_length = source
            .get(index..)
            .and_then(|rest| rest.chars().next())
            .map(char::len_utf8)
            .unwrap_or(1);
        index += character_length;
    }

    flush_literal(
        source,
        literal_start..index,
        base_style,
        out_text,
        out_spans,
    );
}

fn inline_styled(line: &str, styles: &Styles) -> StyledText {
    let mut text = String::new();
    let mut spans = Vec::new();
    scan_inline(line, SpanStyle::default(), &mut text, &mut spans, styles);
    StyledText { text, spans }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_styles() -> Styles {
        Styles {
            code_background: gpui::hsla(0.1, 0.5, 0.5, 1.0),
            code_foreground: gpui::hsla(0.2, 0.5, 0.5, 1.0),
            accent: gpui::hsla(0.3, 0.5, 0.5, 1.0),
            muted: gpui::hsla(0.4, 0.5, 0.5, 1.0),
        }
    }

    #[test]
    fn a_fence_is_verbatim_and_recorded_as_one_fence() {
        let styles = test_styles();
        let rendered = render(
            "```rust\nfn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n```",
            &styles,
        );

        assert_eq!(rendered.fences.len(), 1);
        let fence = &rendered.fences[0];
        assert_eq!(fence.language, "rust");
        assert_eq!(fence.lines, 0..3);
        assert_eq!(
            rendered.lines[0].text.text,
            "fn add(a: u32, b: u32) -> u32 {"
        );
        assert_eq!(rendered.lines[1].text.text, "    a + b");
        assert_eq!(rendered.lines[2].text.text, "}");
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| !line.text.text.contains("```"))
        );
    }

    #[test]
    fn prose_wraps_and_fences_do_not() {
        let styles = test_styles();
        let rendered = render("some prose\n```\ncode\n```", &styles);
        assert!(rendered.lines[0].wrap);
        assert!(!rendered.lines[1].wrap);
    }

    #[test]
    fn bold_and_code_spans_cover_the_right_bytes_and_drop_their_markers() {
        let styles = test_styles();
        let rendered = render("a **bold** and `code` word", &styles);
        let line = &rendered.lines[0];
        assert_eq!(line.text.text, "a bold and code word");

        let bold_span = line
            .text
            .spans
            .iter()
            .find(|span| span.style.bold)
            .expect("a bold span");
        assert_eq!(&line.text.text[bold_span.range.clone()], "bold");

        let code_span = line
            .text
            .spans
            .iter()
            .find(|span| span.style.background == Some(styles.code_background))
            .expect("a code span");
        assert_eq!(&line.text.text[code_span.range.clone()], "code");
    }

    #[test]
    fn stray_asterisks_around_spaces_stay_literal() {
        let styles = test_styles();
        let rendered = render("2 * 3 * 4", &styles);
        let line = &rendered.lines[0];
        assert_eq!(line.text.text, "2 * 3 * 4");
        assert!(line.text.spans.iter().all(|span| !span.style.italic));
    }

    #[test]
    fn a_link_underlines_its_text_and_dims_its_url() {
        let styles = test_styles();
        let rendered = render("[Zed](https://zed.dev)", &styles);
        let line = &rendered.lines[0];
        assert_eq!(line.text.text, "Zed https://zed.dev");

        let link_span = line
            .text
            .spans
            .iter()
            .find(|span| span.style.underline.is_some())
            .expect("a link span");
        assert_eq!(&line.text.text[link_span.range.clone()], "Zed");
        assert_eq!(link_span.style.foreground, Some(styles.accent));

        let url_span = line
            .text
            .spans
            .iter()
            .find(|span| span.style.foreground == Some(styles.muted))
            .expect("a url span");
        assert_eq!(&line.text.text[url_span.range.clone()], "https://zed.dev");
    }

    #[test]
    fn a_bullet_indents_its_continuation_under_the_text() {
        let styles = test_styles();
        let rendered = render("- an item", &styles);
        let line = &rendered.lines[0];
        assert_eq!(line.text.text, "• an item");
        assert!(line.wrap);
        assert_eq!(line.indent, text_cells("• ") as u16);
    }

    #[test]
    fn a_pipe_table_is_box_drawn_with_aligned_widths() {
        let styles = test_styles();
        let rendered = render("| a | bb |\n|---|---|\n| x | y |", &styles);

        assert_eq!(rendered.lines.len(), 5);
        assert!(rendered.lines.iter().all(|line| !line.wrap));

        let widths: Vec<usize> = rendered
            .lines
            .iter()
            .map(|line| line.text.text.chars().count())
            .collect();
        assert!(widths.iter().all(|&width| width == widths[0]));

        assert!(rendered.lines[0].text.text.starts_with('┌'));
        assert!(rendered.lines[2].text.text.starts_with('├'));
        assert!(rendered.lines[4].text.text.starts_with('└'));
    }

    #[test]
    fn a_header_without_a_delimiter_row_is_not_a_table() {
        let styles = test_styles();
        let rendered = render("| a | b |\njust prose", &styles);
        assert_eq!(rendered.lines.len(), 2);
        assert!(rendered.lines[0].wrap);
        assert_eq!(rendered.lines[0].text.text, "| a | b |");
    }

    #[test]
    fn leading_and_trailing_blank_lines_are_trimmed_and_interior_breaks_survive() {
        let styles = test_styles();
        let rendered = render("\n\none\n\n\ntwo\n\n", &styles);
        assert_eq!(rendered.lines.len(), 3);
        assert_eq!(rendered.lines[0].text.text, "one");
        assert!(rendered.lines[1].text.is_empty());
        assert_eq!(rendered.lines[2].text.text, "two");
    }

    #[test]
    fn spans_stay_ordered_and_in_bounds_across_a_mixed_document() {
        let styles = test_styles();
        let source = "# Heading\n\nSome **bold** and `code` and a [link](https://example.com).\n\n\
                       - a list item with *italic* text\n\n> a quoted **line**\n\n---\n\n```rust\nlet x = 1;\n```\n";
        let rendered = render(source, &styles);

        for line in &rendered.lines {
            let mut last_end = 0usize;
            for span in &line.text.spans {
                assert!(span.range.start >= last_end, "spans must stay in order");
                assert!(span.range.start <= span.range.end);
                assert!(
                    span.range.end <= line.text.text.len(),
                    "span must stay in bounds"
                );
                last_end = span.range.end;
            }
        }
    }
}
