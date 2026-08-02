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
use fuzzy_nucleo::StringMatchCandidate;
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

/// A `:` command that means something only because `ted` owns a tty (SPEC
/// §13.4). These cannot go through the interceptor, which resolves a query to a
/// `Box<dyn Action>`; they suspend the process instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostCommand {
    /// `:!<command>` — run a command with the terminal to itself.
    Shell(String),
    /// `:Explore` — browse files in a file manager (SPEC §13.4).
    Explore,
}

/// What `enter` does with the completion the user chose.
pub enum Effect {
    Dispatch(Box<dyn Action>),
    Host(HostCommand),
}

impl Clone for Effect {
    fn clone(&self) -> Self {
        match self {
            Self::Dispatch(action) => Self::Dispatch(action.boxed_clone()),
            Self::Host(command) => Self::Host(command.clone()),
        }
    }
}

struct Completion {
    label: String,
    effect: Effect,
}

/// What the rest of the line says about the selected candidate (SPEC §24.3).
enum Ghost {
    /// The query is a prefix of the command, so `right` accepts the rest of it.
    Rest(String),
    /// It is not, so the line describes the candidate instead. There is nothing
    /// to accept: appending a description would not be a command.
    Description(String),
}

impl Ghost {
    fn text(&self) -> &str {
        match self {
            Self::Rest(text) | Self::Description(text) => text,
        }
    }
}

/// Which line this is. They are drawn the same and share every editing key; what
/// differs is what is behind them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Resolved through the host table and then vim's interceptor (SPEC §13.2).
    Command,
    /// A bare number, with no interceptor behind it at all. The only go-to-line
    /// code `ted` has, and it exists only for a `--no-vim` session, where there
    /// is no `:` line for `:42` to be typed on (SPEC §24.5).
    Number,
}

pub struct CommandLine {
    mode: Mode,
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
            mode: Mode::Command,
            prefix: prefix.clone(),
            query: prefix,
            cursor,
            completions: Vec::new(),
            selected: None,
            message: None,
        }
    }

    pub fn go_to_line() -> Self {
        Self {
            mode: Mode::Number,
            ..Self::new(String::new())
        }
    }

    /// The line the user typed, when this is the number prompt and they typed
    /// one.
    pub fn line_number(&self) -> Option<u32> {
        (self.mode == Mode::Number)
            .then(|| self.query.trim().parse().ok())
            .flatten()
    }

    pub fn view(&self) -> CommandLineView {
        CommandLineView {
            prefix: ':',
            query: self.query.clone(),
            cursor: self.cursor,
            ghost: self.ghost().map(|ghost| ghost.text().to_owned()),
            trailing: self.candidate_count().into_iter().collect(),
            message: self.message.clone(),
        }
    }

    /// The rest of the selected candidate, or a description of it when the query
    /// is not a prefix of one.
    fn ghost(&self) -> Option<Ghost> {
        if self.mode == Mode::Number {
            return None;
        }
        let label = &self.completions.get(self.selected?)?.label;
        // vim's interceptor reports its commands with the `:` the user already
        // typed to open this line, and `ted`'s query does not carry it.
        let command = label.strip_prefix(':').unwrap_or(label);
        match command.get(..self.query.len()) {
            Some(head) if head.eq_ignore_ascii_case(&self.query) && !self.query.is_empty() => {
                Some(Ghost::Rest(command[self.query.len()..].to_owned()))
            }
            _ => Some(Ghost::Description(format!(" — {label}"))),
        }
    }

    /// How many other candidates the matcher found, which is what the right-hand
    /// end of the line says when the selected action has no keybinding to show
    /// there instead.
    fn candidate_count(&self) -> Option<String> {
        let others = self.completions.len().checked_sub(1).filter(|&n| n > 0)?;
        Some(format!("ctrl-n: {others} more"))
    }

    /// The action `enter` would dispatch, for the frame loop to look a
    /// keybinding up for — `Window::keystroke_text_for` needs a window, and the
    /// selected candidate changes without a refresh.
    pub fn selected_action(&self) -> Option<Box<dyn Action>> {
        match self.completions.get(self.selected?)?.effect.clone() {
            Effect::Dispatch(action) => Some(action),
            Effect::Host(_) => None,
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
            // At the end of the query `right` accepts the ghost, which is the
            // only way the line completes anything; anywhere else it is a
            // cursor movement like any other (SPEC §24.3).
            KeyCode::Right if self.cursor >= self.query.len() => {
                let Some(Ghost::Rest(rest)) = self.ghost() else {
                    return Update::Unchanged;
                };
                if rest.is_empty() {
                    return Update::Unchanged;
                }
                self.query.push_str(&rest);
                self.cursor = self.query.len();
                Update::QueryChanged
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
            // `ctrl-n` / `ctrl-p` move a selection here exactly as they do in
            // `ted`'s other surfaces, and only those do. `up`/`down` stay
            // unbound rather than being reserved for a command history `ted`
            // does not keep (SPEC §24.2, §24.3).
            KeyCode::Char('n') if control => {
                self.cycle_completion(1);
                Update::Unchanged
            }
            KeyCode::Char('p') if control => {
                self.cycle_completion(-1);
                Update::Unchanged
            }
            // A number prompt takes numbers. Anything else typed at it is a key
            // that was meant for the editor behind it.
            KeyCode::Char(character)
                if !control && (self.mode == Mode::Command || character.is_ascii_digit()) =>
            {
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
    /// The host table is checked first (SPEC §13.4). Everything else goes to
    /// vim's interceptor, so `:w`, `:42` and `:%s/a/b/g` resolve to real actions
    /// with no parsing here. When it declines the query — or is non-exclusive —
    /// `ted` falls back to matching action names, which is the same policy Zed's
    /// own palette applies.
    pub async fn refresh(&mut self, workspace: WeakEntity<Workspace>, cx: &mut AsyncApp) {
        if self.mode == Mode::Number {
            return;
        }
        let host = host_command(&self.query);
        // `:!` answers its query alone: everything vim resolves for one is aimed
        // at a terminal panel `ted` does not have. `:Explore` shares its letters
        // with real action names, so it leads the list rather than replacing it.
        if matches!(host, Some(HostCommand::Shell(_))) {
            self.completions = host.into_iter().map(host_completion).collect();
            self.selected = Some(0);
            self.message = None;
            return;
        }

        let query = self.query.clone();
        let intercepted = cx
            .update(|cx| GlobalCommandPaletteInterceptor::intercept(&query, workspace.clone(), cx));

        let mut completions: Vec<Completion> = host.into_iter().map(host_completion).collect();
        let mut exclusive = false;
        if let Some(task) = intercepted {
            let result = task.await;
            exclusive = result.exclusive;
            completions.extend(result.results.into_iter().map(|item| Completion {
                label: item.string,
                effect: Effect::Dispatch(item.action),
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

    /// What `enter` should do: the selected completion's effect, or the first
    /// one's when the user never cycled.
    pub fn selected_effect(&self) -> Option<Effect> {
        let index = self.selected.unwrap_or(0);
        Some(self.completions.get(index)?.effect.clone())
    }
}

/// The host-command table (SPEC §13.4), checked before the interceptor.
///
/// These are the commands that exist *because* `ted` owns a tty, not a second
/// copy of vim's command set: they do not resolve to a `Box<dyn Action>` at all,
/// they suspend the process. Anything expressible as an action stays with the
/// interceptor — adding `:Explore` to `crates/vim/src/command.rs` would be
/// wrong, because GUI Zed has no tty to hand over.
///
/// `:!` is claimed only in its bare form. vim resolves that one to a
/// `SpawnInTerminal` aimed at a terminal panel `ted` does not have, so it is
/// dead here without a host that owns a tty; its other forms — `:%!sort`,
/// `:.,.+3!fmt`, `:r!date` — filter buffer text through the command and are
/// real editor edits, so they stay with the interceptor
/// (`crates/vim/src/command.rs`). A range has been seeded into the query as a
/// prefix by then, which is what makes the two distinguishable here.
fn host_command(query: &str) -> Option<HostCommand> {
    if let Some(command) = query.strip_prefix('!') {
        let command = command.trim();
        return (!command.is_empty()).then(|| HostCommand::Shell(command.to_owned()));
    }

    // Matched as a prefix, as vim matches its own commands: `:Ex` is `:Explore`.
    // Case-sensitively, so a `:e` meant for vim's `:edit` is left alone.
    let typed = query.trim_end();
    (!typed.is_empty() && "Explore".starts_with(typed)).then_some(HostCommand::Explore)
}

fn host_completion(command: HostCommand) -> Completion {
    let label = match &command {
        HostCommand::Shell(_) => "run in this terminal",
        HostCommand::Explore => "Explore — browse files",
    };
    Completion {
        label: label.to_owned(),
        effect: Effect::Host(command),
    }
}

/// Actions matching `query`, ranked by the same matcher and the same arguments
/// Zed's own palette uses, so `:w` ghosts whatever `:w` would have selected in
/// GUI Zed (SPEC §24.3).
///
/// Ordering *is* the feature here: the line shows one candidate at a time, so
/// which one the matcher puts first is the whole of what the user sees. Actions
/// the palette filter hides — SPEC §5.5's font-size actions, and the modals
/// SPEC §24.2 replaced — are left out, the same policy the palette applies.
fn matching_action_names(query: &str, cx: &mut App) -> Vec<Completion> {
    let normalized = command_palette::normalize_action_query(query);
    if normalized.is_empty() {
        return Vec::new();
    }

    let mut actions = Vec::new();
    let mut candidates = Vec::new();
    for name in cx.all_action_names().to_vec() {
        let Ok(action) = cx.build_action(name, None) else {
            continue;
        };
        if CommandPaletteFilter::try_global(cx)
            .is_some_and(|filter| filter.is_hidden(action.as_ref()))
        {
            continue;
        }
        candidates.push(StringMatchCandidate::new(
            actions.len(),
            command_palette::humanize_action_name(name),
        ));
        actions.push(action);
    }

    fuzzy_nucleo::match_strings(
        &candidates,
        &normalized,
        fuzzy_nucleo::Case::Smart,
        fuzzy_nucleo::LengthPenalty::On,
        MAX_COMPLETIONS,
    )
    .into_iter()
    .filter_map(|found| {
        Some(Completion {
            label: found.string.to_string(),
            effect: Effect::Dispatch(actions.get(found.candidate_id)?.boxed_clone()),
        })
    })
    .collect()
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
    fn only_the_bare_bang_is_a_host_command() {
        assert_eq!(
            host_command("!ls -l"),
            Some(HostCommand::Shell("ls -l".to_owned()))
        );
        assert_eq!(
            host_command("! git commit "),
            Some(HostCommand::Shell("git commit".to_owned()))
        );
        assert_eq!(host_command("!"), None);
        assert_eq!(host_command("w"), None);
        // The forms vim turns into buffer edits keep their meaning: a range has
        // already been seeded into the query when one is in play.
        assert_eq!(host_command("'<,'>!sort"), None);
        assert_eq!(host_command(".,.+3!fmt"), None);
        assert_eq!(host_command("%!sort"), None);
        assert_eq!(host_command("r!date"), None);
    }

    #[test]
    fn explore_is_matched_by_prefix_the_way_vim_matches_a_command() {
        for typed in ["E", "Ex", "Explo", "Explore", "Explore "] {
            assert_eq!(
                host_command(typed),
                Some(HostCommand::Explore),
                "{typed:?} should have reached the host table"
            );
        }
        // Lower case belongs to vim: `:e` is `:edit`, and `:explore` is not a
        // command `ted` claims.
        assert_eq!(host_command("e"), None);
        assert_eq!(host_command("explore"), None);
        assert_eq!(host_command("Explores"), None);
        assert_eq!(host_command(""), None);
    }

    /// Stands in for what `refresh` would have put there, so the ghost's own
    /// rules can be exercised without an interceptor or an `App`.
    fn with_candidates(query: &str, labels: &[&str]) -> CommandLine {
        let mut line = CommandLine::new(String::new());
        for character in query.chars() {
            line.handle_key(&key(KeyCode::Char(character)));
        }
        line.completions = labels
            .iter()
            .map(|label| Completion {
                label: (*label).to_owned(),
                effect: Effect::Host(HostCommand::Explore),
            })
            .collect();
        line.selected = (!labels.is_empty()).then_some(0);
        line
    }

    #[test]
    fn the_ghost_is_the_rest_of_a_command_the_query_starts() {
        let line = with_candidates("w", &[":wq"]);
        assert_eq!(line.view().ghost.as_deref(), Some("q"));
    }

    #[test]
    fn a_candidate_the_query_does_not_start_is_described_instead() {
        let line = with_candidates("save", &["workspace: save all"]);
        assert_eq!(line.view().ghost.as_deref(), Some(" — workspace: save all"));
    }

    #[test]
    fn right_accepts_a_ghost_that_completes_and_nothing_else() {
        let mut line = with_candidates("w", &[":wq"]);
        assert_eq!(line.handle_key(&key(KeyCode::Right)), Update::QueryChanged);
        assert_eq!(line.query(), "wq");

        let mut described = with_candidates("save", &["workspace: save all"]);
        assert_eq!(
            described.handle_key(&key(KeyCode::Right)),
            Update::Unchanged,
            "a description is not something to accept"
        );
        assert_eq!(described.query(), "save");
    }

    #[test]
    fn right_inside_the_query_still_moves_the_cursor() {
        let mut line = with_candidates("wq", &[":wq"]);
        line.handle_key(&key(KeyCode::Home));
        assert_eq!(line.handle_key(&key(KeyCode::Right)), Update::Unchanged);
        assert_eq!(line.cursor, 1);
        assert_eq!(line.query(), "wq");
    }

    #[test]
    fn only_ctrl_n_and_ctrl_p_cycle_the_candidates() {
        let mut line = with_candidates("w", &[":wq", ":w!", ":write"]);
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Tab] {
            line.handle_key(&key(code));
            assert_eq!(line.selected, Some(0), "{code:?} cycled the candidates");
        }

        let control = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        line.handle_key(&control);
        assert_eq!(line.selected, Some(1));
        assert_eq!(line.view().ghost.as_deref(), Some("!"));
    }

    #[test]
    fn the_line_says_how_many_other_candidates_there_are() {
        let line = with_candidates("w", &[":wq", ":w!", ":write"]);
        assert_eq!(line.view().trailing, vec!["ctrl-n: 2 more".to_owned()]);
        assert!(with_candidates("w", &[":wq"]).view().trailing.is_empty());
    }
}
