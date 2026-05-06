mod extension_snippet;
pub mod format;
mod registry;
pub mod script;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context as _, Result};
use collections::{BTreeMap, BTreeSet, HashMap};
use format::VsSnippetsFile;
use fs::Fs;
use futures::stream::StreamExt;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, Task, WeakEntity};
use regex::Regex;
pub use registry::*;
use snippet::SnippetVariables;
use util::ResultExt;

pub fn init(cx: &mut App) {
    SnippetRegistry::init_global(cx);
    extension_snippet::init(cx);
}

/// Language name, or `None` if the snippet file is global.
type SnippetKind = Option<String>;
fn file_stem_to_key(stem: &str) -> SnippetKind {
    if stem == "snippets" {
        None
    } else {
        Some(stem.to_owned())
    }
}

#[derive(Clone, Copy)]
enum SnippetFileFormat {
    Json,
    Conl,
}

pub fn file_to_snippets(
    file_contents: VsSnippetsFile,
    source: &Path,
) -> impl Iterator<Item = Result<Arc<Snippet>>> {
    file_to_snippets_with_context(file_contents, HashMap::default(), HashMap::default(), source)
}

/// Like [`file_to_snippets`], but allows the caller to supply named regex
/// fragments that snippet `regex` patterns can splice in via the
/// `{{name}}` placeholder. Aliases are expanded once, at load time, so a
/// misspelled name fails the snippet rather than firing silently.
pub fn file_to_snippets_with_aliases(
    file_contents: VsSnippetsFile,
    aliases: HashMap<String, String>,
    source: &Path,
) -> impl Iterator<Item = Result<Arc<Snippet>>> {
    file_to_snippets_with_context(file_contents, aliases, HashMap::default(), source)
}

/// Like [`file_to_snippets_with_aliases`], but additionally accepts a
/// `defaults` map. Each entry is consulted when a snippet omits the matching
/// field. Currently the supported keys are `auto`, `active`, and
/// `description`; `prefix`/`regex`/`body` are intentionally excluded — those
/// must stay per-snippet because defaulting them would silently change which
/// snippet fires.
pub fn file_to_snippets_with_context(
    file_contents: VsSnippetsFile,
    aliases: HashMap<String, String>,
    defaults: HashMap<String, String>,
    source: &Path,
) -> impl Iterator<Item = Result<Arc<Snippet>>> {
    let aliases = Arc::new(aliases);
    let default_auto = defaults
        .get("auto")
        .map(|s| s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let default_active = defaults.get("active").cloned();
    let default_description = defaults.get("description").cloned();
    file_contents
        .snippets
        .into_iter()
        .map(move |(name, snippet)| {
            let description = snippet
                .description
                .map(|description| description.to_string())
                .or_else(|| default_description.clone());
            let body = snippet.body.to_string();
            let auto = snippet.auto.unwrap_or(default_auto);
            let regex = snippet
                .regex
                .map(|pattern| {
                    let expanded = expand_aliases(&pattern, &aliases)?;
                    Regex::new(&expanded)
                        .map(Arc::new)
                        .with_context(|| format!("invalid regex `{expanded}`"))
                })
                .transpose()
                .with_context(|| format!("Invalid snippet '{name}' in {source:?}"))?;
            // A regex-only snippet has no meaningful textual prefix: it fires
            // on regex match, and its body typically reads `captures[..]`,
            // which would panic the body evaluator if invoked from the
            // prefix-driven completion list with empty captures. So we leave
            // its prefix list empty rather than defaulting to the snippet name.
            let snippet_name = name.clone();
            let prefixes = match snippet.prefix {
                Some(prefixes) => prefixes.into(),
                None if regex.is_some() => Vec::new(),
                None => vec![snippet_name],
            };
            let active_src = snippet.active.as_deref().or(default_active.as_deref());
            let active = active_src
                .map(|src| {
                    script::ActivePredicate::compile(src)
                        .map(Arc::new)
                        .with_context(|| format!("invalid active predicate in '{name}'"))
                })
                .transpose()
                .with_context(|| format!("Invalid snippet '{name}' in {source:?}"))?;
            // Validate that the body compiles. For non-regex snippets, also
            // evaluate it (with empty captures) and parse the result, catching
            // bad LSP-snippet output at load time. Regex snippets are expanded
            // lazily because their bodies typically depend on capture values.
            let compiled = Arc::new(script::CompiledBody::compile(&body));
            if regex.is_none() {
                // Use a scope pre-populated with empty strings for all known
                // LSP variable names so Rhai bodies that reference them
                // (e.g. `TM_FILENAME + " ..."`) can validate at load time.
                let validation_vars = SnippetVariables::lsp_defaults_empty();
                let validation_body = compiled
                    .evaluate(&[], &validation_vars, &BTreeMap::new())
                    .with_context(|| format!("Invalid snippet '{name}' in {source:?}"))?;
                let substituted =
                    snippet::substitute_variables(&validation_body, &validation_vars);
                if let Err(e) = snippet::Snippet::parse(&substituted) {
                    return Err(anyhow::anyhow!(
                        "Invalid snippet '{name}' in {source:?}: {e:#}"
                    ));
                }
            }
            let compiled_body = OnceLock::new();
            let _ = compiled_body.set(compiled);
            Ok(Arc::new(Snippet {
                body,
                compiled_body,
                prefix: prefixes,
                description,
                name,
                auto,
                regex,
                active,
            }))
        })
}

/// Pulls a top-level `aliases` block out of a CONL snippet file. See
/// [`extract_conl_block`] for the parse rules. `serde_conl` does not support
/// `#[serde(flatten)]`, so we cannot model `aliases` as a peer-of-snippets
/// field on the deserialized struct; this string-level pre-pass is the
/// workaround.
pub(crate) fn extract_conl_aliases(source: &str) -> (String, HashMap<String, String>) {
    extract_conl_block(source, "aliases")
}

/// Pulls a top-level `defaults` block out of a CONL snippet file. Each entry
/// supplies a default value for the matching field on every snippet that
/// omits it. See [`extract_conl_block`] for the parse rules.
pub(crate) fn extract_conl_defaults(source: &str) -> (String, HashMap<String, String>) {
    extract_conl_block(source, "defaults")
}

/// Pulls a top-level CONL block named `header` out of `source`, returning the
/// remaining source with the block removed plus the parsed `name = value`
/// entries underneath it. The block is matched at the start of a line (no
/// leading whitespace) and terminated by the first non-indented, non-blank
/// line that follows.
fn extract_conl_block(source: &str, header: &str) -> (String, HashMap<String, String>) {
    let mut entries = HashMap::default();
    let mut output = String::with_capacity(source.len());
    let mut lines = source.lines().peekable();
    let mut found = false;
    while let Some(line) = lines.next() {
        if !found && line.trim_end() == header && !line.starts_with(char::is_whitespace) {
            found = true;
            while let Some(next) = lines.peek() {
                if next.trim().is_empty() {
                    lines.next();
                    continue;
                }
                if !next.starts_with(char::is_whitespace) {
                    break;
                }
                let entry = lines.next().unwrap().trim();
                if let Some((name, value)) = entry.split_once('=') {
                    entries.insert(name.trim().to_string(), value.trim().to_string());
                }
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    (output, entries)
}

/// Substitutes `{{name}}` placeholders in a regex pattern with the
/// corresponding alias body. Aliases may reference other aliases; expansion
/// repeats until the pattern is stable or hits a depth limit (cycle guard).
/// Errors out on an unknown name, unclosed placeholder, or apparent cycle
/// so misspellings and infinite loops don't silently produce a broken regex.
fn expand_aliases(pattern: &str, aliases: &HashMap<String, String>) -> Result<String> {
    const MAX_DEPTH: usize = 32;
    let mut current = pattern.to_string();
    for _ in 0..MAX_DEPTH {
        let (next, did_expand) = expand_aliases_once(&current, aliases, pattern)?;
        if !did_expand {
            return Ok(next);
        }
        current = next;
    }
    anyhow::bail!("alias expansion exceeded depth limit (cycle?) in regex `{pattern}`");
}

fn expand_aliases_once(
    input: &str,
    aliases: &HashMap<String, String>,
    original: &str,
) -> Result<(String, bool)> {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    let mut did_expand = false;
    while let Some(open) = rest.find("{{") {
        result.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        let close = after_open
            .find("}}")
            .with_context(|| format!("unclosed `{{{{` placeholder in regex `{original}`"))?;
        let name = &after_open[..close];
        let value = aliases
            .get(name)
            .with_context(|| format!("unknown alias `{name}` in regex `{original}`"))?;
        result.push_str(value);
        rest = &after_open[close + 2..];
        did_expand = true;
    }
    result.push_str(rest);
    Ok((result, did_expand))
}

// Snippet with all of the metadata
#[derive(Debug)]
pub struct Snippet {
    pub prefix: Vec<String>,
    /// Raw body source, as written by the user. Kept for inspection
    /// (e.g. completion-menu filter text). Use [`Snippet::evaluate`] to obtain
    /// the expanded body string with capture interpolation applied.
    pub body: String,
    /// Cached compiled form of `body`. Populated at load time by
    /// [`file_to_snippets_with_context`]; in tests it is left empty and
    /// initialized on first [`Snippet::evaluate`] call. Either way, repeated
    /// `evaluate` calls — including the editor's reactive re-evaluation on
    /// every keystroke inside an active tabstop — don't pay the Rhai compile
    /// cost more than once.
    pub compiled_body: OnceLock<Arc<script::CompiledBody>>,
    pub description: Option<String>,
    pub name: String,
    pub auto: bool,
    pub regex: Option<Arc<Regex>>,
    /// Compiled Rhai predicate that gates whether the snippet may fire.
    /// `None` means "always active". Tree-sitter ancestor node kinds are
    /// exposed as boolean variables to the predicate at evaluation time.
    pub active: Option<Arc<script::ActivePredicate>>,
}

impl Snippet {
    /// Evaluates the snippet body with the given regex captures and standard
    /// LSP variables bound, returning the expanded LSP-snippet-syntax string
    /// ready to be parsed by `snippet::Snippet::parse`.
    ///
    /// Captures are accessed in scripted bodies via the `captures` array
    /// (`captures[0]` is the full match). LSP standard variables are
    /// available both as Rhai-scope constants (e.g. `TM_FILENAME`) for
    /// scripted bodies and as `$NAME` / `${NAME}` / `${NAME:default}` /
    /// `${NAME/regex/replacement/flags}` substitutions, applied to the body's
    /// output via [`snippet::substitute_variables`]. If the body did not
    /// parse as Rhai (e.g. legacy text bodies), the source is returned
    /// unchanged before substitution.
    ///
    /// `current_tabstops` exposes the user's typed text inside each active
    /// tabstop to the body via the Rhai `tabstop(n)` accessor. Pass an empty
    /// map for the initial expansion; the editor passes a populated map on
    /// each reactive re-evaluation.
    pub fn evaluate(
        &self,
        captures: &[&str],
        variables: &SnippetVariables,
        current_tabstops: &BTreeMap<usize, String>,
    ) -> Result<String> {
        let compiled = self
            .compiled_body
            .get_or_init(|| Arc::new(script::CompiledBody::compile(&self.body)));
        let raw = compiled.evaluate(captures, variables, current_tabstops)?;
        Ok(snippet::substitute_variables(&raw, variables))
    }

    /// Returns `true` if the snippet's `active` predicate evaluates to true
    /// for the given set of ancestor tree-sitter node kinds, or if no
    /// predicate was specified. A predicate evaluation error is treated as
    /// "not active" and logged.
    pub fn is_active<S: AsRef<str>>(&self, ancestor_kinds: &BTreeSet<S>) -> bool {
        let Some(predicate) = self.active.as_ref() else {
            return true;
        };
        match predicate.evaluate(ancestor_kinds) {
            Ok(active) => active,
            Err(e) => {
                log::warn!("snippet '{}' active predicate failed: {e}", self.name);
                false
            }
        }
    }
}

async fn process_updates(
    this: WeakEntity<SnippetProvider>,
    entries: Vec<PathBuf>,
    mut cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&cx, |this, _| this.fs.clone())?;
    for entry_path in entries {
        let format = match entry_path.extension().and_then(|e| e.to_str()) {
            Some("json") => SnippetFileFormat::Json,
            Some("conl") => SnippetFileFormat::Conl,
            _ => continue,
        };
        let entry_metadata = fs.metadata(&entry_path).await;
        // Entry could have been removed, in which case we should no longer show completions for it.
        let entry_exists = entry_metadata.is_ok();
        if entry_metadata.is_ok_and(|entry| entry.is_some_and(|e| e.is_dir)) {
            // Don't process dirs.
            continue;
        }
        let Some(stem) = entry_path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let key = file_stem_to_key(stem);

        let contents = if entry_exists {
            fs.load(&entry_path).await.ok()
        } else {
            None
        };

        this.update(&mut cx, move |this, _| {
            let snippets_of_kind = this.snippets.entry(key).or_default();
            if entry_exists {
                let Some(file_contents) = contents else {
                    return;
                };
                let (parsed, aliases, defaults) = match format {
                    SnippetFileFormat::Json => (
                        serde_json_lenient::from_str::<VsSnippetsFile>(&file_contents).ok(),
                        HashMap::default(),
                        HashMap::default(),
                    ),
                    SnippetFileFormat::Conl => {
                        let (rest, defaults) = extract_conl_defaults(&file_contents);
                        let (rest, aliases) = extract_conl_aliases(&rest);
                        // `serde_conl` does not support `#[serde(flatten)]`,
                        // so we deserialize the bare `HashMap` shape and wrap
                        // it. Going through `VsSnippetsFile` directly silently
                        // drops every snippet in the file.
                        (
                            serde_conl::from_str::<HashMap<String, format::VsCodeSnippet>>(&rest)
                                .ok()
                                .map(|snippets| VsSnippetsFile { snippets }),
                            aliases,
                            defaults,
                        )
                    }
                };
                let Some(parsed) = parsed else {
                    return;
                };
                let snippets = file_to_snippets_with_context(
                    parsed,
                    aliases,
                    defaults,
                    entry_path.as_path(),
                );
                *snippets_of_kind.entry(entry_path).or_default() =
                    snippets.filter_map(Result::log_err).collect();
            } else {
                snippets_of_kind.remove(&entry_path);
            }
        })?;
    }
    Ok(())
}

async fn initial_scan(
    this: WeakEntity<SnippetProvider>,
    path: Arc<Path>,
    cx: AsyncApp,
) -> Result<()> {
    let fs = this.read_with(&cx, |this, _| this.fs.clone())?;
    let entries = fs.read_dir(&path).await;
    if let Ok(entries) = entries {
        let entries = entries
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        process_updates(this, entries, cx).await?;
    }
    Ok(())
}

pub struct SnippetProvider {
    fs: Arc<dyn Fs>,
    snippets: HashMap<SnippetKind, BTreeMap<PathBuf, Vec<Arc<Snippet>>>>,
    watch_tasks: Vec<Task<Result<()>>>,
}

// Watches global snippet directory, is created just once and reused across multiple projects
struct GlobalSnippetWatcher(Entity<SnippetProvider>);

impl GlobalSnippetWatcher {
    fn new(fs: Arc<dyn Fs>, cx: &mut App) -> Self {
        let global_snippets_dir = paths::snippets_dir();
        let provider = cx.new(|_cx| SnippetProvider {
            fs,
            snippets: Default::default(),
            watch_tasks: vec![],
        });
        provider.update(cx, |this, cx| this.watch_directory(global_snippets_dir, cx));
        Self(provider)
    }
}

impl gpui::Global for GlobalSnippetWatcher {}

impl SnippetProvider {
    pub fn new(fs: Arc<dyn Fs>, dirs_to_watch: BTreeSet<PathBuf>, cx: &mut App) -> Entity<Self> {
        cx.new(move |cx| {
            if !cx.has_global::<GlobalSnippetWatcher>() {
                let global_watcher = GlobalSnippetWatcher::new(fs.clone(), cx);
                cx.set_global(global_watcher);
            }
            let mut this = Self {
                fs,
                watch_tasks: Vec::new(),
                snippets: Default::default(),
            };

            for dir in dirs_to_watch {
                this.watch_directory(&dir, cx);
            }

            this
        })
    }

    /// Add directory to be watched for content changes
    fn watch_directory(&mut self, path: &Path, cx: &Context<Self>) {
        let path: Arc<Path> = Arc::from(path);

        self.watch_tasks.push(cx.spawn(async move |this, cx| {
            let fs = this.read_with(cx, |this, _| this.fs.clone())?;
            let watched_path = path.clone();
            let watcher = fs.watch(&watched_path, Duration::from_secs(1));
            initial_scan(this.clone(), path, cx.clone()).await?;

            let (mut entries, _) = watcher.await;
            while let Some(entries) = entries.next().await {
                process_updates(
                    this.clone(),
                    entries.into_iter().map(|event| event.path).collect(),
                    cx.clone(),
                )
                .await?;
            }
            Ok(())
        }));
    }

    fn lookup_snippets<'a, const LOOKUP_GLOBALS: bool>(
        &'a self,
        language: &'a SnippetKind,
        cx: &App,
    ) -> Vec<Arc<Snippet>> {
        let mut user_snippets: Vec<_> = self
            .snippets
            .get(language)
            .cloned()
            .unwrap_or_default()
            .into_values()
            .flat_map(|snippets| snippets.into_iter())
            .collect();
        if LOOKUP_GLOBALS {
            if let Some(global_watcher) = cx.try_global::<GlobalSnippetWatcher>() {
                user_snippets.extend(
                    global_watcher
                        .0
                        .read(cx)
                        .lookup_snippets::<false>(language, cx),
                );
            }

            let Some(registry) = SnippetRegistry::try_global(cx) else {
                return user_snippets;
            };

            let registry_snippets = registry.get_snippets(language);
            user_snippets.extend(registry_snippets);
        }

        user_snippets
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn add_snippet_for_test(
        &mut self,
        language: SnippetKind,
        path: PathBuf,
        snippet: Vec<Arc<Snippet>>,
    ) {
        self.snippets
            .entry(language)
            .or_default()
            .insert(path, snippet);
    }

    pub fn snippets_for(&self, language: SnippetKind, cx: &App) -> Vec<Arc<Snippet>> {
        let mut requested_snippets = self.lookup_snippets::<true>(&language, cx);

        if language.is_some() {
            // Look up global snippets as well.
            requested_snippets.extend(self.lookup_snippets::<true>(&None, cx));
        }
        requested_snippets
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::FakeFs;
    use gpui;
    use gpui::TestAppContext;
    use indoc::indoc;

    #[test]
    fn test_file_to_snippets_parses_auto_and_regex() {
        let json = indoc! {r#"
            {
              "Method call": {
                "regex": "(\\w+)\\.bar",
                "body": ["`${captures[1]}.foo(${captures[1]})$0`"],
                "auto": true
              },
              "Fraction": {
                "prefix": "ff",
                "body": ["\\frac{$1}{$2}$0"],
                "auto": true
              }
            }
        "#};
        let parsed = serde_json_lenient::from_str::<VsSnippetsFile>(json).unwrap();
        let snippets: Vec<_> = file_to_snippets(parsed, std::path::Path::new("test.json"))
            .filter_map(Result::ok)
            .collect();
        assert_eq!(snippets.len(), 2);
        let method_call = snippets
            .iter()
            .find(|s| s.name == "Method call")
            .expect("missing Method call");
        assert!(method_call.auto, "auto should be true");
        let regex = method_call.regex.as_ref().expect("regex should parse");
        assert!(regex.is_match("test.bar"), "regex should match `test.bar`");
        let captures = regex.captures("test.bar").unwrap();
        assert_eq!(captures.get(1).unwrap().as_str(), "test");
    }

    #[test]
    fn test_file_to_snippets_parses_active_predicate() {
        let json = indoc! {r#"
            {
                "Single in": {
                    "prefix": "x",
                    "body": "\"y\"",
                    "active": "string_literal"
                },
                "Disjunction": {
                    "prefix": "x",
                    "body": "\"y\"",
                    "active": "string_literal || raw_string_literal"
                },
                "Negation": {
                    "prefix": "x",
                    "body": "\"y\"",
                    "active": "!block"
                },
                "Combined": {
                    "prefix": "x",
                    "body": "\"y\"",
                    "active": "function_item && !string_literal"
                },
                "No predicate": {
                    "prefix": "x",
                    "body": "\"y\""
                }
            }
        "#};
        let parsed = serde_json_lenient::from_str::<VsSnippetsFile>(json).unwrap();
        let snippets: HashMap<String, Arc<Snippet>> =
            file_to_snippets(parsed, std::path::Path::new("test.json"))
                .filter_map(Result::ok)
                .map(|s| (s.name.clone(), s))
                .collect();

        assert!(snippets["No predicate"].active.is_none());
        assert!(
            snippets["No predicate"].is_active(&BTreeSet::<&str>::new()),
            "no predicate means always active"
        );

        let in_string: BTreeSet<&str> = ["string_literal"].into_iter().collect();
        let in_block: BTreeSet<&str> = ["block"].into_iter().collect();
        let in_function: BTreeSet<&str> = ["function_item"].into_iter().collect();
        let in_function_and_string: BTreeSet<&str> =
            ["function_item", "string_literal"].into_iter().collect();
        let empty: BTreeSet<&str> = BTreeSet::new();

        assert!(snippets["Single in"].is_active(&in_string));
        assert!(!snippets["Single in"].is_active(&empty));

        assert!(snippets["Disjunction"].is_active(&in_string));
        assert!(!snippets["Disjunction"].is_active(&in_block));

        assert!(snippets["Negation"].is_active(&empty));
        assert!(!snippets["Negation"].is_active(&in_block));

        assert!(snippets["Combined"].is_active(&in_function));
        assert!(!snippets["Combined"].is_active(&in_function_and_string));
        assert!(!snippets["Combined"].is_active(&in_string));
    }

    #[test]
    fn test_conl_format_loads_and_evaluates() {
        let conl = r#"alpha
  prefix = @a
  auto = true
  body = """rhai
    "\\alpha"

Hat over letter
  regex = ([a-zA-Z])hat
  auto = true
  body = """rhai
    `\hat{${captures[1]}}`

Matrix
  regex = pmat(\d+)x(\d+)
  auto = true
  body = """rhai
    let r = parse_int(captures[1]);
    let c = parse_int(captures[2]);
    let s = "\\begin{pmatrix}\n";
    for i in 0..r {
      for j in 0..c {
        s += next_tabstop();
        if j < c - 1 { s += " & "; }
      }
      if i < r - 1 { s += " \\\\"; }
      s += "\n";
    }
    s + "\\end{pmatrix}"
"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl)
            .unwrap_or_else(|e| panic!("conl parse: {e:?}"));
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: HashMap<String, Arc<Snippet>> =
            file_to_snippets(file, std::path::Path::new("test.conl"))
                .filter_map(Result::ok)
                .map(|s| (s.name.clone(), s))
                .collect();
        assert_eq!(snippets.len(), 3, "all three snippets should load");

        let alpha = snippets["alpha"].evaluate(&[], &SnippetVariables::default(), &BTreeMap::new()).unwrap();
        assert_eq!(alpha, r"\alpha");

        let hat = snippets["Hat over letter"]
            .evaluate(&["xhat", "x"], &SnippetVariables::default(), &BTreeMap::new())
            .unwrap();
        assert_eq!(hat, r"\hat{x}");

        let matrix = snippets["Matrix"]
            .evaluate(&["pmat2x3", "2", "3"], &SnippetVariables::default(), &BTreeMap::new())
            .unwrap();
        assert!(matrix.contains("$1 & $2 & $3"), "got: {matrix}");
        assert!(matrix.contains("$4 & $5 & $6"), "got: {matrix}");
        assert!(matrix.contains(r"\begin{pmatrix}"));
        assert!(matrix.contains(r"\end{pmatrix}"));
    }

    #[test]
    fn test_aliases_expand_in_regex() {
        let conl = r#"aliases
  greek = alpha|beta|gamma
  letter = [A-Za-z]

Letter subscript digit
  regex = (\\(?:{{greek}})|\b{{letter}})(\d)
  body = """rhai
    captures[1] + "_{" + captures[2] + "}"
"#;
        let (without_aliases, aliases) = extract_conl_aliases(conl);
        assert_eq!(aliases.get("greek").map(String::as_str), Some("alpha|beta|gamma"));
        assert_eq!(aliases.get("letter").map(String::as_str), Some("[A-Za-z]"));
        let parsed: HashMap<String, format::VsCodeSnippet> =
            serde_conl::from_str(&without_aliases).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: Vec<_> = file_to_snippets_with_aliases(
            file,
            aliases,
            std::path::Path::new("t.conl"),
        )
        .filter_map(Result::ok)
        .collect();
        assert_eq!(snippets.len(), 1);
        let regex = snippets[0].regex.as_ref().unwrap();
        assert!(regex.is_match(r"\alpha3"));
        assert!(regex.is_match("x3"));
        // No word boundary before `t` in `pmat3`, so the bare-letter branch
        // shouldn't match — confirms the alias splice preserved the `\b`.
        let m = regex.find("pmat3");
        assert!(m.is_none(), "got match: {m:?}");
    }

    #[test]
    fn test_nested_alias_references_expand() {
        let mut aliases = HashMap::default();
        aliases.insert("greek".into(), "alpha|beta".into());
        aliases.insert("either".into(), r"\\(?:{{greek}})|\b[A-Za-z]".into());
        let expanded = expand_aliases(r"({{either}})(\d)", &aliases).unwrap();
        assert_eq!(expanded, r"(\\(?:alpha|beta)|\b[A-Za-z])(\d)");
    }

    #[test]
    fn test_alias_cycle_is_rejected() {
        let mut aliases = HashMap::default();
        aliases.insert("a".into(), "x{{b}}".into());
        aliases.insert("b".into(), "y{{a}}".into());
        let err = expand_aliases("{{a}}", &aliases).unwrap_err();
        assert!(format!("{err:#}").contains("depth limit"), "got: {err:#}");
    }

    #[test]
    fn test_extract_conl_aliases_pulls_block() {
        let input = "aliases\n  greek = a|b\n  letter = [A-Za-z]\n\nfoo\n  body = x\n";
        let (out, aliases) = extract_conl_aliases(input);
        assert_eq!(aliases.get("greek").map(String::as_str), Some("a|b"));
        assert_eq!(aliases.get("letter").map(String::as_str), Some("[A-Za-z]"));
        assert!(!out.contains("aliases"), "block should be removed:\n{out}");
        assert!(out.contains("foo"));
    }

    #[test]
    fn test_conl_static_body_keeps_backslash() {
        // The static-body shortcut hinges on CONL preserving `\` literally
        // when a body value isn't wrapped in a Rhai block. If CONL ate the
        // backslash we'd silently emit `alpha` instead of `\alpha`.
        let conl = "alpha\n  prefix = @a\n  body = \\alpha\n";
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: Vec<_> = file_to_snippets(file, std::path::Path::new("t.conl"))
            .filter_map(Result::ok)
            .collect();
        assert_eq!(snippets.len(), 1);
        let body = snippets[0].evaluate(&[], &SnippetVariables::default(), &BTreeMap::new()).unwrap();
        assert_eq!(body, r"\alpha", "body should keep backslash");
    }

    #[test]
    fn test_conl_static_body_with_braces() {
        // Bodies like `^{2}` for the Square snippet contain `{` and `}`.
        // Confirm CONL passes them through and the snippet parser accepts
        // the result.
        let conl = "Square\n  prefix = sr\n  body = ^{2}\n";
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: Vec<_> = file_to_snippets(file, std::path::Path::new("t.conl"))
            .filter_map(Result::ok)
            .collect();
        assert_eq!(snippets.len(), 1);
        assert_eq!(
            snippets[0].evaluate(&[], &SnippetVariables::default(), &BTreeMap::new()).unwrap(),
            "^{2}"
        );
    }

    #[test]
    fn test_static_body_substitutes_lsp_variables() {
        // Static body with `$NAME` references resolves them via the variables
        // map before reaching the LSP snippet parser. Body starts with `%` so
        // it doesn't accidentally compile as Rhai (where `//` would be a
        // comment and swallow the rest).
        let conl = "Header\n  prefix = hd\n  body = % $TM_FILENAME ($CURRENT_YEAR)\n";
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: Vec<_> = file_to_snippets(file, std::path::Path::new("t.conl"))
            .filter_map(Result::ok)
            .collect();
        let mut vars = SnippetVariables::default();
        vars.insert("TM_FILENAME", "main.rs");
        vars.insert("CURRENT_YEAR", "2026");
        let body = snippets[0].evaluate(&[], &vars, &BTreeMap::new()).unwrap();
        assert_eq!(body, "% main.rs (2026)");
    }

    #[test]
    fn test_rhai_body_uses_variables_from_scope() {
        // Rhai body that references TM_FILENAME directly (no `$`), then
        // emits LSP snippet text. Confirms scope binding flows end-to-end.
        let conl = r#"Header
  prefix = hd
  body = """rhai
    "% " + TM_FILENAME

"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl)
            .unwrap_or_else(|e| panic!("conl parse: {e:?}"));
        let file = VsSnippetsFile { snippets: parsed };
        let snippets: Vec<_> = file_to_snippets(file, std::path::Path::new("t.conl"))
            .map(|r| r.unwrap_or_else(|e| panic!("snippet load: {e:#}")))
            .collect();
        assert_eq!(snippets.len(), 1, "expected one snippet");
        let mut vars = SnippetVariables::default();
        vars.insert("TM_FILENAME", "main.rs");
        let body = snippets[0].evaluate(&[], &vars, &BTreeMap::new()).unwrap();
        assert_eq!(body, "% main.rs");
    }

    #[test]
    fn test_load_latex_snippets_file() {
        // Smoke test against the user's file at the repo root. Skipped if
        // the file isn't present (e.g. in CI without it).
        let path = std::path::Path::new("../../latex_snippets.conl");
        if !path.exists() {
            return;
        }
        let contents = std::fs::read_to_string(path).unwrap();
        let (rest, defaults) = extract_conl_defaults(&contents);
        let (rest, aliases) = extract_conl_aliases(&rest);
        assert!(!aliases.is_empty(), "expected aliases block");
        assert!(!defaults.is_empty(), "expected defaults block");
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(&rest)
            .unwrap_or_else(|e| panic!("conl parse: {e:?}"));
        let file = VsSnippetsFile { snippets: parsed };
        let errors: Vec<_> =
            file_to_snippets_with_context(file, aliases, defaults, path)
                .filter_map(Result::err)
                .collect();
        assert!(
            errors.is_empty(),
            "loaded latex_snippets with errors:\n{}",
            errors
                .iter()
                .map(|e| format!("{e:#}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_extract_conl_defaults_pulls_block() {
        let input = "defaults\n  auto = true\n  active = math\n\nfoo\n  body = x\n";
        let (out, defaults) = extract_conl_defaults(input);
        assert_eq!(defaults.get("auto").map(String::as_str), Some("true"));
        assert_eq!(defaults.get("active").map(String::as_str), Some("math"));
        assert!(!out.contains("defaults"), "block should be removed:\n{out}");
        assert!(out.contains("foo"));
    }

    #[test]
    fn test_defaults_apply_when_field_omitted() {
        let conl = r#"Defaulted
  prefix = d
  body = """rhai
    "DD"
Overridden
  prefix = o
  auto = false
  body = """rhai
    "OO"
"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let mut defaults = HashMap::default();
        defaults.insert("auto".into(), "true".into());
        defaults.insert("active".into(), "string_literal".into());
        defaults.insert("description".into(), "from-default".into());
        let snippets: HashMap<String, Arc<Snippet>> = file_to_snippets_with_context(
            file,
            HashMap::default(),
            defaults,
            std::path::Path::new("t.conl"),
        )
        .filter_map(Result::ok)
        .map(|s| (s.name.clone(), s))
        .collect();
        let defaulted = &snippets["Defaulted"];
        assert!(defaulted.auto, "should inherit auto = true");
        assert!(defaulted.active.is_some(), "should inherit active");
        assert_eq!(defaulted.description.as_deref(), Some("from-default"));
        let overridden = &snippets["Overridden"];
        assert!(!overridden.auto, "explicit auto = false should win");
        assert!(
            overridden.active.is_some(),
            "active not specified, default still applies"
        );
    }

    #[test]
    fn test_defaults_dont_force_prefix_or_body() {
        // Defaults shouldn't carry `prefix`/`regex`/`body` keys — those must
        // be per-snippet. We verify by stuffing those keys into the defaults
        // map and confirming they have no effect.
        let conl = r#"Plain
  prefix = p
  body = """rhai
    "P"
"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let mut defaults = HashMap::default();
        defaults.insert("prefix".into(), "OOPS".into());
        defaults.insert("body".into(), "OOPS".into());
        defaults.insert("regex".into(), "OOPS".into());
        let snippets: Vec<_> = file_to_snippets_with_context(
            file,
            HashMap::default(),
            defaults,
            std::path::Path::new("t.conl"),
        )
        .filter_map(Result::ok)
        .collect();
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].prefix, vec!["p".to_string()]);
        assert!(snippets[0].regex.is_none());
        assert!(!snippets[0].body.contains("OOPS"));
    }

    #[test]
    fn test_conl_regex_snippet_with_auto_false_overrides_default() {
        // Mirrors the user's `Partial of x by y` shape: defaults set
        // `auto = true`, but the snippet overrides to `auto = false`. The
        // regex must survive the load, and the resulting snippet must keep
        // its empty prefix list (so the prefix-driven completion path is
        // skipped) while having `auto = false` and a callable regex.
        let conl = r#"Partial of x by y
  regex = pa([A-Za-z])([A-Za-z])
  auto = false
  body = """rhai
    "\\frac{ \\partial " + captures[1] + " }{ \\partial " + captures[2] + " } "
"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let mut defaults = HashMap::default();
        defaults.insert("auto".into(), "true".into());
        let snippets: Vec<_> = file_to_snippets_with_context(
            file,
            HashMap::default(),
            defaults,
            std::path::Path::new("t.conl"),
        )
        .filter_map(Result::ok)
        .collect();
        assert_eq!(snippets.len(), 1);
        assert!(!snippets[0].auto, "auto = false override should stick");
        assert!(snippets[0].prefix.is_empty(), "regex-only snippet has no prefix");
        let regex = snippets[0].regex.as_ref().expect("regex should parse");
        assert!(regex.is_match("paxy"));
        let captures = regex.captures("paxy").unwrap();
        let cap_strs: Vec<&str> = (0..captures.len())
            .map(|i| captures.get(i).map(|c| c.as_str()).unwrap_or(""))
            .collect();
        let body = snippets[0]
            .evaluate(&cap_strs, &SnippetVariables::default(), &BTreeMap::new())
            .unwrap();
        assert_eq!(body, r"\frac{ \partial x }{ \partial y } ");
    }

    #[test]
    fn test_unknown_alias_in_regex_is_rejected() {
        let conl = r#"Bad
  regex = (\\(?:{{nope}}))(\d)
  body = """rhai
    "x"
"#;
        let parsed: HashMap<String, format::VsCodeSnippet> = serde_conl::from_str(conl).unwrap();
        let file = VsSnippetsFile { snippets: parsed };
        let results: Vec<_> = file_to_snippets_with_aliases(
            file,
            HashMap::default(),
            std::path::Path::new("t.conl"),
        )
        .collect();
        assert_eq!(results.len(), 1);
        let err = results.into_iter().next().unwrap().unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown alias `nope`"),
            "got: {err:#}"
        );
    }

    #[test]
    fn test_file_to_snippets_invalid_regex_is_rejected() {
        let json = indoc! {r#"
            {
              "Bad": {
                "regex": "(unbalanced",
                "body": ["x"],
                "auto": true
              }
            }
        "#};
        let parsed = serde_json_lenient::from_str::<VsSnippetsFile>(json).unwrap();
        let results: Vec<_> = file_to_snippets(parsed, std::path::Path::new("test.json")).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[gpui::test]
    fn test_lookup_snippets_dup_registry_snippets(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.background_executor.clone());
        cx.update(|cx| {
            SnippetRegistry::init_global(cx);
            SnippetRegistry::global(cx)
                .register_snippets(
                    "ruby".as_ref(),
                    indoc! {r#"
                    {
                      "Log to console": {
                        "prefix": "log",
                        "body": ["console.info(\"Hello, ${1:World}!\")", "$0"],
                        "description": "Logs to console"
                      }
                    }
            "#},
                )
                .unwrap();
            let provider = SnippetProvider::new(fs.clone(), Default::default(), cx);
            cx.update_entity(&provider, |provider, cx| {
                assert_eq!(1, provider.snippets_for(Some("ruby".to_owned()), cx).len());
            });
        });
    }
}
