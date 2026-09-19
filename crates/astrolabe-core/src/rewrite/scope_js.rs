//! JavaScript / TypeScript scope-aware binding resolution.
//!
//! `ast-grep` matches text and syntax, not symbols. A safe rename has to know
//! which occurrences of a name are the *same* declaration — including uses
//! captured by a closure, and excluding a shadowed inner `const` or an
//! `obj.x` property that just happens to share the spelling.
//!
//! [`oxc_semantic`] already builds that table (scopes, symbols, references)
//! without running `tsc`. This resolver maps a byte range onto one symbol and
//! returns every identifier span bound to it, declaration included.
//!
//! Parse failures surface as [`RewriteError::Io`]: the rewrite error enum has
//! no dedicated parse variant, and an `Ok(vec![])` would be read as "this
//! identifier has no references".

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::{Semantic, SemanticBuilder, SymbolId};
use oxc_span::{SourceType, Span};

use super::{ByteRange, Evidence, RewriteError, ScopeResolver};
use crate::types::RelPath;

/// Resolves which identifier occurrences in a JS/TS file bind to one declaration.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsScopeResolver;

impl JsScopeResolver {
    pub fn new() -> Self {
        Self
    }
}

impl ScopeResolver for JsScopeResolver {
    fn bindings(
        &self,
        path: &RelPath,
        source: &str,
        at: ByteRange,
    ) -> Result<Vec<ByteRange>, RewriteError> {
        let source_type = SourceType::from_path(Path::new(path.as_str()))
            .map_err(|err| RewriteError::NoResolver(err.to_string()))?;

        let Some(query) = span_from_range(at) else {
            return Ok(Vec::new());
        };

        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, source_type).parse();
        if parsed.fatal_error || parsed.diagnostics.has_errors() {
            return Err(parse_failed(path, &parsed));
        }

        // Reference spans live on the AST node store, which is off by default.
        // Class-member names (methods, getters/setters, private fields) are
        // not lexical symbols; they sit in the class table, also off by default.
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .with_class_table(true)
            .build(&parsed.program)
            .semantic;

        match binding_at(&semantic, query) {
            None => Ok(Vec::new()),
            Some(Binding::Symbol(id)) => Ok(ranges_for_symbol(&semantic, id)),
            Some(Binding::ClassMember(ranges)) => Ok(sorted_unique(ranges)),
        }
    }

    fn evidence(&self) -> Evidence {
        Evidence::ScopeBinding
    }
}

enum Binding {
    Symbol(SymbolId),
    ClassMember(Vec<ByteRange>),
}

fn parse_failed(path: &RelPath, parsed: &oxc_parser::ParserReturn<'_>) -> RewriteError {
    let message = parsed.diagnostics.errors().next().map_or_else(
        || "JavaScript/TypeScript parse failed".to_string(),
        ToString::to_string,
    );
    RewriteError::Io(path.clone(), message)
}

fn span_from_range(at: ByteRange) -> Option<Span> {
    let start = u32::try_from(at.start).ok()?;
    let end = u32::try_from(at.end).ok()?;
    if start > end {
        return None;
    }
    Some(Span::new(start, end))
}

fn byte_range(span: Span) -> ByteRange {
    ByteRange {
        start: span.start as usize,
        end: span.end as usize,
    }
}

fn span_hits_query(span: Span, query: Span) -> bool {
    if span.is_empty() {
        return false;
    }
    if query.is_empty() {
        // A caret on or immediately after the identifier still counts.
        span.start <= query.start && query.start <= span.end
    } else {
        span.contains_inclusive(query) || query.contains_inclusive(span)
    }
}

fn tighter(candidate: Span, current: Span) -> bool {
    match candidate.size().cmp(&current.size()) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => candidate.start < current.start,
        std::cmp::Ordering::Greater => false,
    }
}

fn sorted_unique(mut ranges: Vec<ByteRange>) -> Vec<ByteRange> {
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

fn ranges_for_symbol(semantic: &Semantic<'_>, symbol_id: SymbolId) -> Vec<ByteRange> {
    let scoping = semantic.scoping();
    let mut ranges = Vec::new();
    ranges.push(byte_range(scoping.symbol_span(symbol_id)));
    for redeclaration in scoping.symbol_redeclarations(symbol_id) {
        ranges.push(byte_range(redeclaration.span));
    }
    for reference in semantic.symbol_references(symbol_id) {
        ranges.push(byte_range(semantic.reference_span(reference)));
    }
    sorted_unique(ranges)
}

fn binding_at(semantic: &Semantic<'_>, query: Span) -> Option<Binding> {
    let mut best_symbol: Option<(Span, SymbolId)> = None;
    let scoping = semantic.scoping();
    for symbol_id in scoping.symbol_ids() {
        consider_span(
            &mut best_symbol,
            scoping.symbol_span(symbol_id),
            symbol_id,
            query,
        );
        for redeclaration in scoping.symbol_redeclarations(symbol_id) {
            consider_span(&mut best_symbol, redeclaration.span, symbol_id, query);
        }
        for reference in semantic.symbol_references(symbol_id) {
            consider_span(
                &mut best_symbol,
                semantic.reference_span(reference),
                symbol_id,
                query,
            );
        }
    }

    let mut best_member: Option<(Span, Vec<ByteRange>)> = None;
    let classes = semantic.classes();
    for (class_id, elements) in classes.elements.iter_enumerated() {
        for element in elements.iter() {
            if !span_hits_query(element.span, query) {
                continue;
            }
            let name = element.name.as_ref();
            let is_private = element.is_private;
            let mut ranges = Vec::new();
            for other in elements.iter() {
                if other.name.as_ref() == name && other.is_private == is_private {
                    ranges.push(byte_range(other.span));
                }
            }
            if is_private {
                for pref in classes.iter_private_identifiers(class_id) {
                    if pref.name.as_str() == name {
                        ranges.push(byte_range(pref.span));
                    }
                }
            }
            consider_member(&mut best_member, element.span, ranges);
        }
        for reference in classes.iter_private_identifiers(class_id) {
            if !span_hits_query(reference.span, query) {
                continue;
            }
            let name = reference.name.as_str();
            let mut ranges = Vec::new();
            for other in elements.iter() {
                if other.name.as_ref() == name && other.is_private {
                    ranges.push(byte_range(other.span));
                }
            }
            for pref in classes.iter_private_identifiers(class_id) {
                if pref.name.as_str() == name {
                    ranges.push(byte_range(pref.span));
                }
            }
            consider_member(&mut best_member, reference.span, ranges);
        }
    }

    match (best_symbol, best_member) {
        (None, None) => None,
        (Some((_, id)), None) => Some(Binding::Symbol(id)),
        (None, Some((_, ranges))) => Some(Binding::ClassMember(ranges)),
        (Some((sym_span, id)), Some((mem_span, ranges))) => {
            if tighter(mem_span, sym_span) {
                Some(Binding::ClassMember(ranges))
            } else {
                Some(Binding::Symbol(id))
            }
        }
    }
}

fn consider_span(
    best: &mut Option<(Span, SymbolId)>,
    span: Span,
    symbol_id: SymbolId,
    query: Span,
) {
    if !span_hits_query(span, query) {
        return;
    }
    if best.is_none_or(|(current, _)| tighter(span, current)) {
        *best = Some((span, symbol_id));
    }
}

fn consider_member(best: &mut Option<(Span, Vec<ByteRange>)>, span: Span, ranges: Vec<ByteRange>) {
    if best
        .as_ref()
        .is_none_or(|(current, _)| tighter(span, *current))
    {
        *best = Some((span, ranges));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(path: &str, source: &str, at: ByteRange) -> Vec<ByteRange> {
        JsScopeResolver
            .bindings(&RelPath::new(path), source, at)
            .unwrap_or_else(|err| panic!("bindings failed: {err}"))
    }

    /// `name` immediately after a unique `prefix` in `source`.
    fn ident(source: &str, prefix: &str, name: &str) -> ByteRange {
        let start = source
            .find(prefix)
            .unwrap_or_else(|| panic!("prefix not found: {prefix:?}"))
            + prefix.len();
        let end = start + name.len();
        assert_eq!(
            source.get(start..end),
            Some(name),
            "expected {name:?} after {prefix:?}"
        );
        ByteRange { start, end }
    }

    fn names<'a>(source: &'a str, ranges: &[ByteRange]) -> Vec<&'a str> {
        ranges
            .iter()
            .map(|range| &source[range.start..range.end])
            .collect()
    }

    fn assert_sorted(ranges: &[ByteRange]) {
        assert!(
            ranges.windows(2).all(|pair| pair[0] <= pair[1]),
            "ranges must be sorted: {ranges:?}"
        );
    }

    #[test]
    fn evidence_is_scope_binding() {
        assert_eq!(JsScopeResolver.evidence(), Evidence::ScopeBinding);
    }

    #[test]
    fn shadowing_keeps_inner_and_outer_apart() {
        let source = "const x = 1;\nfunction f() { const x = 2; return x; }";
        let outer = ident(source, "const ", "x");
        let inner_decl = ident(source, "function f() { const ", "x");
        let inner_use = ident(source, "return ", "x");

        let outer_hits = bind("shadow.js", source, outer);
        assert_eq!(outer_hits, vec![outer]);
        assert_sorted(&outer_hits);

        let inner_hits = bind("shadow.js", source, inner_use);
        assert_eq!(inner_hits, vec![inner_decl, inner_use]);
        assert_eq!(bind("shadow.js", source, inner_decl), inner_hits);
    }

    #[test]
    fn closure_captures_the_outer_binding() {
        let source = "const x = 1;\nfunction f() { return x; }";
        let decl = ident(source, "const ", "x");
        let captured = ident(source, "return ", "x");
        let hits = bind("closure.js", source, decl);
        assert_eq!(hits, vec![decl, captured]);
        assert_eq!(bind("closure.js", source, captured), hits);
    }

    #[test]
    fn var_is_function_scoped_let_is_block_scoped() {
        let source = concat!(
            "function f() {\n",
            "  if (true) { var x = 1; let y = 2; }\n",
            "  return x;\n",
            "}\n",
        );
        let var_decl = ident(source, "var ", "x");
        let var_use = ident(source, "return ", "x");
        let let_decl = ident(source, "let ", "y");

        let var_hits = bind("scope.js", source, var_decl);
        assert_eq!(var_hits, vec![var_decl, var_use]);

        let let_hits = bind("scope.js", source, let_decl);
        assert_eq!(let_hits, vec![let_decl]);
    }

    #[test]
    fn function_params_destructuring_and_defaults() {
        let source = "function f(x = 1, { a: b, c } = {}) { return x + b + c; }";
        let x_param = ident(source, "function f(", "x");
        let x_use = ident(source, "return ", "x");
        assert_eq!(bind("params.js", source, x_param), vec![x_param, x_use]);

        let b_bind = ident(source, "{ a: ", "b");
        let b_use = ident(source, "return x + ", "b");
        assert_eq!(bind("params.js", source, b_bind), vec![b_bind, b_use]);

        // `a` is the property name of the pattern, not a binding.
        let a_prop = ident(source, "{ ", "a");
        assert!(bind("params.js", source, a_prop).is_empty());

        let c_bind = ident(source, "a: b, ", "c");
        let c_use = ident(source, "x + b + ", "c");
        assert_eq!(bind("params.js", source, c_bind), vec![c_bind, c_use]);
    }

    #[test]
    fn class_name_methods_getters_setters_and_private_fields() {
        let source = concat!(
            "class Point {\n",
            "  #x = 1;\n",
            "  constructor(x) { this.#x = x; }\n",
            "  getX() { return this.#x; }\n",
            "  get x() { return this.#x; }\n",
            "  set x(v) { this.#x = v; }\n",
            "}\n",
            "new Point();\n",
        );
        let class_name = ident(source, "class ", "Point");
        let class_use = ident(source, "new ", "Point");
        assert_eq!(
            bind("point.js", source, class_name),
            vec![class_name, class_use]
        );

        let ctor_param = ident(source, "constructor(", "x");
        let ctor_use = ident(source, "this.#x = ", "x");
        assert_eq!(
            bind("point.js", source, ctor_param),
            vec![ctor_param, ctor_use]
        );

        let method = ident(source, "x; }\n  ", "getX");
        assert_eq!(bind("point.js", source, method), vec![method]);

        let getter = ident(source, "get ", "x");
        let setter = ident(source, "set ", "x");
        let accessor_hits = bind("point.js", source, getter);
        assert_eq!(accessor_hits, vec![getter, setter]);
        assert_eq!(bind("point.js", source, setter), accessor_hits);

        let private_decl = ident(source, "  ", "#x");
        let private_hits = bind("point.js", source, private_decl);
        assert!(private_hits.contains(&private_decl));
        assert!(private_hits.len() >= 2);
        assert!(names(source, &private_hits).iter().all(|n| *n == "#x"));
        assert_sorted(&private_hits);
    }

    #[test]
    fn import_alias_is_the_local_symbol() {
        let source = "import { a as b } from './m';\nexport function f() { return b; }\n";
        let local = ident(source, "a as ", "b");
        let use_site = ident(source, "return ", "b");
        assert_eq!(bind("mod.js", source, local), vec![local, use_site]);

        // Imported name `a` is not a local binding.
        let imported = ident(source, "import { ", "a");
        assert!(bind("mod.js", source, imported).is_empty());
    }

    #[test]
    fn export_reexport_binds_the_local_name() {
        let source = "const a = 1;\nexport { a as b };\n";
        let decl = ident(source, "const ", "a");
        let exported = ident(source, "export { ", "a");
        assert_eq!(bind("exp.js", source, decl), vec![decl, exported]);

        let as_name = ident(source, "a as ", "b");
        assert!(bind("exp.js", source, as_name).is_empty());
    }

    #[test]
    fn typescript_types_interfaces_enums_generics_and_declare() {
        let source = concat!(
            "declare const ready: number;\n",
            "type Foo = number;\n",
            "interface Bar { n: Foo }\n",
            "enum Color { Red = 1 }\n",
            "function id<T>(value: T): T { return value; }\n",
            "function use(x: Foo, y: Bar, c: Color): Foo { return x; }\n",
            "const n = ready;\n",
            "const c = Color.Red;\n",
        );

        let foo_alias = ident(source, "type ", "Foo");
        let foo_in_iface = ident(source, "n: ", "Foo");
        let foo_param = ident(source, "use(x: ", "Foo");
        let foo_ret = ident(source, "y: Bar, c: Color): ", "Foo");
        let foo_hits = bind("types.ts", source, foo_alias);
        assert_eq!(foo_hits, vec![foo_alias, foo_in_iface, foo_param, foo_ret]);

        let bar_iface = ident(source, "interface ", "Bar");
        let bar_ann = ident(source, "x: Foo, y: ", "Bar");
        assert_eq!(
            bind("types.ts", source, bar_iface),
            vec![bar_iface, bar_ann]
        );

        let color_enum = ident(source, "enum ", "Color");
        let color_ann = ident(source, "y: Bar, c: ", "Color");
        let color_use = ident(source, "const c = ", "Color");
        assert_eq!(
            bind("types.ts", source, color_enum),
            vec![color_enum, color_ann, color_use]
        );

        let t_param = ident(source, "id<", "T");
        let t_arg = ident(source, "value: ", "T");
        let t_ret = ident(source, "value: T): ", "T");
        assert_eq!(
            bind("types.ts", source, t_param),
            vec![t_param, t_arg, t_ret]
        );

        let ready_decl = ident(source, "declare const ", "ready");
        let ready_use = ident(source, "const n = ", "ready");
        assert_eq!(
            bind("types.ts", source, ready_decl),
            vec![ready_decl, ready_use]
        );

        let value_param = ident(source, "id<T>(", "value");
        let value_ret = ident(source, "return ", "value");
        assert_eq!(
            bind("types.ts", source, value_param),
            vec![value_param, value_ret]
        );
    }

    #[test]
    fn property_name_is_not_the_variable() {
        let source = "const x = 1;\nobj.x = 2;\nconst o = { x };\n";
        let decl = ident(source, "const ", "x");
        let shorthand = ident(source, "{ ", "x");
        let prop = ident(source, "obj.", "x");

        let hits = bind("prop.js", source, decl);
        assert_eq!(hits, vec![decl, shorthand]);
        assert!(!hits.contains(&prop));
        assert!(bind("prop.js", source, prop).is_empty());
    }

    #[test]
    fn strings_and_comments_are_not_bindings() {
        let source = "const x = 1; const s = \"x\"; // x\n";
        let decl = ident(source, "const ", "x");
        let in_string = ident(source, "const s = \"", "x");
        let in_comment = ident(source, "// ", "x");

        assert_eq!(bind("text.js", source, decl), vec![decl]);
        assert!(bind("text.js", source, in_string).is_empty());
        assert!(bind("text.js", source, in_comment).is_empty());
    }

    #[test]
    fn jsx_and_tsx_see_identifiers_in_braces() {
        let jsx = "const n = 1; export const C = () => <div>{n}</div>;";
        let n_js = ident(jsx, "const ", "n");
        let n_jsx = ident(jsx, "<div>{", "n");
        assert_eq!(bind("app.jsx", jsx, n_js), vec![n_js, n_jsx]);

        let tsx = "const n: number = 1; export const C = () => <div>{n}</div>;";
        let n_ts = ident(tsx, "const ", "n");
        let n_tsx = ident(tsx, "<div>{", "n");
        assert_eq!(bind("app.tsx", tsx, n_ts), vec![n_ts, n_tsx]);
    }

    #[test]
    fn mts_and_cts_source_types_parse() {
        let source = "export const x = 1; const y = x;";
        let decl = ident(source, "export const ", "x");
        let use_site = ident(source, "const y = ", "x");
        assert_eq!(bind("a.mts", source, decl), vec![decl, use_site]);
        assert_eq!(bind("a.cts", source, decl), vec![decl, use_site]);
    }

    #[test]
    fn caret_inside_identifier_resolves() {
        let source = "const value = 1; console.log(value);";
        let decl = ident(source, "const ", "value");
        let caret = ByteRange {
            start: decl.start + 2,
            end: decl.start + 2,
        };
        let hits = bind("caret.js", source, caret);
        assert_eq!(hits, bind("caret.js", source, decl));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn syntax_error_is_err_not_empty() {
        let source = "function ( {";
        let at = ByteRange { start: 0, end: 8 };
        match JsScopeResolver.bindings(&RelPath::new("broken.js"), source, at) {
            Err(RewriteError::Io(path, message)) => {
                assert_eq!(path.as_str(), "broken.js");
                assert!(!message.is_empty());
            }
            Ok(ranges) => panic!("parse failure must not look like zero refs: {ranges:?}"),
            Err(other) => panic!("expected RewriteError::Io, got {other:?}"),
        }
    }

    #[test]
    fn unknown_extension_is_no_resolver() {
        match JsScopeResolver.bindings(
            &RelPath::new("notes.md"),
            "const x = 1;",
            ByteRange { start: 6, end: 7 },
        ) {
            Err(RewriteError::NoResolver(_)) => {}
            other => panic!("expected NoResolver, got {other:?}"),
        }
    }

    #[test]
    fn unicode_identifier_uses_byte_offsets() {
        let source = "const 名称 = 1; console.log(名称);";
        let decl = ident(source, "const ", "名称");
        let use_site = ident(source, "console.log(", "名称");
        assert_eq!(bind("zh.js", source, decl), vec![decl, use_site]);
        assert_eq!(&source[decl.start..decl.end], "名称");
    }
}
