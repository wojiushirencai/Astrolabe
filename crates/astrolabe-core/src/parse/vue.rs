//! Vue single-file component indexing via embedded `<script>` extraction.
//!
//! tree-sitter-vue is ABI-incompatible with the workspace tree-sitter 0.27
//! pin, so P0 follows the OpenVisio-shaped approach: peel `<script>` /
//! `<script setup>` blocks out of the SFC and reuse the JS/TS/TSX parsers.
//! Template/style symbols are deferred; Volar (`vue-language-server`) covers
//! those via LSP as a **single** process (see `lsp::discovery` `VUE` candidates).
//!
//! # Gaps / follow-ups
//!
//! - No template/custom-block symbol extraction yet.
//! - Dual-server Volar + `typescript-language-server` with `@vue/typescript-plugin`
//!   (Serena `hybridMode`) is **TODO** — not a P0 fallback.

use crate::types::{CodeSymbol, FileId, Language, RelPath, SymbolId, SymbolKind};

use super::{ParseError, ParsedFile, ParserPool};

/// Parse a `.vue` SFC by extracting script blocks and delegating to JS/TS.
pub(crate) fn parse_sfc(
    pool: &ParserPool,
    path: &RelPath,
    source: &str,
) -> Result<ParsedFile, ParseError> {
    let mut symbols = Vec::new();
    let mut imports = Vec::new();
    let mut calls = Vec::new();

    let component = component_name(path);
    let total_lines = line_number(source.lines().count().saturating_sub(1));
    symbols.push(CodeSymbol {
        id: SymbolId(0),
        file: FileId(0),
        name: component.clone(),
        kind: SymbolKind::Module,
        signature: format!("vue SFC {component}"),
        start_line: 1,
        end_line: total_lines.max(1),
        exported: true,
    });

    let blocks = extract_script_blocks(source);
    if blocks.is_empty() {
        // Template-only SFCs still get the module symbol so the file is indexed.
        return Ok(ParsedFile {
            symbols,
            imports,
            calls,
        });
    }

    for block in blocks {
        let parsed = pool.parse(block.lang, path, &block.body)?;
        let line_offset = block.body_start_line.saturating_sub(1);
        for mut sym in parsed.symbols {
            // Avoid duplicating the synthetic SFC module name from script.
            if sym.name == component && sym.kind == SymbolKind::Module {
                continue;
            }
            sym.start_line = sym.start_line.saturating_add(line_offset);
            sym.end_line = sym.end_line.saturating_add(line_offset);
            // Script bindings are part of the SFC public surface for indexing.
            if matches!(
                sym.kind,
                SymbolKind::Function
                    | SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::Type
                    | SymbolKind::Const
                    | SymbolKind::Variable
            ) {
                sym.exported = true;
            }
            symbols.push(sym);
        }
        let mut seen: std::collections::HashSet<(String, u32)> = symbols
            .iter()
            .map(|s| (s.name.clone(), s.start_line))
            .collect();
        for mut sym in sfc_macro_symbols(block.lang, &block.body, &component) {
            sym.start_line = sym.start_line.saturating_add(line_offset);
            sym.end_line = sym.end_line.saturating_add(line_offset);
            if seen.insert((sym.name.clone(), sym.start_line)) {
                symbols.push(sym);
            }
        }
        imports.extend(parsed.imports);
        for (name, line) in parsed.calls {
            calls.push((name, line.saturating_add(line_offset)));
        }
    }

    Ok(ParsedFile {
        symbols,
        imports,
        calls,
    })
}

#[derive(Debug)]
struct ScriptBlock {
    lang: Language,
    body: String,
    /// 1-based line of the first line of `body` inside the SFC.
    body_start_line: u32,
}

/// Path-derived component name; also salts the parse cache key because
/// identical SFC bodies at different paths must yield different symbols.
pub(crate) fn component_name(path: &RelPath) -> String {
    let file = path.file_name();
    let stem = file.strip_suffix(".vue").unwrap_or(file);
    if stem.is_empty() {
        "Component".to_string()
    } else {
        stem.to_string()
    }
}

fn line_number(zero_based_row: usize) -> u32 {
    u32::try_from(zero_based_row)
        .unwrap_or(u32::MAX - 1)
        .saturating_add(1)
}

fn extract_script_blocks(source: &str) -> Vec<ScriptBlock> {
    let lower = source.to_ascii_lowercase();
    let bytes = source.as_bytes();
    let mut blocks = Vec::new();
    let mut search_from = 0;

    while let Some(rel) = lower[search_from..].find("<script") {
        let start = search_from + rel;
        // Avoid matching "</script" or partial tags inside strings poorly —
        // require tag open at `<script` and a following `>` for attributes.
        let after = start + "<script".len();
        let Some(tag_end_rel) = lower[after..].find('>') else {
            break;
        };
        let tag_end = after + tag_end_rel; // index of '>'
        let open_tag = &source[start..=tag_end];
        // Skip the closing tag form if somehow matched.
        if open_tag.as_bytes().get(1) == Some(&b'/') {
            search_from = tag_end + 1;
            continue;
        }

        let close_pat = "</script>";
        let Some(close_rel) = lower[tag_end + 1..].find(close_pat) else {
            break;
        };
        let body_start = tag_end + 1;
        let body_end = tag_end + 1 + close_rel;
        let body = &source[body_start..body_end];
        // Drop a single leading newline so line 1 of the script aligns with
        // the first content line after `<script...>`.
        let (body, body_adj) = if let Some(stripped) = body.strip_prefix('\n') {
            (stripped, 1usize)
        } else if let Some(stripped) = body.strip_prefix("\r\n") {
            (stripped, 1usize)
        } else {
            (body, 0usize)
        };

        let body_start_line = line_number(line_of_offset(bytes, body_start) + body_adj);
        let lang = script_language(open_tag);
        blocks.push(ScriptBlock {
            lang,
            body: body.to_string(),
            body_start_line,
        });
        search_from = body_end + close_pat.len();
    }
    blocks
}

fn line_of_offset(bytes: &[u8], offset: usize) -> usize {
    bytes[..offset.min(bytes.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

fn script_language(open_tag: &str) -> Language {
    let lower = open_tag.to_ascii_lowercase();
    // lang='tsx' / lang="tsx" / lang=tsx
    if lang_is(&lower, "tsx") {
        Language::Tsx
    } else if lang_is(&lower, "ts") || lang_is(&lower, "typescript") {
        Language::TypeScript
    } else if lang_is(&lower, "jsx") {
        // JSX-in-SFC is uncommon; treat as JS for the shared grammar.
        Language::JavaScript
    } else {
        Language::JavaScript
    }
}

fn lang_is(open_tag_lower: &str, want: &str) -> bool {
    // Match lang="want", lang='want', lang=want as a tag attribute.
    let patterns = [
        format!("lang=\"{want}\""),
        format!("lang='{want}'"),
        format!("lang={want}"),
    ];
    patterns.iter().any(|p| open_tag_lower.contains(p.as_str()))
}

const SFC_MACRO_TRIGGERS: &[&str] = &[
    "defineProps",
    "defineEmits",
    "defineModel",
    "defineOptions",
    "defineComponent",
    "export default",
];

/// SFC compiler-macro / options surface that the plain TS/JS queries do not
/// name: the component `name` option (`defineOptions`, `defineComponent`,
/// `export default { name }`), props (`defineProps` object/array/inline type,
/// `defineModel`, options-API `props`), and emitted events (`defineEmits`
/// array/call-signature/named-tuple forms, options-API `emits`). Lines are
/// 1-based inside `body`.
fn sfc_macro_symbols(lang: Language, body: &str, component: &str) -> Vec<CodeSymbol> {
    if !SFC_MACRO_TRIGGERS.iter().any(|t| body.contains(t)) {
        return Vec::new();
    }
    let Some(grammar) = super::grammar(lang) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(body, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "call_expression" => {
                let callee = node
                    .child_by_field_name("function")
                    .map(|f| text(f, body))
                    .unwrap_or_default();
                let args = node.child_by_field_name("arguments");
                let first_arg = args.and_then(|a| a.named_child(0));
                let type_arg = node
                    .child_by_field_name("type_arguments")
                    .and_then(|t| t.named_child(0));
                match callee {
                    "defineOptions" | "defineComponent" => {
                        if let Some(obj) = first_arg.filter(|n| n.kind() == "object") {
                            options_object(obj, body, component, &mut out);
                        }
                    }
                    "defineProps" => {
                        if let Some(arg) = first_arg {
                            keys_of(arg, body, "prop", &mut out);
                        }
                        if let Some(ty) = type_arg {
                            keys_of(ty, body, "prop", &mut out);
                        }
                    }
                    "defineEmits" => {
                        if let Some(arg) = first_arg {
                            keys_of(arg, body, "emit", &mut out);
                        }
                        if let Some(ty) = type_arg {
                            keys_of(ty, body, "emit", &mut out);
                        }
                    }
                    "defineModel" => {
                        let name = match first_arg.filter(|n| n.kind() == "string") {
                            Some(s) => string_value(s, body),
                            None => "modelValue".to_string(),
                        };
                        push_field(&mut out, name, node, "model");
                    }
                    _ => {}
                }
            }
            "export_statement" => {
                if let Some(value) = node.child_by_field_name("value") {
                    if value.kind() == "object" {
                        options_object(value, body, component, &mut out);
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

fn text<'a>(node: tree_sitter::Node<'_>, src: &'a str) -> &'a str {
    src.get(node.byte_range()).unwrap_or("")
}

fn string_value(node: tree_sitter::Node<'_>, src: &str) -> String {
    text(node, src)
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn key_name(key: tree_sitter::Node<'_>, src: &str) -> Option<String> {
    match key.kind() {
        "property_identifier" | "identifier" | "shorthand_property_identifier" => {
            Some(text(key, src).to_string())
        }
        "string" => Some(string_value(key, src)),
        _ => None,
    }
}

fn push_field(out: &mut Vec<CodeSymbol>, name: String, at: tree_sitter::Node<'_>, what: &str) {
    if name.is_empty() {
        return;
    }
    out.push(CodeSymbol {
        id: SymbolId(0),
        file: FileId(0),
        signature: format!("vue {what} {name}"),
        name,
        kind: SymbolKind::Field,
        start_line: line_number(at.start_position().row),
        end_line: line_number(at.end_position().row),
        exported: true,
    });
}

/// Options object of `defineComponent` / `defineOptions` / `export default`.
fn options_object(
    obj: tree_sitter::Node<'_>,
    src: &str,
    component: &str,
    out: &mut Vec<CodeSymbol>,
) {
    let mut cursor = obj.walk();
    for pair in obj.named_children(&mut cursor) {
        if pair.kind() != "pair" {
            continue;
        }
        let (Some(key), Some(value)) = (
            pair.child_by_field_name("key"),
            pair.child_by_field_name("value"),
        ) else {
            continue;
        };
        match key_name(key, src).as_deref() {
            Some("name") if value.kind() == "string" => {
                let name = string_value(value, src);
                if !name.is_empty() && name != component {
                    out.push(CodeSymbol {
                        id: SymbolId(0),
                        file: FileId(0),
                        signature: format!("vue component {name}"),
                        name,
                        kind: SymbolKind::Class,
                        start_line: line_number(pair.start_position().row),
                        end_line: line_number(pair.end_position().row),
                        exported: true,
                    });
                }
            }
            Some("props") => keys_of(value, src, "prop", out),
            Some("emits") => keys_of(value, src, "emit", out),
            _ => {}
        }
    }
}

/// Names declared by a props/emits argument: object keys, string array
/// elements, inline object-type members, and emit call signatures
/// `(e: 'change', ...)`. Type references (`Props`) are skipped: their
/// interface members are already indexed by the TS query.
fn keys_of(node: tree_sitter::Node<'_>, src: &str, what: &str, out: &mut Vec<CodeSymbol>) {
    let mut cursor = node.walk();
    match node.kind() {
        "object" => {
            for child in node.named_children(&mut cursor) {
                let key = match child.kind() {
                    "pair" => child.child_by_field_name("key"),
                    "method_definition" => child.child_by_field_name("name"),
                    "shorthand_property_identifier" => Some(child),
                    _ => None,
                };
                if let Some(name) = key.and_then(|k| key_name(k, src)) {
                    push_field(out, name, child, what);
                }
            }
        }
        "array" => {
            for child in node.named_children(&mut cursor) {
                if child.kind() == "string" {
                    push_field(out, string_value(child, src), child, what);
                }
            }
        }
        "object_type" => {
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "property_signature" | "method_signature" => {
                        if let Some(name) = child
                            .child_by_field_name("name")
                            .and_then(|k| key_name(k, src))
                        {
                            push_field(out, name, child, what);
                        }
                    }
                    "call_signature" => {
                        if let Some(name) = first_param_literal(child, src) {
                            push_field(out, name, child, what);
                        }
                    }
                    _ => {}
                }
            }
        }
        "parenthesized_expression" | "as_expression" | "satisfies_expression" => {
            if let Some(inner) = node.named_child(0) {
                keys_of(inner, src, what, out);
            }
        }
        _ => {}
    }
}

/// `(e: 'change', v: number): void` → `change`.
fn first_param_literal(sig: tree_sitter::Node<'_>, src: &str) -> Option<String> {
    let params = sig.child_by_field_name("parameters")?;
    let first = params.named_child(0)?;
    let ty = first.child_by_field_name("type")?;
    let lit = ty.named_child(0)?;
    if lit.kind() != "literal_type" {
        return None;
    }
    let s = lit.named_child(0)?;
    (s.kind() == "string").then(|| string_value(s, src))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SymbolKind;

    fn pool() -> ParserPool {
        ParserPool::new()
    }

    #[test]
    fn extracts_script_setup_ts_symbols_and_imports() {
        let src = r#"
<template>
  <p>{{ msg }}</p>
</template>

<script setup lang="ts">
import { ref } from 'vue'
export function greet(name: string): string {
  return name
}
const msg = ref('hi')
</script>
"#;
        let parsed =
            parse_sfc(&pool(), &RelPath::new("src/HelloWorld.vue"), src).expect("vue sfc parse");
        assert!(
            parsed
                .symbols
                .iter()
                .any(|s| s.name == "HelloWorld" && s.kind == SymbolKind::Module),
            "expected SFC module symbol, got {:?}",
            parsed
                .symbols
                .iter()
                .map(|s| (&s.name, s.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            parsed.symbols.iter().any(|s| s.name == "greet"),
            "expected greet from script, got {:?}",
            parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            parsed.imports.iter().any(|i| i == "vue"),
            "expected vue import, got {:?}",
            parsed.imports
        );
        let greet = parsed.symbols.iter().find(|s| s.name == "greet").unwrap();
        assert!(
            greet.start_line > 5,
            "script symbol lines must be remapped into SFC coordinates, got {}",
            greet.start_line
        );
    }

    #[test]
    fn template_only_sfc_still_emits_module_symbol() {
        let src = "<template><div /></template>\n";
        let parsed = parse_sfc(&pool(), &RelPath::new("Empty.vue"), src).unwrap();
        assert_eq!(parsed.symbols.len(), 1);
        assert_eq!(parsed.symbols[0].name, "Empty");
        assert_eq!(parsed.symbols[0].kind, SymbolKind::Module);
        assert!(parsed.imports.is_empty());
    }

    #[test]
    fn script_lang_ts_selects_typescript() {
        let tag = "<script setup lang=\"ts\">";
        assert_eq!(script_language(tag), Language::TypeScript);
        assert_eq!(script_language("<script lang='tsx'>"), Language::Tsx);
        assert_eq!(script_language("<script>"), Language::JavaScript);
    }

    #[test]
    fn indexes_component_name_props_and_emits() {
        let src = r#"<template><button /></template>
<script setup lang="ts">
defineOptions({ name: 'ElButton', inheritAttrs: false })
const props = withDefaults(defineProps<{ size?: string; plain: boolean }>(), { size: 'md' })
const emit = defineEmits<{
  (e: 'click', ev: MouseEvent): void
  (e: 'update:modelValue', v: string): void
}>()
const checked = defineModel<boolean>('checked')
defineEmits<{ focus: [ev: FocusEvent] }>()
</script>
"#;
        let parsed = parse_sfc(&pool(), &RelPath::new("src/button.vue"), src).unwrap();
        let find = |n: &str| parsed.symbols.iter().find(|s| s.name == n);
        let el = find("ElButton").expect("component name option");
        assert_eq!(el.kind, SymbolKind::Class);
        assert_eq!(el.start_line, 3);
        for n in [
            "size",
            "plain",
            "click",
            "update:modelValue",
            "checked",
            "focus",
            "props",
            "emit",
        ] {
            assert!(
                find(n).is_some(),
                "missing {n}: {:?}",
                parsed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
        assert_eq!(find("size").unwrap().kind, SymbolKind::Field);
        assert_eq!(find("click").unwrap().start_line, 6);
    }

    #[test]
    fn options_api_and_js_runtime_declarations() {
        let src = r#"<script>
import { defineComponent } from 'vue'
export default defineComponent({
  name: 'MyTable',
  props: { rows: Array, dense: { type: Boolean } },
  emits: ['select', 'sort'],
  methods: { refresh() {} },
})
</script>
<script setup>
const p = defineProps(['title'])
const e = defineEmits({ close: () => true })
</script>
"#;
        let parsed = parse_sfc(&pool(), &RelPath::new("Table.vue"), src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        for n in [
            "Table", "MyTable", "rows", "dense", "select", "sort", "title", "close",
        ] {
            assert!(names.contains(&n), "missing {n}: {names:?}");
        }
        let title = parsed.symbols.iter().find(|s| s.name == "title").unwrap();
        assert_eq!(title.start_line, 11);
    }

    #[test]
    fn plain_export_default_name_and_tsx_lang() {
        let src = "<script lang=\"tsx\">\nexport default { name: 'Cell', props: ['value'] }\nexport const Render = () => <div />\n</script>\n";
        let parsed = parse_sfc(&pool(), &RelPath::new("cell.vue"), src).unwrap();
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        for n in ["cell", "Cell", "value", "Render"] {
            assert!(names.contains(&n), "missing {n}: {names:?}");
        }
    }
}
