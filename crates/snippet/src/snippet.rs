use anyhow::{Context as _, Result};
use smallvec::SmallVec;
use std::{collections::BTreeMap, ops::Range};

/// LSP-standard snippet variables (`TM_FILENAME`, `CURRENT_YEAR`, …) keyed by
/// their LSP names. Used by [`substitute_variables`] to expand `$NAME` /
/// `${NAME}` / `${NAME:default}` / `${NAME/regex/replacement/flags}` in
/// snippet body text, and pushed into the Rhai scope by `snippet_provider` so
/// scripted bodies can reference the same names directly.
///
/// Names are `&'static str` to match the LSP spec list and to avoid extra
/// allocations when iterating into a Rhai scope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnippetVariables {
    values: BTreeMap<&'static str, String>,
}

impl SnippetVariables {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: &'static str, value: impl Into<String>) {
        self.values.insert(name, value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.values.iter().map(|(k, v)| (*k, v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// All known LSP standard variable names mapped to empty strings. Useful
    /// when load-time-validating a Rhai snippet body that references LSP
    /// variables which won't be bound until expansion: pre-populating the
    /// scope with empty strings lets the body compile and evaluate to an
    /// empty / partial string, which is enough to confirm the body type
    /// checks against known names.
    pub fn lsp_defaults_empty() -> Self {
        let mut v = Self::new();
        for name in LSP_VARIABLE_NAMES {
            v.insert(name, String::new());
        }
        v
    }
}

/// The complete list of LSP standard snippet variable names recognised by Zed.
/// Source: <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#variables>.
pub const LSP_VARIABLE_NAMES: &[&str] = &[
    "TM_FILENAME",
    "TM_FILENAME_BASE",
    "TM_DIRECTORY",
    "TM_FILEPATH",
    "RELATIVE_FILEPATH",
    "CLIPBOARD",
    "WORKSPACE_NAME",
    "WORKSPACE_FOLDER",
    "TM_SELECTED_TEXT",
    "TM_CURRENT_LINE",
    "TM_CURRENT_WORD",
    "TM_LINE_INDEX",
    "TM_LINE_NUMBER",
    "CURRENT_YEAR",
    "CURRENT_YEAR_SHORT",
    "CURRENT_MONTH",
    "CURRENT_MONTH_NAME",
    "CURRENT_MONTH_NAME_SHORT",
    "CURRENT_DATE",
    "CURRENT_DAY_NAME",
    "CURRENT_DAY_NAME_SHORT",
    "CURRENT_HOUR",
    "CURRENT_MINUTE",
    "CURRENT_SECOND",
    "CURRENT_SECONDS_UNIX",
    "CURRENT_TIMEZONE_OFFSET",
    "RANDOM",
    "RANDOM_HEX",
    "UUID",
    "BLOCK_COMMENT_START",
    "BLOCK_COMMENT_END",
    "LINE_COMMENT",
];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snippet {
    pub text: String,
    pub tabstops: Vec<TabStop>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStop {
    pub ranges: SmallVec<[Range<isize>; 2]>,
    pub choices: Option<Vec<String>>,
    /// LSP `${N/regex/replacement/flags}` transforms attached to this tabstop.
    /// Each transform owns a region of `Snippet::text` whose contents are
    /// derived from the tabstop's current value. After [`Snippet::parse`] the
    /// region holds the transform applied to the placeholder default.
    pub transforms: SmallVec<[TabStopTransform; 1]>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TabStopTransform {
    pub range: Range<isize>,
    pub regex: String,
    pub replacement: String,
    pub flags: String,
}

impl Snippet {
    pub fn parse(source: &str) -> Result<Self> {
        let mut text = String::with_capacity(source.len());
        let mut tabstops = BTreeMap::new();
        parse_snippet(source, false, &mut text, &mut tabstops)
            .context("failed to parse snippet")?;

        render_initial_transforms(&mut text, &mut tabstops);

        let len = text.len() as isize;
        let final_tabstop = tabstops.remove(&0);
        let mut tabstops = tabstops.into_values().collect::<Vec<_>>();

        if let Some(final_tabstop) = final_tabstop {
            tabstops.push(final_tabstop);
        } else {
            let end_tabstop = TabStop {
                ranges: [len..len].into_iter().collect(),
                choices: None,
                transforms: Default::default(),
            };

            if !tabstops.last().is_some_and(|t| *t == end_tabstop) {
                tabstops.push(end_tabstop);
            }
        }

        Ok(Snippet { text, tabstops })
    }
}

/// Walk the parsed tabstops in slot order, render each transform using the
/// placeholder text of its source tabstop, and shift later ranges/slots by the
/// rendered length. Mutates `text` and `tabstops` in place.
fn render_initial_transforms(text: &mut String, tabstops: &mut BTreeMap<usize, TabStop>) {
    let mut order: Vec<(usize, usize, isize)> = Vec::new();
    for (key, ts) in tabstops.iter() {
        for (i, t) in ts.transforms.iter().enumerate() {
            order.push((*key, i, t.range.start));
        }
    }
    order.sort_by_key(|(_, _, start)| *start);

    for (key, idx, _) in order {
        let (regex, replacement, flags, slot_range) = {
            let t = &tabstops[&key].transforms[idx];
            (
                t.regex.clone(),
                t.replacement.clone(),
                t.flags.clone(),
                t.range.clone(),
            )
        };
        let source_value = {
            let ts = &tabstops[&key];
            match ts.ranges.first() {
                Some(r) => text[r.start as usize..r.end as usize].to_string(),
                None => String::new(),
            }
        };
        let output = apply_transform(&source_value, &regex, &replacement, &flags);
        let slot_start = slot_range.start as usize;
        let slot_end = slot_range.end as usize;
        let old_len = (slot_end - slot_start) as isize;
        let new_len = output.len() as isize;
        let len_diff = new_len - old_len;
        text.replace_range(slot_start..slot_end, &output);

        let pivot = slot_range.start;
        for ts in tabstops.values_mut() {
            for r in ts.ranges.iter_mut() {
                if r.start > pivot {
                    r.start += len_diff;
                }
                if r.end > pivot {
                    r.end += len_diff;
                }
            }
            for t in ts.transforms.iter_mut() {
                if t.range.start > pivot {
                    t.range.start += len_diff;
                }
                if t.range.end > pivot {
                    t.range.end += len_diff;
                }
            }
        }
        tabstops.get_mut(&key).unwrap().transforms[idx].range =
            slot_range.start..slot_range.start + new_len;
    }
}

fn parse_snippet<'a>(
    mut source: &'a str,
    nested: bool,
    text: &mut String,
    tabstops: &mut BTreeMap<usize, TabStop>,
) -> Result<&'a str> {
    loop {
        match source.chars().next() {
            None => return Ok(""),
            Some('$') => {
                source = parse_tabstop(&source[1..], text, tabstops)?;
            }
            Some('\\') => {
                // As specified in the LSP spec (`Grammar` section),
                // backslashes can escape some characters:
                // https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax
                source = &source[1..];
                if let Some(c) = source.chars().next() {
                    if c == '$' || c == '\\' || c == '}' {
                        text.push(c);
                        // All escapable characters are 1 byte long:
                        source = &source[1..];
                    } else {
                        text.push('\\');
                    }
                } else {
                    text.push('\\');
                }
            }
            Some('}') => {
                if nested {
                    return Ok(source);
                } else {
                    text.push('}');
                    source = &source[1..];
                }
            }
            Some(_) => {
                let chunk_end = source.find(['}', '$', '\\']).unwrap_or(source.len());
                let (chunk, rest) = source.split_at(chunk_end);
                text.push_str(chunk);
                source = rest;
            }
        }
    }
}

fn parse_tabstop<'a>(
    mut source: &'a str,
    text: &mut String,
    tabstops: &mut BTreeMap<usize, TabStop>,
) -> Result<&'a str> {
    let tabstop_start = text.len();
    let tabstop_index;
    let mut choices = None;
    let mut transform: Option<TabStopTransform> = None;

    if source.starts_with('{') {
        let (index, rest) = parse_int(&source[1..])?;
        tabstop_index = index;
        source = rest;

        if source.starts_with('|') {
            (source, choices) = parse_choices(&source[1..], text)?;
        } else if source.starts_with('/') {
            let (regex, n1) = read_transform_part(&source[1..])
                .context("malformed regex in tabstop transform")?;
            let after_regex = 1 + n1;
            let (replacement, n2) = read_transform_part(&source[after_regex..])
                .context("malformed replacement in tabstop transform")?;
            let after_repl = after_regex + n2;
            let (flags, n3) = read_until_close_brace(&source[after_repl..])
                .context("missing closing brace in tabstop transform")?;
            transform = Some(TabStopTransform {
                range: tabstop_start as isize..tabstop_start as isize,
                regex,
                replacement,
                flags,
            });
            source = &source[after_repl + n3..];
        }

        if transform.is_none() {
            if source.starts_with(':') {
                source = parse_snippet(&source[1..], true, text, tabstops)?;
            }

            if source.starts_with('}') {
                source = &source[1..];
            } else {
                anyhow::bail!("expected a closing brace");
            }
        }
    } else {
        let (index, rest) = parse_int(source)?;
        tabstop_index = index;
        source = rest;
    }

    let entry = tabstops.entry(tabstop_index).or_insert_with(|| TabStop {
        ranges: Default::default(),
        choices,
        transforms: Default::default(),
    });

    if let Some(t) = transform {
        entry.transforms.push(t);
    } else {
        entry
            .ranges
            .push(tabstop_start as isize..text.len() as isize);
    }
    Ok(source)
}

fn parse_int(source: &str) -> Result<(usize, &str)> {
    let len = source
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(source.len());
    anyhow::ensure!(len > 0, "expected an integer");
    let (prefix, suffix) = source.split_at(len);
    Ok((prefix.parse()?, suffix))
}

/// Replaces LSP standard variable references in `text` with values from
/// `vars`. Tabstops (`$0`, `${1:placeholder}`, `${1|a,b|}`) and backslash
/// escape sequences (`\$`, `\\`, `\}`) are preserved untouched so the result
/// can still be passed to [`Snippet::parse`].
///
/// Recognised forms:
/// - `$NAME` and `${NAME}` — substitute the variable's value (empty string
///   when absent).
/// - `${NAME:default}` — substitute the value, or the default when the
///   variable is absent or its value is empty.
/// - `${NAME/regex/replacement/flags}` — apply a regex transform to the
///   variable's value. Replacement uses Rust's `regex` syntax (`$1`, `$2`).
///   Supported flags: `g` (replace all), `i`, `m`, `s`. An invalid regex or
///   missing closing brace falls back to leaving the original text in place.
///
/// Unknown bare/braced names without a default are emitted as literal text
/// (escaped so the snippet parser doesn't choke), which is friendlier than
/// silently swallowing typos.
pub fn substitute_variables(text: &str, vars: &SnippetVariables) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            // Preserve LSP escape sequences verbatim so the parser still sees
            // them. We don't interpret them here.
            out.push('\\');
            let next = text[i + 1..].chars().next().unwrap_or('\0');
            out.push(next);
            i += 1 + next.len_utf8();
            continue;
        }
        if c == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next.is_ascii_digit() {
                // Numeric tabstop — leave for the parser.
                out.push('$');
                i += 1;
                continue;
            }
            if next == b'{' {
                if i + 2 < bytes.len() {
                    let inner = bytes[i + 2];
                    if inner.is_ascii_digit() {
                        // ${1...} numeric tabstop — leave for the parser.
                        out.push('$');
                        i += 1;
                        continue;
                    }
                    if inner.is_ascii_alphabetic() || inner == b'_' {
                        match consume_braced_variable(&text[i + 2..], vars) {
                            Some((value, consumed)) => {
                                out.push_str(&value);
                                i += 2 + consumed;
                                continue;
                            }
                            None => {
                                // Malformed expression — emit the `$` and
                                // continue; downstream parsing will likely
                                // fail loudly, which is the right signal.
                                out.push('$');
                                i += 1;
                                continue;
                            }
                        }
                    }
                }
                // `${` followed by something we don't handle (e.g. `${|...|}`
                // for choice-only tabstops aren't a thing). Pass `$` through.
                out.push('$');
                i += 1;
                continue;
            }
            if next.is_ascii_alphabetic() || next == b'_' {
                let name_start = i + 1;
                let mut j = name_start;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                {
                    j += 1;
                }
                let name = &text[name_start..j];
                if let Some(value) = vars.get(name) {
                    out.push_str(value);
                } else {
                    // Echo back the original text, but escape the `$` so the
                    // LSP parser treats it as literal rather than an invalid
                    // tabstop.
                    out.push_str("\\$");
                    out.push_str(name);
                }
                i = j;
                continue;
            }
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parses the inside of `${…}` after the opening `${`. Returns the substituted
/// value and the number of bytes consumed (including the closing `}`), or
/// `None` if the expression is malformed (no closing brace, unsupported
/// separator, …) so the caller can fall back.
fn consume_braced_variable(text: &str, vars: &SnippetVariables) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut j = 0;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j == 0 {
        return None;
    }
    let name = &text[..j];
    if j >= bytes.len() {
        return None;
    }
    match bytes[j] {
        b'}' => {
            let value = match vars.get(name) {
                Some(v) => v.to_string(),
                None => format!("\\${{{name}}}"),
            };
            Some((value, j + 1))
        }
        b':' => {
            let (default, consumed) = read_until_close_brace(&text[j + 1..])?;
            let value = vars
                .get(name)
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string())
                .unwrap_or(default);
            Some((value, j + 1 + consumed))
        }
        b'/' => {
            let (regex_src, consumed1) = read_transform_part(&text[j + 1..])?;
            let after_regex = j + 1 + consumed1;
            let (replacement, consumed2) = read_transform_part(&text[after_regex..])?;
            let after_replacement = after_regex + consumed2;
            let (flags, consumed3) = read_until_close_brace(&text[after_replacement..])?;
            let total = after_replacement + consumed3;
            let original = vars.get(name).unwrap_or("").to_string();
            Some((apply_transform(&original, &regex_src, &replacement, &flags), total))
        }
        _ => None,
    }
}

/// Read until an unescaped `/`. Returns `(content, bytes_consumed_including_slash)`
/// or `None` if no `/` is found. Only `\/` is unescaped (to a literal `/`);
/// other backslash sequences pass through verbatim so regex escapes like `\.`,
/// `\d`, `\s` reach the regex engine unchanged.
fn read_transform_part(text: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let next = text[i + 1..].chars().next().unwrap_or('\0');
            if next == '/' {
                out.push('/');
            } else {
                out.push('\\');
                out.push(next);
            }
            i += 1 + next.len_utf8();
            continue;
        }
        if c == b'/' {
            return Some((out, i + 1));
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    None
}

/// Read until an unescaped `}`. Returns `(content, bytes_consumed_including_brace)`
/// or `None` if no `}` is found. Only `\}` is unescaped (to a literal `}`);
/// other backslash sequences pass through verbatim.
fn read_until_close_brace(text: &str) -> Option<(String, usize)> {
    let mut out = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && i + 1 < bytes.len() {
            let next = text[i + 1..].chars().next().unwrap_or('\0');
            if next == '}' {
                out.push('}');
            } else {
                out.push('\\');
                out.push(next);
            }
            i += 1 + next.len_utf8();
            continue;
        }
        if c == b'}' {
            return Some((out, i + 1));
        }
        let ch = text[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    None
}

pub fn apply_transform(value: &str, regex_src: &str, replacement: &str, flags: &str) -> String {
    let mut prefix = String::new();
    let mut global = false;
    for c in flags.chars() {
        match c {
            'g' => global = true,
            'i' => prefix.push_str("(?i)"),
            'm' => prefix.push_str("(?m)"),
            's' => prefix.push_str("(?s)"),
            // Unknown flags (e.g. JS-only `u`) are silently ignored.
            _ => {}
        }
    }
    let pattern = format!("{prefix}{regex_src}");
    let regex = match regex::Regex::new(&pattern) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("snippet variable transform regex `{pattern}` failed to compile: {e}");
            return value.to_string();
        }
    };
    if global {
        regex.replace_all(value, replacement).into_owned()
    } else {
        regex.replace(value, replacement).into_owned()
    }
}

fn parse_choices<'a>(
    mut source: &'a str,
    text: &mut String,
) -> Result<(&'a str, Option<Vec<String>>)> {
    let mut found_default_choice = false;
    let mut current_choice = String::new();
    let mut choices = Vec::new();

    loop {
        match source.chars().next() {
            None => return Ok(("", Some(choices))),
            Some('\\') => {
                source = &source[1..];

                if let Some(c) = source.chars().next() {
                    current_choice.push(c);
                    if !found_default_choice {
                        text.push(c);
                    }
                    source = &source[c.len_utf8()..];
                }
            }
            Some(',') => {
                found_default_choice = true;
                source = &source[1..];
                choices.push(current_choice);
                current_choice = String::new();
            }
            Some('|') => {
                source = &source[1..];
                choices.push(current_choice);
                return Ok((source, Some(choices)));
            }
            Some(_) => {
                let chunk_end = source.find([',', '|', '\\']);

                anyhow::ensure!(
                    chunk_end.is_some(),
                    "Placeholder choice doesn't contain closing pipe-character '|'"
                );

                let (chunk, rest) = source.split_at(chunk_end.unwrap());

                if !found_default_choice {
                    text.push_str(chunk);
                }

                current_choice.push_str(chunk);
                source = rest;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snippet_without_tabstops() {
        let snippet = Snippet::parse("one-two-three").unwrap();
        assert_eq!(snippet.text, "one-two-three");
        assert_eq!(tabstops(&snippet), &[vec![13..13]]);
    }

    #[test]
    fn test_snippet_with_tabstops() {
        let snippet = Snippet::parse("one$1two").unwrap();
        assert_eq!(snippet.text, "onetwo");
        assert_eq!(tabstops(&snippet), &[vec![3..3], vec![6..6]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None]);

        // Multi-digit numbers
        let snippet = Snippet::parse("one$123-$99-two").unwrap();
        assert_eq!(snippet.text, "one--two");
        assert_eq!(tabstops(&snippet), &[vec![4..4], vec![3..3], vec![8..8]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None, &None]);
    }

    #[test]
    fn test_snippet_with_last_tabstop_at_end() {
        let snippet = Snippet::parse(r#"foo.$1"#).unwrap();

        // If the final tabstop is already at the end of the text, don't insert
        // an additional tabstop at the end.
        assert_eq!(snippet.text, r#"foo."#);
        assert_eq!(tabstops(&snippet), &[vec![4..4]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);
    }

    #[test]
    fn test_snippet_with_explicit_final_tabstop() {
        let snippet = Snippet::parse(r#"<div class="$1">$0</div>"#).unwrap();

        // If the final tabstop is explicitly specified via '$0', then
        // don't insert an additional tabstop at the end.
        assert_eq!(snippet.text, r#"<div class=""></div>"#);
        assert_eq!(tabstops(&snippet), &[vec![12..12], vec![14..14]]);
        assert_eq!(tabstop_choices(&snippet), &[&None, &None]);
    }

    #[test]
    fn test_snippet_with_placeholders() {
        let snippet = Snippet::parse("one${1:two}three${2:four}").unwrap();
        assert_eq!(snippet.text, "onetwothreefour");
        assert_eq!(
            tabstops(&snippet),
            &[vec![3..6], vec![11..15], vec![15..15]]
        );
        assert_eq!(tabstop_choices(&snippet), &[&None, &None, &None]);
    }

    #[test]
    fn test_snippet_with_choice_placeholders() {
        let snippet = Snippet::parse("type ${1|i32, u32|} = $2")
            .expect("Should be able to unpack choice placeholders");

        assert_eq!(snippet.text, "type i32 = ");
        assert_eq!(tabstops(&snippet), &[vec![5..8], vec![11..11],]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["i32".to_string(), " u32".to_string()]), &None]
        );

        let snippet = Snippet::parse(r"${1|\$\{1\|one\,two\,tree\|\}|}")
            .expect("Should be able to parse choice with escape characters");

        assert_eq!(snippet.text, "${1|one,two,tree|}");
        assert_eq!(tabstops(&snippet), &[vec![0..18], vec![18..18]]);
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["${1|one,two,tree|}".to_string(),]), &None]
        );
    }

    #[test]
    fn test_snippet_with_nested_placeholders() {
        let snippet = Snippet::parse(
            "for (${1:var ${2:i} = 0; ${2:i} < ${3:${4:array}.length}; ${2:i}++}) {$0}",
        )
        .unwrap();
        assert_eq!(snippet.text, "for (var i = 0; i < array.length; i++) {}");
        assert_eq!(
            tabstops(&snippet),
            &[
                vec![5..37],
                vec![9..10, 16..17, 34..35],
                vec![20..32],
                vec![20..25],
                vec![40..40],
            ]
        );
        assert_eq!(
            tabstop_choices(&snippet),
            &[&None, &None, &None, &None, &None]
        );
    }

    #[test]
    fn test_snippet_with_three_choices_no_leading_empty() {
        // Regression: `${1|string,char,int|}` should parse to a single tabstop
        // whose placeholder is "string" and whose choice list is exactly the
        // three values. The first choice is the default placeholder.
        let snippet = Snippet::parse("${1|string,char,int|}").unwrap();
        assert_eq!(snippet.text, "string");
        assert_eq!(snippet.tabstops[0].ranges.as_slice(), &[0..6]);
        assert_eq!(
            snippet.tabstops[0].choices,
            Some(vec![
                "string".to_string(),
                "char".to_string(),
                "int".to_string()
            ])
        );
    }

    #[test]
    fn test_snippet_with_single_choice() {
        // Single-element choice list. Per LSP spec the picker still appears,
        // but the only option is selected as the placeholder text.
        let snippet = Snippet::parse("${1|only|}").unwrap();
        assert_eq!(snippet.text, "only");
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["only".to_string()]), &None]
        );
    }

    #[test]
    fn test_snippet_choice_with_escaped_separators() {
        // Commas and pipes inside a choice must be escaped with `\`. Verify
        // they survive parsing and end up as literal characters in the choice
        // values.
        let snippet = Snippet::parse(r"${1|a\,b,c\|d|}").unwrap();
        assert_eq!(snippet.text, "a,b");
        assert_eq!(
            tabstop_choices(&snippet),
            &[&Some(vec!["a,b".to_string(), "c|d".to_string()]), &None]
        );
    }

    #[test]
    fn test_nested_placeholders_with_transform() {
        // Outer placeholder contains an inner one; a transform on the outer
        // tabstop should still render correctly using the placeholder text.
        let snippet = Snippet::parse("${1:foo ${2:bar}}-${1/(.+)/[$1]/}").unwrap();
        assert_eq!(snippet.text, "foo bar-[foo bar]");
        // Tabstop 1 covers "foo bar" (length 7).
        assert_eq!(snippet.tabstops[0].ranges.as_slice(), &[0..7]);
        assert_eq!(snippet.tabstops[0].transforms.len(), 1);
        assert_eq!(snippet.tabstops[0].transforms[0].range, 8..17);
        // Inner tabstop 2 covers "bar" inside the outer placeholder.
        assert_eq!(snippet.tabstops[1].ranges.as_slice(), &[4..7]);
    }

    #[test]
    fn test_tabstop_transform_renders_initial_value() {
        // `${1:foo}` placeholder followed by `${1/(.+)/$1!/}` transform.
        // After parsing, the transform region is filled with the regex applied
        // to the placeholder text.
        let snippet = Snippet::parse("${1:foo}-${1/(.+)/$1!/}").unwrap();
        assert_eq!(snippet.text, "foo-foo!");
        // Tabstop 1 has the placeholder range [0..3] and the rendered transform
        // range [4..8] (the `foo!` region).
        assert_eq!(snippet.tabstops[0].ranges.as_slice(), &[0..3]);
        assert_eq!(snippet.tabstops[0].transforms.len(), 1);
        let t = &snippet.tabstops[0].transforms[0];
        assert_eq!(t.range, 4..8);
        assert_eq!(t.regex, "(.+)");
        assert_eq!(t.replacement, "$1!");
    }

    #[test]
    fn test_tabstop_transform_with_no_placeholder_uses_empty_input() {
        // A transform whose source tabstop has no placeholder yet (only a
        // bare `$1` reference appears later) should render against the empty
        // string. Per the LSP spec this leaves the region empty when the
        // regex doesn't match.
        let snippet = Snippet::parse("${1/(.+)/[$1]/}-$1").unwrap();
        // The transform region is empty (regex doesn't match ""), then `-`
        // separator, then the bare `$1` (zero-width).
        assert_eq!(snippet.text, "-");
        assert_eq!(snippet.tabstops[0].transforms.len(), 1);
        assert_eq!(snippet.tabstops[0].transforms[0].range, 0..0);
        // Bare `$1` sits after the `-`.
        assert_eq!(snippet.tabstops[0].ranges.as_slice(), &[1..1]);
    }

    #[test]
    fn test_tabstop_transform_global_flag() {
        let snippet = Snippet::parse("${1:abc}-${1/[a-z]/X/g}").unwrap();
        assert_eq!(snippet.text, "abc-XXX");
        assert_eq!(snippet.tabstops[0].transforms[0].range, 4..7);
    }

    #[test]
    fn test_tabstop_transform_shifts_later_tabstops() {
        let snippet = Snippet::parse("${1:hi}-${1/(.+)/[$1]/}-${2:bye}").unwrap();
        assert_eq!(snippet.text, "hi-[hi]-bye");
        // Tabstop 1 placeholder is at 0..2, transform at 3..7.
        assert_eq!(snippet.tabstops[0].ranges.as_slice(), &[0..2]);
        assert_eq!(snippet.tabstops[0].transforms[0].range, 3..7);
        // Tabstop 2 placeholder is at 8..11 — shifted by the transform output.
        assert_eq!(snippet.tabstops[1].ranges.as_slice(), &[8..11]);
    }

    #[test]
    fn test_tabstop_transform_missing_closing_brace_errors() {
        assert!(Snippet::parse("${1/foo/bar/").is_err());
    }

    #[test]
    fn test_snippet_parsing_with_escaped_chars() {
        let snippet = Snippet::parse("\"\\$schema\": $1").unwrap();
        assert_eq!(snippet.text, "\"$schema\": ");
        assert_eq!(tabstops(&snippet), &[vec![11..11]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);

        let snippet = Snippet::parse("{a\\}").unwrap();
        assert_eq!(snippet.text, "{a}");
        assert_eq!(tabstops(&snippet), &[vec![3..3]]);
        assert_eq!(tabstop_choices(&snippet), &[&None]);

        // backslash not functioning as an escape
        let snippet = Snippet::parse("a\\b").unwrap();
        assert_eq!(snippet.text, "a\\b");
        assert_eq!(tabstops(&snippet), &[vec![3..3]]);

        // first backslash cancelling escaping that would
        // have happened with second backslash
        let snippet = Snippet::parse("one\\\\$1two").unwrap();
        assert_eq!(snippet.text, "one\\two");
        assert_eq!(tabstops(&snippet), &[vec![4..4], vec![7..7]]);
    }

    fn tabstops(snippet: &Snippet) -> Vec<Vec<Range<isize>>> {
        snippet.tabstops.iter().map(|t| t.ranges.to_vec()).collect()
    }

    fn tabstop_choices(snippet: &Snippet) -> Vec<&Option<Vec<String>>> {
        snippet.tabstops.iter().map(|t| &t.choices).collect()
    }

    fn vars(entries: &[(&'static str, &str)]) -> SnippetVariables {
        let mut v = SnippetVariables::new();
        for (k, value) in entries {
            v.insert(k, *value);
        }
        v
    }

    #[test]
    fn substitute_bare_and_braced_forms_match() {
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        assert_eq!(substitute_variables("$TM_FILENAME", &v), "main.rs");
        assert_eq!(substitute_variables("${TM_FILENAME}", &v), "main.rs");
        assert_eq!(
            substitute_variables("see $TM_FILENAME for details", &v),
            "see main.rs for details"
        );
    }

    #[test]
    fn substitute_unknown_variable_emits_literal() {
        // Unknown variables shouldn't error; they should round-trip through
        // the LSP parser as literal text. We escape the `$` so the parser
        // doesn't try to read it as a tabstop.
        let v = SnippetVariables::new();
        let out = substitute_variables("hello $UNKNOWN_VAR end", &v);
        let parsed = Snippet::parse(&out).unwrap();
        assert_eq!(parsed.text, "hello $UNKNOWN_VAR end");

        let out = substitute_variables("hello ${UNKNOWN_VAR} end", &v);
        let parsed = Snippet::parse(&out).unwrap();
        assert_eq!(parsed.text, "hello ${UNKNOWN_VAR} end");
    }

    #[test]
    fn substitute_default_value() {
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        assert_eq!(
            substitute_variables("${TM_FILENAME:fallback.txt}", &v),
            "main.rs"
        );
        let empty = SnippetVariables::new();
        assert_eq!(
            substitute_variables("${TM_FILENAME:fallback.txt}", &empty),
            "fallback.txt"
        );
    }

    #[test]
    fn substitute_transform_strips_extension() {
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        assert_eq!(
            substitute_variables(r"${TM_FILENAME/(.+)\..*/$1/}", &v),
            "main"
        );
    }

    #[test]
    fn substitute_transform_no_match_returns_original() {
        // Per LSP spec: when the regex doesn't match, the variable's value is
        // emitted unchanged.
        let v = vars(&[("TM_FILENAME", "main")]);
        assert_eq!(
            substitute_variables(r"${TM_FILENAME/(.+)\.(.+)/$1/}", &v),
            "main"
        );
    }

    #[test]
    fn substitute_transform_global_flag() {
        let v = vars(&[("WORD", "abc-abc-abc")]);
        assert_eq!(
            substitute_variables("${WORD/-/_/g}", &v),
            "abc_abc_abc"
        );
        assert_eq!(
            substitute_variables("${WORD/-/_/}", &v),
            "abc_abc-abc"
        );
    }

    #[test]
    fn substitute_preserves_escapes() {
        // `\$TM_FILENAME` must remain a literal `$TM_FILENAME` so users can
        // opt out of substitution; the parser later turns `\$` into `$`.
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        let out = substitute_variables(r"\$TM_FILENAME and $TM_FILENAME", &v);
        let parsed = Snippet::parse(&out).unwrap();
        assert_eq!(parsed.text, "$TM_FILENAME and main.rs");
    }

    #[test]
    fn substitute_passes_tabstops_through() {
        // Tabstops, both bare and braced, must not be touched: the LSP parser
        // is the one that understands them.
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        let out = substitute_variables("fn $1($2) {} $0", &v);
        assert_eq!(out, "fn $1($2) {} $0");
        let out = substitute_variables("${1:default} ${2|a,b|}", &v);
        assert_eq!(out, "${1:default} ${2|a,b|}");
    }

    #[test]
    fn substitute_invalid_transform_regex_is_logged_and_skipped() {
        // An invalid regex shouldn't panic — we keep the variable's value as
        // a safe fallback.
        let v = vars(&[("TM_FILENAME", "main.rs")]);
        let out = substitute_variables("${TM_FILENAME/(/x/}", &v);
        assert_eq!(out, "main.rs");
    }
}
