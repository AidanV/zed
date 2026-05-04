use anyhow::{Context as _, Result, anyhow};
use rhai::{AST, Dynamic, Engine, OptimizationLevel, Scope};
use snippet::{LSP_VARIABLE_NAMES, SnippetVariables};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::OnceLock;

thread_local! {
    /// Per-evaluation tabstop counter. Reset to 1 at the start of each
    /// `evaluate` call. The `tabstop()` Rhai function reads and increments it.
    static TABSTOP_COUNTER: Cell<usize> = const { Cell::new(1) };

    /// Set of ancestor tree-sitter node kinds active for the current
    /// `is_active` evaluation. `None` outside `is_active` so that body
    /// evaluation doesn't accidentally treat undefined identifiers as `false`.
    static ACTIVE_NODE_KINDS: RefCell<Option<BTreeSet<String>>> = const { RefCell::new(None) };
}

fn next_tabstop() -> String {
    let n = TABSTOP_COUNTER.with(|c| {
        let v = c.get();
        c.set(v + 1);
        v
    });
    format!("${n}")
}

/// Configures the shared Rhai engine used for snippet body evaluation.
///
/// The engine is sandbox-only: no I/O, no process, no filesystem bindings are
/// registered, and resource limits keep a misbehaving body from hanging the
/// editor. An `on_var` resolver is installed so that during `is_active`
/// evaluation, undefined identifiers are looked up as ancestor-node-kind
/// booleans (true if the cursor is inside such a node, false otherwise).
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut engine = Engine::new();
        engine.set_max_operations(10_000);
        engine.set_max_string_size(64_000);
        engine.set_max_call_levels(16);
        engine.set_max_array_size(1_024);
        engine.set_max_map_size(1_024);
        engine.set_optimization_level(OptimizationLevel::Simple);
        engine.register_fn("tabstop", next_tabstop);
        // `on_var` is marked deprecated only because Rhai considers it
        // "volatile". We rely on it intentionally for the active-predicate
        // lookup; if the API changes upstream this is the one place to update.
        #[allow(deprecated)]
        engine.on_var(|name, _index, _ctx| {
            ACTIVE_NODE_KINDS.with(|cell| match cell.borrow().as_ref() {
                Some(kinds) => Ok(Some(Dynamic::from(kinds.contains(name)))),
                None => Ok(None),
            })
        });
        engine
    })
}

/// Snippet body representation produced at load time.
///
/// `Static` is the fallback path for bodies that don't parse as Rhai
/// (e.g. legacy JSON snippets whose body is raw LSP-snippet text). `Dynamic`
/// is the canonical path: a compiled Rhai program whose final expression is
/// the expanded body string.
#[derive(Debug)]
pub enum CompiledBody {
    Static(String),
    Dynamic(Box<AST>),
}

impl CompiledBody {
    pub fn compile(source: &str) -> Self {
        match engine().compile(source) {
            Ok(ast) => Self::Dynamic(Box::new(ast)),
            Err(_err) => Self::Static(source.to_owned()),
        }
    }

    pub fn evaluate(&self, captures: &[&str], variables: &SnippetVariables) -> Result<String> {
        match self {
            Self::Static(body) => Ok(body.clone()),
            Self::Dynamic(ast) => {
                let mut scope = Scope::new();
                let captures_array: rhai::Array = captures
                    .iter()
                    .map(|c| Dynamic::from(c.to_string()))
                    .collect();
                scope.push_constant("captures", captures_array);
                scope.push_constant(
                    "prefix",
                    captures.first().map(|s| (*s).to_string()).unwrap_or_default(),
                );
                // Bind every known LSP variable name. Missing ones become
                // empty strings so Rhai bodies can branch on
                // `if LINE_COMMENT != ""` instead of erroring on an undefined
                // identifier when the editor didn't supply that variable
                // (e.g. a plain-text buffer with no comment syntax).
                for name in LSP_VARIABLE_NAMES {
                    let value = variables.get(name).unwrap_or("").to_string();
                    scope.push_constant(*name, value);
                }

                TABSTOP_COUNTER.with(|c| c.set(1));
                let result = engine()
                    .eval_ast_with_scope::<Dynamic>(&mut scope, ast)
                    .map_err(|e| anyhow!("snippet body evaluation failed: {e}"))?;

                if result.is_string() {
                    result.into_string().map_err(|e| anyhow!("{e}"))
                } else {
                    Ok(result.to_string())
                }
            }
        }
    }
}

/// Validates that `source` either compiles as Rhai or is treated as a static
/// string. The caller can also test that the resulting body parses as a valid
/// snippet by calling [`CompiledBody::evaluate`] with empty captures and
/// passing the result to `Snippet::parse`.
pub fn validate(source: &str) -> Result<CompiledBody> {
    let compiled = CompiledBody::compile(source);
    let expanded = compiled
        .evaluate(&[], &SnippetVariables::default())
        .with_context(|| "evaluating body with empty captures")?;
    snippet::Snippet::parse(&expanded).with_context(|| "parsing expanded body")?;
    Ok(compiled)
}

/// Compiled snippet `active` predicate. The expression is Rhai code that
/// returns a boolean. During evaluation, ancestor tree-sitter node kinds are
/// exposed as boolean variables (true if the cursor is inside such a node,
/// false otherwise), so users can write predicates like
/// `inline_formula || displayed_equation`.
#[derive(Debug)]
pub struct ActivePredicate {
    ast: Box<AST>,
}

impl ActivePredicate {
    /// Compiles a predicate expression. Returns an error if the source is not
    /// valid Rhai (we don't fall back to a static interpretation here because
    /// a string isn't a meaningful boolean).
    pub fn compile(source: &str) -> Result<Self> {
        let ast = engine()
            .compile(source)
            .map_err(|e| anyhow!("active predicate parse error: {e}"))?;
        Ok(Self { ast: Box::new(ast) })
    }

    /// Evaluates the predicate against a set of ancestor node kinds. Names
    /// referenced in the expression that are present in the set evaluate to
    /// `true`; others evaluate to `false`.
    pub fn evaluate<S: AsRef<str>>(&self, ancestor_kinds: &BTreeSet<S>) -> Result<bool> {
        let owned: BTreeSet<String> = ancestor_kinds
            .iter()
            .map(|k| k.as_ref().to_string())
            .collect();
        ACTIVE_NODE_KINDS.with(|cell| *cell.borrow_mut() = Some(owned));
        let result = engine()
            .eval_ast::<Dynamic>(&self.ast)
            .map_err(|e| anyhow!("active predicate evaluation failed: {e}"));
        ACTIVE_NODE_KINDS.with(|cell| *cell.borrow_mut() = None);

        let value = result?;
        if value.is_bool() {
            value.as_bool().map_err(|e| anyhow!("{e}"))
        } else {
            Err(anyhow!(
                "active predicate must return a bool, got {}",
                value.type_name()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_vars() -> SnippetVariables {
        SnippetVariables::default()
    }

    #[test]
    fn static_body_passes_through() {
        let body = CompiledBody::compile(r"\hat{$1}$0");
        let result = body.evaluate(&[], &no_vars()).unwrap();
        assert_eq!(result, r"\hat{$1}$0");
    }

    #[test]
    fn rhai_string_literal_evaluates() {
        let body = CompiledBody::compile(r#""\\alpha""#);
        let result = body.evaluate(&[], &no_vars()).unwrap();
        assert_eq!(result, r"\alpha");
    }

    #[test]
    fn rhai_map_lookup_with_fallback() {
        // Used by greek-shortcut snippet: look up captures[1] in an object
        // map, falling back to captures[0] when the key isn't present.
        let body = CompiledBody::compile(
            r#"
            let m = #{ a: "\\alpha", b: "\\beta" };
            let key = captures[1];
            if key in m { m[key] } else { captures[0] }
        "#,
        );
        assert_eq!(body.evaluate(&["@a", "a"], &no_vars()).unwrap(), r"\alpha");
        assert_eq!(body.evaluate(&["@b", "b"], &no_vars()).unwrap(), r"\beta");
        // Unknown letter: fall back to original text.
        assert_eq!(body.evaluate(&["@z", "z"], &no_vars()).unwrap(), "@z");
    }

    #[test]
    fn captures_are_bound() {
        let body = CompiledBody::compile(r#"`\hat{${captures[1]}}`"#);
        let result = body.evaluate(&["xhat", "x"], &no_vars()).unwrap();
        assert_eq!(result, r"\hat{x}");
    }

    #[test]
    fn variables_are_bound_to_rhai_scope() {
        // The point of pushing variables as scope constants: Rhai bodies can
        // reference them directly with their LSP names.
        let body = CompiledBody::compile(r#"TM_FILENAME + " :: " + CURRENT_YEAR"#);
        let mut vars = SnippetVariables::default();
        vars.insert("TM_FILENAME", "main.rs");
        vars.insert("CURRENT_YEAR", "2026");
        let result = body.evaluate(&[], &vars).unwrap();
        assert_eq!(result, "main.rs :: 2026");
    }

    #[test]
    fn missing_lsp_variable_is_empty_string_not_error() {
        // A plain-text buffer has no LINE_COMMENT, but a Rhai body must still
        // be able to reference it and branch on its value rather than failing
        // with "variable not defined". Empty string lets bodies write
        // `if LINE_COMMENT != "" { ... }` as the definedness check.
        let body = CompiledBody::compile(
            r#"if LINE_COMMENT != "" { LINE_COMMENT + " todo" } else { "no comment syntax" }"#,
        );
        let result = body.evaluate(&[], &no_vars()).unwrap();
        assert_eq!(result, "no comment syntax");

        let mut vars = SnippetVariables::default();
        vars.insert("LINE_COMMENT", "//");
        let result = body.evaluate(&[], &vars).unwrap();
        assert_eq!(result, "// todo");
    }

    #[test]
    fn variables_can_be_transformed_in_rhai_with_string_methods() {
        // Confirms that a Rhai body can do its own transformation on a
        // variable without needing the LSP `${VAR/.../.../}` syntax.
        let body = CompiledBody::compile(r#"TM_FILENAME.to_upper()"#);
        let mut vars = SnippetVariables::default();
        vars.insert("TM_FILENAME", "main.rs");
        let result = body.evaluate(&[], &vars).unwrap();
        assert_eq!(result, "MAIN.RS");
    }

    #[test]
    fn matrix_snippet_generates_tabstops() {
        let source = r#"
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
        let body = CompiledBody::compile(source);
        let result = body
            .evaluate(&["pmat2x3", "2", "3"], &no_vars())
            .unwrap_or_else(|e| panic!("evaluation failed: {e}"));
        assert!(
            result.contains("$1 & $2 & $3"),
            "expected first row in:\n{result}"
        );
        assert!(
            result.contains("$4 & $5 & $6"),
            "expected second row in:\n{result}"
        );
        assert!(result.contains("\\begin{pmatrix}"));
        assert!(result.contains("\\end{pmatrix}"));
    }

    #[test]
    fn infinite_loop_is_halted_by_op_limit() {
        let body = CompiledBody::compile(r#"loop { 1 + 1 }"#);
        let result = body.evaluate(&[], &no_vars());
        assert!(result.is_err(), "expected operation limit to halt loop");
    }

    #[test]
    fn no_filesystem_access() {
        // `open_file` and `read_file` are not registered.
        let body = CompiledBody::compile(r#"open_file("/etc/passwd")"#);
        let result = body.evaluate(&[], &no_vars());
        assert!(result.is_err(), "expected filesystem call to fail");
    }

    #[test]
    fn no_process_or_network_access() {
        // Each of these references a function that is not registered on the
        // sandboxed engine. Either the compile step fails (Static fallback ->
        // body returned as literal text, no execution) or the eval step fails.
        // Both outcomes are safe: in neither case is a process spawned or a
        // network request made.
        for snippet in [
            r#"system("rm -rf /")"#,
            r#"exec("curl evil.example.com")"#,
            r#"http_get("http://evil.example.com")"#,
            r#"spawn("sh")"#,
        ] {
            let compiled = CompiledBody::compile(snippet);
            match (&compiled, compiled.evaluate(&[], &no_vars())) {
                (CompiledBody::Static(_), Ok(out)) => {
                    assert_eq!(out, snippet, "static fallback should return source verbatim");
                }
                (CompiledBody::Dynamic(_), Err(_)) => {}
                other => panic!("`{snippet}` produced unexpected result: {other:?}"),
            }
        }
    }

    #[test]
    fn string_bomb_is_halted_by_size_limit() {
        // Concatenating a string with itself in a loop grows it exponentially.
        let body = CompiledBody::compile(
            r#"
            let s = "x";
            for i in 0..100 {
                s += s;
            }
            s
        "#,
        );
        let result = body.evaluate(&[], &no_vars());
        assert!(
            result.is_err(),
            "expected string-size limit to halt growth, got: {result:?}"
        );
    }

    #[test]
    fn active_predicate_disjunction() {
        let pred = ActivePredicate::compile("inline_formula || displayed_equation").unwrap();
        let mut kinds = BTreeSet::new();
        kinds.insert("displayed_equation".to_string());
        assert!(pred.evaluate(&kinds).unwrap());

        kinds.clear();
        kinds.insert("paragraph".to_string());
        assert!(!pred.evaluate(&kinds).unwrap());
    }

    #[test]
    fn active_predicate_negation_and_combos() {
        let pred = ActivePredicate::compile("!string_literal && function_item").unwrap();
        let mut kinds = BTreeSet::<String>::new();
        kinds.insert("function_item".to_string());
        assert!(pred.evaluate(&kinds).unwrap());

        kinds.insert("string_literal".to_string());
        assert!(!pred.evaluate(&kinds).unwrap());
    }

    #[test]
    fn active_kinds_are_not_visible_to_body_evaluation() {
        // Evaluate a predicate first to set ancestor kinds, then evaluate a
        // body that references the same identifier — the body should fail
        // because ACTIVE_NODE_KINDS is cleared after the predicate runs.
        let pred = ActivePredicate::compile("inline_formula").unwrap();
        let mut kinds = BTreeSet::new();
        kinds.insert("inline_formula".to_string());
        assert!(pred.evaluate(&kinds).unwrap());

        let body = CompiledBody::compile("inline_formula");
        let result = body.evaluate(&[], &no_vars());
        assert!(
            matches!(&body, CompiledBody::Static(_)) || result.is_err(),
            "body referencing a node kind should not silently get a bool: {result:?}"
        );
    }

    #[test]
    fn deep_recursion_is_halted_by_call_depth_limit() {
        // Self-recursion should hit the max_call_levels cap.
        let body = CompiledBody::compile(
            r#"
            fn rec(n) { rec(n + 1) }
            rec(0)
        "#,
        );
        let result = body.evaluate(&[], &no_vars());
        assert!(
            result.is_err(),
            "expected call-depth limit to halt recursion, got: {result:?}"
        );
    }
}
