use collections::HashMap;
use schemars::{JsonSchema, json_schema};
use serde::Deserialize;
use std::borrow::Cow;
use util::schemars::{AllowTrailingCommas, DefaultDenyUnknownFields};

#[derive(Deserialize)]
pub struct VsSnippetsFile {
    #[serde(flatten)]
    pub(crate) snippets: HashMap<String, VsCodeSnippet>,
}

impl VsSnippetsFile {
    pub fn generate_json_schema() -> serde_json::Value {
        let schema = schemars::generate::SchemaSettings::draft2019_09()
            .with_transform(DefaultDenyUnknownFields)
            .with_transform(AllowTrailingCommas)
            .into_generator()
            .root_schema_for::<Self>();

        serde_json::to_value(schema).unwrap()
    }
}

impl JsonSchema for VsSnippetsFile {
    fn schema_name() -> Cow<'static, str> {
        "VsSnippetsFile".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let snippet_schema = generator.subschema_for::<VsCodeSnippet>();
        json_schema!({
            "type": "object",
            "additionalProperties": snippet_schema
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum ListOrDirect {
    Single(String),
    List(Vec<String>),
}

impl From<ListOrDirect> for Vec<String> {
    fn from(list: ListOrDirect) -> Self {
        match list {
            ListOrDirect::Single(entry) => vec![entry],
            ListOrDirect::List(entries) => entries,
        }
    }
}

impl std::fmt::Display for ListOrDirect {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Single(v) => v.to_owned(),
                Self::List(v) => v.join("\n"),
            }
        )
    }
}

#[derive(Deserialize, JsonSchema)]
pub(crate) struct VsCodeSnippet {
    /// The snippet prefix used to decide whether a completion menu should be shown.
    pub(crate) prefix: Option<ListOrDirect>,

    /// The snippet content. Use `$1` and `${1:defaultText}` to define cursor positions and `$0` for final cursor position.
    pub(crate) body: ListOrDirect,

    /// The snippet description displayed inside the completion menu.
    pub(crate) description: Option<ListOrDirect>,

    /// When true, the snippet expands as soon as its trigger matches, without
    /// requiring the user to open the completion menu and press Tab/Enter.
    pub(crate) auto: Option<bool>,

    /// A regular expression used as the snippet trigger instead of (or in
    /// addition to) `prefix`. The match must end at the cursor. Capture groups
    /// are exposed to the body as the `captures` array (`captures[0]` is the
    /// entire match, `captures[1]` the first group, etc.) and can be referenced
    /// from a Rhai body via `${captures[N]}` template interpolation.
    pub(crate) regex: Option<String>,

    /// Rhai expression evaluating to a bool that gates whether the snippet
    /// is allowed to fire at the cursor. Tree-sitter ancestor node kinds are
    /// injected as boolean variables (`true` if any ancestor has that kind,
    /// `false` otherwise), so predicates read naturally:
    ///
    /// ```conl
    /// active = """rhai
    ///   inline_formula || displayed_equation
    /// ```
    ///
    /// Omit to allow the snippet anywhere.
    pub(crate) active: Option<String>,
}
