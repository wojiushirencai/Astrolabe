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
    let total_lines = line_number(source.lines().count().saturating_sub(1).max(0));
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

fn component_name(path: &RelPath) -> String {
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
        let (body, body_adj) = if body.starts_with('\n') {
            (&body[1..], 1usize)
        } else if body.starts_with("\r\n") {
            (&body[2..], 1usize)
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
        let parsed = parse_sfc(&pool(), &RelPath::new("src/HelloWorld.vue"), src)
            .expect("vue sfc parse");
        assert!(
            parsed.symbols.iter().any(|s| s.name == "HelloWorld" && s.kind == SymbolKind::Module),
            "expected SFC module symbol, got {:?}",
            parsed.symbols.iter().map(|s| (&s.name, s.kind)).collect::<Vec<_>>()
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
        assert_eq!(
            script_language("<script lang='tsx'>"),
            Language::Tsx
        );
        assert_eq!(script_language("<script>"), Language::JavaScript);
    }
}
