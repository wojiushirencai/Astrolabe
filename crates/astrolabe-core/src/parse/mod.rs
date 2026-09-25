//! Tree-sitter parsing and symbol extraction.
//!
//! Three hard requirements, each from a measured failure in the prior art:
//!
//! 1. **Release resources.** Drop every `Tree` and reuse `Parser` instances via
//!    a pool keyed by language. The WASM implementation allocated a parser per
//!    file and never freed the trees, which cost 5 GB of peak RSS on a
//!    1200-file repo — index a two-file repo and it still took 1.4 GB.
//! 2. **Never swallow a failure.** Every error returns an `Err` that the caller
//!    records in `IndexReport::parse_failures`. A `catch {}` here lost 205 of
//!    1026 files silently.
//! 3. **Real timeouts.** If a per-file budget is needed, use tree-sitter's own
//!    cancellation (`Parser::set_timeout_micros` / cancellation flag). Racing a
//!    synchronous parse against a timer does nothing — the parse holds the
//!    thread and the timer only fires afterwards, which is how the prior art
//!    produced 913 bogus "timeout" logs for files that had parsed fine.
//!
//! Symbol extraction uses the `.scm` queries in `queries/`, loaded per
//! language. Prefer the upstream grammar's `tags.scm` conventions
//! (`@definition.*` / `@reference.*`) over inventing capture names.

use std::ops::ControlFlow;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::types::{CodeSymbol, FileId, Language, RelPath, SymbolId, SymbolKind};
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

pub mod queries;
mod vue;

const PARSE_BUDGET: Duration = Duration::from_secs(5);
const MAX_SIGNATURE_CHARS: usize = 240;
const LANGUAGES: [Language; 10] = [
    Language::Python,
    Language::Go,
    Language::Java,
    Language::Rust,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Php,
    Language::C,
    Language::Cpp,
];

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("no grammar for {0:?}")]
    NoGrammar(Language),
    #[error("tree-sitter failed to produce a tree")]
    NoTree,
    #[error("parse exceeded its budget")]
    Timeout,
    #[error("query error: {0}")]
    Query(String),
}

#[derive(Debug, Default)]
pub struct ParsedFile {
    pub symbols: Vec<CodeSymbol>,
    /// Raw import specifiers, in source order. Resolution happens later, in
    /// `resolvers` — this layer must not guess at paths.
    pub imports: Vec<String>,
    /// `(callee_name, line)`; name-based, so downstream confidence is
    /// `Syntactic`.
    pub calls: Vec<(String, u32)>,
}

/// Parser pool. Holds one reusable `Parser` per language.
pub struct ParserPool {
    // A fixed mutex per language keeps Parser (which is not Sync) reusable and
    // makes the pool Sync. Same-language parses serialize; different languages
    // proceed concurrently. Unlike a checkout Vec, the pool cannot grow with
    // file count or accidentally retain a Tree.
    parsers: [Mutex<Option<Parser>>; LANGUAGES.len()],
}

impl ParserPool {
    pub fn new() -> Self {
        ParserPool {
            // `new` cannot report grammar ABI failures, so retain them as
            // `None`; `parse` turns that state into an explicit NoGrammar.
            parsers: std::array::from_fn(|index| {
                Mutex::new(configured_parser(LANGUAGES[index]).ok())
            }),
        }
    }

    pub fn parse(
        &self,
        lang: Language,
        path: &RelPath,
        source: &str,
    ) -> Result<ParsedFile, ParseError> {
        // Vue SFCs: embedded <script> extraction (no tree-sitter-vue on ABI 0.27).
        if lang == Language::Vue {
            return vue::parse_sfc(self, path, source);
        }
        let index = language_index(lang).ok_or(ParseError::NoGrammar(lang))?;
        let mut slot = self.parsers[index]
            .lock()
            .map_err(|_| ParseError::Query("parser pool mutex poisoned".into()))?;
        let parser = slot.as_mut().ok_or(ParseError::NoGrammar(lang))?;

        // Tree-sitter invokes this callback from inside the synchronous parse,
        // so expiration really interrupts parser work. There is no racing
        // timer thread that can fire after a successful parse.
        let deadline = Instant::now() + PARSE_BUDGET;
        let mut timed_out = false;
        let tree = {
            let mut progress = |_: &tree_sitter::ParseState| {
                if Instant::now() >= deadline {
                    timed_out = true;
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
            let bytes = source.as_bytes();
            parser.parse_with_options(&mut |offset, _| &bytes[offset..], None, Some(options))
        };

        let tree = match tree {
            Some(tree) => tree,
            None => {
                // A cancelled parse is resumable by default. Resetting ensures
                // the pooled parser starts the next file from a clean state.
                parser.reset();
                return Err(if timed_out {
                    ParseError::Timeout
                } else {
                    ParseError::NoTree
                });
            }
        };
        drop(slot);

        let query_sources = queries::for_language(lang)
            .ok_or_else(|| ParseError::Query(format!("no queries for {lang:?}")))?;
        let grammar = grammar(lang).ok_or(ParseError::NoGrammar(lang))?;
        let symbols_query = Query::new(&grammar, query_sources.symbols).map_err(query_error)?;
        let imports_query = Query::new(&grammar, query_sources.imports).map_err(query_error)?;
        let calls_query = Query::new(&grammar, query_sources.calls).map_err(query_error)?;

        let symbols = extract_symbols(lang, &symbols_query, tree.root_node(), source);
        let imports = extract_imports(&imports_query, tree.root_node(), source);
        let calls = extract_calls(&calls_query, tree.root_node(), source);

        // `tree`, all Nodes, queries, and cursors are local and drop here.
        // ParsedFile contains only owned strings and scalar line numbers.
        Ok(ParsedFile {
            symbols,
            imports,
            calls,
        })
    }
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

fn language_index(lang: Language) -> Option<usize> {
    Some(match lang {
        Language::Python => 0,
        Language::Go => 1,
        Language::Java => 2,
        Language::Rust => 3,
        Language::TypeScript => 4,
        Language::Tsx => 5,
        Language::JavaScript => 6,
        Language::Php => 7,
        Language::C => 8,
        Language::Cpp => 9,
        Language::ObjC
        | Language::ObjCpp
        | Language::Swift
        | Language::Vue => return None,
    })
}

fn grammar(lang: Language) -> Option<tree_sitter::Language> {
    Some(match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::ObjC
        | Language::ObjCpp
        | Language::Swift
        | Language::Vue => return None,
    })
}

fn configured_parser(lang: Language) -> Result<Parser, ParseError> {
    let language = grammar(lang).ok_or(ParseError::NoGrammar(lang))?;
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|_| ParseError::NoGrammar(lang))?;
    Ok(parser)
}

fn query_error(error: tree_sitter::QueryError) -> ParseError {
    ParseError::Query(error.to_string())
}

fn extract_symbols(
    lang: Language,
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
) -> Vec<CodeSymbol> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, root, source.as_bytes());
    let mut symbols = Vec::new();

    while let Some(query_match) = matches.next() {
        let definition = query_match.captures().iter().find_map(|capture| {
            let capture_name = capture_names[capture.index as usize];
            kind_for_definition_capture(capture_name).map(|kind| (capture.node, kind))
        });
        let Some((definition_node, kind)) = definition else {
            continue;
        };
        let name_node = query_match
            .captures()
            .iter()
            .find(|capture| capture_names[capture.index as usize] == queries::NAME)
            .map(|capture| capture.node)
            .unwrap_or(definition_node);
        let Ok(name) = name_node.utf8_text(source.as_bytes()) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }

        symbols.push(CodeSymbol {
            // Stable IDs are assigned by the graph builder after all files
            // have been parsed; this layer has no FileId allocation context.
            id: SymbolId(0),
            file: FileId(0),
            name: name.to_owned(),
            kind,
            signature: signature(definition_node, source),
            start_line: line_number(definition_node.start_position().row),
            end_line: line_number(definition_node.end_position().row),
            exported: is_exported(lang, name, definition_node, source),
        });
    }
    symbols
}

fn extract_imports(query: &Query, root: tree_sitter::Node<'_>, source: &str) -> Vec<String> {
    extract_named_captures(query, root, source, |name| {
        name == "import" || name == "source" || name == "module"
    })
    .into_iter()
    .map(|(text, _)| unquote(text.trim()).to_owned())
    .collect()
}

fn extract_calls(query: &Query, root: tree_sitter::Node<'_>, source: &str) -> Vec<(String, u32)> {
    extract_named_captures(query, root, source, |name| {
        name == "call" || name == "name" || name == "reference.call"
    })
}

fn extract_named_captures(
    query: &Query,
    root: tree_sitter::Node<'_>,
    source: &str,
    wanted: impl Fn(&str) -> bool,
) -> Vec<(String, u32)> {
    let capture_names = query.capture_names();
    let mut cursor = QueryCursor::new();
    let mut captures = cursor.captures(query, root, source.as_bytes());
    let mut output = Vec::new();
    while let Some((query_match, capture_index)) = captures.next() {
        let capture = query_match.captures()[*capture_index];
        if wanted(capture_names[capture.index as usize]) {
            if let Ok(text) = capture.node.utf8_text(source.as_bytes()) {
                output.push((
                    text.to_owned(),
                    line_number(capture.node.start_position().row),
                ));
            }
        }
    }
    output
}

/// Map a `@definition.<kind>` capture to [`SymbolKind`].
///
/// Prefer [`queries::symbol_kind_for_capture`] so this layer stays aligned with
/// the query pack. Field / Variable (and any other kind the pack has not yet
/// wired) are filled in locally so a concurrent query update cannot drop the
/// new categories on the floor.
fn kind_for_definition_capture(capture: &str) -> Option<SymbolKind> {
    if let Some(kind) = queries::symbol_kind_for_capture(capture) {
        return Some(kind);
    }
    match capture.strip_prefix(queries::DEFINITION_PREFIX)? {
        "field" | "property" | "enum_variant" => Some(SymbolKind::Field),
        "variable" | "var" => Some(SymbolKind::Variable),
        "macro" => Some(SymbolKind::Function),
        _ => None,
    }
}

fn is_exported(lang: Language, name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    match lang {
        Language::Go => name.chars().next().is_some_and(char::is_uppercase),
        Language::Python => python_exported(name),
        Language::Java => java_exported(node),
        Language::Rust => rust_exported(node, source),
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            js_exported(name, node, source)
        }
        Language::Php => php_exported(node, source),
        // Sibling P0 languages: treat as exported until their agents land visibility rules.
        Language::C
        | Language::Cpp
        | Language::ObjC
        | Language::ObjCpp
        | Language::Swift
        | Language::Vue => true,
    }
}

fn php_exported(node: tree_sitter::Node<'_>, source: &str) -> bool {
    // Walk up to a declaration that may carry a visibility modifier.
    let mut cur = Some(node);
    while let Some(n) = cur {
        match n.kind() {
            "method_declaration"
            | "property_declaration"
            | "const_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition" => {
                if let Ok(text) = n.utf8_text(source.as_bytes()) {
                    // First keyword tokens: private is never exported; protected
                    // stays internal to the inheritance hierarchy.
                    let head = text.split('{').next().unwrap_or(text);
                    if head.split_whitespace().any(|t| t == "private" || t == "protected") {
                        return false;
                    }
                }
                return true;
            }
            _ => cur = n.parent(),
        }
    }
    true
}

fn python_exported(name: &str) -> bool {
    let dunder = name.len() >= 4 && name.starts_with("__") && name.ends_with("__");
    dunder || !name.starts_with('_')
}

fn java_exported(node: tree_sitter::Node<'_>) -> bool {
    let item = java_defining_item(node);
    if let Some(exported) = java_modifiers_exported(item) {
        return exported;
    }
    if item.kind() == "enum_constant" || has_ancestor_kind(item, "enum_constant") {
        return true;
    }
    java_inside_interface(item)
}

fn java_defining_item(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "record_declaration",
        "annotation_type_declaration",
        "method_declaration",
        "constructor_declaration",
        "compact_constructor_declaration",
        "field_declaration",
        "constant_declaration",
        "enum_constant",
        "variable_declarator",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            if node.kind() == "variable_declarator" {
                if let Some(parent) = node.parent() {
                    if matches!(
                        parent.kind(),
                        "field_declaration" | "constant_declaration" | "local_variable_declaration"
                    ) {
                        return parent;
                    }
                }
            }
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn java_modifiers_exported(node: tree_sitter::Node<'_>) -> Option<bool> {
    for i in 0..node.child_count() {
        let child = node.child(i)?;
        if child.kind() != "modifiers" {
            continue;
        }
        let mut saw_public = false;
        for j in 0..child.child_count() {
            match child.child(j).map(|c| c.kind()) {
                Some("private") | Some("protected") => return Some(false),
                Some("public") => saw_public = true,
                _ => {}
            }
        }
        return Some(saw_public);
    }
    None
}

fn java_inside_interface(node: tree_sitter::Node<'_>) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "interface_declaration" | "annotation_type_declaration" => return true,
            "class_declaration" | "enum_declaration" | "record_declaration" => return false,
            _ => current = parent,
        }
    }
    false
}

fn rust_exported(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let item = rust_defining_item(node);
    match item.kind() {
        "field_declaration" => rust_has_pub(item, source),
        "enum_variant" => {
            rust_enclosing_kind(item, "enum_item")
                .map(|enum_item| rust_has_pub(enum_item, source))
                .unwrap_or(false)
                || rust_has_pub(item, source)
        }
        "function_item" | "function_signature_item" => {
            rust_has_pub(item, source)
                || rust_enclosing_kind(item, "trait_item")
                    .is_some_and(|trait_item| rust_has_pub(trait_item, source))
        }
        _ => rust_has_pub(item, source),
    }
}

fn rust_defining_item(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "function_item",
        "function_signature_item",
        "struct_item",
        "enum_item",
        "union_item",
        "trait_item",
        "impl_item",
        "type_item",
        "const_item",
        "static_item",
        "mod_item",
        "macro_definition",
        "field_declaration",
        "enum_variant",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn rust_has_pub(node: tree_sitter::Node<'_>, source: &str) -> bool {
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if child.kind() == "visibility_modifier" {
            return child
                .utf8_text(source.as_bytes())
                .map(|text| text.starts_with("pub"))
                .unwrap_or(false);
        }
    }
    false
}

fn rust_enclosing_kind<'a>(
    mut node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return Some(parent);
        }
        node = parent;
    }
    None
}

fn js_exported(name: &str, node: tree_sitter::Node<'_>, source: &str) -> bool {
    if name.starts_with('#') {
        return false;
    }
    let member = js_member_node(node);
    if js_private_member(member, source) {
        return false;
    }
    if has_ancestor_kind(node, "export_statement") {
        return true;
    }
    js_commonjs_export(node, source)
}

fn js_member_node(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    const KINDS: &[&str] = &[
        "method_definition",
        "method_signature",
        "abstract_method_signature",
        "public_field_definition",
        "field_definition",
        "property_signature",
        "function_declaration",
        "function_signature",
        "generator_function_declaration",
        "class_declaration",
        "abstract_class_declaration",
        "interface_declaration",
        "type_alias_declaration",
        "enum_declaration",
        "internal_module",
        "module",
        "lexical_declaration",
        "variable_declaration",
        "variable_declarator",
        "assignment_expression",
        "export_statement",
    ];
    loop {
        if KINDS.contains(&node.kind()) {
            return node;
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => return node,
        }
    }
}

fn js_private_member(node: tree_sitter::Node<'_>, source: &str) -> bool {
    if node.kind() == "private_property_identifier" {
        return true;
    }
    for i in 0..node.child_count() {
        let Some(child) = node.child(i) else {
            continue;
        };
        if child.kind() == "private_property_identifier" {
            return true;
        }
        if child.kind() == "accessibility_modifier" {
            let hidden = child
                .child(0)
                .is_some_and(|c| matches!(c.kind(), "private" | "protected"))
                || child
                    .utf8_text(source.as_bytes())
                    .map(|text| text == "private" || text == "protected")
                    .unwrap_or(false);
            if hidden {
                return true;
            }
        }
    }
    false
}

fn js_commonjs_export(node: tree_sitter::Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "assignment_expression" {
            if let Some(left) = n.child_by_field_name("left") {
                if let Ok(text) = left.utf8_text(source.as_bytes()) {
                    let text = text.trim();
                    if text.starts_with("module.exports") || text.starts_with("exports.") {
                        return true;
                    }
                }
            }
        }
        current = n.parent();
    }
    false
}

fn has_ancestor_kind(mut node: tree_sitter::Node<'_>, kind: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == kind {
            return true;
        }
        node = parent;
    }
    false
}

fn line_number(zero_based_row: usize) -> u32 {
    u32::try_from(zero_based_row)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

fn signature(node: tree_sitter::Node<'_>, source: &str) -> String {
    let text = node.utf8_text(source.as_bytes()).unwrap_or_default();
    let first_line = text.split(['\n', '\r']).next().unwrap_or_default();
    let before_body = first_line.split('{').next().unwrap_or(first_line).trim();
    let mut result: String = before_body.chars().take(MAX_SIGNATURE_CHARS).collect();
    if before_body.chars().count() > MAX_SIGNATURE_CHARS {
        result.push('…');
    }
    result
}

fn unquote(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        // Quotes for ordinary string literals; angle brackets for C/C++
        // `#include <…>` (`system_lib_string` nodes include the brackets).
        if matches!(
            (bytes[0], bytes[value.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"') | (b'`', b'`') | (b'<', b'>')
        ) {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn parse_src(lang: Language, path: &str, src: &str) -> ParsedFile {
        ParserPool::new()
            .parse(lang, &RelPath::new(path), src)
            .unwrap_or_else(|e| panic!("parse {path}: {e}"))
    }

    fn named<'a>(parsed: &'a ParsedFile, name: &str) -> &'a CodeSymbol {
        parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "missing symbol {name} in {:?}",
                    parsed
                        .symbols
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                )
            })
    }

    #[test]
    fn all_languages_have_compatible_parsers() {
        let pool = ParserPool::new();
        for language in LANGUAGES {
            assert!(
                pool.parsers[language_index(language).expect("wired language")]
                    .lock()
                    .unwrap()
                    .is_some(),
                "failed to configure the {} parser",
                language.name()
            );
        }
    }

    #[test]
    fn parser_pool_size_does_not_grow_with_file_count() {
        let pool = ParserPool::new();
        for _ in 0..100 {
            let mut parser = pool.parsers[language_index(Language::Rust).expect("rust wired")].lock().unwrap();
            assert!(parser.as_mut().unwrap().parse("fn f() {}", None).is_some());
        }
        assert_eq!(pool.parsers.len(), LANGUAGES.len());
        assert_eq!(
            pool.parsers
                .iter()
                .filter(|parser| parser.lock().unwrap().is_some())
                .count(),
            LANGUAGES.len()
        );
    }

    #[test]
    fn malformed_source_and_empty_source_are_recoverable() {
        let pool = ParserPool::new();
        let mut parser = pool.parsers[language_index(Language::Rust).expect("rust wired")].lock().unwrap();
        let parser = parser.as_mut().unwrap();
        let malformed = parser.parse("fn broken( {", None).unwrap();
        assert!(malformed.root_node().has_error());
        assert!(parser.parse("", None).is_some());
    }

    #[test]
    fn parse_recovers_from_syntax_errors_and_accepts_empty_source() {
        let pool = ParserPool::new();
        let malformed = pool
            .parse(Language::Rust, &RelPath::new("broken.rs"), "fn broken( {")
            .unwrap();
        assert!(malformed.symbols.len() <= 1);
        assert!(pool
            .parse(Language::Rust, &RelPath::new("empty.rs"), "")
            .is_ok());
    }

    #[test]
    fn symbol_lines_are_one_based() {
        let parsed = parse_src(Language::Rust, "line.rs", "\nfn second_line() {}\n");
        let symbol = named(&parsed, "second_line");
        assert_eq!(symbol.start_line, 2);
        assert_eq!(symbol.end_line, 2);
    }

    #[test]
    fn new_and_existing_definition_captures_map_to_kinds() {
        assert_eq!(
            kind_for_definition_capture("definition.function"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            kind_for_definition_capture("definition.macro"),
            Some(SymbolKind::Function)
        );
        assert_eq!(
            kind_for_definition_capture("definition.field"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.property"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.enum_variant"),
            Some(SymbolKind::Field)
        );
        assert_eq!(
            kind_for_definition_capture("definition.variable"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(
            kind_for_definition_capture("definition.var"),
            Some(SymbolKind::Variable)
        );
        assert_eq!(kind_for_definition_capture("name"), None);
        assert_eq!(kind_for_definition_capture("_scope"), None);
        assert_eq!(kind_for_definition_capture("definition.bogus"), None);
    }

    #[test]
    fn rust_macros_are_kept_as_functions() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "macro_rules! my_macro { () => {}; }\n",
        );
        let symbol = named(&parsed, "my_macro");
        assert_eq!(symbol.kind, SymbolKind::Function);
        assert!(!symbol.exported);
    }

    #[test]
    fn python_export_follows_leading_underscore_convention() {
        let parsed = parse_src(
            Language::Python,
            "m.py",
            "def public_fn():\n    pass\ndef _private():\n    pass\n__all__ = []\n",
        );
        assert!(named(&parsed, "public_fn").exported);
        assert!(!named(&parsed, "_private").exported);
        assert!(named(&parsed, "__all__").exported);
    }

    #[test]
    fn go_export_follows_capitalization() {
        let parsed = parse_src(
            Language::Go,
            "m.go",
            "package p\nfunc New() {}\nfunc helper() {}\n",
        );
        assert!(named(&parsed, "New").exported);
        assert!(!named(&parsed, "helper").exported);
    }

    #[test]
    fn java_export_honours_public_versus_private() {
        let parsed = parse_src(
            Language::Java,
            "Box.java",
            "public class Box {\n  public void open() {}\n  private void hide() {}\n  void pkg() {}\n}\n",
        );
        assert!(named(&parsed, "Box").exported);
        assert!(named(&parsed, "open").exported);
        assert!(!named(&parsed, "hide").exported);
        assert!(!named(&parsed, "pkg").exported);
    }

    #[test]
    fn rust_export_honours_pub_and_does_not_export_private_items() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub fn free() {}\nfn hidden() {}\npub struct Point { x: i32 }\nstruct Secret { pub y: i32 }\n",
        );
        assert!(named(&parsed, "free").exported);
        assert!(!named(&parsed, "hidden").exported);
        assert!(named(&parsed, "Point").exported);
        assert!(!named(&parsed, "Secret").exported);
    }

    #[test]
    fn typescript_export_and_private_members() {
        let parsed = parse_src(
            Language::TypeScript,
            "m.ts",
            "export function greet() {}\nfunction hidden() {}\nexport class App {\n  render() {}\n  #secret() {}\n  private hide() {}\n}\n",
        );
        assert!(named(&parsed, "greet").exported);
        assert!(!named(&parsed, "hidden").exported);
        assert!(named(&parsed, "App").exported);
        assert!(named(&parsed, "render").exported);
        assert!(!named(&parsed, "#secret").exported);
        assert!(!named(&parsed, "hide").exported);
    }

    #[test]
    fn javascript_commonjs_assignment_counts_as_exported() {
        let parsed = parse_src(
            Language::JavaScript,
            "m.js",
            "module.exports.legacy = function () {};\nfunction hidden() {}\n",
        );
        assert!(named(&parsed, "legacy").exported);
        assert!(!named(&parsed, "hidden").exported);
    }

    #[test]
    fn java_fields_carry_kind_and_visibility() {
        let parsed = parse_src(
            Language::Java,
            "Box.java",
            "public class Box {\n  public int open;\n  private int secret;\n}\n",
        );
        let open = parsed
            .symbols
            .iter()
            .find(|s| s.name == "open")
            .expect("missing Field `open` — queries 尚未发出 definition.field");
        assert_eq!(open.kind, SymbolKind::Field);
        assert!(open.exported);
        let secret = parsed
            .symbols
            .iter()
            .find(|s| s.name == "secret")
            .expect("missing Field `secret`");
        assert_eq!(secret.kind, SymbolKind::Field);
        assert!(!secret.exported);
    }

    #[test]
    fn rust_struct_fields_require_pub_to_export() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub struct Point { pub x: i32, y: i32 }\n",
        );
        let x = named(&parsed, "x");
        assert_eq!(x.kind, SymbolKind::Field);
        assert!(x.exported);
        let y = named(&parsed, "y");
        assert_eq!(y.kind, SymbolKind::Field);
        assert!(!y.exported);
    }

    #[test]
    fn rust_enum_variants_inherit_enum_visibility() {
        let parsed = parse_src(
            Language::Rust,
            "m.rs",
            "pub enum Shape { Circle, Square }\nenum Hidden { A }\n",
        );
        let circle = named(&parsed, "Circle");
        assert_eq!(circle.kind, SymbolKind::Field);
        assert!(circle.exported);
        let hidden = named(&parsed, "A");
        assert_eq!(hidden.kind, SymbolKind::Field);
        assert!(!hidden.exported);
    }

    #[test]
    fn go_vars_are_variables_and_struct_fields_follow_capitalization() {
        let parsed = parse_src(
            Language::Go,
            "m.go",
            "package p\nvar Global = 1\nvar hidden = 2\ntype Point struct{ X, y int }\n",
        );
        assert_eq!(named(&parsed, "Global").kind, SymbolKind::Variable);
        assert!(named(&parsed, "Global").exported);
        assert_eq!(named(&parsed, "hidden").kind, SymbolKind::Variable);
        assert!(!named(&parsed, "hidden").exported);
        assert_eq!(named(&parsed, "X").kind, SymbolKind::Field);
        assert!(named(&parsed, "X").exported);
        assert_eq!(named(&parsed, "y").kind, SymbolKind::Field);
        assert!(!named(&parsed, "y").exported);
    }

    #[test]
    fn python_distinguishes_const_variable_and_class_field() {
        let parsed = parse_src(
            Language::Python,
            "m.py",
            "MAX = 10\nname = \"x\"\nclass Foo:\n    attr = 1\n",
        );
        assert_eq!(named(&parsed, "MAX").kind, SymbolKind::Const);
        assert_eq!(named(&parsed, "name").kind, SymbolKind::Variable);
        assert_eq!(named(&parsed, "attr").kind, SymbolKind::Field);
        assert!(named(&parsed, "attr").exported);
    }

    #[test]
    fn typescript_fields_and_variables_honour_export_and_private() {
        let parsed = parse_src(
            Language::TypeScript,
            "m.ts",
            "export let counter = 0;\nlet hidden = 1;\nexport interface Props { title: string }\nexport class App {\n  private count = 0;\n  visible = 1;\n}\n",
        );
        assert_eq!(named(&parsed, "counter").kind, SymbolKind::Variable);
        assert!(named(&parsed, "counter").exported);
        assert_eq!(named(&parsed, "hidden").kind, SymbolKind::Variable);
        assert!(!named(&parsed, "hidden").exported);
        assert_eq!(named(&parsed, "title").kind, SymbolKind::Field);
        assert!(named(&parsed, "title").exported);
        assert_eq!(named(&parsed, "count").kind, SymbolKind::Field);
        assert!(!named(&parsed, "count").exported);
        assert_eq!(named(&parsed, "visible").kind, SymbolKind::Field);
        assert!(named(&parsed, "visible").exported);
    }

    #[test]
    fn ground_truth_skips_function_locals_and_call_sites() {
        let go = go_defs(&mask_c_like(
            "package p\ntype Point struct {\n\tX int\n\ty int\n}\nfunc New() *Point {\n\treq := 1\n\tuser := req\n\treturn &Point{}\n}\n",
            CFlavor::Go,
        ));
        assert!(go.contains(&"Point".to_string()));
        assert!(go.contains(&"X".to_string()));
        assert!(go.contains(&"y".to_string()));
        assert!(go.contains(&"New".to_string()));
        assert!(!go.contains(&"req".to_string()));
        assert!(!go.contains(&"user".to_string()));

        let rust = rust_defs(&mask_c_like(
            "fn main() { let Ok(x) = f(); }\nfn helper() {}\npub struct Point { pub x: i32, y: i32 }\n",
            CFlavor::Rust,
        ));
        assert!(rust.contains(&"main".to_string()));
        assert!(rust.contains(&"helper".to_string()));
        assert!(rust.contains(&"Point".to_string()));
        assert!(rust.contains(&"x".to_string()));
        assert!(rust.contains(&"y".to_string()));
        assert!(!rust.contains(&"Ok".to_string()));

        let ts = ts_defs(&mask_c_like(
            "export function greet() { const id = 1; assignNodeIds(); }\nexport interface Props { title: string }\nfunction exploredFiles(\n  graph: CodeGraph,\n): { greppedIds: number } {\n  const content = 1;\n}\nfunction assembleGraph(\n  ctx: AssembleContext = {},\n): Promise<CodeGraph> {\n  const cache = 1;\n}\n",
            CFlavor::Js,
        ));
        assert!(ts.contains(&"greet".to_string()));
        assert!(ts.contains(&"Props".to_string()));
        assert!(ts.contains(&"title".to_string()));
        assert!(ts.contains(&"exploredFiles".to_string()));
        assert!(!ts.contains(&"id".to_string()));
        assert!(!ts.contains(&"assignNodeIds".to_string()));
        assert!(!ts.contains(&"content".to_string()));
        assert!(!ts.contains(&"greppedIds".to_string()));
        assert!(ts.contains(&"assembleGraph".to_string()));
        assert!(!ts.contains(&"cache".to_string()));
    }

    #[test]
    fn ground_truth_mask_drops_defs_inside_comments_and_strings() {
        let py = mask_python(
            "\"\"\"\ndef fake():\n    pass\n\"\"\"\ndef real():\n    # def also_fake():\n    pass\n",
        );
        assert_eq!(python_defs(&py), ["real"]);

        let java = mask_c_like(
            "class Real {}\n/* class Fake {} */\n// class AlsoFake {}\nString s = \"class Nope {}\";\n",
            CFlavor::Java,
        );
        assert_eq!(java_defs(&java), ["Real"]);
    }

    // ---------------------------------------------------------------- recall
    //
    // Quality gate: symbol recall ≥90% vs an independent definition scan.
    //
    // Denominator design (the 209/209 lesson): ground truth is produced from a
    // filesystem walk by extension plus a comment/string-stripped keyword scan.
    // ParserPool is consulted *afterwards*. A parse failure, empty extract, or
    // binary misclassification in the code under test cannot shrink the
    // denominator — unread files contribute a sentinel `<unreadable>` name that
    // the parser will never match.
    //
    // Known residual misses (not used to shrink GT):
    // - Java annotation type elements (`value()`, `serialize()`) are true
    //   misses until queries capture `annotation_type_element_declaration`.
    // - Java `String` / enum constant names / `K` are field-scanner noise.
    // - Go embedded `http.ResponseWriter` is recorded as `http` by the line
    //   scanner; the parser correctly names `ResponseWriter`.
    // - Rust `$name` macro metavariables and test `HAYSTACK` macros are noise.
    // - TypeScript leftover short names in generated grammar files are noise.

    const RECALL_THRESHOLD: f64 = 0.90;

    #[derive(Clone, Copy)]
    enum RecallLang {
        Python,
        Go,
        Java,
        Rust,
        TypeScript,
    }

    struct RecallCorpus {
        language: &'static str,
        name: &'static str,
        root: PathBuf,
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("astrolabe-core must be inside the workspace")
            .to_path_buf()
    }

    fn recall_corpus(kind: RecallLang) -> RecallCorpus {
        let workspace = workspace_root();
        // Env override first, then a sibling of the workspace. No absolute
        // fallback: a baked-in path only works on one machine.
        let sibling = |name: &str, env_key: &str| {
            if let Some(p) = std::env::var_os(env_key).map(PathBuf::from) {
                return p;
            }
            workspace
                .parent()
                .map(|p| p.join(name))
                .unwrap_or_else(|| workspace.join(name))
        };
        match kind {
            RecallLang::Python => RecallCorpus {
                language: "Python",
                name: "serena",
                root: sibling("serena", "ASTROLABE_CORPUS_PYTHON"),
            },
            RecallLang::Go => RecallCorpus {
                language: "Go",
                name: "gin",
                root: workspace.join("corpus/go"),
            },
            RecallLang::Java => RecallCorpus {
                language: "Java",
                name: "gson",
                root: workspace.join("corpus/java"),
            },
            RecallLang::Rust => RecallCorpus {
                language: "Rust",
                name: "ripgrep",
                root: workspace.join("corpus/rust"),
            },
            RecallLang::TypeScript => RecallCorpus {
                language: "TypeScript",
                name: "openvisio-oss",
                root: sibling("openvisio-oss", "ASTROLABE_CORPUS_TYPESCRIPT"),
            },
        }
    }

    fn ignored_dir(name: &str) -> bool {
        matches!(
            name,
            ".git"
                | ".venv"
                | "venv"
                | "node_modules"
                | "target"
                | "dist"
                | "build"
                | "coverage"
                | "__pycache__"
                | ".mypy_cache"
                | ".next"
                | ".openvisio"
                | ".serena"
        )
    }

    fn walk_files(dir: &Path, root: &Path, out: &mut Vec<RelPath>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if !ignored_dir(&entry.file_name().to_string_lossy()) {
                    walk_files(&path, root, out);
                }
            } else if kind.is_file() {
                if let Ok(relative) = path.strip_prefix(root) {
                    out.push(RelPath::new(relative.to_string_lossy()));
                }
            }
        }
    }

    fn language_of(kind: RecallLang, path: &RelPath) -> Option<Language> {
        let lang = Language::from_path(path)?;
        match kind {
            RecallLang::Python => matches!(lang, Language::Python).then_some(lang),
            RecallLang::Go => matches!(lang, Language::Go).then_some(lang),
            RecallLang::Java => matches!(lang, Language::Java).then_some(lang),
            RecallLang::Rust => matches!(lang, Language::Rust).then_some(lang),
            RecallLang::TypeScript => {
                matches!(lang, Language::TypeScript | Language::Tsx).then_some(lang)
            }
        }
    }

    fn ground_truth_names(lang: Language, source: &str) -> Vec<String> {
        match lang {
            Language::Python => python_defs(&mask_python(source)),
            Language::Go => go_defs(&mask_c_like(source, CFlavor::Go)),
            Language::Java => java_defs(&mask_c_like(source, CFlavor::Java)),
            Language::Rust => rust_defs(&mask_c_like(source, CFlavor::Rust)),
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                ts_defs(&mask_c_like(source, CFlavor::Js))
            }
            // P0 languages without recall baselines yet.
            Language::Php
            | Language::C
            | Language::Cpp
            | Language::ObjC
            | Language::ObjCpp
            | Language::Swift
            | Language::Vue => Vec::new(),
        }
    }

    fn run_symbol_recall(kind: RecallLang) {
        let corpus = recall_corpus(kind);
        assert!(
            corpus.root.is_dir(),
            "语料目录不存在: {}",
            corpus.root.display()
        );

        let mut files = Vec::new();
        walk_files(&corpus.root, &corpus.root, &mut files);
        files.sort();
        files.retain(|path| language_of(kind, path).is_some());
        assert!(
            !files.is_empty(),
            "{} 语料没有源文件: {}",
            corpus.language,
            corpus.root.display()
        );

        // Ground truth is built from the filesystem walk + regex-like scans of
        // file bytes. ParserPool is consulted afterwards. A parse failure
        // therefore cannot shrink the denominator — the file's GT names stay.
        let pool = ParserPool::new();
        let mut gt: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut got: BTreeMap<(String, String), usize> = BTreeMap::new();
        let mut kind_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut parse_fail = 0usize;
        let mut unread = 0usize;
        let mut fail_samples = Vec::new();

        for path in &files {
            let lang = language_of(kind, path).expect("filtered");
            let disk = corpus.root.join(path.as_str());
            let source = match fs::read_to_string(&disk) {
                Ok(source) => source,
                Err(_) => {
                    unread += 1;
                    *gt.entry((path.as_str().to_string(), "<unreadable>".into()))
                        .or_insert(0) += 1;
                    continue;
                }
            };
            for name in ground_truth_names(lang, &source) {
                *gt.entry((path.as_str().to_string(), name)).or_insert(0) += 1;
            }
            match pool.parse(lang, path, &source) {
                Ok(parsed) => {
                    for symbol in parsed.symbols {
                        *got.entry((path.as_str().to_string(), symbol.name))
                            .or_insert(0) += 1;
                        let label = match symbol.kind {
                            SymbolKind::Function => "function",
                            SymbolKind::Method => "method",
                            SymbolKind::Class => "class",
                            SymbolKind::Interface => "interface",
                            SymbolKind::Struct => "struct",
                            SymbolKind::Enum => "enum",
                            SymbolKind::Trait => "trait",
                            SymbolKind::Type => "type",
                            SymbolKind::Const => "const",
                            SymbolKind::Module => "module",
                            SymbolKind::Field => "field",
                            SymbolKind::Variable => "variable",
                        };
                        *kind_counts.entry(label).or_insert(0) += 1;
                    }
                }
                Err(error) => {
                    parse_fail += 1;
                    if fail_samples.len() < 8 {
                        fail_samples.push(format!("{path}: {error}"));
                    }
                }
            }
        }

        let mut gt_total = 0usize;
        let mut matched = 0usize;
        let mut misses = Vec::new();
        for (key, &count) in &gt {
            gt_total += count;
            let hit = got.get(key).copied().unwrap_or(0).min(count);
            matched += hit;
            for _ in hit..count {
                if misses.len() < 24 {
                    misses.push(format!("{} :: {}", key.0, key.1));
                }
            }
        }

        let rate = if gt_total == 0 {
            1.0
        } else {
            matched as f64 / gt_total as f64
        };
        println!(
            "ASTROLABE_SYMBOL_RECALL|{}|{}|{}|{}|{:.2}|{}|{}|{}|{}",
            corpus.language,
            corpus.name,
            gt_total,
            matched,
            rate * 100.0,
            files.len(),
            parse_fail,
            unread,
            if rate + f64::EPSILON >= RECALL_THRESHOLD {
                "PASS"
            } else {
                "FAIL"
            }
        );
        println!("  kinds: {kind_counts:?}");
        if !fail_samples.is_empty() {
            println!("  parse_failures: {fail_samples:?}");
        }
        if !misses.is_empty() {
            println!("  miss_samples: {misses:?}");
        }
        assert!(
            gt_total > 0,
            "{} ground truth 为空；请检查语料路径和提取规则",
            corpus.language
        );
        assert!(
            rate + f64::EPSILON >= RECALL_THRESHOLD,
            "{} 符号召回率 {:.2}% 低于 {:.0}%（GT={gt_total} matched={matched} files={} parse_fail={parse_fail} unread={unread}）。分母来自独立扫描，parse 失败不会把它缩小。漏报样例: {misses:?}",
            corpus.language,
            rate * 100.0,
            RECALL_THRESHOLD * 100.0,
            files.len(),
        );
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn python_symbol_recall() {
        run_symbol_recall(RecallLang::Python);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn go_symbol_recall() {
        run_symbol_recall(RecallLang::Go);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn java_symbol_recall() {
        run_symbol_recall(RecallLang::Java);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn rust_symbol_recall() {
        run_symbol_recall(RecallLang::Rust);
    }

    #[test]
    #[ignore = "真实语料符号召回验收；使用 cargo test -- --ignored 运行"]
    fn typescript_symbol_recall() {
        run_symbol_recall(RecallLang::TypeScript);
    }

    // ------------------------------------------------------------- masking

    #[derive(Clone, Copy)]
    enum CFlavor {
        Go,
        Java,
        Rust,
        Js,
    }

    fn mask_python(source: &str) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            if let Some((consumed, quote, triple, raw)) = python_string_start(&chars, i) {
                for _ in 0..consumed {
                    out.push(' ');
                }
                i += consumed;
                if triple {
                    while i + 2 < chars.len()
                        && !(chars[i] == quote && chars[i + 1] == quote && chars[i + 2] == quote)
                    {
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    for _ in 0..3 {
                        if i < chars.len() {
                            out.push(' ');
                            i += 1;
                        }
                    }
                } else {
                    while i < chars.len() && chars[i] != quote {
                        if !raw && chars[i] == '\\' && i + 1 < chars.len() {
                            out.push(' ');
                            out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                            i += 2;
                            continue;
                        }
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                }
                continue;
            }
            if chars[i] == '#' {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn python_string_start(chars: &[char], i: usize) -> Option<(usize, char, bool, bool)> {
        let mut j = i;
        let mut prefix = 0usize;
        let mut raw = false;
        while j < chars.len() && matches!(chars[j], 'r' | 'R' | 'u' | 'U' | 'f' | 'F' | 'b' | 'B') {
            if matches!(chars[j], 'r' | 'R') {
                raw = true;
            }
            prefix += 1;
            j += 1;
            if prefix > 2 {
                return None;
            }
        }
        let q = *chars.get(j)?;
        if q != '\'' && q != '"' {
            return None;
        }
        let triple = j + 2 < chars.len() && chars[j + 1] == q && chars[j + 2] == q;
        let quote_len = if triple { 3 } else { 1 };
        Some((j - i + quote_len, q, triple, raw))
    }

    fn mask_c_like(source: &str, flavor: CFlavor) -> String {
        let chars: Vec<char> = source.chars().collect();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < chars.len() {
            if matches!(flavor, CFlavor::Rust) {
                if let Some(hashes) = rust_raw_string_hashes(&chars, i) {
                    let start_len = rust_raw_prefix_len(&chars, i, hashes);
                    for _ in 0..start_len {
                        out.push(' ');
                    }
                    i += start_len;
                    loop {
                        if i >= chars.len() {
                            break;
                        }
                        if chars[i] == '"' && rust_raw_closer(&chars, i + 1, hashes) {
                            out.push(' ');
                            i += 1;
                            for _ in 0..hashes {
                                out.push(' ');
                                i += 1;
                            }
                            break;
                        }
                        out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                        i += 1;
                    }
                    continue;
                }
            }
            if matches!(flavor, CFlavor::Java)
                && chars[i] == '"'
                && i + 2 < chars.len()
                && chars[i + 1] == '"'
                && chars[i + 2] == '"'
            {
                for _ in 0..3 {
                    out.push(' ');
                }
                i += 3;
                while i + 2 < chars.len()
                    && !(chars[i] == '"' && chars[i + 1] == '"' && chars[i + 2] == '"')
                {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                for _ in 0..3 {
                    if i < chars.len() {
                        out.push(' ');
                        i += 1;
                    }
                }
                continue;
            }
            if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                while i < chars.len() && chars[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                out.push(' ');
                out.push(' ');
                i += 2;
                while i < chars.len() {
                    if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        out.push(' ');
                        out.push(' ');
                        i += 2;
                        break;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                continue;
            }
            if chars[i] == '"' {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(' ');
                        out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                        i += 2;
                        continue;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '\'' {
                if matches!(flavor, CFlavor::Rust) && rust_lifetime(&chars, i) {
                    out.push('\'');
                    i += 1;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        out.push(chars[i]);
                        i += 1;
                    }
                    continue;
                }
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        out.push(' ');
                        out.push(if chars[i + 1] == '\n' { '\n' } else { ' ' });
                        i += 2;
                        continue;
                    }
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            if chars[i] == '`' && matches!(flavor, CFlavor::Go | CFlavor::Js) {
                out.push(' ');
                i += 1;
                while i < chars.len() && chars[i] != '`' {
                    out.push(if chars[i] == '\n' { '\n' } else { ' ' });
                    i += 1;
                }
                if i < chars.len() {
                    out.push(' ');
                    i += 1;
                }
                continue;
            }
            out.push(chars[i]);
            i += 1;
        }
        out
    }

    fn rust_lifetime(chars: &[char], i: usize) -> bool {
        let Some(&next) = chars.get(i + 1) else {
            return false;
        };
        if !(next.is_alphabetic() || next == '_') {
            return false;
        }
        let mut j = i + 2;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        chars.get(j) != Some(&'\'') || j > i + 2
    }

    fn rust_raw_prefix_len(chars: &[char], i: usize, hashes: usize) -> usize {
        let mut n = 0;
        if chars.get(i) == Some(&'b') || chars.get(i) == Some(&'c') {
            n += 1;
        }
        n += 1; // r
        n += hashes;
        n + 1 // opening quote
    }

    fn rust_raw_string_hashes(chars: &[char], i: usize) -> Option<usize> {
        let mut j = i;
        if matches!(chars.get(j), Some('b') | Some('c')) {
            j += 1;
        }
        if chars.get(j) != Some(&'r') {
            return None;
        }
        j += 1;
        let mut hashes = 0usize;
        while chars.get(j) == Some(&'#') {
            hashes += 1;
            j += 1;
        }
        if chars.get(j) == Some(&'"') {
            Some(hashes)
        } else {
            None
        }
    }

    fn rust_raw_closer(chars: &[char], mut i: usize, hashes: usize) -> bool {
        for _ in 0..hashes {
            if chars.get(i) != Some(&'#') {
                return false;
            }
            i += 1;
        }
        true
    }

    fn leading_ident(s: &str) -> Option<&str> {
        let s = s.trim_start();
        let mut chars = s.char_indices();
        let (_, first) = chars.next()?;
        if !(first.is_alphabetic() || first == '_' || first == '$' || first == '#') {
            return None;
        }
        let mut end = first.len_utf8();
        for (idx, ch) in chars {
            if ch.is_alphanumeric() || ch == '_' {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        Some(&s[..end])
    }

    fn push_ident(s: &str, names: &mut Vec<String>) {
        if let Some(name) = leading_ident(s) {
            names.push(name.to_string());
        }
    }

    fn skip_balanced_parens(s: &str) -> Option<&str> {
        let s = s.trim_start();
        if !s.starts_with('(') {
            return Some(s);
        }
        let mut depth = 0i32;
        for (idx, ch) in s.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(s[idx + 1..].trim_start());
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn net_count(s: &str, open: char, close: char) -> i32 {
        s.chars().filter(|&c| c == open).count() as i32
            - s.chars().filter(|&c| c == close).count() as i32
    }

    fn python_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if", "elif", "else", "for", "while", "try", "except", "finally", "with", "import",
            "from", "return", "raise", "assert", "class", "def", "async", "await", "yield", "pass",
            "break", "continue", "global", "nonlocal", "lambda", "and", "or", "not", "in", "is",
            "match", "case",
        ];
        let mut names = Vec::new();
        for line in masked.lines() {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix("class ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = trimmed.strip_prefix("async def ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = trimmed.strip_prefix("def ") {
                push_ident(rest, &mut names);
            } else if indent == 0 {
                if let Some(name) = leading_ident(trimmed) {
                    if SKIP.contains(&name) {
                        continue;
                    }
                    let after = trimmed[name.len()..].trim_start();
                    if after.starts_with('=') || after.starts_with(':') {
                        names.push(name.to_string());
                    }
                }
            }
        }
        names
    }

    fn go_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "package",
            "import",
            "func",
            "return",
            "if",
            "for",
            "range",
            "switch",
            "case",
            "default",
            "go",
            "defer",
            "select",
            "struct",
            "interface",
            "map",
            "chan",
            "type",
            "const",
            "var",
            "else",
            "break",
            "continue",
            "fallthrough",
            "goto",
        ];
        let mut names = Vec::new();
        let mut block: Option<&'static str> = None;
        let mut paren = 0i32;
        let mut func_depth = 0i32;
        let mut pending_func = false;
        let mut type_depth = 0i32;

        for line in masked.lines() {
            let trimmed = line.trim();
            let braces = net_count(line, '{', '}');

            if func_depth > 0 || pending_func {
                if pending_func && line.contains('{') {
                    pending_func = false;
                    func_depth = (func_depth + braces).max(0);
                } else if func_depth > 0 {
                    func_depth = (func_depth + braces).max(0);
                }
                continue;
            }

            if type_depth > 0 {
                collect_go_field_names(trimmed, SKIP, &mut names);
                type_depth = (type_depth + braces).max(0);
                continue;
            }

            if let Some(rest) = trimmed.strip_prefix("func ") {
                let rest = rest.trim_start();
                let after = if rest.starts_with('(') {
                    skip_balanced_parens(rest).unwrap_or("")
                } else {
                    rest
                };
                push_ident(after, &mut names);
                if line.contains('{') {
                    func_depth = braces.max(0);
                } else {
                    pending_func = true;
                }
                continue;
            }

            if let Some(kind) = block {
                paren += net_count(line, '(', ')');
                if let Some(name) = leading_ident(trimmed) {
                    if !SKIP.contains(&name) {
                        names.push(name.to_string());
                    }
                }
                if kind == "type"
                    && (trimmed.contains("struct") || trimmed.contains("interface"))
                    && line.contains('{')
                {
                    type_depth = braces.max(0);
                }
                if paren <= 0 {
                    block = None;
                    paren = 0;
                }
                continue;
            }

            for (kw, kind) in [("type ", "type"), ("const ", "const"), ("var ", "var")] {
                if let Some(rest) = trimmed.strip_prefix(kw) {
                    let rest = rest.trim_start();
                    if rest.starts_with('(') {
                        block = Some(kind);
                        paren = net_count(rest, '(', ')');
                        if let Some(after) = rest.strip_prefix('(') {
                            if let Some(name) = leading_ident(after) {
                                if !SKIP.contains(&name) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                        if paren <= 0 {
                            block = None;
                        }
                    } else {
                        push_ident(rest, &mut names);
                        if kind == "type" && (rest.contains("struct") || rest.contains("interface"))
                        {
                            if braces > 0 {
                                type_depth = braces;
                            } else if line.contains('{') && line.contains('}') {
                                if let Some(inner) = line
                                    .split_once('{')
                                    .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                                {
                                    collect_go_field_names(inner.trim(), SKIP, &mut names);
                                }
                            }
                        }
                    }
                    break;
                }
            }
        }
        names
    }

    fn collect_go_field_names(trimmed: &str, skip: &[&str], names: &mut Vec<String>) {
        let mut rest = trimmed;
        while let Some(name) = leading_ident(rest) {
            if skip.contains(&name) {
                break;
            }
            names.push(name.to_string());
            rest = rest[name.len()..].trim_start();
            if rest.starts_with(',') {
                rest = rest[1..].trim_start();
                continue;
            }
            break;
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Tok<'a> {
        Word(&'a str),
        Punct(char),
    }

    fn lex(masked: &str) -> Vec<Tok<'_>> {
        let mut out = Vec::new();
        let mut rest = masked;
        while !rest.is_empty() {
            let ch = rest.chars().next().unwrap();
            if ch.is_whitespace() {
                rest = &rest[ch.len_utf8()..];
                continue;
            }
            if ch.is_alphabetic() || ch == '_' || ch == '$' || ch == '#' {
                let mut len = ch.len_utf8();
                for next in rest[len..].chars() {
                    if next.is_alphanumeric() || next == '_' {
                        len += next.len_utf8();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Word(&rest[..len]));
                rest = &rest[len..];
                continue;
            }
            out.push(Tok::Punct(ch));
            rest = &rest[ch.len_utf8()..];
        }
        out
    }

    fn java_defs(masked: &str) -> Vec<String> {
        const CONTROL: &[&str] = &[
            "if",
            "for",
            "while",
            "switch",
            "catch",
            "synchronized",
            "try",
            "return",
            "throw",
            "new",
            "assert",
            "else",
            "this",
            "super",
            "case",
            "instanceof",
            "when",
            "yield",
            "class",
            "interface",
            "enum",
            "record",
            "package",
            "import",
            "extends",
            "implements",
            "throws",
            "break",
            "continue",
            "finally",
            "do",
        ];
        const ACCESS: &[&str] = &["public", "private", "protected"];
        let toks = lex(masked);
        let mut names = Vec::new();
        let mut i = 0;
        while i < toks.len() {
            match toks[i] {
                Tok::Word("class" | "interface" | "enum" | "record") => {
                    let dotted = i > 0 && toks[i - 1] == Tok::Punct('.');
                    if !dotted {
                        if let Some(Tok::Word(name)) = toks.get(i + 1) {
                            if !CONTROL.contains(name) {
                                names.push((*name).to_string());
                            }
                        }
                    }
                }
                Tok::Word(word) => {
                    let next = toks.get(i + 1);
                    if next == Some(&Tok::Punct('(')) && !CONTROL.contains(&word) {
                        if java_looks_like_method(&toks, i) {
                            names.push(word.to_string());
                        }
                    } else if matches!(
                        next,
                        Some(&Tok::Punct(';')) | Some(&Tok::Punct('=')) | Some(&Tok::Punct(','))
                    ) && !matches!(word, "true" | "false" | "null" | "L" | "D" | "F")
                        && java_looks_like_field(&toks, i, ACCESS)
                    {
                        names.push(word.to_string());
                    }
                }
                _ => {}
            }
            i += 1;
        }
        names
    }

    fn java_looks_like_method(toks: &[Tok<'_>], i: usize) -> bool {
        match toks.get(i.saturating_sub(1)) {
            None => false,
            Some(Tok::Punct('.' | '=' | ',' | '(')) => false,
            Some(Tok::Word(
                "new" | "return" | "throw" | "if" | "while" | "switch" | "for" | "catch" | "this"
                | "super" | "case",
            )) => false,
            Some(Tok::Word(_)) | Some(Tok::Punct('>' | ']')) => true,
            Some(Tok::Punct(_)) => false,
        }
    }

    fn java_looks_like_field(toks: &[Tok<'_>], i: usize, access: &[&str]) -> bool {
        let mut saw_access = false;
        let mut saw_type = false;
        let mut j = i;
        while j > 0 {
            j -= 1;
            match toks[j] {
                Tok::Punct(';' | '{' | '}' | '(' | ')') => break,
                Tok::Word(w) if access.contains(&w) => saw_access = true,
                Tok::Word("static" | "final" | "transient" | "volatile" | "native") => {}
                Tok::Word(_) | Tok::Punct('>' | ']') => saw_type = true,
                _ => {}
            }
        }
        saw_access && saw_type
    }

    fn rust_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if", "else", "for", "while", "loop", "match", "return", "let", "mut", "ref", "self",
            "Self", "crate", "super", "as", "in", "where", "unsafe", "async", "await", "dyn",
            "impl", "use", "extern", "move", "break", "continue", "pub", "enum", "struct", "fn",
        ];
        let mut names = Vec::new();
        let mut field_body = 0i32;
        let mut pending_fields = false;

        for line in masked.lines() {
            let stripped = strip_rust_prefix(line.trim_start());
            let braces = net_count(line, '{', '}');
            let is_field_item = stripped.starts_with("struct ")
                || stripped.starts_with("enum ")
                || stripped.starts_with("union ");

            if let Some(rest) = stripped.strip_prefix("macro_rules!") {
                push_ident(rest.trim_start(), &mut names);
            } else {
                for kw in [
                    "fn ", "struct ", "enum ", "trait ", "union ", "type ", "mod ", "const ",
                    "static ",
                ] {
                    if let Some(rest) = stripped.strip_prefix(kw) {
                        let rest = rest
                            .trim_start()
                            .strip_prefix("mut ")
                            .unwrap_or(rest.trim_start());
                        push_ident(rest, &mut names);
                        break;
                    }
                }
            }

            if field_body > 0 {
                if !is_field_item {
                    if let Some(name) = leading_ident(stripped) {
                        if !SKIP.contains(&name) {
                            let after = stripped[name.len()..].trim_start();
                            if after.starts_with(':')
                                || after.starts_with('(')
                                || after.starts_with('{')
                                || after.starts_with(',')
                                || after.starts_with('=')
                                || after.is_empty()
                            {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
                field_body = (field_body + braces).max(0);
                continue;
            }

            if pending_fields {
                if line.contains('{') {
                    pending_fields = false;
                    field_body = braces.max(0);
                } else if line.contains(';') {
                    pending_fields = false;
                }
                continue;
            }

            if is_field_item {
                if line.contains('{') {
                    field_body = braces.max(0);
                    if braces == 0 && line.contains('}') {
                        if let Some(inner) = line
                            .split_once('{')
                            .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                        {
                            for piece in inner.split(',') {
                                if let Some(name) = leading_ident(strip_rust_prefix(piece)) {
                                    if !SKIP.contains(&name) {
                                        names.push(name.to_string());
                                    }
                                }
                            }
                        }
                    }
                } else if !line.contains(';') {
                    pending_fields = true;
                }
            }
        }
        names
    }

    fn strip_rust_prefix(s: &str) -> &str {
        let s = s.trim_start();
        if let Some(rest) = s.strip_prefix("pub") {
            let rest = rest.trim_start();
            if rest.starts_with('(') {
                if let Some(after) = skip_balanced_parens(rest) {
                    return strip_rust_prefix(after);
                }
            }
            return strip_rust_prefix(rest);
        }
        for prefix in ["async ", "unsafe ", "default "] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return strip_rust_prefix(rest);
            }
        }
        if let Some(rest) = s.strip_prefix("const ") {
            let rest = rest.trim_start();
            if rest.starts_with("fn ") || rest.starts_with("unsafe ") || rest.starts_with("async ")
            {
                return strip_rust_prefix(rest);
            }
        }
        if let Some(rest) = s.strip_prefix("extern ") {
            return strip_rust_prefix(rest.trim_start().trim_start_matches(|c: char| {
                c == '"' || c.is_alphanumeric() || c == '_' || c.is_whitespace()
            }));
        }
        s
    }

    fn ts_defs(masked: &str) -> Vec<String> {
        const SKIP: &[&str] = &[
            "if",
            "else",
            "for",
            "while",
            "switch",
            "case",
            "return",
            "throw",
            "new",
            "typeof",
            "instanceof",
            "await",
            "yield",
            "break",
            "continue",
            "try",
            "catch",
            "finally",
            "do",
            "import",
            "from",
            "as",
            "of",
            "in",
            "void",
            "this",
            "super",
            "with",
            "function",
            "class",
            "interface",
            "enum",
            "type",
            "const",
            "let",
            "var",
        ];
        let mut names = Vec::new();
        let mut func_depth = 0i32;
        let mut type_depth = 0i32;
        let mut pending_func = false;

        for line in masked.lines() {
            let stripped = strip_ts_prefix(line.trim_start());
            let braces = net_count(line, '{', '}');
            let is_fn = stripped.starts_with("function");
            let is_type = stripped.starts_with("class ")
                || stripped.starts_with("interface ")
                || stripped.starts_with("enum ")
                || stripped.starts_with("namespace ")
                || stripped.starts_with("module ");

            if func_depth > 0 || pending_func {
                if pending_func {
                    let open_body = line.trim_end().ends_with('{');
                    if open_body {
                        pending_func = false;
                        func_depth = braces.max(0);
                    } else if line.trim_end().ends_with(';') {
                        pending_func = false;
                    }
                } else {
                    if is_fn {
                        if let Some(rest) = stripped.strip_prefix("function") {
                            let rest = rest.trim_start().trim_start_matches('*').trim_start();
                            push_ident(rest, &mut names);
                        }
                    } else if stripped.starts_with("class ") {
                        if let Some(rest) = stripped.strip_prefix("class ") {
                            push_ident(rest, &mut names);
                        }
                    }
                    func_depth = (func_depth + braces).max(0);
                }
                continue;
            }

            if type_depth > 0 {
                if type_depth == 1 {
                    if let Some(name) = leading_ident(stripped) {
                        if !SKIP.contains(&name) {
                            let after = stripped[name.len()..].trim_start();
                            if after.starts_with('(')
                                || after.starts_with('=')
                                || after.starts_with(':')
                                || after.starts_with('?')
                                || after.starts_with('<')
                            {
                                names.push(name.to_string());
                            }
                        }
                    }
                }
                type_depth = (type_depth + braces).max(0);
                continue;
            }

            if let Some(rest) = stripped.strip_prefix("function") {
                let rest = rest.trim_start().trim_start_matches('*').trim_start();
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("class ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("interface ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("enum ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("namespace ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("module ") {
                push_ident(rest, &mut names);
            } else if let Some(rest) = stripped.strip_prefix("type ") {
                if let Some(name) = leading_ident(rest) {
                    let after = rest[name.len()..].trim_start();
                    if after.starts_with('=') || after.starts_with('<') {
                        names.push(name.to_string());
                    }
                }
            } else {
                for kw in ["const ", "let ", "var "] {
                    if let Some(rest) = stripped.strip_prefix(kw) {
                        let rest = rest.trim_start();
                        if !rest.starts_with('{') && !rest.starts_with('[') {
                            push_ident(rest, &mut names);
                        }
                        break;
                    }
                }
            }

            if is_fn || looks_like_ts_arrow_fn(stripped) {
                if line.contains('{') {
                    func_depth = braces.max(0);
                } else if !line.contains(';') {
                    pending_func = true;
                }
            } else if is_type {
                type_depth = braces.max(0);
                if braces == 0 && line.contains('{') && line.contains('}') {
                    if let Some(inner) = line
                        .split_once('{')
                        .and_then(|(_, r)| r.rsplit_once('}').map(|(i, _)| i))
                    {
                        for piece in inner.split([';', ',']) {
                            if let Some(name) = leading_ident(piece) {
                                if !SKIP.contains(&name) {
                                    names.push(name.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        names
    }

    fn looks_like_ts_arrow_fn(stripped: &str) -> bool {
        (stripped.starts_with("const ")
            || stripped.starts_with("let ")
            || stripped.starts_with("var ")
            || stripped.starts_with("export "))
            && (stripped.contains("=>") || stripped.contains("function"))
    }

    fn strip_ts_prefix(s: &str) -> &str {
        let s = s.trim_start();
        for prefix in [
            "export ",
            "default ",
            "declare ",
            "async ",
            "abstract ",
            "public ",
            "private ",
            "protected ",
            "static ",
            "readonly ",
            "override ",
            "accessor ",
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                return strip_ts_prefix(rest);
            }
        }
        s
    }
}
