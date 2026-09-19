//! Real backend for the precise tools: a pooled language server plus the
//! scope-aware rewrite planner.
//!
//! Everything here is on-demand. No server starts until a precise tool is
//! actually called, and the pool reclaims it once idle or over its memory
//! budget — the whole point of the split is that a language server is a
//! short-lived resource, not a resident one.
//!
//! When a server is missing this layer still answers: the error carries the
//! install hint discovery produced, and the tool renders it as
//! `confidence: unknown` with the command to run. An empty result would read
//! as "there is nothing here", which is a different and wrong claim.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use astrolabe_core::lsp::diagnostics::{query_server, FetchOptions};
use astrolabe_core::lsp::discovery::Discovery;
use astrolabe_core::lsp::pool::{LspPool, LspPoolConfig};
use astrolabe_core::lsp::queries::{find_references, goto_definition, hover, SymbolAt};
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::Position;
use astrolabe_core::lsp::{Diagnostic, LanguageServer, Location, LspError, Precise};
use astrolabe_core::rewrite::engine::{plan_rename, FileSource};
use astrolabe_core::rewrite::scope_js::JsScopeResolver;
use astrolabe_core::rewrite::scope_ts::TreeSitterScopeResolver;
use astrolabe_core::rewrite::{ByteRange, Evidence, RewritePlan, ScopeResolver};
use astrolabe_core::types::{Confidence, Language, RelPath};

use crate::precise_tools::PreciseCapability;

pub(crate) struct LiveBackend {
    root: PathBuf,
    pool: Arc<LspPool>,
}

impl std::fmt::Debug for LiveBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveBackend")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl LiveBackend {
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let pool = LspPool::from_discovery_and_transport(
            Discovery::from_process(),
            StdioSpawn::new(&root),
            root.clone(),
            LspPoolConfig::default(),
        );
        LiveBackend {
            root,
            pool: Arc::new(pool),
        }
    }

    /// Run `op` against a checked-out server. The guard lives for the whole
    /// call, which is what stops the pool reclaiming a server mid-request.
    fn with_server<T>(
        &self,
        language: Language,
        op: impl FnOnce(Option<&dyn LanguageServer>) -> T,
    ) -> Result<T, LspError> {
        let guard = self.pool.acquire(language)?;
        Ok(op(Some(&guard as &dyn LanguageServer)))
    }

    fn resolver_for(language: Language) -> Box<dyn ScopeResolver> {
        match language {
            Language::TypeScript | Language::Tsx | Language::JavaScript => {
                Box::new(JsScopeResolver)
            }
            _ => Box::new(TreeSitterScopeResolver::new()),
        }
    }

    fn read(&self, path: &RelPath) -> std::io::Result<String> {
        std::fs::read_to_string(Path::new(&self.root).join(path.as_str()))
    }
}

/// Note shown when a tool needs a path but was not given one.
fn needs_path(what: &str) -> String {
    format!(
        "无法确定语言：{what} 需要 `path` 来选择语言服务器。\
         请补上仓库相对路径，例如 `crates/core/src/lib.rs`。"
    )
}


/// Note when a path was given but its extension is not mapped to a language.
fn unknown_language(what: &str, path: &str) -> String {
    format!(
        "无法确定语言：路径 `{path}` 未映射到已知语言，{what} 无法选择语言服务器。\
         请改用带已知扩展名的源文件（如 `.rs` / `.ts` / `.py`）。"
    )
}

/// Resolve language from an optional path, distinguishing missing vs unmapped.
fn language_from_opt_path(path: Option<&str>, what: &str) -> Result<Language, String> {
    let Some(path) = path else {
        return Err(needs_path(what));
    };
    Language::from_path(&RelPath::new(path)).ok_or_else(|| unknown_language(what, path))
}


impl PreciseCapability for LiveBackend {
    fn references(&self, symbol: &str, path: Option<&str>) -> Precise<Vec<Location>> {
        let language = match language_from_opt_path(path, "精确引用查询") {
            Ok(lang) => lang,
            Err(note) => return Precise::unknown(Vec::new(), note),
        };
        let rel = RelPath::new(path.expect("language resolved"));
        match self.with_server(language, |server| {
            find_references(server, &self.root, &rel, SymbolAt::Name(symbol))
        }) {
            Ok(result) => result,
            // The error already carries discovery's install hint; passing it
            // through verbatim is what makes "not found" actionable.
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

    fn info(&self, symbol: &str, path: Option<&str>) -> Precise<Option<String>> {
        let language = match language_from_opt_path(path, "符号信息查询") {
            Ok(lang) => lang,
            Err(note) => return Precise::unknown(None, note),
        };
        let rel = RelPath::new(path.expect("language resolved"));
        match self.with_server(language, |server| {
            hover(
                server,
                &self.root,
                &rel,
                SymbolAt::Name(symbol),
                |server, path, position| server.hover(&path, position),
            )
        }) {
            Ok(result) => result,
            Err(error) => Precise::unknown(None, error.to_string()),
        }
    }

    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let Some(language) = Language::from_path(&RelPath::new(path)) else {
            return Precise::unknown(Vec::new(), unknown_language("精确定义查询", path));
        };
        let rel = RelPath::new(path);
        let target = match (line, character, symbol) {
            (Some(line), Some(character), _) => SymbolAt::Position(Position { line, character }),
            (_, _, Some(name)) if !name.is_empty() => SymbolAt::Name(name),
            _ => {
                return Precise::unknown(
                    Vec::new(),
                    "需要提供 symbol，或同时提供 line 与 character。",
                );
            }
        };
        match self.with_server(language, |server| {
            goto_definition(server, &self.root, &rel, target)
        }) {
            Ok(result) => result,
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>> {
        let rel = RelPath::new(path);
        let Some(language) = Language::from_path(&rel) else {
            return Precise::unknown(Vec::new(), unknown_language("诊断", path));
        };
        match self.with_server(language, |server| {
            query_server(server, &rel, FetchOptions::default())
        }) {
            Ok(result) => result,
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

    fn plan_rename(
        &self,
        symbol: &str,
        new_name: &str,
        path: Option<&str>,
    ) -> Precise<RewritePlan> {
        let empty = || RewritePlan {
            symbol: symbol.to_owned(),
            new_name: new_name.to_owned(),
            sites: Vec::new(),
            evidence: Evidence::NameMatch,
            confidence: Confidence::Unknown,
            excluded: Vec::new(),
        };

        let language = match language_from_opt_path(path, "重命名计划") {
            Ok(lang) => lang,
            Err(note) => return Precise::unknown(empty(), note),
        };
        let rel = RelPath::new(path.expect("language resolved"));
        let source = match self.read(&rel) {
            Ok(source) => source,
            Err(error) => {
                return Precise::unknown(empty(), format!("读取 {rel} 失败：{error}"));
            }
        };

        // Locate the symbol's first identifier occurrence as the planning
        // origin. Callers that know the exact cursor should pass it once the
        // tool schema grows a position parameter.
        let Some(at) = first_identifier(&source, symbol) else {
            return Precise::unknown(empty(), format!("在 {rel} 中找不到标识符 `{symbol}`。"));
        };

        let resolver = Self::resolver_for(language);
        let files = [FileSource {
            path: rel.clone(),
            source: source.clone(),
        }];
        match plan_rename(resolver.as_ref(), symbol, new_name, &rel, at, &files) {
            Ok(plan) => {
                let confidence = plan.confidence;
                // Single-file planning. `RewritePlan` already downgrades to
                // syntactic when the resolver reports the symbol may be
                // referenced from files we did not examine.
                Precise {
                    value: plan,
                    confidence,
                    note: (confidence != Confidence::Exact).then(|| {
                        "仅分析了该文件。跨文件引用需要语言服务器级的重命名，\
                         此计划应人工复核后再应用。"
                            .to_string()
                    }),
                }
            }
            Err(error) => Precise::unknown(empty(), error.to_string()),
        }
    }
}

/// First occurrence of `name` at an identifier boundary, so `handler` does not
/// match inside `handlerFactory`.
fn first_identifier(source: &str, name: &str) -> Option<ByteRange> {
    if name.is_empty() {
        return None;
    }
    let bytes = source.as_bytes();
    let is_word = |b: u8| b == b'_' || b.is_ascii_alphanumeric();
    let mut from = 0usize;
    while let Some(rel) = source[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before_ok = start == 0 || !is_word(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return Some(ByteRange { start, end });
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_search_respects_word_boundaries() {
        let source = "let handlerFactory = 1; let handler = 2;\n";
        let at = first_identifier(source, "handler").expect("standalone handler");
        assert_eq!(&source[at.start..at.end], "handler");
        assert!(
            at.start > source.find("handlerFactory").unwrap(),
            "must skip the longer identifier that merely starts with the name"
        );
    }

    #[test]
    fn missing_path_is_reported_not_guessed() {
        let backend = LiveBackend::new(std::env::temp_dir());
        let result = backend.references("anything", None);
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(
            result.note.as_deref().unwrap_or_default().contains("path"),
            "the note must say what is missing: {:?}",
            result.note
        );
    }

    #[test]
    fn definition_without_language_path_is_reported() {
        let backend = LiveBackend::new(std::env::temp_dir());
        let result = backend.definition("README", Some("x"), None, None);
        assert_eq!(result.confidence, Confidence::Unknown);
        let note = result.note.as_deref().unwrap_or_default();
        assert!(
            note.contains("README") && note.contains("未映射"),
            "the note must say the path is unmapped, not that path is missing: {note:?}"
        );
        assert!(
            !note.contains("需要 `path`"),
            "must not claim path is missing when one was supplied: {note:?}"
        );
    }

    #[test]
    fn plan_rename_without_path_does_not_fabricate_a_plan() {
        let backend = LiveBackend::new(std::env::temp_dir());
        let result = backend.plan_rename("old", "new", None);
        assert_eq!(result.confidence, Confidence::Unknown);
        assert!(result.value.sites.is_empty());
        assert!(!result.value.is_auto_applicable());
    }
}
