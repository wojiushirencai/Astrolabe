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
use astrolabe_core::lsp::queries::{
    apple_references_fallback, find_references, goto_definition, identifier_at,
    text_fallback_references, SymbolAt,
};
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::Position;
use astrolabe_core::lsp::Range;
use astrolabe_core::lsp::{Diagnostic, LanguageServer, Location, LspError, Precise};
use astrolabe_core::types::{Confidence, Language, RelPath};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::index::RepoIndex;
use crate::precise_tools::PreciseCapability;

/// Current tree-sitter index, when one has been built.
pub(crate) type SyntaxIndex = Arc<dyn Fn() -> Option<Arc<RepoIndex>> + Send + Sync>;

/// Cap on syntactic stand-in answers so a common name cannot flood a reply.
const MAX_SYNTACTIC_DEFINITIONS: usize = 30;

pub(crate) struct LiveBackend {
    root: PathBuf,
    pool: Arc<LspPool>,
    /// Syntax index used to answer while a server is still importing or
    /// indexing (kotlin-lsp's first Gradle import runs for many minutes).
    syntax: Option<SyntaxIndex>,
    /// Set while a slow-starting server (kotlin-lsp: JVM boot + Gradle
    /// import) is being started on a background thread. Queries meanwhile
    /// get the syntactic fallback instead of blocking on the start.
    starting: Arc<AtomicBool>,
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
            syntax: None,
            starting: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Answer from `index` while a language server is still warming up.
    pub(crate) fn with_syntax_index(mut self, index: SyntaxIndex) -> Self {
        self.syntax = Some(index);
        self
    }

    /// Declarations named `name` in the syntax index, same language family,
    /// `origin`'s file first.
    fn syntactic_definitions(
        &self,
        language: Language,
        origin: &RelPath,
        name: &str,
    ) -> Vec<(Location, String)> {
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return Vec::new();
        };
        let family = |l: Option<Language>| l.is_some_and(|l| same_family(l, language));
        let mut hits: Vec<(bool, Location, String)> = Vec::new();
        for symbol in &index.graph().symbols {
            if symbol.name != name {
                continue;
            }
            let Some(file) = index.file(symbol.file) else {
                continue;
            };
            if !family(file.language) {
                continue;
            }
            let line = symbol.start_line.saturating_sub(1);
            let at = Position { line, character: 0 };
            hits.push((
                file.path != *origin,
                Location {
                    path: file.path.clone(),
                    range: Range { start: at, end: at },
                },
                symbol.signature.clone(),
            ));
        }
        hits.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        hits.into_iter()
            .take(MAX_SYNTACTIC_DEFINITIONS)
            .map(|(_, loc, sig)| (loc, sig))
            .collect()
    }

    fn target_name(&self, path: &RelPath, target: SymbolAt<'_>) -> Option<String> {
        match target {
            SymbolAt::Name(name) if !name.is_empty() => Some(name.to_string()),
            SymbolAt::Name(_) => None,
            SymbolAt::Position(pos) => identifier_at(&self.root, path, pos),
        }
    }

    /// Run `op` against a checked-out server. The guard lives for the whole
    /// call, which is what stops the pool reclaiming a server mid-request.
    fn with_server<T>(
        &self,
        language: Language,
        op: impl FnOnce(Option<&dyn LanguageServer>) -> T,
    ) -> Result<T, LspError> {
        if language == Language::Kotlin
            && self.syntax.is_some()
            && self.start_in_background(language)
        {
            // `None` = server still starting; callers treat it as warming
            // and answer from the syntax index.
            return Ok(op(None));
        }
        let guard = self.pool.acquire(language)?;
        Ok(op(Some(&guard as &dyn LanguageServer)))
    }

    /// For servers whose start alone takes seconds (kotlin-lsp boots a JVM
    /// before it even answers `initialize`): start it on a background thread
    /// and report `true` while that is in progress, so the first queries are
    /// not held up by the start plus the first-checkout ready wait.
    fn start_in_background(&self, language: Language) -> bool {
        if self.starting.load(Ordering::Acquire) {
            return true;
        }
        if self.pool.is_resident(language) {
            return false;
        }
        if self
            .starting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return true;
        }
        let pool = Arc::clone(&self.pool);
        let starting = Arc::clone(&self.starting);
        let spawned = std::thread::Builder::new()
            .name(format!("lsp-start-{}", language.name()))
            .spawn(move || {
                if let Err(error) = pool.acquire(language) {
                    tracing::warn!(language = language.name(), %error, "background start failed");
                }
                starting.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            self.starting.store(false, Ordering::Release);
            return false;
        }
        true
    }

}

/// An empty, unknown answer: what a server gives while still importing.
fn warming_unknown<T>(value: &[T], result: &Precise<Vec<T>>) -> bool {
    value.is_empty() && result.confidence == Confidence::Unknown
}

/// Note for answers given from the syntax index while the server warms up.
fn warming_note(language: Language, n: usize, what: &str) -> String {
    let server = astrolabe_core::lsp::router::primary_server_name(language);
    let head = format!(
        "{server} 仍在导入项目 / 建立索引，精确结果稍后可用（首次导入大型 Gradle 项目可能需要十几分钟，之后有缓存）"
    );
    if n == 0 {
        head
    } else {
        format!("{head}。先给出 tree-sitter 语法索引里的{what} {n} 处（syntactic：按名字匹配，可能含同名的其他实体）。")
    }
}

/// Note for an idle server that returned nothing.
fn empty_note(language: Language, what: &str) -> String {
    let server = astrolabe_core::lsp::router::primary_server_name(language);
    format!(
        "{server} 没有返回{what}：该文件可能不在它导入的项目模型里（例如 Kotlin 多平台 / Android 源集未被导入、未安装 Android SDK），所以空结果不作为精确结论。"
    )
}

/// Languages whose declarations can answer for each other (mixed projects).
fn same_family(a: Language, b: Language) -> bool {
    use Language::*;
    let group = |l: Language| match l {
        Kotlin | Java => 1,
        TypeScript | Tsx | JavaScript | Vue => 2,
        C | Cpp | ObjC | ObjCpp => 3,
        CSharp | VisualBasic => 4,
        _ => 0,
    };
    a == b || (group(a) != 0 && group(a) == group(b))
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

/// Language whose server should answer for `path`: like
/// [`Language::from_path`], except a `.h` header follows its content
/// ([`Language::from_path_and_source`]), so an Objective-C header goes to the
/// ObjC server (clangd in ObjC mode) instead of the C one.
fn server_language(root: &Path, path: &str) -> Option<Language> {
    let rel = RelPath::new(path);
    let lang = Language::from_path(&rel)?;
    if lang == Language::C {
        if let Ok(source) = std::fs::read_to_string(root.join(path)) {
            return Language::from_path_and_source(&rel, &source);
        }
    }
    Some(lang)
}

/// Resolve language-server routing from an optional path.
fn server_language_from_opt_path(
    root: &Path,
    path: Option<&str>,
    what: &str,
) -> Result<Language, String> {
    let Some(path) = path else {
        return Err(needs_path(what));
    };
    server_language(root, path).ok_or_else(|| unknown_language(what, path))
}

impl PreciseCapability for LiveBackend {
    fn is_warm(&self, path: &str) -> bool {
        server_language(&self.root, path).is_some_and(|lang| self.pool.is_resident(lang))
    }

    fn files_changed(&self, changes: &astrolabe_core::watch::ChangeSet) {
        use astrolabe_core::lsp::{FileChange, FileChangeKind};
        use astrolabe_core::watch::ChangeKind;
        let events: Vec<FileChange> = changes
            .iter()
            .map(|(kind, path)| FileChange {
                path: path.clone(),
                kind: match kind {
                    ChangeKind::Added => FileChangeKind::Created,
                    ChangeKind::Modified => FileChangeKind::Changed,
                    ChangeKind::Removed => FileChangeKind::Deleted,
                },
            })
            .collect();
        self.pool.notify_files_changed(&events);
    }

    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
    ) -> Precise<Vec<Location>> {
        let language = match server_language_from_opt_path(&self.root, path, "精确引用查询") {
            Ok(lang) => lang,
            Err(note) => return Precise::unknown(Vec::new(), note),
        };
        let rel = RelPath::new(path.expect("language resolved"));
        let target = match position {
            Some(position) => SymbolAt::Position(position),
            None => SymbolAt::Name(symbol),
        };
        let result = match self.with_server(language, |server| {
            let result = find_references(server, &self.root, &rel, target);
            (result, server.is_none_or(|s| s.is_busy()))
        }) {
            Ok((result, busy)) if busy && warming_unknown(&result.value, &result) => {
                let reason = warming_note(language, 0, "");
                match text_fallback_references(&self.root, &rel, target, &reason) {
                    Some(fallback) if !fallback.value.is_empty() => fallback,
                    _ => result,
                }
            }
            // An idle server that finds no reference at all, while the name
            // occurs elsewhere in the repo, usually does not have the file
            // in its project model (unimported module, KMP / Android source
            // set). That is not an authoritative zero.
            // kotlin-lsp answers a file outside its project model with just
            // the declaration itself, so one hit is as suspect as none there.
            Ok((result, false))
                if result.value.is_empty()
                    || (language == Language::Kotlin && result.value.len() <= 1) =>
            {
                let reason = empty_note(language, "引用");
                match text_fallback_references(&self.root, &rel, target, &reason) {
                    Some(fallback) if fallback.value.len() > result.value.len().max(1) => fallback,
                    _ => result,
                }
            }
            Ok((result, _)) => result,
            // The error already carries discovery's install hint; passing it
            // through verbatim is what makes "not found" actionable.
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        };
        // Swift / ObjC servers without an index answer nothing; fall back to
        // a clearly labelled tree-sitter search instead of an empty Unknown.
        apple_references_fallback(&self.root, &rel, target, result)
    }


    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let Some(language) = server_language(&self.root, path) else {
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
            let result = goto_definition(server, &self.root, &rel, target);
            (result, server.is_none_or(|s| s.is_busy()))
        }) {
            Ok((result, busy))
                if result.value.is_empty()
                    && (!busy || result.confidence == Confidence::Unknown) =>
            {
                let Some(name) = self.target_name(&rel, target) else {
                    return result;
                };
                let hits = self.syntactic_definitions(language, &rel, &name);
                if hits.is_empty() {
                    return result;
                }
                let note = if busy {
                    warming_note(language, hits.len(), "同名声明")
                } else {
                    format!("{}先给出 tree-sitter 语法索引里的同名声明 {} 处（syntactic：按名字匹配，可能含同名的其他实体）。", empty_note(language, "定义"), hits.len())
                };
                Precise {
                    confidence: Confidence::Syntactic,
                    note: Some(note),
                    value: hits.into_iter().map(|(loc, _)| loc).collect(),
                }
            }
            Ok((result, _)) => result,
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>> {
        let rel = RelPath::new(path);
        let Some(language) = server_language(&self.root, path) else {
            return Precise::unknown(Vec::new(), unknown_language("诊断", path));
        };
        match self.with_server(language, |server| {
            query_server(server, &rel, FetchOptions::default())
        }) {
            Ok(result) => result,
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }

}

