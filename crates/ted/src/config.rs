//! `ted`'s own settings (SPEC §9).
//!
//! These are the settings that exist only because `ted` is a terminal host, and
//! they live in `ted.json` rather than in the `settings.json` it shares with GUI
//! Zed. The shared file is shared on purpose — language config, tab size,
//! formatters and LSP settings should mean the same thing in both — but a key
//! only one of the two can act on does not belong in it: it would appear in
//! Zed's settings schema, be offered for completion in a GUI that cannot honour
//! it, and make running `ted` once a permanent addition to a Zed user's
//! configuration. Switching between the two configures neither.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// Beside `settings.json` and `keymap.json` in the same directory, so
/// `--user-data-dir` moves all of them together.
pub fn path() -> PathBuf {
    paths::config_dir().join("ted.json")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The file manager `:Explore` hands the terminal to (SPEC §13.4), as a
    /// command and its arguments. `{chooser}` is replaced with the path of a
    /// file `ted` reads the selection back from, one path per line, and
    /// `{directory}` with the directory to start browsing in.
    pub file_manager: Vec<String>,
    /// Whether the terminal reports the mouse to `ted` (SPEC §17).
    ///
    /// Off by default, and that is not timidity: reporting takes the mouse away
    /// from the terminal itself, so selecting text with the mouse and the
    /// terminal's own copy stop working for as long as it is on. `ted:
    /// ToggleMouse` turns it on for as long as it is wanted.
    pub mouse: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            file_manager: ["yazi", "--chooser-file={chooser}", "{directory}"]
                .map(str::to_owned)
                .to_vec(),
            mouse: false,
        }
    }
}

/// The file as written, where every key is optional and an absent one means the
/// default rather than an empty value.
///
/// Unknown keys are rejected rather than ignored: this file has few enough keys
/// that a misspelt one is far more likely to be a typo than a setting from some
/// other version, and a typo that quietly does nothing is the failure this whole
/// module is trying to avoid.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    file_manager: Option<Vec<String>>,
    mouse: Option<bool>,
}

/// Reads `ted.json`, and says what went wrong rather than quietly falling back.
///
/// A missing file is the ordinary case and means the defaults. A malformed one
/// is not: the difference between "`ted` ran the file manager you asked for" and
/// "`ted` ran Yazi" is invisible until `:Explore` starts the wrong program, so
/// it is reported on the notification line (SPEC §13.3) instead.
pub fn load() -> (Config, Option<String>) {
    let path = path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Config::default(), None);
        }
        Err(error) => {
            return (
                Config::default(),
                Some(format!("could not read {}: {error}", path.display())),
            );
        }
    };

    match parse(&contents) {
        Ok(config) => (config, None),
        Err(error) => (
            Config::default(),
            Some(format!("{}: {error}", path.display())),
        ),
    }
}

fn parse(contents: &str) -> Result<Config> {
    // A file someone has created but not yet written anything into is not a
    // parse failure worth reporting.
    if contents.trim().is_empty() {
        return Ok(Config::default());
    }

    // Comments and trailing commas, as in every other JSON file Zed asks a user
    // to write by hand.
    let file: File = serde_json_lenient::from_str(contents).context("could not be parsed")?;

    let defaults = Config::default();
    Ok(Config {
        file_manager: file.file_manager.unwrap_or(defaults.file_manager),
        mouse: file.mouse.unwrap_or(defaults.mouse),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_key_keeps_its_default() {
        let parsed = parse("{}").expect("an empty object is a valid config");
        assert_eq!(parsed, Config::default());
        assert_eq!(parse("").expect("an empty file is not a failure"), parsed);
        assert_eq!(parse("  \n").expect("nor is a blank one"), parsed);
    }

    #[test]
    fn a_configured_file_manager_replaces_the_default() {
        let parsed = parse(r#"{ "file_manager": ["ranger", "--choosefiles={chooser}"] }"#)
            .expect("a file manager should parse");
        assert_eq!(parsed.file_manager, ["ranger", "--choosefiles={chooser}"]);
    }

    #[test]
    fn comments_and_trailing_commas_are_allowed() {
        let parsed = parse(
            r#"{
                // The file manager :Explore hands the terminal to.
                "file_manager": ["nnn", "-p", "{chooser}",],
            }"#,
        )
        .expect("ted.json is written by hand, like every other JSON file Zed reads");
        assert_eq!(parsed.file_manager, ["nnn", "-p", "{chooser}"]);
    }

    #[test]
    fn the_mouse_is_off_until_it_is_asked_for() {
        assert!(!Config::default().mouse);
        let parsed = parse(r#"{ "mouse": true }"#).expect("mouse should parse");
        assert!(parsed.mouse);
    }

    #[test]
    fn a_misspelt_key_is_an_error_rather_than_silence() {
        let error = parse(r#"{ "file-manager": ["yazi"] }"#)
            .expect_err("a key that does nothing should say so");
        assert!(
            error.to_string().contains("could not be parsed"),
            "{error:?}"
        );
    }
}
