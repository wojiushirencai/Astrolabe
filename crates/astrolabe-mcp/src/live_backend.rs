//! Real backend for the precise tools: a pooled language server plus the
//! scope-aware precise queries.
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

use astrolabe_core::graph::{paired_files, CodeGraph};
use astrolabe_core::lsp::diagnostics::{query_server, FetchOptions};
use astrolabe_core::lsp::discovery::Discovery;
use astrolabe_core::lsp::pool::{LspPool, LspPoolConfig};
use astrolabe_core::lsp::queries::{
    apple_references_fallback_in, find_references, goto_definition, identifier_at,
    text_fallback_references_in, SymbolAt,
};
use astrolabe_core::lsp::router::primary_server_name;
use astrolabe_core::lsp::transport::StdioSpawn;
use astrolabe_core::lsp::Position;
use astrolabe_core::lsp::Range;
use astrolabe_core::lsp::{Diagnostic, LanguageServer, Location, LspError, Precise};
use astrolabe_core::types::{Confidence, EdgeKind, FileId, Language, RelPath};
use std::collections::{BTreeSet, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::index::RepoIndex;
use crate::precise_tools::PreciseCapability;
use crate::refs_graph::{self, PathFix};

/// Current tree-sitter index, when one has been built.
pub(crate) type SyntaxIndex = Arc<dyn Fn() -> Option<Arc<RepoIndex>> + Send + Sync>;

/// Cap on syntactic stand-in answers so a common name cannot flood a reply.
const MAX_SYNTACTIC_DEFINITIONS: usize = 30;
/// Go implementors listed in a `find_references` note.
const GO_IMPLEMENTORS_LISTED: usize = 12;

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
    /// Set when the background start failed (server missing / crashed). Calls
    /// then take the synchronous `acquire` path so the real error (install
    /// hint) reaches the user instead of an endless "still importing".
    start_failed: Arc<AtomicBool>,
    /// The session's search_code file cache; `find_references` reads
    /// through it instead of re-reading each file several times per call.
    file_cache: Option<Arc<astrolabe_core::cache::FileCache>>,
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
            Discovery::from_process().with_project_root(&root),
            StdioSpawn::new(&root),
            root.clone(),
            LspPoolConfig::default(),
        );
        LiveBackend {
            root,
            pool: Arc::new(pool),
            syntax: None,
            starting: Arc::new(AtomicBool::new(false)),
            start_failed: Arc::new(AtomicBool::new(false)),
            file_cache: None,
        }
    }

    /// Serve `find_references` file reads from the session's file cache.
    pub(crate) fn with_file_cache(mut self, cache: Arc<astrolabe_core::cache::FileCache>) -> Self {
        self.file_cache = Some(cache);
        self
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
        let near = near_files(index.graph(), origin);
        let mut hits: Vec<(u8, Location, String)> = Vec::new();
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
            // Origin first, then its header/source partner and the files it
            // includes (with their partners), then the rest.
            let rank = if file.path == *origin {
                0
            } else if near.contains(&symbol.file) {
                1
            } else {
                2
            };
            hits.push((
                rank,
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

    /// C family: a server without a compile database (clangd with no
    /// `compile_commands.json`) only knows the header it parsed, so "go to
    /// definition" on a call lands on the prototype. When every answer is in
    /// a header, add the same-name declarations from that header's paired
    /// source file ([`EdgeKind::Pair`] in the syntax index). Labelled
    /// syntactic: the pairing is by file name, the match by symbol name.
    fn with_paired_definitions(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        result: Precise<Vec<Location>>,
    ) -> Precise<Vec<Location>> {
        if !is_c_family(language)
            || result.value.is_empty()
            || !result.value.iter().all(|l| is_c_header(l.path.as_str()))
        {
            return result;
        }
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return result;
        };
        let Some(name) = self.target_name(rel, target) else {
            return result;
        };
        let extra = paired_definitions(index.graph(), &result.value, &name);
        if extra.is_empty() {
            return result;
        }
        let partners: BTreeSet<&str> = extra.iter().map(|l| l.path.as_str()).collect();
        let note = format!(
            "{}语言服务器只给出了头文件里的声明；后面追加的 {} 处是配对源文件 {} 里的同名定义（syntactic：按头文件/源文件同名配对和符号名匹配）。",
            result.note.as_deref().map(|n| format!("{n} ")).unwrap_or_default(),
            extra.len(),
            partners.into_iter().collect::<Vec<_>>().join("、")
        );
        let mut value = result.value;
        value.extend(extra);
        Precise {
            confidence: result.confidence.max(Confidence::Syntactic),
            note: Some(note),
            value,
        }
    }

    /// C family: the server "resolved" a call to the call site itself. clangd
    /// does this for a function it has no declaration of (missing include
    /// path, no compile database): C's implicit declaration makes the call
    /// its own declaration. Not an answer when the syntax index has no
    /// declaration of that name on that line.
    fn unresolved_call(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        found: &[Location],
    ) -> bool {
        let SymbolAt::Position(pos) = target else {
            return false;
        };
        if !is_c_family(language)
            || found.is_empty()
            || !found
                .iter()
                .all(|l| l.path == *rel && l.range.start.line == pos.line)
        {
            return false;
        }
        let Some(index) = self.syntax.as_ref().and_then(|f| f()) else {
            return false;
        };
        let Some(name) = self.target_name(rel, target) else {
            return false;
        };
        let g = index.graph();
        let Some(file) = g.files.iter().find(|f| f.path == *rel).map(|f| f.id) else {
            return false;
        };
        let line = pos.line + 1;
        !g.symbols
            .iter()
            .any(|s| s.file == file && s.name == name && s.start_line <= line && line <= s.end_line)
    }

    /// A Go interface named by the query in `path`: its implementing types
    /// from the syntax index (references never include them; Go has no
    /// `implements` clause to refer to).
    fn go_implementors_line(&self, path: &RelPath, target: SymbolAt<'_>) -> Option<String> {
        let name = self.target_name(path, target)?;
        let index = self.syntax.as_ref().and_then(|f| f())?;
        let graph = index.graph();
        let file = graph.files.iter().find(|f| f.path == *path)?.id;
        let ifaces: std::collections::HashSet<u32> = graph
            .symbols
            .iter()
            .filter(|s| {
                s.file == file && s.name == name && s.kind == astrolabe_core::SymbolKind::Interface
            })
            .map(|s| s.id.0)
            .collect();
        let by = astrolabe_core::go_iface::implementors_of(graph, &ifaces);
        let text: String = by
            .iter()
            .map(|(iface, impls)| {
                astrolabe_core::go_iface::render_implementors(
                    graph,
                    *iface,
                    impls,
                    GO_IMPLEMENTORS_LISTED,
                )
            })
            .collect();
        (!text.is_empty()).then_some(text)
    }

    /// `find_references` through the language server, with the text
    /// fallbacks for a warming / empty server and the Apple tree-sitter
    /// fallback.
    fn server_references(
        &self,
        language: Language,
        rel: &RelPath,
        target: SymbolAt<'_>,
        index: Option<&RepoIndex>,
    ) -> Precise<Vec<Location>> {
        // The text fallbacks walk the file list of the current (barrier-
        // fresh) index instead of re-scanning the whole tree per query.
        let files = index
            .and_then(RepoIndex::incremental)
            .map(|inc| inc.admitted_paths());
        let result = match self.with_server(language, |server| {
            let result = find_references(server, &self.root, rel, target);
            (result, server.is_none_or(|s| s.is_busy()))
        }) {
            Ok((result, busy)) if busy && warming_unknown(&result.value, &result) => {
                let reason = warming_note(language, 0, "");
                match text_fallback_references_in(&self.root, rel, target, &reason, files) {
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
                match text_fallback_references_in(&self.root, rel, target, &reason, files) {
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
        apple_references_fallback_in(&self.root, rel, target, result, files)
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
        self.start_failed.store(false, Ordering::Release);
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
        if self.start_failed.load(Ordering::Acquire) {
            return false;
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
        let start_failed = Arc::clone(&self.start_failed);
        let spawned = std::thread::Builder::new()
            .name(format!("lsp-start-{}", language.name()))
            .spawn(move || {
                if let Err(error) = pool.acquire(language) {
                    tracing::warn!(language = language.name(), %error, "background start failed");
                    start_failed.store(true, Ordering::Release);
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

fn is_c_family(language: Language) -> bool {
    matches!(
        language,
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp
    )
}

fn is_c_header(path: &str) -> bool {
    path.rsplit_once('.').is_some_and(|(_, e)| {
        ["h", "hh", "hpp", "hxx", "h++"]
            .iter()
            .any(|x| e.eq_ignore_ascii_case(x))
    })
}

/// Files a definition lookup from `origin` should try before the rest of
/// the repo: `origin`'s header/source partner, the files it includes, and
/// their partners.
fn near_files(g: &CodeGraph, origin: &RelPath) -> HashSet<FileId> {
    let Some(origin) = g.files.iter().find(|f| f.path == *origin).map(|f| f.id) else {
        return HashSet::new();
    };
    let mut near: HashSet<FileId> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Import && e.from == origin.0)
        .map(|e| FileId(e.to))
        .collect();
    near.insert(origin);
    let partners: Vec<FileId> = g
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Pair)
        .filter_map(|e| {
            if near.contains(&FileId(e.from)) {
                Some(FileId(e.to))
            } else if near.contains(&FileId(e.to)) {
                Some(FileId(e.from))
            } else {
                None
            }
        })
        .collect();
    near.extend(partners);
    near.remove(&origin);
    near
}

/// Declarations named `name` in the paired source files of the headers in
/// `found`, not already in `found`.
fn paired_definitions(g: &CodeGraph, found: &[Location], name: &str) -> Vec<Location> {
    let mut out: Vec<Location> = Vec::new();
    let headers: Vec<FileId> = g
        .files
        .iter()
        .filter(|f| found.iter().any(|l| l.path == f.path))
        .map(|f| f.id)
        .collect();
    let partners: BTreeSet<FileId> = headers
        .iter()
        .flat_map(|&h| paired_files(g, h))
        .filter(|p| !headers.contains(p))
        .collect();
    if partners.is_empty() {
        return out;
    }
    for symbol in &g.symbols {
        if symbol.name != name || !partners.contains(&symbol.file) {
            continue;
        }
        let Some(file) = g.files.iter().find(|f| f.id == symbol.file) else {
            continue;
        };
        let at = Position {
            line: symbol.start_line.saturating_sub(1),
            character: 0,
        };
        let loc = Location {
            path: file.path.clone(),
            range: Range { start: at, end: at },
        };
        if !found.contains(&loc) && !out.contains(&loc) {
            out.push(loc);
        }
        if out.len() >= MAX_SYNTACTIC_DEFINITIONS {
            break;
        }
    }
    out
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

/// Note for an idle server that returned nothing. The project-model
/// examples name the build system of `language`; Kotlin / Android wording
/// only for Kotlin.
fn empty_note(language: Language, what: &str) -> String {
    let server = astrolabe_core::lsp::router::primary_server_name(language);
    let why = match language {
        Language::Kotlin => "该文件可能不在它导入的项目模型里（例如 Kotlin 多平台 / Android 源集未被导入、未安装 Android SDK）",
        Language::Java => "该文件可能不在它导入的 Maven / Gradle 项目里",
        Language::Go => "该文件可能不在 go.mod 模块或当前构建标签里",
        Language::TypeScript | Language::Tsx | Language::JavaScript | Language::Vue => {
            "该文件可能不在 tsconfig / jsconfig 覆盖的范围里"
        }
        Language::Python => "该文件可能不在 pyright 的分析范围里（解释器 / 虚拟环境未配置）",
        Language::Rust => "该文件可能不在 Cargo 工作区的 crate 里",
        Language::C | Language::Cpp | Language::ObjC | Language::ObjCpp => {
            "该文件可能没有编译参数（缺 compile_commands.json）"
        }
        Language::CSharp | Language::VisualBasic => "该文件可能不在已加载的 .csproj / .sln 里",
        _ => "该文件可能不在它加载的项目里",
    };
    format!("{server} 没有返回{what}：{why}，所以空结果不作为精确结论。")
}

/// Languages whose declarations can answer for each other (mixed projects).
fn same_family(a: Language, b: Language) -> bool {
    refs_graph::same_family(a, b)
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
        exact: bool,
    ) -> Precise<Vec<Location>> {
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            self.references_inner(symbol, path, position, exact)
        })
    }

    fn annotate(&self, symbol: &str, locations: &[Location]) -> Vec<refs_graph::HitInfo> {
        let index = self.syntax.as_ref().and_then(|f| f());
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            refs_graph::annotate(index.as_deref(), &self.root, symbol, locations)
        })
    }

    fn source_lines(&self, path: &str) -> Option<Arc<Vec<String>>> {
        refs_graph::with_file_cache(self.file_cache.as_ref(), || {
            refs_graph::read_lines(&self.root, path)
        })
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
        // The symbol wins over coordinates that disagree with it.
        let mut snap_note = None;
        let (line, character) = match (line, character, symbol) {
            (Some(l), Some(c), Some(name)) if !name.trim().is_empty() => {
                let snap = refs_graph::snap_position(
                    &self.root,
                    path,
                    name,
                    Position {
                        line: l,
                        character: c,
                    },
                );
                snap_note = snap.note;
                match snap.position {
                    Some(p) => (Some(p.line), Some(p.character)),
                    None => (None, None),
                }
            }
            other => (other.0, other.1),
        };
        let mut result = self.definition_at(language, &rel, symbol, line, character);
        if let Some(note) = snap_note {
            result.note = Some(match result.note.take().filter(|n| !n.is_empty()) {
                Some(rest) => format!("{note}\n{rest}"),
                None => note,
            });
        }
        result
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

impl LiveBackend {
    fn references_inner(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
        exact: bool,
    ) -> Precise<Vec<Location>> {
        let index = self.syntax.as_ref().and_then(|f| f());
        let mut notes: Vec<String> = Vec::new();
        let mut path: Option<String> = path.map(str::to_string);
        let mut position = position;
        // A missing / wrong path is fixed from the index when the symbol has
        // one declaring file; several candidates are listed instead.
        if let Some(ix) = index.as_deref() {
            match refs_graph::fix_path(ix, &self.root, symbol, path.as_deref()) {
                PathFix::Keep => {}
                PathFix::Use { path: fixed, note } => {
                    path = Some(fixed);
                    // The coordinates belonged to the other file.
                    position = None;
                    notes.push(note);
                }
                PathFix::Ambiguous { note } => return Precise::unknown(Vec::new(), note),
            }
        }
        let language =
            match server_language_from_opt_path(&self.root, path.as_deref(), "精确引用查询") {
                Ok(lang) => lang,
                Err(note) => return Precise::unknown(Vec::new(), note),
            };
        let rel = RelPath::new(path.as_deref().expect("language resolved"));
        // The symbol wins over coordinates that disagree with it.
        if let Some(pos) = position {
            let snap = refs_graph::snap_position(&self.root, rel.as_str(), symbol, pos);
            position = snap.position;
            notes.extend(snap.note);
        }
        let target = match position {
            Some(position) => SymbolAt::Position(position),
            None => SymbolAt::Name(symbol),
        };
        // Graph first: the language server is the memory-heavy fallback and
        // is not started when the index alone resolves the name. Renames
        // (`exact`) always go to the server.
        let graph = (!exact && refs_graph::graph_first_language(language))
            .then_some(index.as_deref())
            .flatten()
            .and_then(|ix| {
                let name = symbol.trim();
                (!name.is_empty())
                    .then(|| refs_graph::graph_references(ix, &self.root, name, &rel))
                    .flatten()
            });
        let mut result = match graph {
            Some(result) => result,
            None => self.server_references(language, &rel, target, index.as_deref()),
        };
        if language == Language::Go {
            if let Some(line) = self.go_implementors_line(&rel, target) {
                let note = result.note.get_or_insert_with(String::new);
                if !note.is_empty() && !note.ends_with('\n') {
                    note.push('\n');
                }
                note.push_str(&line);
            }
        }
        if !notes.is_empty() {
            let mut head = notes.join("\n");
            if let Some(rest) = result.note.take().filter(|n| !n.is_empty()) {
                head.push('\n');
                head.push_str(&rest);
            }
            result.note = Some(head);
        }
        result
    }
}

impl LiveBackend {
    fn definition_at(
        &self,
        language: Language,
        rel: &RelPath,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let rel = rel.clone();
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
                if (result.value.is_empty()
                    && (!busy || result.confidence == Confidence::Unknown))
                    || self.unresolved_call(language, &rel, target, &result.value) =>
            {
                let Some(name) = self.target_name(&rel, target) else {
                    return result;
                };
                let hits = self.syntactic_definitions(language, &rel, &name);
                if hits.is_empty() {
                    return result;
                }
                let note = if !result.value.is_empty() {
                    format!(
                        "{}没能解析这个调用（只指回了调用处本身，常见于没有 compile_commands.json 或缺 include 路径）；先给出 tree-sitter 语法索引里的同名声明 {} 处，按头文件/源文件配对和 include 关系排在前面（syntactic：按名字匹配，可能含同名的其他实体）。",
                        primary_server_name(language),
                        hits.len()
                    )
                } else if busy {
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
            Ok((result, _)) => self.with_paired_definitions(language, &rel, target, result),
            Err(error) => Precise::unknown(Vec::new(), error.to_string()),
        }
    }
}

#[cfg(test)]
mod start_failure_tests {
    use super::*;

    /// A missing kotlin server must surface its install hint, not loop on
    /// "still importing" forever.
    #[test]
    fn missing_kotlin_server_reports_real_error_after_background_failure() {
        let root = std::env::temp_dir();
        let empty_path = std::env::temp_dir().join("astrolabe-no-such-bin-dir");
        let pool = LspPool::from_discovery_and_transport(
            Discovery::new(&empty_path),
            StdioSpawn::new(&root),
            root.clone(),
            LspPoolConfig::default(),
        );
        let syntax: SyntaxIndex = Arc::new(|| None);
        let backend = LiveBackend {
            root,
            pool: Arc::new(pool),
            syntax: Some(syntax),
            starting: Arc::new(AtomicBool::new(false)),
            start_failed: Arc::new(AtomicBool::new(false)),
            file_cache: None,
        };
        // First call: warming while the background start runs.
        let first = backend.with_server(Language::Kotlin, |s| s.is_none());
        assert!(matches!(first, Ok(true)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !backend.start_failed.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "start never failed");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        while backend.starting.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Next call: the real acquire error, every time.
        for _ in 0..2 {
            assert!(backend
                .with_server(Language::Kotlin, |s| s.is_none())
                .is_err());
        }
    }
}

#[cfg(test)]
mod pair_tests {
    use super::*;
    use astrolabe_core::types::{CodeEdge, CodeFile, CodeSymbol, SymbolId, SymbolKind};

    fn graph() -> CodeGraph {
        let file = |id: u32, path: &str| CodeFile {
            id: FileId(id),
            path: RelPath::new(path),
            language: Some(Language::C),
            loc: 10,
            sha: String::new(),
        };
        let sym = |id: u32, file: u32, name: &str, line: u32| CodeSymbol {
            id: SymbolId(id),
            file: FileId(file),
            name: name.into(),
            kind: SymbolKind::Function,
            signature: String::new(),
            start_line: line,
            end_line: line + 3,
            exported: true,
        };
        let edge = |from: u32, to: u32, kind: EdgeKind| CodeEdge {
            from,
            to,
            kind,
            weight: 1,
            confidence: Confidence::Scoped,
        };
        CodeGraph {
            files: vec![
                file(0, "src/sds.h"),
                file(1, "src/sds.c"),
                file(2, "src/server.c"),
                file(3, "deps/other/sds.c"),
            ],
            symbols: vec![
                sym(0, 0, "sdsnew", 10),
                sym(1, 1, "sdsnew", 80),
                sym(2, 3, "sdsnew", 5),
                sym(3, 2, "main", 1),
            ],
            edges: vec![edge(2, 0, EdgeKind::Import), edge(0, 1, EdgeKind::Pair)],
        }
    }

    #[test]
    fn header_only_answer_gains_the_paired_source_definition() {
        let g = graph();
        let at = |line| Position { line, character: 0 };
        let found = vec![Location {
            path: RelPath::new("src/sds.h"),
            range: Range {
                start: at(9),
                end: at(9),
            },
        }];
        let extra = paired_definitions(&g, &found, "sdsnew");
        // The partner of sds.h only, not the unrelated deps/other/sds.c.
        assert_eq!(
            extra,
            vec![Location {
                path: RelPath::new("src/sds.c"),
                range: Range {
                    start: at(79),
                    end: at(79),
                },
            }]
        );
        assert!(paired_definitions(&g, &found, "nothere").is_empty());
    }

    #[test]
    fn near_files_follow_includes_and_their_partners() {
        let g = graph();
        let near = near_files(&g, &RelPath::new("src/server.c"));
        assert_eq!(near, HashSet::from([FileId(0), FileId(1)]));
        let near = near_files(&g, &RelPath::new("src/sds.c"));
        assert_eq!(near, HashSet::from([FileId(0)]));
        assert!(near_files(&g, &RelPath::new("missing.c")).is_empty());
        assert!(is_c_header("a/b.HPP") && !is_c_header("a/b.c") && !is_c_header("h"));
    }

    /// The old fallback order: origin file first, then by path.
    fn self_free_syntactic(index: &RepoIndex, origin: &RelPath, name: &str) -> Vec<Location> {
        let g = index.graph();
        let mut hits: Vec<(bool, Location)> = g
            .symbols
            .iter()
            .filter(|s| s.name == name)
            .filter_map(|s| {
                let f = g.files.iter().find(|f| f.id == s.file)?;
                let at = Position {
                    line: s.start_line.saturating_sub(1),
                    character: 0,
                };
                Some((
                    f.path != *origin,
                    Location {
                        path: f.path.clone(),
                        range: Range { start: at, end: at },
                    },
                ))
            })
            .collect();
        hits.sort();
        hits.into_iter().map(|(_, l)| l).collect()
    }

    /// Real clangd, no compile database. Ignored: needs
    /// `ASTROLABE_LSP_C=/path/to/clangd` and `ASTROLABE_TEST_C_REPO=/copy/of/redis`.
    #[test]
    #[ignore = "needs clangd and a C repo (ASTROLABE_LSP_C, ASTROLABE_TEST_C_REPO)"]
    fn live_clangd_definition_hops_from_header_to_source() {
        let Some(root) = std::env::var_os("ASTROLABE_TEST_C_REPO").map(PathBuf::from) else {
            return;
        };
        let max_cases: usize = std::env::var("PAIR_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let t = std::time::Instant::now();
        let index = Arc::new(crate::index::build(&root).expect("index"));
        eprintln!("index {:?}", t.elapsed());
        let g = index.graph();
        let pairs: Vec<(FileId, FileId)> = g
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Pair)
            .map(|e| (FileId(e.from), FileId(e.to)))
            .collect();
        eprintln!("pair edges {}", pairs.len());
        let path_of = |id: FileId| g.files.iter().find(|f| f.id == id).unwrap().path.clone();
        // Cases: a function with a prototype in header h and a body in its
        // partner s, called from another .c file.
        let mut cases: Vec<(RelPath, u32, u32, String, RelPath, u32)> = Vec::new();
        let c_files: Vec<&CodeFile> = g
            .files
            .iter()
            .filter(|f| f.path.as_str().ends_with(".c"))
            .collect();
        'outer: for (h, s) in &pairs {
            let mut names: Vec<(&str, u32)> = g
                .symbols
                .iter()
                .filter(|x| x.file == *s && x.kind == SymbolKind::Function)
                .filter(|x| g.symbols.iter().any(|y| y.file == *h && y.name == x.name))
                .map(|x| (x.name.as_str(), x.start_line))
                .collect();
            names.sort();
            names.dedup_by(|a, b| a.0 == b.0);
            for (name, def_line) in names.into_iter().take(3) {
                for f in &c_files {
                    if f.id == *s {
                        continue;
                    }
                    let Ok(text) = std::fs::read_to_string(root.join(f.path.as_str())) else {
                        continue;
                    };
                    let needle = format!("{name}(");
                    let hit = text.lines().enumerate().find_map(|(i, l)| {
                        let c = l.find(&needle)?;
                        let before = l[..c].chars().last();
                        (!l.trim_start().starts_with('#')
                            && !before.is_some_and(|b| b.is_alphanumeric() || b == '_'))
                        .then_some((i as u32, c as u32))
                    });
                    if let Some((line, col)) = hit {
                        cases.push((
                            f.path.clone(),
                            line,
                            col,
                            name.to_string(),
                            path_of(*s),
                            def_line,
                        ));
                        if cases.len() >= max_cases {
                            break 'outer;
                        }
                        break;
                    }
                }
            }
        }
        eprintln!("cases {}", cases.len());
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let (mut raw_ok, mut raw_header_only, mut after_ok, mut raw_empty, mut old_ok) =
            (0, 0, 0, 0, 0);
        let mut hop_time = std::time::Duration::ZERO;
        for (path, line, col, name, src, def_line) in &cases {
            let target = SymbolAt::Position(Position {
                line: *line,
                character: *col,
            });
            let raw = backend
                .with_server(Language::C, |server| {
                    goto_definition(server, &root, path, target)
                })
                .expect("clangd");
            let _ = def_line;
            let hits_def = |locs: &[Location]| locs.iter().any(|l| l.path == *src);
            if raw.value.is_empty() {
                raw_empty += 1;
            }
            // What the previous code answered: the server's list as is, or
            // (empty only) the syntactic list in path order.
            let old: Vec<Location> = if raw.value.is_empty() {
                let mut v: Vec<Location> = self_free_syntactic(&index, path, name);
                v.truncate(MAX_SYNTACTIC_DEFINITIONS);
                v
            } else {
                raw.value.clone()
            };
            if hits_def(&old) {
                old_ok += 1;
            }
            if hits_def(&raw.value) {
                raw_ok += 1;
            }
            if !raw.value.is_empty() && raw.value.iter().all(|l| is_c_header(l.path.as_str())) {
                raw_header_only += 1;
            }
            let t = std::time::Instant::now();
            let after = backend.definition(path.as_str(), None, Some(*line), Some(*col));
            hop_time += t.elapsed();
            if hits_def(&after.value) {
                after_ok += 1;
            } else {
                eprintln!(
                    "miss: {} {}:{} -> {:?}",
                    name,
                    path.as_str(),
                    line + 1,
                    after.value
                );
            }
        }
        eprintln!(
            "cases={} old_correct={} raw_correct={} raw_header_only={} raw_empty={} after_correct={} full_definition_total={:?}",
            cases.len(),
            old_ok,
            raw_ok,
            raw_header_only,
            raw_empty,
            after_ok,
            hop_time
        );
        backend.pool.shutdown();
    }
}

#[cfg(test)]
mod go_implementor_tests {
    use super::*;

    #[test]
    fn go_interface_references_note_lists_implementors() {
        let dir = std::env::temp_dir().join(format!("astrolabe-lb-goiface-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("p")).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(
            dir.join("p/i.go"),
            "package p\n\ntype Shape interface {\n\tArea() float64\n}\n\ntype Sq struct{}\n\nfunc (Sq) Area() float64 { return 1 }\n",
        )
        .unwrap();
        let index =
            Arc::new(crate::index::build_with_options(&dir, false, 1 << 20).expect("index"));
        let backend =
            LiveBackend::new(dir.clone()).with_syntax_index(Arc::new(move || Some(index.clone())));
        let rel = RelPath::new("p/i.go");
        let line = backend
            .go_implementors_line(&rel, SymbolAt::Name("Shape"))
            .expect("implementors");
        assert!(line.contains("Shape @p/i.go:3 ← Sq @p/i.go:7"), "{line}");
        assert!(backend
            .go_implementors_line(&rel, SymbolAt::Name("Sq"))
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Graph-vs-language-server recall harness for `find_references`
/// (batch 6). Ignored: needs real servers and corpora.
///
/// `REFS_RECALL_REPO=/abs/repo` and either `REFS_RECALL_CASES="path:sym,…"`
/// or `REFS_RECALL_SAMPLE=<lang ext>:<n>` (unique-declaration symbols from
/// non-test files, every k-th). Prints one row per case and a summary line.
#[cfg(test)]
mod recall_harness {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    #[ignore = "needs language servers and a corpus (REFS_RECALL_REPO)"]
    fn graph_vs_server_recall() {
        let Some(root) = std::env::var_os("REFS_RECALL_REPO").map(PathBuf::from) else {
            return;
        };
        let index = Arc::new(crate::index::build(&root).expect("index"));
        let g = index.graph();
        let mut cases: Vec<(String, String)> = Vec::new();
        if let Ok(list) = std::env::var("REFS_RECALL_CASES") {
            for c in list.split(',').filter(|c| !c.is_empty()) {
                let (p, s) = c.rsplit_once(':').expect("path:sym");
                cases.push((p.to_string(), s.to_string()));
            }
        }
        if let Ok(spec) = std::env::var("REFS_RECALL_SAMPLE") {
            let (exts, n) = spec.rsplit_once(':').expect("ext:n");
            let n: usize = n.parse().unwrap();
            let exts: Vec<&str> = exts.split('|').collect();
            let mut pool: Vec<(String, String)> = Vec::new();
            let mut count: std::collections::HashMap<&str, usize> = Default::default();
            for s in &g.symbols {
                *count.entry(s.name.as_str()).or_default() += 1;
            }
            for s in &g.symbols {
                let Some(f) = index.file(s.file) else {
                    continue;
                };
                let p = f.path.as_str();
                if !exts.iter().any(|e| p.ends_with(e))
                    || crate::explain::is_test_caller(p, &s.name)
                    || count[s.name.as_str()] != 1
                    || s.name.len() < 4
                    || !matches!(
                        s.kind,
                        astrolabe_core::SymbolKind::Function
                            | astrolabe_core::SymbolKind::Method
                            | astrolabe_core::SymbolKind::Class
                            | astrolabe_core::SymbolKind::Interface
                            | astrolabe_core::SymbolKind::Struct
                            | astrolabe_core::SymbolKind::Type
                    )
                {
                    continue;
                }
                pool.push((p.to_string(), s.name.clone()));
            }
            pool.sort();
            let step = (pool.len() / n.max(1)).max(1);
            cases.extend(pool.into_iter().step_by(step).take(n));
        }
        let backend = LiveBackend::new(&root).with_syntax_index(Arc::new({
            let index = Arc::clone(&index);
            move || Some(Arc::clone(&index))
        }));
        let key = |l: &Location| {
            (
                l.path.as_str().to_string(),
                l.range.start.line,
                l.range.start.character,
            )
        };
        let (mut answered, mut deferred, mut exact_eq) = (0usize, 0usize, 0usize);
        let (mut tp, mut ls, mut gs) = (0usize, 0usize, 0usize);
        for (path, sym) in &cases {
            let rel = RelPath::new(path);
            let Some(lang) = server_language(&root, path) else {
                continue;
            };
            let t = std::time::Instant::now();
            let lsp = backend
                .with_server(lang, |s| {
                    find_references(s, &root, &rel, SymbolAt::Name(sym))
                })
                .expect("server");
            let lsp_ms = t.elapsed().as_millis();
            let t = std::time::Instant::now();
            let graph = refs_graph::graph_references(&index, &root, sym, &rel);
            let graph_ms = t.elapsed().as_millis();
            let l: BTreeSet<_> = lsp.value.iter().map(key).collect();
            match graph {
                None => {
                    deferred += 1;
                    println!(
                        "ROW\t{path}\t{sym}\tlsp={}\t{:?}\tgraph=defer\t{lsp_ms}ms",
                        l.len(),
                        lsp.confidence
                    );
                }
                Some(gr) => {
                    answered += 1;
                    let gset: BTreeSet<_> = gr.value.iter().map(key).collect();
                    let inter = l.intersection(&gset).count();
                    tp += inter;
                    ls += l.len();
                    gs += gset.len();
                    if l == gset {
                        exact_eq += 1;
                    }
                    let missed: Vec<_> = l.difference(&gset).take(3).collect();
                    let extra: Vec<_> = gset.difference(&l).take(3).collect();
                    println!(
                        "ROW\t{path}\t{sym}\tlsp={}\t{:?}\tgraph={}\tboth={inter}\t{lsp_ms}ms/{graph_ms}ms\tmissed={missed:?}\textra={extra:?}",
                        l.len(),
                        lsp.confidence,
                        gset.len()
                    );
                }
            }
        }
        println!(
            "SUMMARY\tcases={}\tgraph_answered={answered}\tdeferred={deferred}\tidentical={exact_eq}\trecall={:.3}\tprecision={:.3}\t(tp={tp} lsp={ls} graph={gs})",
            cases.len(),
            tp as f64 / ls.max(1) as f64,
            tp as f64 / gs.max(1) as f64
        );
        backend.pool.shutdown();
    }
}

#[cfg(test)]
mod graph_first_tests {
    use super::*;

    fn go_repo() -> (PathBuf, LiveBackend) {
        let dir =
            std::env::temp_dir().join(format!("astrolabe-lb-graphfirst-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("service")).unwrap();
        std::fs::create_dir_all(dir.join("controller")).unwrap();
        std::fs::write(dir.join("go.mod"), "module example.com/m\n").unwrap();
        std::fs::write(
            dir.join("service/channel.go"),
            "package service\n\n// ShouldDisableChannel decides.\nfunc ShouldDisableChannel() bool { return false }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("controller/relay.go"),
            "package controller\n\nimport \"example.com/m/service\"\n\nfunc R() {\n\t_ = service.ShouldDisableChannel()\n}\n",
        )
        .unwrap();
        let index =
            Arc::new(crate::index::build_with_options(&dir, false, 1 << 20).expect("index"));
        let backend =
            LiveBackend::new(dir.clone()).with_syntax_index(Arc::new(move || Some(index.clone())));
        (dir, backend)
    }

    /// Unambiguous Go name, wrong path, off coordinates: answered from the
    /// index, the path fixed, and no language server started.
    #[test]
    fn go_references_answer_from_the_index_without_starting_gopls() {
        let (dir, backend) = go_repo();
        let r = backend.references(
            "ShouldDisableChannel",
            Some("relay/channel/disable.go"),
            Some(Position {
                line: 3,
                character: 0,
            }),
            false,
        );
        assert_eq!(r.confidence, Confidence::Scoped, "{r:?}");
        let note = r.note.clone().unwrap();
        assert!(
            note.contains("不存在") && note.contains("service/channel.go"),
            "{note}"
        );
        assert!(note.contains("未启动语言服务器"), "{note}");
        assert!(!note.contains("Kotlin"), "{note}");
        let got: Vec<String> = r
            .value
            .iter()
            .map(|l| format!("{}:{}", l.path.as_str(), l.range.start.line + 1))
            .collect();
        assert_eq!(got, vec!["controller/relay.go:6", "service/channel.go:4"]);
        assert!(!backend.pool.is_resident(Language::Go));
        // No path at all: same answer.
        let r = backend.references("ShouldDisableChannel", None, None, false);
        assert_eq!(r.value.len(), 2, "{r:?}");
        // Annotation: declaration and enclosing function.
        let info = backend.annotate("ShouldDisableChannel", &r.value);
        assert!(info.iter().any(|h| h.declaration));
        assert!(info
            .iter()
            .any(|h| h.enclosing.as_ref().is_some_and(|e| e.0 == "R")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_note_mentions_kotlin_only_for_kotlin() {
        assert!(empty_note(Language::Kotlin, "引用").contains("Android"));
        for lang in [
            Language::Go,
            Language::TypeScript,
            Language::Python,
            Language::Rust,
        ] {
            let n = empty_note(lang, "引用");
            assert!(!n.contains("Kotlin") && !n.contains("Android"), "{n}");
        }
        assert!(empty_note(Language::Go, "引用").contains("go.mod"));
    }
}
