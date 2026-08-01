//! `ted`'s own `:` line (SPEC §13.2).
//!
//! The line itself is a rendering concern here; the *semantics* stay in `vim`,
//! which registers a `GlobalCommandPaletteInterceptor` that parses `:w`, `:wq`,
//! `:q!`, `:42`, `:%s/a/b/g`, ranges and the rest of its command table
//! (`crates/vim/src/command.rs`). `ted` sends the typed query through that
//! interceptor and dispatches whatever action comes back, so there is no
//! duplicated parsing anywhere in this file.

use command_palette_hooks::{CommandPaletteFilter, GlobalCommandPaletteInterceptor};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use gpui::{Action, App, AsyncApp, Entity, WeakEntity};
use workspace::Workspace;

use crate::snapshot::CommandLineView;

/// How many completions `ted` keeps. The line shows one at a time, and a user
/// cycling further than this is better served by typing more of the command.
const MAX_COMPLETIONS: usize = 32;

/// What a keystroke did to the command line, as far as the frame loop is
/// concerned.
#[derive(Debug, PartialEq, Eq)]
pub enum Update {
    /// The query changed; completions need refreshing.
    QueryChanged,
    /// Nothing that affects completions changed.
    Unchanged,
    Submit,
    Cancel,
}

struct Completion {
    label: String,
    action: Box<dyn Action>,
}

pub struct CommandLine {
    /// The range vim would have seeded the palette with (`'<,'>`, `.,.+4`, or
    /// empty), which is part of the query the interceptor parses.
    prefix: String,
    query: String,
    /// Byte offset of the insertion point within `query`.
    cursor: usize,
    completions: Vec<Completion>,
    selected: Option<usize>,
    message: Option<String>,
}

impl CommandLine {
    pub fn new(prefix: String) -> Self {
        let cursor = prefix.len();
        Self {
            prefix: prefix.clone(),
            query: prefix,
            cursor,
            completions: Vec::new(),
            selected: None,
            message: None,
        }
    }

    pub fn view(&self) -> CommandLineView {
        CommandLineView {
            prefix: ':',
            query: self.query.clone(),
            cursor: self.cursor,
            completions: self
                .completions
                .iter()
                .map(|completion| completion.label.clone())
                .collect(),
            selected_completion: self.selected,
            message: self.message.clone(),
        }
    }

    pub fn handle_key(&mut self, event: &KeyEvent) -> Update {
        if matches!(event.kind, KeyEventKind::Release) {
            return Update::Unchanged;
        }
        let control = event.modifiers.contains(KeyModifiers::CONTROL);

        match event.code {
            KeyCode::Esc => Update::Cancel,
            KeyCode::Char('c') if control => Update::Cancel,
            KeyCode::Enter => Update::Submit,
            KeyCode::Backspace => {
                // Backspacing past the prefix closes the line, the way vim's
                // command line does rather than leaving an empty `:` open.
                if self.cursor <= self.prefix.len() {
                    return Update::Cancel;
                }
                let Some(previous) = self.query[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                else {
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
            KeyCode::Tab | KeyCode::Down => {
                self.cycle_completion(1);
                Update::Unchanged
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.cycle_completion(-1);
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

    fn cycle_completion(&mut self, delta: isize) {
        if self.completions.is_empty() {
            self.selected = None;
            return;
        }
        let count = self.completions.len() as isize;
        let current = self.selected.map(|index| index as isize).unwrap_or(-1);
        let next = (current + delta).rem_euclid(count);
        self.selected = Some(next as usize);
    }

    /// The text the interceptor parses: the range prefix plus what was typed
    /// after it.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Recomputes the completion list for the current query.
    ///
    /// The interceptor is vim's, so `:w`, `:42` and `:%s/a/b/g` resolve to real
    /// actions with no parsing here. When it declines the query — or is
    /// non-exclusive — `ted` falls back to matching action names, which is the
    /// same policy Zed's own palette applies.
    pub async fn refresh(&mut self, workspace: WeakEntity<Workspace>, cx: &mut AsyncApp) {
        let query = self.query.clone();
        let intercepted = cx
            .update(|cx| GlobalCommandPaletteInterceptor::intercept(&query, workspace.clone(), cx));

        let mut completions = Vec::new();
        let mut exclusive = false;
        if let Some(task) = intercepted {
            let result = task.await;
            exclusive = result.exclusive;
            completions.extend(result.results.into_iter().map(|item| Completion {
                label: item.string,
                action: item.action,
            }));
        }

        if !exclusive {
            let by_name = cx.update(|cx| matching_action_names(&query, cx));
            completions.extend(by_name);
        }
        completions.truncate(MAX_COMPLETIONS);

        self.selected = (!completions.is_empty()).then_some(0);
        self.message = completions
            .is_empty()
            .then(|| format!("no command matches {query:?}"));
        self.completions = completions;
    }

    /// The action `enter` should dispatch: the selected completion, or the
    /// first one when the user never cycled.
    pub fn selected_action(&self) -> Option<Box<dyn Action>> {
        let index = self.selected.unwrap_or(0);
        Some(self.completions.get(index)?.action.boxed_clone())
    }
}

/// Actions whose humanized name contains every character of `query` in order,
/// excluding the ones the palette filter hides (which is where SPEC §5.5's
/// font-size actions are suppressed).
fn matching_action_names(query: &str, cx: &mut App) -> Vec<Completion> {
    let normalized = command_palette::normalize_action_query(query).to_lowercase();
    if normalized.is_empty() {
        return Vec::new();
    }

    let names = cx.all_action_names().to_vec();
    let mut matches = Vec::new();
    for name in names {
        let humanized = command_palette::humanize_action_name(name);
        if !is_subsequence(&normalized, &humanized.to_lowercase()) {
            continue;
        }
        let Ok(action) = cx.build_action(name, None) else {
            continue;
        };
        if CommandPaletteFilter::try_global(cx)
            .is_some_and(|filter| filter.is_hidden(action.as_ref()))
        {
            continue;
        }
        matches.push(Completion {
            label: humanized,
            action,
        });
    }

    matches.sort_by_key(|completion| completion.label.len());
    matches
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle
        .chars()
        .all(|wanted| haystack.any(|candidate| candidate == wanted))
}

/// The prefix vim's own `:` bindings would have used, consuming the pending
/// count so the next motion does not inherit it.
pub fn prefix_for(editor: &Entity<editor::Editor>, cx: &mut App) -> String {
    let editor = editor.clone();
    editor.update(cx, |editor, cx| vim::take_command_line_prefix(editor, cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_builds_the_query_after_the_prefix() {
        let mut line = CommandLine::new("'<,'>".to_owned());
        assert_eq!(line.query(), "'<,'>");
        for character in "s/a/b/".chars() {
            assert_eq!(
                line.handle_key(&key(KeyCode::Char(character))),
                Update::QueryChanged
            );
        }
        assert_eq!(line.query(), "'<,'>s/a/b/");
    }

    #[test]
    fn backspacing_into_the_prefix_cancels() {
        let mut line = CommandLine::new(String::new());
        line.handle_key(&key(KeyCode::Char('w')));
        assert_eq!(
            line.handle_key(&key(KeyCode::Backspace)),
            Update::QueryChanged
        );
        assert_eq!(line.query(), "");
        assert_eq!(line.handle_key(&key(KeyCode::Backspace)), Update::Cancel);
    }

    #[test]
    fn a_range_prefix_is_not_backspaced_away() {
        let mut line = CommandLine::new("'<,'>".to_owned());
        assert_eq!(line.handle_key(&key(KeyCode::Backspace)), Update::Cancel);
        assert_eq!(line.query(), "'<,'>");
    }

    #[test]
    fn escape_and_enter_are_distinct_outcomes() {
        let mut line = CommandLine::new(String::new());
        assert_eq!(line.handle_key(&key(KeyCode::Esc)), Update::Cancel);
        assert_eq!(line.handle_key(&key(KeyCode::Enter)), Update::Submit);
    }

    #[test]
    fn cursor_movement_does_not_invalidate_completions() {
        let mut line = CommandLine::new(String::new());
        for character in "wq".chars() {
            line.handle_key(&key(KeyCode::Char(character)));
        }
        assert_eq!(line.handle_key(&key(KeyCode::Left)), Update::Unchanged);
        assert_eq!(line.cursor, 1);
        assert_eq!(line.handle_key(&key(KeyCode::Home)), Update::Unchanged);
        assert_eq!(line.cursor, 0);
        assert_eq!(line.handle_key(&key(KeyCode::End)), Update::Unchanged);
        assert_eq!(line.cursor, 2);
    }

    #[test]
    fn inserting_at_the_cursor_rather_than_the_end() {
        let mut line = CommandLine::new(String::new());
        for character in "wq".chars() {
            line.handle_key(&key(KeyCode::Char(character)));
        }
        line.handle_key(&key(KeyCode::Left));
        line.handle_key(&key(KeyCode::Char('!')));
        assert_eq!(line.query(), "w!q");
    }

    #[test]
    fn multibyte_characters_survive_editing() {
        let mut line = CommandLine::new(String::new());
        for character in "s/é/e/".chars() {
            line.handle_key(&key(KeyCode::Char(character)));
        }
        assert_eq!(line.query(), "s/é/e/");
        line.handle_key(&key(KeyCode::Backspace));
        assert_eq!(line.query(), "s/é/e");
    }

    #[test]
    fn subsequence_matching_is_order_sensitive() {
        assert!(is_subsequence("wq", "write quit"));
        assert!(is_subsequence("save", "workspace: save"));
        assert!(!is_subsequence("qw", "write quit"));
    }
}
