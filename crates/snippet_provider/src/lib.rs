mod extension_snippet;
pub mod format;
mod registry;
pub mod script;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
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
    file_contents
        .snippets
        .into_iter()
        .map(move |(name, snippet)| {
            let snippet_name = name.clone();
            let prefixes = snippet
                .prefix
                .map_or_else(move || vec![snippet_name], |prefixes| prefixes.into());
            let description = snippet
                .description
                .map(|description| description.to_string());
            let body = snippet.body.to_string();
            let auto = snippet.auto.unwrap_or(false);
            let regex = snippet
                .regex
                .map(|pattern| {
                    Regex::new(&pattern)
                        .map(Arc::new)
                        .with_context(|| format!("invalid regex `{pattern}`"))
                })
                .transpose()
                .with_context(|| format!("Invalid snippet '{name}' in {source:?}"))?;
            let active = snippet
                .active
                .as_deref()
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
            if regex.is_none() {
                let validation_body = script::CompiledBody::compile(&body)
                    .evaluate(&[])
                    .with_context(|| format!("Invalid snippet '{name}' in {source:?}"))?;
                if let Err(e) = snippet::Snippet::parse(&validation_body) {
                    return Err(anyhow::anyhow!(
                        "Invalid snippet '{name}' in {source:?}: {e:#}"
                    ));
                }
            }
            Ok(Arc::new(Snippet {
                body,
                prefix: prefixes,
                description,
                name,
                auto,
                regex,
                active,
            }))
        })
}

// Snippet with all of the metadata
#[derive(Debug)]
pub struct Snippet {
    pub prefix: Vec<String>,
    /// Raw body source, as written by the user. Kept for inspection
    /// (e.g. completion-menu filter text). Use [`Snippet::evaluate`] to obtain
    /// the expanded body string with capture interpolation applied.
    pub body: String,
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
    /// Evaluates the snippet body with the given regex captures bound, returning
    /// the expanded LSP-snippet-syntax string ready to be parsed by
    /// `snippet::Snippet::parse`.
    ///
    /// Captures are accessed in scripted bodies via the `captures` array
    /// (`captures[0]` is the full match). If the body did not parse as Rhai
    /// (e.g. legacy text bodies), the source is returned unchanged.
    pub fn evaluate(&self, captures: &[&str]) -> Result<String> {
        script::CompiledBody::compile(&self.body).evaluate(captures)
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
                let parsed = match format {
                    SnippetFileFormat::Json => {
                        serde_json_lenient::from_str::<VsSnippetsFile>(&file_contents).ok()
                    }
                    SnippetFileFormat::Conl => serde_conl::from_str::<
                        HashMap<String, format::VsCodeSnippet>,
                    >(&file_contents)
                    .ok()
                    .map(|snippets| VsSnippetsFile { snippets }),
                };
                let Some(parsed) = parsed else {
                    return;
                };
                let snippets = file_to_snippets(parsed, entry_path.as_path());
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
        s += tabstop();
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

        let alpha = snippets["alpha"].evaluate(&[]).unwrap();
        assert_eq!(alpha, r"\alpha");

        let hat = snippets["Hat over letter"]
            .evaluate(&["xhat", "x"])
            .unwrap();
        assert_eq!(hat, r"\hat{x}");

        let matrix = snippets["Matrix"].evaluate(&["pmat2x3", "2", "3"]).unwrap();
        assert!(matrix.contains("$1 & $2 & $3"), "got: {matrix}");
        assert!(matrix.contains("$4 & $5 & $6"), "got: {matrix}");
        assert!(matrix.contains(r"\begin{pmatrix}"));
        assert!(matrix.contains(r"\end{pmatrix}"));
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
