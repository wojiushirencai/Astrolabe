//! Minimal tool catalog for clients that resend the whole tool list on every
//! request (claude-code). Keeps the five tools that answer most lookups in
//! one call, with terse descriptions and only the everyday parameters.

use serde_json::{Map, Value};

/// (tool, description, [(param, description)]) — params not listed are
/// dropped from the advertised schema (they still deserialize to defaults).
type SlimTool = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

pub(crate) const SLIM_TOOLS: &[SlimTool] = &[
    (
        "search_code",
        "Literal (or regex=true) search over the repo. Few hits: each comes inside its enclosing function/class body, so no follow-up Read. Many hits: counts plus top hits; narrow with path_filter. group=true: per-file, code vs tests, assert/load tags. offset pages.",
        &[
            ("query", "Text to find (literal unless regex=true)"),
            ("regex", "Treat query as regex"),
            ("path_filter", "Repo-relative path substring, e.g. src/api"),
            ("offset", "Page start (next_offset from a previous page)"),
            ("group", "Group by file: code first, tests last, tagged assert/load (who asserts or loads this literal)"),
            ("context_lines", "Fixed lines of context instead of the automatic body/lines"),
        ],
    ),
    (
        "find_symbol",
        "Find definitions by name. include_body=true returns the full bodies (use instead of Read).",
        &[
            ("query", "Symbol name or substring"),
            ("include_body", "Return source bodies"),
            ("path_filter", "Repo-relative path substring"),
            ("offset", "Page start"),
        ],
    ),
    (
        "explain_symbol",
        "One call for a named symbol: definition body plus every call site (path:line + code line, non-test first; tests collapsed). Prefer over find_symbol + search_code + Read.",
        &[
            ("name", "Exact symbol name"),
            ("path_filter", "Repo-relative path substring to pick a definition"),
            ("include_tests", "Also list call sites in tests"),
        ],
    ),
    (
        "find_references",
        "Precise references/callers via the language server. Use for edits and renames.",
        &[
            ("symbol", "Symbol name"),
            ("path", "Repo-relative file declaring or using it"),
        ],
    ),
    (
        "goto_definition",
        "Precise definition of the symbol at path:line:character (0-based).",
        &[
            ("path", "Repo-relative file"),
            ("line", "0-based line"),
            ("character", "0-based column"),
            ("symbol", "Name at that position (alternative to character)"),
        ],
    ),
];

/// Rewrite `tools` to the slim catalog, in `SLIM_TOOLS` order.
pub(crate) fn slim_catalog(tools: Vec<rmcp::model::Tool>) -> Vec<rmcp::model::Tool> {
    let mut out = Vec::new();
    for (name, description, params) in SLIM_TOOLS {
        let Some(mut tool) = tools.iter().find(|t| t.name == *name).cloned() else {
            continue;
        };
        tool.description = Some((*description).into());
        let mut schema = (*tool.input_schema).clone();
        if let Some(Value::Object(props)) = schema.get_mut("properties") {
            let mut kept = Map::new();
            for (param, text) in *params {
                if let Some(Value::Object(prop)) = props.get(*param) {
                    let mut prop = prop.clone();
                    prop.retain(|k, _| k == "type" || k == "items" || k == "enum");
                    prop.insert("description".into(), Value::String((*text).into()));
                    kept.insert((*param).into(), Value::Object(prop));
                }
            }
            *props = kept;
        }
        if let Some(Value::Array(required)) = schema.get_mut("required") {
            required.retain(|r| params.iter().any(|(p, _)| r == p));
        }
        schema.remove("$defs");
        schema.remove("definitions");
        schema.remove("title");
        schema.remove("$schema");
        tool.input_schema = std::sync::Arc::new(schema);
        tool.annotations = None;
        tool.output_schema = None;
        tool.title = None;
        out.push(tool);
    }
    out
}

/// L1 instructions for slim clients: the doctrine itself, no bootstrap call.
/// Ablation: `ASTROLABE_NO_EXPLAIN=1` hides explain_symbol.
pub(crate) fn explain_disabled() -> bool {
    matches!(
        std::env::var("ASTROLABE_NO_EXPLAIN").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub(crate) fn slim_instructions(root: &std::path::Path) -> String {
    let named = if explain_disabled() {
        "find_symbol include_body=true"
    } else {
        "explain_symbol (body + callers) or find_symbol include_body=true"
    };
    format!(
        "Astrolabe indexes this repo ({}). Prefer it over Grep/Read for code:\n\
         - a named function/class: {named}\n\
         - callers/usages for an edit: find_references\n\
         - literals (strings, errors, keys): search_code with path_filter; few hits arrive inside their enclosing body — don't Read again\n\
         - many hits: search_code returns per-file counts; narrow path_filter instead of paging\n\
         Bodies already sent this session are referenced by location, not resent.",
        root.display(),
        named = named
    )
}
