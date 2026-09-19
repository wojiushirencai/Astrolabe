//! Tree-sitter local-scope resolver for Python, Go, Java, and Rust.
//!
//! JS/TS belong to [`super::scope_js`] (oxc). This type covers the four
//! languages that have no equivalent compile-free semantic analyzer.
//!
//! # What this can do
//!
//! File-local bindings: parameters, locals, block-scoped names, and the
//! uses that resolve to them. Answers are [`Evidence::ScopeBinding`].
//!
//! # What this cannot do
//!
//! - Cross-file references (imports, other package files, subclasses).
//! - Dynamic dispatch, reflection, macro expansion.
//! - Inherited members defined in another file.
//!
//! [`ScopeResolver::bindings`] can only return [`ByteRange`]s. When a binding
//! is module-level / `pub` / exported, the returned ranges are still
//! file-local; the caller cannot tell from the `Vec` that other files may
//! refer to the same name. See the contract note at the bottom of this
//! module's docs.
//!
//! # `locals.scm` survey (checked against upstream, 2026-09)
//!
//! Official grammar repos ship `queries/highlights.scm` and `tags.scm` only
//! — **none** of `tree-sitter-python`, `tree-sitter-go`, `tree-sitter-java`,
//! or `tree-sitter-rust` include `queries/locals.scm`.
//!
//! nvim-treesitter maintains `queries/{python,go,java,rust}/locals.scm`
//! using `@local.scope` / `@local.definition*` / `@local.reference`. Those
//! queries are a useful map of node types, but they are not sufficient as
//! an engine:
//!
//! - Python: no `global` / `nonlocal`; no comprehension iterator isolation.
//! - Go: treats `field_identifier` as a reference (so `obj.x` is `x`);
//!   no `:=` mixed-redeclare rule.
//! - Java: no field-vs-local disambiguation for bare names.
//! - Rust: no sequential `let` shadowing in one block; no macro skip.
//!
//! This module therefore walks the tree with language rules, rather than
//! executing those `.scm` files as-is. `ParserPool` is not used: its
//! `parse` drops the `Tree` after extracting symbols, and `grammar` is
//! private. Parsers here use the same four `tree_sitter_*::LANGUAGE` values.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use tree_sitter::{Node, Parser, Tree};

use crate::types::{Language, RelPath};

use super::{ByteRange, Evidence, RewriteError, ScopeResolver};

/// File-local scope resolver backed by tree-sitter.
pub struct TreeSitterScopeResolver {
    parsers: [Mutex<Option<Parser>>; 4],
}

impl TreeSitterScopeResolver {
    pub fn new() -> Self {
        Self {
            parsers: [
                Mutex::new(make_parser(Lang::Python)),
                Mutex::new(make_parser(Lang::Go)),
                Mutex::new(make_parser(Lang::Java)),
                Mutex::new(make_parser(Lang::Rust)),
            ],
        }
    }
}

impl Default for TreeSitterScopeResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeResolver for TreeSitterScopeResolver {
    fn bindings(
        &self,
        path: &RelPath,
        source: &str,
        at: ByteRange,
    ) -> Result<Vec<ByteRange>, RewriteError> {
        let lang = lang_of(path)?;
        let tree = self.parse(lang, source)?;
        let analysis = Analyzer::run(lang, source, &tree);
        Ok(analysis.bindings_at(at))
    }

    fn evidence(&self) -> Evidence {
        Evidence::ScopeBinding
    }

    /// This resolver only sees one file, so an exported name (`pub`, a Go
    /// capital, `public`, a non-underscore Python module-level name) may have
    /// callers it cannot reach. Reporting that keeps the engine from treating
    /// a single-file result as a finished rename.
    fn may_span_files(&self, path: &RelPath, source: &str, at: ByteRange) -> bool {
        let Ok(lang) = lang_of(path) else {
            // Unknown language: assume the worst rather than imply completeness.
            return true;
        };
        match self.parse(lang, source) {
            Ok(tree) => Analyzer::run(lang, source, &tree).exported_at(at),
            Err(_) => true,
        }
    }
}

impl TreeSitterScopeResolver {
    fn parse(&self, lang: Lang, source: &str) -> Result<Tree, RewriteError> {
        let mut slot = self.parsers[lang.index()]
            .lock()
            .map_err(|_| RewriteError::NoResolver("parser pool mutex poisoned".into()))?;
        let parser = slot
            .as_mut()
            .ok_or_else(|| RewriteError::NoResolver(format!("no grammar for {lang:?}")))?;
        parser
            .parse(source, None)
            .ok_or_else(|| RewriteError::NoResolver("tree-sitter failed to produce a tree".into()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    Python,
    Go,
    Java,
    Rust,
}

impl Lang {
    fn index(self) -> usize {
        match self {
            Lang::Python => 0,
            Lang::Go => 1,
            Lang::Java => 2,
            Lang::Rust => 3,
        }
    }
}

fn lang_of(path: &RelPath) -> Result<Lang, RewriteError> {
    match Language::from_path(path) {
        Some(Language::Python) => Ok(Lang::Python),
        Some(Language::Go) => Ok(Lang::Go),
        Some(Language::Java) => Ok(Lang::Java),
        Some(Language::Rust) => Ok(Lang::Rust),
        Some(other) => Err(RewriteError::NoResolver(format!(
            "{} is handled by the oxc resolver, not TreeSitterScopeResolver",
            other.name()
        ))),
        None => Err(RewriteError::NoResolver(format!(
            "no tree-sitter scope resolver for {path}"
        ))),
    }
}

fn make_parser(lang: Lang) -> Option<Parser> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(lang)).ok()?;
    Some(parser)
}

fn grammar(lang: Lang) -> tree_sitter::Language {
    match lang {
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
    }
}

fn byte_range(node: Node<'_>) -> ByteRange {
    ByteRange {
        start: node.start_byte(),
        end: node.end_byte(),
    }
}

fn overlaps(ident: ByteRange, at: ByteRange) -> bool {
    if at.start == at.end {
        ident.start <= at.start && at.start < ident.end
    } else {
        ident.start < at.end && at.start < ident.end
    }
}

fn child_field<'a>(parent: Node<'a>, child: Node<'a>) -> Option<&'a str> {
    for i in 0..parent.child_count() {
        if parent.child(i) == Some(child) {
            return parent.field_name_for_child(i);
        }
    }
    None
}

fn is_under_field(mut node: Node<'_>, ancestor_kind: &str, field: &str) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == ancestor_kind {
            for i in 0..parent.child_count() {
                let Some(child) = parent.child(i) else {
                    continue;
                };
                if parent.field_name_for_child(i) != Some(field) {
                    continue;
                }
                if child.start_byte() <= node.start_byte() && node.end_byte() <= child.end_byte() {
                    return true;
                }
            }
            return false;
        }
        node = parent;
    }
    false
}

fn parent_kind(node: Node<'_>) -> Option<&str> {
    node.parent().map(|p| p.kind())
}

fn skip_name(name: &str) -> bool {
    name.is_empty() || name == "_"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Ns {
    Value,
    Type,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Function,
    Class,
    Block,
    Comprehension,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BindingKind {
    Local,
    Param,
    Function,
    Type,
    Field,
}

struct Scope {
    parent: Option<usize>,
    kind: ScopeKind,
    start: usize,
    globals: HashSet<String>,
    nonlocals: HashSet<String>,
}

#[allow(dead_code)]
struct Binding {
    name: String,
    ns: Ns,
    def: ByteRange,
    scope: usize,
    visible_from: usize,
    kind: BindingKind,
    /// Visible outside this file; surfaced through `may_span_files`.
    exported: bool,
}

#[derive(Clone, Debug)]
struct Occ {
    range: ByteRange,
    name: String,
    ns: Ns,
    scope: usize,
    pos: usize,
    binding: Option<u32>,
    /// Look up only in class scopes (Java `this.x` / `obj.x` field names).
    field_ref: bool,
}

struct Analysis {
    occs: Vec<Occ>,
    /// Per-binding "visible outside this file" flags, indexed by binding id.
    exported: Vec<bool>,
}

impl Analysis {
    fn bindings_at(&self, at: ByteRange) -> Vec<ByteRange> {
        let Some(idx) = self.pick_occ(at) else {
            return Vec::new();
        };
        let Some(id) = self.occs[idx].binding else {
            return vec![self.occs[idx].range];
        };
        let mut ranges: Vec<ByteRange> = self
            .occs
            .iter()
            .filter(|occ| occ.binding == Some(id))
            .map(|occ| occ.range)
            .collect();
        ranges.sort();
        ranges.dedup();
        ranges
    }

    /// Whether the binding under `at` is visible outside this file. An
    /// unbound occurrence counts as exported: it resolves to something this
    /// file never declared, so the declaration is elsewhere.
    fn exported_at(&self, at: ByteRange) -> bool {
        let Some(idx) = self.pick_occ(at) else {
            return true;
        };
        match self.occs[idx].binding {
            Some(id) => self.exported.get(id as usize).copied().unwrap_or(true),
            None => true,
        }
    }

    fn pick_occ(&self, at: ByteRange) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, occ) in self.occs.iter().enumerate() {
            if !overlaps(occ.range, at) {
                continue;
            }
            if occ.range == at {
                return Some(i);
            }
            match best {
                None => best = Some(i),
                Some(j) => {
                    let a = self.occs[i].range.end - self.occs[i].range.start;
                    let b = self.occs[j].range.end - self.occs[j].range.start;
                    if a < b {
                        best = Some(i);
                    }
                }
            }
        }
        best
    }
}

struct Analyzer<'a> {
    lang: Lang,
    source: &'a str,
    scopes: Vec<Scope>,
    stack: Vec<usize>,
    bindings: Vec<Binding>,
    occs: Vec<Occ>,
}

impl<'a> Analyzer<'a> {
    fn run(lang: Lang, source: &'a str, tree: &'a Tree) -> Analysis {
        let mut a = Analyzer {
            lang,
            source,
            scopes: Vec::new(),
            stack: Vec::new(),
            bindings: Vec::new(),
            occs: Vec::new(),
        };
        let root = tree.root_node();
        a.push_scope(root, ScopeKind::Module);
        a.walk(root);
        a.pop_scope();
        a.resolve_all();
        let exported = a.bindings.iter().map(|b| b.exported).collect();
        Analysis {
            occs: a.occs,
            exported,
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn cur(&self) -> usize {
        *self.stack.last().expect("scope stack")
    }

    fn push_scope(&mut self, node: Node<'_>, kind: ScopeKind) -> usize {
        let id = self.scopes.len();
        self.scopes.push(Scope {
            parent: self.stack.last().copied(),
            kind,
            start: node.start_byte(),
            globals: HashSet::new(),
            nonlocals: HashSet::new(),
        });
        self.stack.push(id);
        id
    }

    fn pop_scope(&mut self) {
        self.stack.pop();
    }

    fn walk_children(&mut self, node: Node<'_>) {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.walk(child);
            }
        }
    }

    fn walk_field(&mut self, node: Node<'_>, field: &str) {
        for i in 0..node.child_count() {
            if node.field_name_for_child(i) == Some(field) {
                if let Some(child) = node.child(i) {
                    self.walk(child);
                }
            }
        }
    }

    fn new_binding(
        &mut self,
        scope: usize,
        name: &str,
        ns: Ns,
        def: Node<'_>,
        visible_from: usize,
        kind: BindingKind,
    ) -> u32 {
        let id = self.bindings.len() as u32;
        let exported = self.compute_exported(scope, name, kind, def);
        self.bindings.push(Binding {
            name: name.to_string(),
            ns,
            def: byte_range(def),
            scope,
            visible_from,
            kind,
            exported,
        });
        id
    }

    fn compute_exported(&self, scope: usize, name: &str, kind: BindingKind, def: Node<'_>) -> bool {
        match self.lang {
            Lang::Python => {
                self.scopes[scope].kind == ScopeKind::Module && python_exported_name(name)
            }
            Lang::Go => {
                self.scopes[scope].kind == ScopeKind::Module
                    && name.chars().next().is_some_and(char::is_uppercase)
            }
            Lang::Java => {
                matches!(
                    kind,
                    BindingKind::Type | BindingKind::Field | BindingKind::Function
                ) && java_node_public(def)
            }
            Lang::Rust => rust_has_pub_ancestor(def),
        }
    }

    fn existing(&self, scope: usize, name: &str, ns: Ns) -> Option<u32> {
        self.bindings.iter().enumerate().rev().find_map(|(i, b)| {
            (b.scope == scope && b.name == name && b.ns == ns).then_some(i as u32)
        })
    }

    fn add_def(&mut self, node: Node<'_>, name: &str, ns: Ns, id: u32) {
        self.occs.push(Occ {
            range: byte_range(node),
            name: name.to_string(),
            ns,
            scope: self.cur(),
            pos: node.start_byte(),
            binding: Some(id),
            field_ref: false,
        });
    }

    fn add_ref(&mut self, node: Node<'_>, name: &str, ns: Ns) {
        if skip_name(name) {
            return;
        }
        self.occs.push(Occ {
            range: byte_range(node),
            name: name.to_string(),
            ns,
            scope: self.cur(),
            pos: node.start_byte(),
            binding: None,
            field_ref: false,
        });
    }

    fn add_field_ref(&mut self, node: Node<'_>, name: &str) {
        if skip_name(name) {
            return;
        }
        self.occs.push(Occ {
            range: byte_range(node),
            name: name.to_string(),
            ns: Ns::Value,
            scope: self.cur(),
            pos: node.start_byte(),
            binding: None,
            field_ref: true,
        });
    }

    fn define_new(
        &mut self,
        node: Node<'_>,
        ns: Ns,
        visible_from: usize,
        kind: BindingKind,
    ) -> Option<u32> {
        let name = self.text(node);
        if skip_name(name) {
            return None;
        }
        let id = self.new_binding(self.cur(), name, ns, node, visible_from, kind);
        self.add_def(node, name, ns, id);
        Some(id)
    }

    fn python_define(&mut self, node: Node<'_>, kind: BindingKind) {
        let name = self.text(node);
        if skip_name(name) {
            return;
        }
        let scope = self.python_var_scope();
        if self.scopes[scope].globals.contains(name) || self.scopes[scope].nonlocals.contains(name)
        {
            self.add_ref(node, name, Ns::Value);
            return;
        }
        if let Some(id) = self.existing(scope, name, Ns::Value) {
            self.add_def(node, name, Ns::Value, id);
            return;
        }
        let saved = self.stack.clone();
        self.stack.truncate(
            self.stack
                .iter()
                .position(|&s| s == scope)
                .map(|i| i + 1)
                .unwrap_or(self.stack.len()),
        );
        let id = self.new_binding(scope, name, Ns::Value, node, 0, kind);
        self.add_def(node, name, Ns::Value, id);
        self.stack = saved;
    }

    fn python_var_scope(&self) -> usize {
        for &id in self.stack.iter().rev() {
            match self.scopes[id].kind {
                ScopeKind::Function
                | ScopeKind::Class
                | ScopeKind::Comprehension
                | ScopeKind::Module => return id,
                ScopeKind::Block => {}
            }
        }
        0
    }

    fn python_walrus_scope(&self) -> usize {
        for &id in self.stack.iter().rev() {
            match self.scopes[id].kind {
                ScopeKind::Function | ScopeKind::Class | ScopeKind::Module => return id,
                ScopeKind::Block | ScopeKind::Comprehension => {}
            }
        }
        0
    }

    fn walk(&mut self, node: Node<'_>) {
        if !node.is_named() {
            self.walk_children(node);
            return;
        }
        match (self.lang, node.kind()) {
            (_, "comment" | "line_comment" | "block_comment" | "doc_comment") => {}
            (Lang::Python, kind) => self.walk_python(node, kind),
            (Lang::Go, kind) => self.walk_go(node, kind),
            (Lang::Java, kind) => self.walk_java(node, kind),
            (Lang::Rust, kind) => self.walk_rust(node, kind),
        }
    }

    fn walk_python(&mut self, node: Node<'_>, kind: &str) {
        match kind {
            "function_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.python_define(name, BindingKind::Function);
                }
                self.push_scope(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.python_bind_params(params);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.python_prescan_decls(body);
                }
                if let Some(ret) = node.child_by_field_name("return_type") {
                    self.walk(ret);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "lambda" => {
                self.push_scope(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.python_bind_params(params);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.python_define(name, BindingKind::Type);
                }
                self.push_scope(node, ScopeKind::Class);
                if let Some(supers) = node.child_by_field_name("superclasses") {
                    self.walk(supers);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "generator_expression" => {
                self.python_walk_comprehension(node);
            }
            "assignment" | "augmented_assignment" => {
                self.walk_field(node, "right");
                self.walk_field(node, "type");
                if let Some(left) = node.child_by_field_name("left") {
                    self.python_bind_pattern(left);
                }
            }
            "named_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
                if let Some(name) = node.child_by_field_name("name") {
                    let saved = self.stack.clone();
                    let scope = self.python_walrus_scope();
                    self.stack.truncate(
                        self.stack
                            .iter()
                            .position(|&s| s == scope)
                            .map(|i| i + 1)
                            .unwrap_or(self.stack.len()),
                    );
                    self.python_define(name, BindingKind::Local);
                    self.stack = saved;
                }
            }
            "for_statement" => {
                self.walk_field(node, "right");
                if let Some(left) = node.child_by_field_name("left") {
                    self.python_bind_pattern(left);
                }
                self.walk_field(node, "body");
                self.walk_field(node, "alternative");
            }
            "with_item" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.python_walk_as_or_expr(value);
                }
            }
            "except_clause" => {
                self.walk_field(node, "value");
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.python_bind_pattern(alias);
                }
                self.walk_children_except_fields(node, &["value", "alias"]);
            }
            "as_pattern" => {
                for i in 0..node.child_count() {
                    let Some(child) = node.child(i) else {
                        continue;
                    };
                    if node.field_name_for_child(i) == Some("alias") {
                        self.python_bind_pattern(child);
                    } else {
                        self.walk(child);
                    }
                }
            }
            "global_statement" | "nonlocal_statement" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "identifier" {
                            self.add_ref(child, self.text(child), Ns::Value);
                        }
                    }
                }
            }
            "import_statement" | "import_from_statement" => self.python_walk_import(node),
            "attribute" => {
                if let Some(obj) = node.child_by_field_name("object") {
                    self.walk(obj);
                }
            }
            "keyword_argument" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
            }
            "identifier" => {
                if self.python_ignore_identifier(node) {
                    return;
                }
                self.add_ref(node, self.text(node), Ns::Value);
            }
            _ => self.walk_children(node),
        }
    }

    fn python_walk_as_or_expr(&mut self, node: Node<'_>) {
        if node.kind() == "as_pattern" {
            self.walk_python(node, "as_pattern");
        } else {
            self.walk(node);
        }
    }

    fn python_walk_comprehension(&mut self, node: Node<'_>) {
        self.push_scope(node, ScopeKind::Comprehension);
        let comp = self.cur();
        let mut first_for = true;
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else {
                continue;
            };
            if child.kind() == "for_in_clause" {
                if let Some(right) = child.child_by_field_name("right") {
                    if first_for {
                        self.stack.pop();
                        self.walk(right);
                        self.stack.push(comp);
                        first_for = false;
                    } else {
                        self.walk(right);
                    }
                }
                if let Some(left) = child.child_by_field_name("left") {
                    self.python_bind_pattern(left);
                }
            } else {
                // Everything that is not a for-clause — the element
                // expression, the body, and any if-clauses — is evaluated
                // inside the comprehension scope.
                self.walk(child);
            }
        }
        self.pop_scope();
    }

    fn python_ignore_identifier(&self, node: Node<'_>) -> bool {
        if let Some(parent) = node.parent() {
            if parent.kind() == "attribute" && child_field(parent, node) == Some("attribute") {
                return true;
            }
            if parent.kind() == "keyword_argument" && child_field(parent, node) == Some("name") {
                return true;
            }
        }
        false
    }

    fn python_bind_params(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => self.python_define(node, BindingKind::Param),
            "typed_parameter" => {
                self.walk_field(node, "type");
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("type") {
                        continue;
                    }
                    if let Some(child) = node.child(i) {
                        self.python_bind_params(child);
                    }
                }
            }
            "default_parameter" | "typed_default_parameter" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.python_bind_pattern(name);
                }
                self.walk_field(node, "type");
                self.walk_field(node, "value");
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                self.python_bind_pattern(node);
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.python_bind_params(child);
                    }
                }
            }
        }
    }

    fn python_bind_pattern(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" => self.python_define(node, BindingKind::Local),
            "attribute" => {
                if let Some(obj) = node.child_by_field_name("object") {
                    self.walk(obj);
                }
            }
            "subscript" => self.walk(node),
            "as_pattern_target" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.python_bind_pattern(child);
                    }
                }
                if node.kind() == "as_pattern_target" && node.named_child_count() == 0 {
                    // Some grammars expose the target as the node itself wrapping an identifier.
                    if self
                        .text(node)
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '_')
                    {
                        self.python_define(node, BindingKind::Local);
                    }
                }
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.python_bind_pattern(child);
                    }
                }
            }
        }
    }

    fn python_prescan_decls(&mut self, node: Node<'_>) {
        match node.kind() {
            "function_definition"
            | "class_definition"
            | "lambda"
            | "list_comprehension"
            | "dictionary_comprehension"
            | "set_comprehension"
            | "generator_expression" => {}
            "global_statement" => {
                let scope = self.cur();
                let names: Vec<String> = (0..node.child_count())
                    .filter_map(|i| node.child(i))
                    .filter(|c| c.kind() == "identifier")
                    .map(|c| self.text(c).to_string())
                    .collect();
                self.scopes[scope].globals.extend(names);
            }
            "nonlocal_statement" => {
                let scope = self.cur();
                let names: Vec<String> = (0..node.child_count())
                    .filter_map(|i| node.child(i))
                    .filter(|c| c.kind() == "identifier")
                    .map(|c| self.text(c).to_string())
                    .collect();
                self.scopes[scope].nonlocals.extend(names);
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.python_prescan_decls(child);
                    }
                }
            }
        }
    }

    fn python_walk_import(&mut self, node: Node<'_>) {
        for i in 0..node.child_count() {
            if node.field_name_for_child(i) != Some("name") {
                continue;
            }
            if let Some(child) = node.child(i) {
                self.python_bind_import_name(child);
            }
        }
    }

    fn python_bind_import_name(&mut self, node: Node<'_>) {
        match node.kind() {
            "aliased_import" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.python_define(alias, BindingKind::Local);
                }
            }
            "dotted_name" => {
                if let Some(first) = node.named_child(0) {
                    if first.kind() == "identifier" {
                        self.python_define(first, BindingKind::Local);
                    }
                }
            }
            "identifier" => self.python_define(node, BindingKind::Local),
            _ => {}
        }
    }

    fn walk_children_except_fields(&mut self, node: Node<'_>, skip: &[&str]) {
        for i in 0..node.child_count() {
            if skip.contains(&node.field_name_for_child(i).unwrap_or("")) {
                continue;
            }
            if let Some(child) = node.child(i) {
                self.walk(child);
            }
        }
    }

    fn walk_go(&mut self, node: Node<'_>, kind: &str) {
        match kind {
            "function_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Function);
                }
                self.push_scope(node, ScopeKind::Function);
                self.walk_go_params_field(node, "type_parameters");
                self.walk_go_params_field(node, "parameters");
                self.walk_go_result(node);
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "method_declaration" => {
                self.push_scope(node, ScopeKind::Function);
                self.walk_go_params_field(node, "receiver");
                self.walk_go_params_field(node, "parameters");
                self.walk_go_result(node);
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "func_literal" => {
                self.push_scope(node, ScopeKind::Function);
                self.walk_go_params_field(node, "parameters");
                self.walk_go_result(node);
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "block" => {
                if go_function_body(node) {
                    self.walk_children(node);
                } else {
                    self.push_scope(node, ScopeKind::Block);
                    self.walk_children(node);
                    self.pop_scope();
                }
            }
            "if_statement"
            | "for_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "select_statement" => {
                self.push_scope(node, ScopeKind::Block);
                self.walk_children(node);
                self.pop_scope();
            }
            "parameter_declaration" | "variadic_parameter_declaration" => {
                self.walk_field(node, "type");
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("name") {
                        if let Some(name) = node.child(i) {
                            self.define_new(name, Ns::Value, 0, BindingKind::Param);
                        }
                    }
                }
            }
            "type_parameter_declaration" => {
                self.walk_field(node, "type");
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("name") {
                        if let Some(name) = node.child(i) {
                            self.define_new(name, Ns::Type, 0, BindingKind::Type);
                        }
                    }
                }
            }
            "short_var_declaration" => {
                self.walk_field(node, "right");
                if let Some(left) = node.child_by_field_name("left") {
                    self.go_short_left(left);
                }
            }
            "range_clause" => {
                self.walk_field(node, "right");
                if let Some(left) = node.child_by_field_name("left") {
                    self.go_short_left(left);
                }
            }
            "var_spec" | "const_spec" => {
                self.walk_field(node, "type");
                self.walk_field(node, "value");
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("name") {
                        if let Some(name) = node.child(i) {
                            self.define_new(name, Ns::Value, 0, BindingKind::Local);
                        }
                    }
                }
            }
            "type_spec" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Type, 0, BindingKind::Type);
                }
                self.walk_field(node, "type");
            }
            "selector_expression" => {
                if let Some(op) = node.child_by_field_name("operand") {
                    self.walk(op);
                }
            }
            "keyed_element" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
            }
            "field_declaration" => {
                self.walk_field(node, "type");
            }
            "identifier" => {
                if self.go_ignore_identifier(node) {
                    return;
                }
                self.add_ref(node, self.text(node), Ns::Value);
            }
            "type_identifier" => {
                self.add_ref(node, self.text(node), Ns::Type);
            }
            "field_identifier" | "package_identifier" | "label_name" => {}
            _ => self.walk_children(node),
        }
    }

    fn walk_go_params_field(&mut self, node: Node<'_>, field: &str) {
        self.walk_field(node, field);
    }

    fn walk_go_result(&mut self, node: Node<'_>) {
        if let Some(result) = node.child_by_field_name("result") {
            self.walk(result);
        }
    }

    fn go_short_left(&mut self, node: Node<'_>) {
        if node.kind() == "identifier" {
            let name = self.text(node);
            if skip_name(name) {
                return;
            }
            if self.existing(self.cur(), name, Ns::Value).is_some() {
                self.add_ref(node, name, Ns::Value);
            } else {
                self.define_new(node, Ns::Value, node.start_byte(), BindingKind::Local);
            }
            return;
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.go_short_left(child);
            }
        }
    }

    fn go_ignore_identifier(&self, node: Node<'_>) -> bool {
        is_under_field(node, "short_var_declaration", "left")
            || is_under_field(node, "range_clause", "left")
            || is_under_field(node, "parameter_declaration", "name")
            || is_under_field(node, "variadic_parameter_declaration", "name")
            || is_under_field(node, "var_spec", "name")
            || is_under_field(node, "const_spec", "name")
            || is_under_field(node, "function_declaration", "name")
    }

    fn walk_java(&mut self, node: Node<'_>, kind: &str) {
        match kind {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Type, 0, BindingKind::Type);
                }
                self.push_scope(node, ScopeKind::Class);
                if kind == "record_declaration" {
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.java_bind_params(params, BindingKind::Field);
                    }
                }
                self.walk_children_except_fields(node, &["name", "parameters"]);
                self.pop_scope();
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => {
                self.push_scope(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.java_bind_params(params, BindingKind::Param);
                }
                self.walk_field(node, "type");
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "block" | "constructor_body" => {
                if java_method_body(node) {
                    self.walk_children(node);
                } else {
                    self.push_scope(node, ScopeKind::Block);
                    self.walk_children(node);
                    self.pop_scope();
                }
            }
            "for_statement" => {
                self.push_scope(node, ScopeKind::Block);
                self.walk_children(node);
                self.pop_scope();
            }
            "enhanced_for_statement" => {
                self.walk_field(node, "value");
                self.push_scope(node, ScopeKind::Block);
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Local);
                }
                self.walk_field(node, "type");
                self.walk_field(node, "body");
                self.pop_scope();
            }
            "catch_clause" => {
                self.push_scope(node, ScopeKind::Block);
                self.walk_children(node);
                self.pop_scope();
            }
            "catch_formal_parameter" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Param);
                }
            }
            "lambda_expression" => {
                self.push_scope(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.java_bind_params(params, BindingKind::Param);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "local_variable_declaration" | "field_declaration" | "constant_declaration" => {
                self.walk_field(node, "type");
                let field = kind != "local_variable_declaration";
                let kind = if field {
                    BindingKind::Field
                } else {
                    BindingKind::Local
                };
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) != Some("declarator") {
                        continue;
                    }
                    let Some(decl) = node.child(i) else {
                        continue;
                    };
                    if let Some(value) = decl.child_by_field_name("value") {
                        self.walk(value);
                    }
                    if let Some(name) = decl.child_by_field_name("name") {
                        if name.kind() == "identifier" {
                            self.define_new(name, Ns::Value, name.start_byte(), kind);
                        }
                    }
                }
            }
            "resource" => {
                self.walk_field(node, "value");
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        self.define_new(name, Ns::Value, 0, BindingKind::Local);
                    }
                }
            }
            "field_access" => {
                if let Some(obj) = node.child_by_field_name("object") {
                    self.walk(obj);
                }
                if let Some(field) = node.child_by_field_name("field") {
                    if field.kind() == "identifier" {
                        self.add_field_ref(field, self.text(field));
                    }
                }
            }
            "method_invocation" => {
                if let Some(obj) = node.child_by_field_name("object") {
                    self.walk(obj);
                }
                self.walk_field(node, "arguments");
                self.walk_field(node, "type_arguments");
            }
            "formal_parameter" => {
                self.walk_field(node, "type");
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        self.define_new(name, Ns::Value, 0, BindingKind::Param);
                    }
                }
            }
            "inferred_parameters" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "identifier" {
                            self.define_new(child, Ns::Value, 0, BindingKind::Param);
                        }
                    }
                }
            }
            "identifier" => {
                if self.java_ignore_identifier(node) {
                    return;
                }
                self.add_ref(node, self.text(node), Ns::Value);
            }
            "type_identifier" => {
                self.add_ref(node, self.text(node), Ns::Type);
            }
            _ => self.walk_children(node),
        }
    }

    fn java_bind_params(&mut self, node: Node<'_>, kind: BindingKind) {
        match node.kind() {
            "identifier" => {
                self.define_new(node, Ns::Value, 0, kind);
            }
            "formal_parameter" => {
                self.walk_field(node, "type");
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        self.define_new(name, Ns::Value, 0, kind);
                    }
                }
            }
            "inferred_parameters" | "formal_parameters" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.java_bind_params(child, kind);
                    }
                }
            }
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.java_bind_params(child, kind);
                    }
                }
            }
        }
    }

    fn java_ignore_identifier(&self, node: Node<'_>) -> bool {
        if let Some(parent) = node.parent() {
            let field = child_field(parent, node);
            if parent.kind() == "field_access" && field == Some("field") {
                return true;
            }
            if parent.kind() == "method_invocation" && field == Some("name") {
                return true;
            }
            if parent.kind() == "variable_declarator" && field == Some("name") {
                return true;
            }
            if parent.kind() == "formal_parameter" && field == Some("name") {
                return true;
            }
            if matches!(
                parent.kind(),
                "class_declaration"
                    | "interface_declaration"
                    | "enum_declaration"
                    | "record_declaration"
                    | "method_declaration"
                    | "constructor_declaration"
            ) && field == Some("name")
            {
                return true;
            }
        }
        false
    }

    fn walk_rust(&mut self, node: Node<'_>, kind: &str) {
        match kind {
            "function_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        self.define_new(name, Ns::Value, 0, BindingKind::Function);
                    }
                }
                self.push_scope(node, ScopeKind::Function);
                for i in 0..node.child_count() {
                    let Some(child) = node.child(i) else {
                        continue;
                    };
                    match node.field_name_for_child(i) {
                        Some("name") => {}
                        Some("parameters") => self.rust_bind_parameters(child),
                        _ => self.walk(child),
                    }
                }
                self.pop_scope();
            }
            "closure_expression" => {
                self.push_scope(node, ScopeKind::Function);
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.rust_bind_parameters(params);
                }
                if let Some(ret) = node.child_by_field_name("return_type") {
                    self.walk(ret);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "block" | "unsafe_block" | "async_block" | "const_block" | "gen_block"
            | "try_block" => {
                self.push_scope(node, ScopeKind::Block);
                self.walk_children(node);
                self.pop_scope();
            }
            "let_declaration" => {
                self.walk_field(node, "value");
                self.walk_field(node, "type");
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    self.rust_bind_pattern(pattern, node.end_byte());
                }
                self.walk_field(node, "alternative");
            }
            "if_expression" => self.rust_walk_if(node),
            "while_expression" => self.rust_walk_while(node),
            "for_expression" => {
                self.walk_field(node, "value");
                self.push_scope(node, ScopeKind::Block);
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    self.rust_bind_pattern(pattern, node.start_byte());
                }
                self.walk_field(node, "body");
                self.pop_scope();
            }
            "match_arm" => {
                self.push_scope(node, ScopeKind::Block);
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    self.rust_bind_pattern(pattern, node.start_byte());
                    if let Some(cond) = pattern.child_by_field_name("condition") {
                        self.walk(cond);
                    }
                }
                self.walk_field(node, "value");
                self.pop_scope();
            }
            "macro_invocation" => {
                if let Some(mac) = node.child_by_field_name("macro") {
                    self.walk(mac);
                }
            }
            "macro_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Function);
                }
            }
            "token_tree"
            | "token_tree_pattern"
            | "token_repetition"
            | "token_repetition_pattern" => {}
            "field_expression" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
            }
            "field_initializer" => {
                if let Some(value) = node.child_by_field_name("value") {
                    self.walk(value);
                }
            }
            "parameter" => {
                self.walk_field(node, "type");
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    self.rust_bind_pattern(pattern, self.scopes[self.cur()].start);
                }
            }
            "self_parameter" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "self" {
                            self.define_new(child, Ns::Value, 0, BindingKind::Param);
                        }
                    }
                }
            }
            "const_item" | "static_item" => {
                self.walk_field(node, "type");
                self.walk_field(node, "value");
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Local);
                }
            }
            "struct_item" | "enum_item" | "union_item" | "type_item" | "trait_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Type, 0, BindingKind::Type);
                }
                self.walk_children_except_fields(node, &["name"]);
            }
            "mod_item" => {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define_new(name, Ns::Value, 0, BindingKind::Type);
                }
                self.push_scope(node, ScopeKind::Module);
                if let Some(body) = node.child_by_field_name("body") {
                    self.walk(body);
                }
                self.pop_scope();
            }
            "use_declaration" => {
                if let Some(arg) = node.child_by_field_name("argument") {
                    self.rust_bind_use(arg);
                }
            }
            "identifier" | "self" | "shorthand_field_identifier" => {
                if self.rust_ignore_value_name(node) {
                    return;
                }
                self.add_ref(node, self.text(node), Ns::Value);
            }
            "type_identifier" => {
                self.add_ref(node, self.text(node), Ns::Type);
            }
            "field_identifier" | "metavariable" | "lifetime" => {}
            _ => self.walk_children(node),
        }
    }

    fn rust_walk_if(&mut self, node: Node<'_>) {
        let cond = node.child_by_field_name("condition");
        let pats = cond.map(|c| rust_let_patterns(c)).unwrap_or_default();
        if pats.is_empty() {
            if let Some(c) = cond {
                self.walk(c);
            }
            self.walk_field(node, "consequence");
            self.walk_field(node, "alternative");
            return;
        }
        if let Some(c) = cond {
            rust_walk_let_values(self, c);
        }
        if let Some(cons) = node.child_by_field_name("consequence") {
            self.push_scope(cons, ScopeKind::Block);
            for p in pats {
                self.rust_bind_pattern(p, cons.start_byte());
            }
            self.walk_children(cons);
            self.pop_scope();
        }
        self.walk_field(node, "alternative");
    }

    fn rust_walk_while(&mut self, node: Node<'_>) {
        let cond = node.child_by_field_name("condition");
        let pats = cond.map(|c| rust_let_patterns(c)).unwrap_or_default();
        if pats.is_empty() {
            if let Some(c) = cond {
                self.walk(c);
            }
            self.walk_field(node, "body");
            return;
        }
        if let Some(c) = cond {
            rust_walk_let_values(self, c);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.push_scope(body, ScopeKind::Block);
            for p in pats {
                self.rust_bind_pattern(p, body.start_byte());
            }
            self.walk_children(body);
            self.pop_scope();
        }
    }

    fn rust_bind_parameters(&mut self, node: Node<'_>) {
        match node.kind() {
            "parameters" | "closure_parameters" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.rust_bind_parameters(child);
                    }
                }
            }
            "parameter" => {
                self.walk_field(node, "type");
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    self.rust_bind_pattern(pattern, self.scopes[self.cur()].start);
                }
            }
            "self_parameter" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "self" {
                            self.define_new(child, Ns::Value, 0, BindingKind::Param);
                        }
                    }
                }
            }
            "identifier" => {
                self.define_new(node, Ns::Value, 0, BindingKind::Param);
            }
            _ => {
                if node.kind() != "type" {
                    self.rust_bind_pattern(node, self.scopes[self.cur()].start);
                }
            }
        }
    }

    fn rust_bind_pattern(&mut self, node: Node<'_>, visible_from: usize) {
        match node.kind() {
            "identifier" | "self" | "shorthand_field_identifier" => {
                self.define_new(node, Ns::Value, visible_from, BindingKind::Local);
            }
            "or_pattern" => {
                let mut first: HashMap<String, u32> = HashMap::new();
                self.rust_bind_or(node, visible_from, &mut first);
            }
            "tuple_struct_pattern" => {
                self.walk_field(node, "type");
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("type") {
                        continue;
                    }
                    if let Some(child) = node.child(i) {
                        self.rust_bind_pattern(child, visible_from);
                    }
                }
            }
            "struct_pattern" => {
                self.walk_field(node, "type");
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "field_pattern" {
                            self.rust_field_pattern(child, visible_from);
                        }
                    }
                }
            }
            "field_pattern" => self.rust_field_pattern(node, visible_from),
            "captured_pattern" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "identifier" {
                            self.define_new(child, Ns::Value, visible_from, BindingKind::Local);
                        } else if child.kind() != "@" {
                            self.rust_bind_pattern(child, visible_from);
                        }
                    }
                }
            }
            "match_pattern" => {
                for i in 0..node.child_count() {
                    if node.field_name_for_child(i) == Some("condition") {
                        continue;
                    }
                    if let Some(child) = node.child(i) {
                        self.rust_bind_pattern(child, visible_from);
                    }
                }
            }
            "remaining_field_pattern" | "_" => {}
            _ => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        if child.kind() == "type_identifier" || child.kind() == "field_identifier" {
                            continue;
                        }
                        self.rust_bind_pattern(child, visible_from);
                    }
                }
            }
        }
    }

    fn rust_bind_or(
        &mut self,
        node: Node<'_>,
        visible_from: usize,
        first: &mut HashMap<String, u32>,
    ) {
        if matches!(
            node.kind(),
            "identifier" | "self" | "shorthand_field_identifier"
        ) {
            let name = self.text(node);
            if skip_name(name) {
                return;
            }
            if let Some(&id) = first.get(name) {
                self.add_def(node, name, Ns::Value, id);
            } else if let Some(id) =
                self.define_new(node, Ns::Value, visible_from, BindingKind::Local)
            {
                first.insert(name.to_string(), id);
            }
            return;
        }
        if node.kind() == "field_pattern" {
            self.rust_field_pattern(node, visible_from);
            return;
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                self.rust_bind_or(child, visible_from, first);
            }
        }
    }

    fn rust_field_pattern(&mut self, node: Node<'_>, visible_from: usize) {
        if let Some(name) = node.child_by_field_name("name") {
            if name.kind() == "shorthand_field_identifier" {
                self.define_new(name, Ns::Value, visible_from, BindingKind::Local);
            }
        }
        if let Some(pattern) = node.child_by_field_name("pattern") {
            self.rust_bind_pattern(pattern, visible_from);
        }
    }

    fn rust_bind_use(&mut self, node: Node<'_>) {
        match node.kind() {
            "use_as_clause" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    self.define_new(alias, Ns::Value, 0, BindingKind::Local);
                }
            }
            "use_list" | "scoped_use_list" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        self.rust_bind_use(child);
                    }
                }
            }
            "scoped_identifier" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if name.kind() == "identifier" {
                        self.define_new(name, Ns::Value, 0, BindingKind::Local);
                    }
                }
            }
            "identifier" => {
                self.define_new(node, Ns::Value, 0, BindingKind::Local);
            }
            _ => {}
        }
    }

    fn rust_ignore_value_name(&self, node: Node<'_>) -> bool {
        if let Some(parent) = node.parent() {
            let field = child_field(parent, node);
            if parent.kind() == "field_expression" && field == Some("field") {
                return true;
            }
            if parent.kind() == "function_item" && field == Some("name") {
                return true;
            }
            if parent.kind() == "scoped_identifier" && field == Some("name") {
                return true;
            }
            if parent.kind() == "macro_invocation" && field == Some("macro") {
                return false;
            }
            if parent.kind() == "let_declaration" && field == Some("pattern") {
                return true;
            }
            if parent.kind() == "parameter" && field == Some("pattern") {
                return true;
            }
        }
        is_under_field(node, "let_declaration", "pattern")
            || is_under_field(node, "parameter", "pattern")
            || is_under_field(node, "for_expression", "pattern")
            || is_under_field(node, "match_arm", "pattern")
            || rust_inside_skipped_macro(node)
    }

    fn resolve_all(&mut self) {
        let mut implicit: HashMap<(String, Ns), u32> = HashMap::new();
        for i in 0..self.occs.len() {
            if self.occs[i].binding.is_some() {
                continue;
            }
            let name = self.occs[i].name.clone();
            let ns = self.occs[i].ns;
            let scope = self.occs[i].scope;
            let pos = self.occs[i].pos;
            let field_ref = self.occs[i].field_ref;
            let id = if field_ref {
                self.lookup_field(&name, scope)
            } else {
                self.lookup(&name, ns, scope, pos).or_else(|| {
                    let range = self.occs[i].range;
                    Some(*implicit.entry((name.clone(), ns)).or_insert_with(|| {
                        let id = self.bindings.len() as u32;
                        self.bindings.push(Binding {
                            name: name.clone(),
                            ns,
                            def: range,
                            scope: 0,
                            visible_from: 0,
                            kind: BindingKind::Local,
                            exported: true,
                        });
                        id
                    }))
                })
            };
            self.occs[i].binding = id;
        }
    }

    fn lookup(&self, name: &str, ns: Ns, from: usize, pos: usize) -> Option<u32> {
        match self.lang {
            Lang::Python => self.lookup_python(name, from),
            _ => self.lookup_sequential(name, ns, from, pos),
        }
    }

    fn lookup_python(&self, name: &str, from: usize) -> Option<u32> {
        let mut passed_function = false;
        let mut sid = Some(from);
        let mut first = true;
        while let Some(id) = sid {
            let scope = &self.scopes[id];
            if first && scope.kind == ScopeKind::Function {
                if scope.globals.contains(name) {
                    return self.binding_in_module(name, Ns::Value);
                }
                if scope.nonlocals.contains(name) {
                    sid = scope.parent;
                    first = false;
                    passed_function = true;
                    continue;
                }
            }
            match scope.kind {
                ScopeKind::Function => {
                    if let Some(b) = self.binding_in(id, name, Ns::Value, usize::MAX) {
                        return Some(b);
                    }
                    passed_function = true;
                }
                ScopeKind::Class => {
                    if !passed_function {
                        if let Some(b) = self.binding_in(id, name, Ns::Value, usize::MAX) {
                            return Some(b);
                        }
                    }
                }
                ScopeKind::Comprehension | ScopeKind::Module => {
                    if let Some(b) = self.binding_in(id, name, Ns::Value, usize::MAX) {
                        return Some(b);
                    }
                }
                ScopeKind::Block => {}
            }
            sid = scope.parent;
            first = false;
        }
        None
    }

    fn lookup_sequential(&self, name: &str, ns: Ns, from: usize, pos: usize) -> Option<u32> {
        let mut sid = Some(from);
        while let Some(id) = sid {
            if let Some(b) = self.binding_in(id, name, ns, pos) {
                return Some(b);
            }
            sid = self.scopes[id].parent;
        }
        None
    }

    fn lookup_field(&self, name: &str, from: usize) -> Option<u32> {
        let mut sid = Some(from);
        while let Some(id) = sid {
            if self.scopes[id].kind == ScopeKind::Class {
                if let Some(b) = self.binding_in(id, name, Ns::Value, usize::MAX) {
                    return Some(b);
                }
            }
            sid = self.scopes[id].parent;
        }
        None
    }

    fn binding_in_module(&self, name: &str, ns: Ns) -> Option<u32> {
        self.scopes
            .iter()
            .enumerate()
            .find(|(_, s)| s.kind == ScopeKind::Module && s.parent.is_none())
            .and_then(|(id, _)| self.binding_in(id, name, ns, usize::MAX))
    }

    fn binding_in(&self, scope: usize, name: &str, ns: Ns, pos: usize) -> Option<u32> {
        let mut best = None;
        for (i, b) in self.bindings.iter().enumerate() {
            if b.scope == scope && b.name == name && b.ns == ns && b.visible_from <= pos {
                best = Some(i as u32);
            }
        }
        best
    }
}

fn go_function_body(node: Node<'_>) -> bool {
    matches!(
        parent_kind(node),
        Some("function_declaration" | "method_declaration" | "func_literal")
    )
}

fn java_method_body(node: Node<'_>) -> bool {
    matches!(
        parent_kind(node),
        Some("method_declaration" | "constructor_declaration" | "compact_constructor_declaration")
    )
}

fn python_exported_name(name: &str) -> bool {
    let dunder = name.len() >= 4 && name.starts_with("__") && name.ends_with("__");
    dunder || !name.starts_with('_')
}

fn java_node_public(mut node: Node<'_>) -> bool {
    for _ in 0..8 {
        for i in 0..node.child_count() {
            let Some(child) = node.child(i) else {
                continue;
            };
            if child.kind() != "modifiers" {
                continue;
            }
            for j in 0..child.child_count() {
                if child.child(j).map(|c| c.kind()) == Some("public") {
                    return true;
                }
            }
        }
        match node.parent() {
            Some(p) => node = p,
            None => break,
        }
    }
    false
}

fn rust_has_pub_ancestor(mut node: Node<'_>) -> bool {
    for _ in 0..12 {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                if child.kind() == "visibility_modifier" {
                    return true;
                }
            }
        }
        match node.parent() {
            Some(p) => node = p,
            None => return false,
        }
    }
    false
}

fn rust_inside_skipped_macro(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if matches!(
            parent.kind(),
            "token_tree" | "token_tree_pattern" | "macro_definition"
        ) {
            return true;
        }
        node = parent;
    }
    false
}

fn rust_let_patterns<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    fn rec<'a>(n: Node<'a>, out: &mut Vec<Node<'a>>) {
        match n.kind() {
            "let_condition" => {
                if let Some(p) = n.child_by_field_name("pattern") {
                    out.push(p);
                }
            }
            "let_chain" => {
                for i in 0..n.child_count() {
                    if let Some(c) = n.child(i) {
                        rec(c, out);
                    }
                }
            }
            _ => {
                for i in 0..n.child_count() {
                    if let Some(c) = n.child(i) {
                        rec(c, out);
                    }
                }
            }
        }
    }
    rec(node, &mut out);
    out
}

fn rust_walk_let_values(analyzer: &mut Analyzer<'_>, node: Node<'_>) {
    match node.kind() {
        "let_condition" => {
            if let Some(v) = node.child_by_field_name("value") {
                analyzer.walk(v);
            }
        }
        _ => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i) {
                    rust_walk_let_values(analyzer, c);
                }
            }
        }
    }
}

/// Contract gap: `bindings` cannot say "these ranges are file-local; the
/// name is exported". A useful extension would be:
///
/// ```ignore
/// struct BindingSet {
///     ranges: Vec<ByteRange>,
///     /// True if other files may contain further references.
///     possibly_cross_file: bool,
/// }
/// ```
///
/// Do not change [`ScopeResolver`] from this workstream.
#[allow(dead_code)]
fn _contract_note() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::ScopeResolver;

    fn bind(path: &str, src: &str, at: ByteRange) -> Vec<ByteRange> {
        TreeSitterScopeResolver::new()
            .bindings(&RelPath::new(path), src, at)
            .unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn word(src: &str, name: &str, n: usize) -> ByteRange {
        let mut seen = 0usize;
        let mut i = 0usize;
        let bytes = src.as_bytes();
        let nb = name.as_bytes();
        while i + nb.len() <= bytes.len() {
            if &bytes[i..i + nb.len()] == nb && word_bound(bytes, i, i + nb.len()) {
                if seen == n {
                    return ByteRange {
                        start: i,
                        end: i + nb.len(),
                    };
                }
                seen += 1;
            }
            i += 1;
        }
        panic!("word {name:?} occurrence {n} not found in:\n{src}");
    }

    fn word_bound(bytes: &[u8], start: usize, end: usize) -> bool {
        let before = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after = end >= bytes.len() || !is_ident_byte(bytes[end]);
        before && after
    }

    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }

    fn starts(ranges: &[ByteRange]) -> Vec<usize> {
        ranges.iter().map(|r| r.start).collect()
    }

    fn contains_range(got: &[ByteRange], needle: ByteRange) -> bool {
        got.contains(&needle)
    }

    #[test]
    fn js_is_not_this_resolver() {
        let err = TreeSitterScopeResolver::new()
            .bindings(
                &RelPath::new("a.ts"),
                "let x = 1;",
                word("let x = 1;", "x", 0),
            )
            .unwrap_err();
        assert!(matches!(err, RewriteError::NoResolver(_)));
    }

    #[test]
    fn evidence_is_scope_binding() {
        assert_eq!(
            TreeSitterScopeResolver::new().evidence(),
            Evidence::ScopeBinding
        );
    }

    // ------------------------------------------------------------------ Python

    #[test]
    fn python_normal_binding() {
        let src = "def f(alpha):\n    beta = alpha\n    return beta\n";
        let got = bind("a.py", src, word(src, "alpha", 0));
        assert_eq!(
            starts(&got),
            vec![word(src, "alpha", 0).start, word(src, "alpha", 1).start]
        );
        let got = bind("a.py", src, word(src, "beta", 0));
        assert_eq!(
            starts(&got),
            vec![word(src, "beta", 0).start, word(src, "beta", 1).start]
        );
    }

    #[test]
    fn python_nested_function_shadowing() {
        let src =
            "def f():\n    x = 1\n    def g():\n        x = 2\n        return x\n    return x\n";
        let outer = bind("a.py", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        assert!(contains_range(&outer, word(src, "x", 3)));
        assert!(!contains_range(&outer, word(src, "x", 1)));
        assert!(!contains_range(&outer, word(src, "x", 2)));
        let inner = bind("a.py", src, word(src, "x", 1));
        assert!(contains_range(&inner, word(src, "x", 1)));
        assert!(contains_range(&inner, word(src, "x", 2)));
        assert!(!contains_range(&inner, word(src, "x", 0)));
        assert!(!contains_range(&inner, word(src, "x", 3)));
    }

    #[test]
    fn python_attribute_is_not_the_variable() {
        let src = "x = 1\nobj.x = 2\nprint(x)\n";
        let got = bind("a.py", src, word(src, "x", 0));
        let field = ByteRange {
            start: src.find("obj.x").unwrap() + 4,
            end: src.find("obj.x").unwrap() + 5,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(!contains_range(&got, field));
    }

    #[test]
    fn python_string_and_comment_are_not_bindings() {
        let src = "x = 1\ns = \"x\"\n# x\nprint(x)\n";
        let got = bind("a.py", src, word(src, "x", 0));
        let in_string = ByteRange {
            start: src.find("\"x\"").unwrap() + 1,
            end: src.find("\"x\"").unwrap() + 2,
        };
        let in_comment = ByteRange {
            start: src.find("# x").unwrap() + 2,
            end: src.find("# x").unwrap() + 3,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 3)));
        assert!(!contains_range(&got, in_string));
        assert!(!contains_range(&got, in_comment));
    }

    #[test]
    fn python_global_binds_module_name() {
        let src = "x = 1\ndef f():\n    global x\n    x = 2\ndef g():\n    x = 3\n    return x\nprint(x)\n";
        let got = bind("a.py", src, word(src, "x", 0));
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 1)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(contains_range(&got, word(src, "x", 5)));
        assert!(!contains_range(&got, word(src, "x", 3)));
        assert!(!contains_range(&got, word(src, "x", 4)));
        let local = bind("a.py", src, word(src, "x", 3));
        assert!(contains_range(&local, word(src, "x", 3)));
        assert!(contains_range(&local, word(src, "x", 4)));
        assert!(!contains_range(&local, word(src, "x", 0)));
    }

    #[test]
    fn python_nonlocal_binds_enclosing_function() {
        let src = "def outer():\n    x = 1\n    def inner():\n        nonlocal x\n        x = 2\n    return x\n";
        let got = bind("a.py", src, word(src, "x", 0));
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 1)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(contains_range(&got, word(src, "x", 3)));
    }

    #[test]
    fn python_comprehension_has_its_own_iterator() {
        let src = "x = 1\nys = [x for x in xs]\nprint(x)\n";
        let outer = bind("a.py", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        assert!(contains_range(&outer, word(src, "x", 3)));
        assert!(!contains_range(&outer, word(src, "x", 1)));
        assert!(!contains_range(&outer, word(src, "x", 2)));
        let inner = bind("a.py", src, word(src, "x", 1));
        assert!(contains_range(&inner, word(src, "x", 1)));
        assert!(contains_range(&inner, word(src, "x", 2)));
        assert!(!contains_range(&inner, word(src, "x", 0)));
    }

    // --------------------------------------------------------------------- Go

    #[test]
    fn go_normal_binding() {
        let src = "package p\nfunc f(alpha int) int {\n\tbeta := alpha\n\treturn beta\n}\n";
        let got = bind("a.go", src, word(src, "alpha", 0));
        assert_eq!(
            starts(&got),
            vec![word(src, "alpha", 0).start, word(src, "alpha", 1).start]
        );
    }

    #[test]
    fn go_block_shadowing() {
        let src = "package p\nfunc f() {\n\tx := 1\n\t{\n\t\tx := 2\n\t\t_ = x\n\t}\n\t_ = x\n}\n";
        let outer = bind("a.go", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        assert!(contains_range(&outer, word(src, "x", 3)));
        assert!(!contains_range(&outer, word(src, "x", 1)));
        assert!(!contains_range(&outer, word(src, "x", 2)));
        let inner = bind("a.go", src, word(src, "x", 1));
        assert!(contains_range(&inner, word(src, "x", 1)));
        assert!(contains_range(&inner, word(src, "x", 2)));
        assert!(!contains_range(&inner, word(src, "x", 0)));
    }

    #[test]
    fn go_selector_field_is_not_the_variable() {
        let src = "package p\nfunc f() {\n\tx := 1\n\t_ = obj.x\n\t_ = x\n}\n";
        let got = bind("a.go", src, word(src, "x", 0));
        let field = ByteRange {
            start: src.find("obj.x").unwrap() + 4,
            end: src.find("obj.x").unwrap() + 5,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(!contains_range(&got, field));
    }

    #[test]
    fn go_string_and_comment_are_not_bindings() {
        let src = "package p\nfunc f() {\n\tx := 1\n\t_ = \"x\"\n\t// x\n\t_ = x\n}\n";
        let got = bind("a.go", src, word(src, "x", 0));
        let in_string = ByteRange {
            start: src.find("\"x\"").unwrap() + 1,
            end: src.find("\"x\"").unwrap() + 2,
        };
        let in_comment = ByteRange {
            start: src.find("// x").unwrap() + 3,
            end: src.find("// x").unwrap() + 4,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 3)));
        assert!(!contains_range(&got, in_string));
        assert!(!contains_range(&got, in_comment));
    }

    #[test]
    fn go_short_decl_and_receiver() {
        let src = "package p\nfunc (s *Srv) Run() {\n\tx := 1\n\tx, y := 2, 3\n\t_ = s\n\t_ = x\n\t_ = y\n}\n";
        let recv = bind("a.go", src, word(src, "s", 0));
        assert!(contains_range(&recv, word(src, "s", 0)));
        assert!(contains_range(&recv, word(src, "s", 1)));
        let x = bind("a.go", src, word(src, "x", 0));
        assert!(contains_range(&x, word(src, "x", 0)));
        assert!(contains_range(&x, word(src, "x", 1)));
        assert!(contains_range(&x, word(src, "x", 2)));
        assert!(!contains_range(&x, word(src, "y", 0)));
    }

    // -------------------------------------------------------------------- Java

    #[test]
    fn java_normal_binding() {
        let src =
            "class C {\n  void f(int alpha) {\n    int beta = alpha;\n    return beta;\n  }\n}\n";
        let got = bind("C.java", src, word(src, "alpha", 0));
        assert_eq!(
            starts(&got),
            vec![word(src, "alpha", 0).start, word(src, "alpha", 1).start]
        );
    }

    #[test]
    fn java_inner_class_shadowing() {
        let src = "class O {\n  void f() {\n    int x = 1;\n    class I {\n      int x = 2;\n      void g() { System.out.println(x); }\n    }\n    System.out.println(x);\n  }\n}\n";
        let outer = bind("O.java", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        assert!(contains_range(&outer, word(src, "x", 3)));
        assert!(!contains_range(&outer, word(src, "x", 1)));
        assert!(!contains_range(&outer, word(src, "x", 2)));
        let field = bind("O.java", src, word(src, "x", 1));
        assert!(contains_range(&field, word(src, "x", 1)));
        assert!(contains_range(&field, word(src, "x", 2)));
        assert!(!contains_range(&field, word(src, "x", 0)));
    }

    #[test]
    fn java_field_access_is_not_the_local() {
        let src = "class C {\n  void f() {\n    int x = 1;\n    obj.x = 2;\n    System.out.println(x);\n  }\n}\n";
        let got = bind("C.java", src, word(src, "x", 0));
        let field = ByteRange {
            start: src.find("obj.x").unwrap() + 4,
            end: src.find("obj.x").unwrap() + 5,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(!contains_range(&got, field));
    }

    #[test]
    fn java_string_and_comment_are_not_bindings() {
        let src = "class C {\n  void f() {\n    int x = 1;\n    String s = \"x\";\n    // x\n    System.out.println(x);\n  }\n}\n";
        let got = bind("C.java", src, word(src, "x", 0));
        let in_string = ByteRange {
            start: src.find("\"x\"").unwrap() + 1,
            end: src.find("\"x\"").unwrap() + 2,
        };
        let in_comment = ByteRange {
            start: src.find("// x").unwrap() + 3,
            end: src.find("// x").unwrap() + 4,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 3)));
        assert!(!contains_range(&got, in_string));
        assert!(!contains_range(&got, in_comment));
    }

    #[test]
    fn java_field_vs_local_same_name() {
        let src = "class C {\n  int x;\n  void f() {\n    int x = 1;\n    System.out.println(x);\n  }\n  void g() {\n    System.out.println(x);\n  }\n}\n";
        let local = bind("C.java", src, word(src, "x", 1));
        assert!(contains_range(&local, word(src, "x", 1)));
        assert!(contains_range(&local, word(src, "x", 2)));
        assert!(!contains_range(&local, word(src, "x", 0)));
        assert!(!contains_range(&local, word(src, "x", 3)));
        let field = bind("C.java", src, word(src, "x", 0));
        assert!(contains_range(&field, word(src, "x", 0)));
        assert!(contains_range(&field, word(src, "x", 3)));
        assert!(!contains_range(&field, word(src, "x", 1)));
        assert!(!contains_range(&field, word(src, "x", 2)));
    }

    // -------------------------------------------------------------------- Rust

    #[test]
    fn rust_normal_binding() {
        let src = "fn f(alpha: i32) -> i32 {\n    let beta = alpha;\n    beta\n}\n";
        let got = bind("a.rs", src, word(src, "alpha", 0));
        assert_eq!(
            starts(&got),
            vec![word(src, "alpha", 0).start, word(src, "alpha", 1).start]
        );
    }

    #[test]
    fn rust_block_shadowing() {
        let src = "fn f() {\n    let x = 1;\n    {\n        let x = 2;\n        let _ = x;\n    }\n    let _ = x;\n}\n";
        let outer = bind("a.rs", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        assert!(contains_range(&outer, word(src, "x", 3)));
        assert!(!contains_range(&outer, word(src, "x", 1)));
        assert!(!contains_range(&outer, word(src, "x", 2)));
    }

    #[test]
    fn rust_field_expression_is_not_the_variable() {
        let src = "fn f() {\n    let x = 1;\n    let _ = obj.x;\n    let _ = x;\n}\n";
        let got = bind("a.rs", src, word(src, "x", 0));
        let field = ByteRange {
            start: src.find("obj.x").unwrap() + 4,
            end: src.find("obj.x").unwrap() + 5,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 2)));
        assert!(!contains_range(&got, field));
    }

    #[test]
    fn rust_string_and_comment_are_not_bindings() {
        let src = "fn f() {\n    let x = 1;\n    let _ = \"x\";\n    // x\n    let _ = x;\n}\n";
        let got = bind("a.rs", src, word(src, "x", 0));
        let in_string = ByteRange {
            start: src.find("\"x\"").unwrap() + 1,
            end: src.find("\"x\"").unwrap() + 2,
        };
        let in_comment = ByteRange {
            start: src.find("// x").unwrap() + 3,
            end: src.find("// x").unwrap() + 4,
        };
        assert!(contains_range(&got, word(src, "x", 0)));
        assert!(contains_range(&got, word(src, "x", 3)));
        assert!(!contains_range(&got, in_string));
        assert!(!contains_range(&got, in_comment));
    }

    #[test]
    fn rust_let_shadowing_in_the_same_block() {
        let src = "fn f() {\n    let x = 1;\n    let y = x;\n    let x = 2;\n    let z = x;\n}\n";
        let first = bind("a.rs", src, word(src, "x", 0));
        assert!(contains_range(&first, word(src, "x", 0)));
        assert!(contains_range(&first, word(src, "x", 1)));
        assert!(!contains_range(&first, word(src, "x", 2)));
        assert!(!contains_range(&first, word(src, "x", 3)));
        let second = bind("a.rs", src, word(src, "x", 2));
        assert!(contains_range(&second, word(src, "x", 2)));
        assert!(contains_range(&second, word(src, "x", 3)));
        assert!(!contains_range(&second, word(src, "x", 0)));
        assert!(!contains_range(&second, word(src, "x", 1)));
        let rhs = bind("a.rs", src, word(src, "x", 1));
        assert_eq!(starts(&rhs), starts(&first));
        let src = "fn f() {\n    let x = 1;\n    let x = x;\n}\n";
        let first = bind("a.rs", src, word(src, "x", 0));
        assert!(contains_range(&first, word(src, "x", 0)));
        assert!(contains_range(&first, word(src, "x", 2)));
        assert!(!contains_range(&first, word(src, "x", 1)));
        let second = bind("a.rs", src, word(src, "x", 1));
        assert!(contains_range(&second, word(src, "x", 1)));
        assert!(!contains_range(&second, word(src, "x", 0)));
        assert!(!contains_range(&second, word(src, "x", 2)));
    }

    #[test]
    fn rust_pattern_binding_and_macro_body_skipped() {
        let src = "fn f(opt: Option<i32>) {\n    let x = 1;\n    match opt {\n        Some(x) => x,\n        None => 0,\n    };\n    println!(\"{}\", x);\n}\n";
        let outer = bind("a.rs", src, word(src, "x", 0));
        assert!(contains_range(&outer, word(src, "x", 0)));
        let match_x = word(src, "x", 1);
        let match_use = word(src, "x", 2);
        assert!(!contains_range(&outer, match_x));
        assert!(!contains_range(&outer, match_use));
        let arm = bind("a.rs", src, match_x);
        assert!(contains_range(&arm, match_x));
        assert!(contains_range(&arm, match_use));
        assert!(!contains_range(&arm, word(src, "x", 0)));
        let println_x = src.rfind(", x)").map(|i| ByteRange {
            start: i + 2,
            end: i + 3,
        });
        if let Some(px) = println_x {
            assert!(
                !contains_range(&outer, px),
                "identifiers inside macro token trees must not be treated as the local"
            );
        }
    }
}
