use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked, TokenBudget},
    cache::{CacheConfig, FileCache, DEFAULT_MEMORY_BUDGET_BYTES},
    graph::{compute_centrality, dependencies, dependents, rank_for_task},
    render::{confidence_note, symbol_line},
    Confidence, EdgeKind, FileId,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CacheScope, CallToolResult, ContentBlock, Implementation, InitializeRequestParams,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    context::ClientContext,
    index::{self, RepoIndex},
    openai_schema::sanitize_for_openai_tools,
    TOOL_CATALOG_TTL_MS,
};

const DEFAULT_BUDGET: usize = 2_500;
const RESOURCE_THRESHOLD_TOKENS: usize = 4_000;
const BYTES_PER_MB: u64 = 1024 * 1024;

fn default_budget() -> usize {
    DEFAULT_BUDGET
}

/// Heuristic: query looks like a regex but `regex` was left false.
/// `|` / `.*` / `\b` 是上一会话假阴性的主因；单独的 `.` 不警告（`foo.bar` 常为字面搜索）。
fn looks_like_regex_query(query: &str) -> bool {
    query.contains('|')
        || query.contains(".*")
        || query.contains(".+")
        || query.contains("\\b")
        || query.contains("\\s")
        || query.contains("\\d")
        || query.contains("\\w")
        || query.contains("(?")
        || (query.starts_with('^') && query.len() > 1)
        || (query.ends_with('$') && query.len() > 1 && !query.ends_with("\\$"))
}

fn quote_query_for_note(query: &str) -> String {
    const MAX: usize = 80;
    let count = query.chars().count();
    if count <= MAX {
        return query.to_string();
    }
    let clipped: String = query.chars().take(MAX).collect();
    format!("{clipped}…")
}

fn render_search_mode_header(regex: bool, query: &str) -> String {
    let mode = if regex { "regex" } else { "literal" };
    let mut header = format!("mode: {mode}\n");
    if !regex && looks_like_regex_query(query) {
        header.push_str(&format!(
            "注意：query 含正则元字符，但 regex=false，已按字面搜索「{}」。要按正则请传 regex=true；多符号请拆成多次字面搜索。\n",
            quote_query_for_note(query)
        ));
    }
    header
}

/// Claude Code 默认丢弃 structuredContent，截断声明必须出现在正文。
fn render_budget_status(shown: usize, omitted: usize, budget_tokens: usize) -> String {
    let truncated = omitted > 0;
    let mut status = format!("shown={shown} omitted={omitted} truncated={truncated}\n");
    if truncated {
        status.push_str(&format!(
            "这不是全集：{omitted} 条因 budget_tokens={budget_tokens} 被省略。加大 budget_tokens 或收紧 path_filter 后再查；本工具没有翻页。\n"
        ));
    }
    status
}

/// Parse `ASTROLABE_CACHE_MB` (whole megabytes). Invalid values fall back to
/// the core default so a typo cannot silently disable the ceiling.
fn parse_cache_budget_mb(raw: Option<&str>) -> u64 {
    match raw {
        None => DEFAULT_MEMORY_BUDGET_BYTES,
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(mb) => mb.saturating_mul(BYTES_PER_MB),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_CACHE_MB; using default"
                );
                DEFAULT_MEMORY_BUDGET_BYTES
            }
        },
    }
}

fn cache_budget_from_env() -> u64 {
    parse_cache_budget_mb(std::env::var("ASTROLABE_CACHE_MB").ok().as_deref())
}

/// `ASTROLABE_STRUCTURED` overrides the context default when set to a
/// recognised true/false token. Unset or unrecognised → context value
/// (`null`/auto resolves to off).
fn structured_output_override() -> Option<bool> {
    match std::env::var("ASTROLABE_STRUCTURED")
        .ok()
        .as_deref()
        .map(str::trim)
    {
        Some("1" | "true" | "TRUE" | "yes" | "YES") => Some(true),
        Some("0" | "false" | "FALSE" | "no" | "NO") => Some(false),
        _ => None,
    }
}

fn structured_output_for(context: &ClientContext) -> bool {
    structured_output_override().unwrap_or_else(|| context.structured_or_auto_off())
}

/// `FileCache` is not `Debug`; wrap it so `AstrolabeServer` can keep its derive.
#[derive(Clone)]
struct SharedFileCache(Arc<FileCache>);

impl Deref for SharedFileCache {
    type Target = FileCache;

    fn deref(&self) -> &FileCache {
        &self.0
    }
}

impl fmt::Debug for SharedFileCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileCache")
            .field("weighted_size", &self.weighted_size())
            .finish()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct BudgetOnly {
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ResolveContextParams {
    #[schemars(description = "Natural-language coding task used to rank relevant context")]
    pub task_description: String,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QueryParams {
    #[schemars(
        description = "Symbol name, substring, or hierarchical path like 'Class/method' ('/Abs/Path' anchors at file top level)"
    )]
    pub query: String,
    /// 对展示列表前 5 个命中符号附带源码体（include_body，对齐 Serena）。
    #[serde(default)]
    #[schemars(
        description = "Include source bodies for up to 5 displayed matches (Serena include_body equivalent; bodies count against budget_tokens)"
    )]
    pub include_body: bool,
    /// 路径查询（含 /）时的最后段子串开关（Serena substring_matching）。
    #[serde(default)]
    #[schemars(description = "Path queries: substring-match the last segment (default exact)")]
    pub substring: bool,
    /// 路径查询：kind 过滤（Function/Method/Class/Struct/Trait/...）。
    #[serde(default)]
    #[schemars(description = "Path queries: filter results by SymbolKind name")]
    pub kind: Option<String>,
    /// 路径查询：展开匹配者的嵌套子符号层数（0=不展开）。
    #[serde(default)]
    #[schemars(
        description = "Path queries: expand nested children of matches to this depth (0 = matches only)"
    )]
    pub depth: u8,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

/// Valid `kind` filter names for path queries (case-insensitive).
const VALID_KIND_NAMES: &str =
    "function, method, class, interface, struct, enum, trait, type, const, module, field";

/// SymbolKind 名字（schema 字符串）→ 枚举。未知名字返回 Err（列出有效值）。
fn kind_from_name(name: &str) -> Result<astrolabe_core::SymbolKind, String> {
    use astrolabe_core::SymbolKind as K;
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "function" => K::Function,
        "method" => K::Method,
        "class" => K::Class,
        "interface" => K::Interface,
        "struct" => K::Struct,
        "enum" => K::Enum,
        "trait" => K::Trait,
        "type" => K::Type,
        "const" => K::Const,
        "module" => K::Module,
        "field" => K::Field,
        other => {
            return Err(format!("未知 kind `{other}`；有效值：{VALID_KIND_NAMES}"));
        }
    })
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct SearchParams {
    #[schemars(
        description = "Search string. Literal case-insensitive substring unless regex=true. Do not put A|B here without regex=true — that searches for a vertical bar."
    )]
    pub query: String,
    #[serde(default)]
    #[schemars(
        description = "Default false: treat query as literal text. Set true to compile query as a regular expression. Required for A|B, .*, \\b, and other regex syntax."
    )]
    pub regex: bool,
    #[serde(default)]
    #[schemars(
        description = "Optional case-sensitive substring of the repo-relative path (e.g. relay/channel). Not a glob. Do not pass an absolute path or a ./ prefix."
    )]
    pub path_filter: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(
        description = "Maximum approximate tokens in the rendered result (default 2500). Overflow drops later hits and sets truncated=true; raise this or tighten path_filter — there is no page or cursor."
    )]
    pub budget_tokens: usize,
}

fn default_direction() -> String {
    "dependents".into()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct DependentsParams {
    #[schemars(description = "Repo-relative path or unique path suffix")]
    pub target: String,
    #[serde(default = "default_direction")]
    #[schemars(description = "dependents or dependencies")]
    pub direction: String,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

fn default_call_direction() -> String {
    "both".into()
}

fn default_neighborhood_depth() -> u16 {
    2
}

/// Trace / group-graph depth default: docs say 1 (flat / top-level).
fn default_trace_or_group_depth() -> u16 {
    1
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GroupGraphParams {
    /// 1..=3，超域钳制。
    #[serde(default = "default_trace_or_group_depth")]
    #[schemars(description = "Folder grouping depth 1-3 (default 1 = top-level dirs)")]
    pub depth: u16,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct MemoryNameParams {
    #[schemars(description = "Memory name ([A-Za-z0-9_-], <=64 chars)")]
    pub name: String,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct MemoryContentParams {
    #[schemars(description = "Memory name ([A-Za-z0-9_-], <=64 chars)")]
    pub name: String,
    #[schemars(description = "Markdown content")]
    pub content: String,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct NeighborhoodParams {
    #[schemars(description = "Repo-relative path or unique path suffix")]
    pub target: String,
    #[serde(default = "default_call_direction")]
    #[schemars(description = "dependents, dependencies, or both")]
    pub direction: String,
    /// 1..=3，超域钳制（openvisio 同款上限）。u16 让 256+ 先能反序列化再钳。
    #[serde(default = "default_neighborhood_depth")]
    #[schemars(description = "BFS hops over import edges, 1-3 (default 2)")]
    pub depth: u16,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct TraceParams {
    pub symbol: String,
    #[serde(default = "default_call_direction")]
    #[schemars(description = "callers, callees, or both")]
    pub direction: String,
    /// \>1 时走多跳调用树（1..=6，超出钳制；默认 1 保持旧行为）。u16 让
    /// 256+ 先能反序列化再钳。
    #[serde(default = "default_trace_or_group_depth")]
    #[schemars(
        description = "Trace depth 1-6; >1 renders a tree with cycle markers (default 1 = flat)"
    )]
    pub depth: u16,
    #[serde(default = "default_budget")]
    pub budget_tokens: usize,
}

#[derive(Debug)]
enum IndexState {
    Building,
    Ready(Arc<RepoIndex>),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct AstrolabeServer {
    root: PathBuf,
    state: Arc<RwLock<IndexState>>,
    resources: Arc<Mutex<BTreeMap<String, String>>>,
    next_resource: Arc<AtomicU64>,
    file_cache: SharedFileCache,
    cache_budget_bytes: u64,
    cache_hits: Arc<AtomicU64>,
    cache_misses: Arc<AtomicU64>,
    /// Kept alive for the process lifetime so the watch thread is not stopped
    /// by `WatchHandle::drop`. `None` when watching is disabled.
    watch: Arc<Mutex<Option<astrolabe_core::watch::WatchHandle>>>,
    /// Backend for the language-server-backed tools. Defaults to a reporting
    /// stub until a real server is wired in, so the tools stay callable and
    /// explain what is missing instead of vanishing from the catalog.
    precise: Arc<dyn crate::precise_tools::PreciseCapability>,
    /// Named client context (`--context` / `ASTROLABE_CONTEXT`). Drives the
    /// structured-output default and optional `excluded_tools` catalog filter.
    context: ClientContext,
    /// Claude Code substitutes `structuredContent` for `content[0].text`.
    /// Default off (`default` / `claude-code`). `ASTROLABE_STRUCTURED` overrides.
    structured_output: bool,
    /// git churn 表缓存（get_hotspots 用）：每次调用都 spawn `git log` 会把
    /// 最坏 10s 的子进程超时压进 serve 循环（审查 major）。TTL 内复用。
    churn_cache: Arc<Mutex<Option<(std::time::Instant, astrolabe_core::churn::ChurnTable)>>>,
    tool_router: ToolRouter<Self>,
}

/// Index once, converting a panic in the engine into a reportable failure
/// rather than taking the server down.
fn build_index(root: &Path) -> IndexState {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| index::build(root)))
        .map_err(|_| "底层索引能力尚未实现或发生 panic".to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
        .map(Arc::new)
        .map(IndexState::Ready)
        .unwrap_or_else(IndexState::Failed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchMode {
    Events,
    Poll(Duration),
}

/// Watch the repository and rebuild the graph when files change.
///
/// The previous index keeps serving for the whole rebuild: dropping back to
/// `Building` would make every tool call fail for a second because someone
/// saved a file. A stale answer beats no answer here, and the window is short.
/// A failed rebuild is also discarded rather than published, so one bad edit
/// cannot replace a working index with an error.
fn start_watching(
    root: PathBuf,
    state: Arc<RwLock<IndexState>>,
    slot: Arc<Mutex<Option<astrolabe_core::watch::WatchHandle>>>,
    mode: WatchMode,
) {
    let watcher = astrolabe_core::watch::Watcher::new(root.clone());
    let (rx, handle) = match mode {
        WatchMode::Events => {
            tracing::info!("watching for changes (native OS events)");
            watcher.spawn()
        }
        WatchMode::Poll(interval) => {
            tracing::info!(
                interval_secs = interval.as_secs(),
                "watching for changes (forced polling)"
            );
            watcher.poll_interval(interval).events(false).spawn()
        }
    };
    *slot.lock().expect("watch lock poisoned") = Some(handle);

    std::thread::Builder::new()
        .name("astrolabe-reindex".into())
        .spawn(move || {
            while let Ok(changes) = rx.recv() {
                // Drain any pending changesets accumulated during debounce/wake-up.
                // Reindexing is a full rebuild, so batching back-to-back notifications
                // prevents redundant rebuild bursts during heavy file activity.
                while rx.try_recv().is_ok() {}

                tracing::info!(
                    added = changes.added.len(),
                    modified = changes.modified.len(),
                    removed = changes.removed.len(),
                    "reindexing after file changes"
                );
                match build_index(&root) {
                    IndexState::Ready(index) => {
                        *state.write().expect("index state lock poisoned") =
                            IndexState::Ready(index);
                    }
                    IndexState::Failed(error) => {
                        tracing::warn!(%error, "reindex failed; keeping previous index");
                    }
                    IndexState::Building => unreachable!("build_index never returns Building"),
                }
            }
        })
        .expect("failed to spawn reindex thread");
}

/// Parse watcher mode from the environment variable value:
/// - Unset/None: `Some(WatchMode::Events)` (default: native OS events)
/// - "0": `None` (watching disabled)
/// - N > 0: `Some(WatchMode::Poll(Duration::from_secs(N)))` (forced polling fallback)
/// - Invalid/non-integer: logs warning and defaults to `Some(WatchMode::Events)`
fn parse_watch_mode(raw: Option<&str>) -> Option<WatchMode> {
    match raw {
        None => Some(WatchMode::Events),
        Some(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(secs) => Some(WatchMode::Poll(Duration::from_secs(secs))),
            Err(_) => {
                tracing::warn!(
                    value = %raw,
                    "invalid ASTROLABE_WATCH_SECS; using default events watcher"
                );
                Some(WatchMode::Events)
            }
        },
    }
}

fn watch_mode_from_env() -> Option<WatchMode> {
    parse_watch_mode(std::env::var("ASTROLABE_WATCH_SECS").ok().as_deref())
}

impl AstrolabeServer {
    pub fn new(root: PathBuf) -> Self {
        Self::with_context(root, ClientContext::default_builtin())
    }

    pub fn with_context(root: PathBuf, context: ClientContext) -> Self {
        Self::new_with_cache_budget_and_context(root, cache_budget_from_env(), context)
    }

    #[allow(dead_code)]
    pub(crate) fn new_with_cache_budget(root: PathBuf, budget_bytes: u64) -> Self {
        Self::new_with_cache_budget_and_context(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
        )
    }

    pub(crate) fn new_with_cache_budget_and_context(
        root: PathBuf,
        budget_bytes: u64,
        context: ClientContext,
    ) -> Self {
        // Detection (`.` → walk up for `.git` / `.serena/project.yml`) lives
        // in `main` / `resolve_index_root`. Here we only store an absolute
        // path so every tool result can declare it.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let structured_output = structured_output_for(&context);
        tracing::info!(
            root = %root.display(),
            context = %context.name,
            structured_output,
            cache_budget_bytes = budget_bytes,
            "configured bounded file cache"
        );
        // Real pool: servers start on the first precise query and are
        // reclaimed when idle, so the graph-only tools pay nothing resident.
        let precise: Arc<dyn crate::precise_tools::PreciseCapability> =
            Arc::new(crate::live_backend::LiveBackend::new(root.clone()));
        Self {
            root,
            state: Arc::new(RwLock::new(IndexState::Building)),
            resources: Arc::new(Mutex::new(BTreeMap::new())),
            next_resource: Arc::new(AtomicU64::new(1)),
            file_cache: SharedFileCache(Arc::new(FileCache::new(CacheConfig {
                budget_bytes,
                ..CacheConfig::default()
            }))),
            cache_budget_bytes: budget_bytes,
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
            watch: Arc::new(Mutex::new(None)),
            precise,
            context,
            structured_output,
            churn_cache: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// churn 表带 TTL 缓存（600s）：避免每次 get_hotspots 同步跑 git log
    /// 子进程（审查 major：最坏 10s 阻塞 serve 循环）。
    fn cached_churn(&self) -> astrolabe_core::churn::ChurnTable {
        const CHURN_TTL: Duration = Duration::from_secs(600);
        {
            let guard = self.churn_cache.lock().expect("churn cache lock poisoned");
            if let Some((at, table)) = guard.as_ref() {
                if at.elapsed() < CHURN_TTL {
                    return table.clone();
                }
            }
        }
        let table = astrolabe_core::churn::compute_churn(&self.root);
        *self.churn_cache.lock().expect("churn cache lock poisoned") =
            Some((std::time::Instant::now(), table.clone()));
        table
    }

    pub fn start_indexing(&self) {
        let root = self.root.clone();
        let state = Arc::clone(&self.state);
        let watch_slot = Arc::clone(&self.watch);
        tokio::task::spawn_blocking(move || {
            let outcome = build_index(&root);
            let ready = matches!(outcome, IndexState::Ready(_));
            *state.write().expect("index state lock poisoned") = outcome;
            if ready {
                // Start watching immediately after the first index lands.
                // Anything edited between the two would otherwise be folded
                // into the watcher's baseline and never reported.
                if let Some(mode) = watch_mode_from_env() {
                    start_watching(root, state, watch_slot, mode);
                }
            }
        });
    }

    /// Stop the watch thread. Called on shutdown; also lets tests avoid
    /// leaving a poller running.
    pub fn stop_watching(&self) {
        if let Some(handle) = self.watch.lock().expect("watch lock poisoned").take() {
            handle.stop();
        }
    }

    fn with_index(&self, f: impl FnOnce(&RepoIndex) -> CallToolResult) -> CallToolResult {
        match &*self.state.read().expect("index state lock poisoned") {
            IndexState::Building => self.finish(text_error(
                "索引正在后台构建；MCP 握手已完成，请稍后重试。confidence: unknown",
            )),
            IndexState::Failed(error) => self.finish(text_error(format!(
                "索引构建失败：{error}\nconfidence: unknown"
            ))),
            IndexState::Ready(index) => f(index),
        }
    }

    /// Catalog for `tools/list`: drop `excluded_tools`, optionally sanitize
    /// schemas for OpenAI/Codex, then sort by name.
    fn listed_tools(&self) -> Vec<rmcp::model::Tool> {
        let mut tools = self.tool_router.list_all();
        tools.retain(|tool| !self.context.excludes(tool.name.as_ref()));
        if self.context.openai_tool_compatible() {
            for tool in &mut tools {
                let schema = Value::Object((*tool.input_schema).clone());
                if let Value::Object(sanitized) = sanitize_for_openai_tools(schema) {
                    tool.input_schema = Arc::new(sanitized);
                }
            }
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    fn finish(&self, mut result: CallToolResult) -> CallToolResult {
        let root = self.root.display().to_string();
        let mut first_text = None;
        let mut content = Vec::with_capacity(result.content.len());
        for (index, block) in result.content.into_iter().enumerate() {
            if index == 0 {
                if let Some(existing) = block.as_text() {
                    let text = if existing.text.starts_with("index_root:") {
                        existing.text.clone()
                    } else {
                        format!("index_root: {root}\n{}", existing.text)
                    };
                    first_text = Some(text.clone());
                    content.push(ContentBlock::text(text));
                    continue;
                }
            }
            content.push(block);
        }
        result.content = content;

        if self.structured_output {
            let text = first_text.unwrap_or_default();
            let mut structured = result
                .structured_content
                .take()
                .unwrap_or_else(|| json!({}));
            if let Some(obj) = structured.as_object_mut() {
                obj.insert("root".into(), json!(root));
                obj.insert("text".into(), json!(text));
            }
            result.structured_content = Some(structured);
        } else {
            result.structured_content = None;
        }
        result
    }

    fn result(&self, title: &str, body: String, structured: Value) -> CallToolResult {
        let body = if body.trim().is_empty() {
            "没有匹配结果。confidence: unknown".to_string()
        } else {
            body
        };
        if estimate_tokens(&body) > RESOURCE_THRESHOLD_TOKENS {
            let id = self.next_resource.fetch_add(1, Ordering::Relaxed);
            let uri = format!("astrolabe://results/{id}");
            self.resources
                .lock()
                .expect("resource lock poisoned")
                .insert(uri.clone(), body.clone());
            let resource = Resource::new(&uri, title)
                .with_description("Astrolabe 大型工具结果；通过 resources/read 获取完整文本")
                .with_mime_type("text/plain")
                .with_size(body.len() as u64);
            let mut result = CallToolResult::success(vec![
                ContentBlock::text(format!(
                    "{title} 结果较大（约 {} tokens），完整内容见资源：{uri}",
                    estimate_tokens(&body)
                )),
                ContentBlock::resource_link(resource),
            ]);
            result.structured_content = Some(structured);
            self.finish(result)
        } else {
            let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
            result.structured_content = Some(structured);
            self.finish(result)
        }
    }

    fn ranked_files(
        &self,
        index: &RepoIndex,
        ids: impl IntoIterator<Item = FileId>,
        budget: usize,
        confidence: Confidence,
    ) -> String {
        let items: Vec<_> = ids.into_iter().filter_map(|id| index.file(id)).collect();
        let (kept, omitted) = truncate_ranked(&items, budget, |file| {
            format!("@{}:1 ({} LOC)\n", file.path, file.loc)
        });
        let mut text = format!(
            "confidence: {:?} {}\n",
            confidence,
            confidence_note(confidence)
        );
        for file in kept {
            text.push_str(&format!("@{}:1 ({} LOC)\n", file.path, file.loc));
        }
        if omitted > 0 {
            text.push_str(&format!(
                "{omitted} 个较低排名结果因 budget_tokens 被省略。\n"
            ));
        }
        text
    }
}

#[tool_router]
impl AstrolabeServer {
    /// L2 bootstrap（Serena 模式）：返回完整《Astrolabe Instructions Manual》——
    /// 工具选择军规（claude-code 语境为 FORBIDDEN 级，default 为温和建议级）。
    /// 不依赖索引，任何时候可调用。
    #[tool(
        description = "CRITICAL: read this first — the Astrolabe Instructions Manual (tool-selection doctrine for this client). Call once at the start of a coding task."
    )]
    pub(crate) fn initial_instructions(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        let manual = crate::instructions::manual(&self.context.name).to_string();
        let body = truncate_body_text(&manual, params.budget_tokens);
        self.result(
            "initial_instructions",
            body,
            json!({
                "confidence": "exact",
                "context": self.context.name,
                "budget_tokens": params.budget_tokens,
            }),
        )
    }

    #[tool(
        description = "主入口：根据编码任务聚合最相关文件、符号、依赖与索引完整性信息。FIRST CALL for any coding task — before Read/Grep."
    )]
    pub(crate) fn resolve_context(
        &self,
        Parameters(params): Parameters<ResolveContextParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let base = compute_centrality(&index.graph);
            let ranked = rank_for_task(&index.graph, &params.task_description, &base);
            let mut ids: Vec<_> = index.graph.files.iter().map(|file| file.id).collect();
            ids.sort_by(|a, b| {
                ranked
                    .by_file
                    .get(b)
                    .copied()
                    .unwrap_or_default()
                    .partial_cmp(&ranked.by_file.get(a).copied().unwrap_or_default())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            });
            let mut body = self.ranked_files(
                index,
                ids.iter().copied(),
                params.budget_tokens,
                Confidence::Scoped,
            );
            body.push_str(&format!(
                "index: scanned={}, parsed={}, parse_failures={}, unresolved_imports={}\n",
                index.report.files_scanned,
                index.report.files_parsed,
                index.report.parse_failures.len(),
                index.report.unresolved_imports.len()
            ));
            let mut remaining =
                TokenBudget::new(params.budget_tokens.saturating_sub(estimate_tokens(&body)));
            let mut omitted_symbols = 0;
            for id in ids.into_iter().take(12) {
                if let Some(file) = index.file(id) {
                    for symbol in index
                        .graph
                        .symbols
                        .iter()
                        .filter(|symbol| symbol.file == id)
                        .take(4)
                    {
                        let line = format!("{}\n", symbol_line(symbol, &file.path));
                        if remaining.try_add(&line) {
                            body.push_str(&line);
                        } else {
                            omitted_symbols += 1;
                        }
                    }
                }
            }
            if omitted_symbols > 0 {
                body.push_str(&format!(
                    "{omitted_symbols} 个符号因 budget_tokens 被省略。\n"
                ));
            }
            self.result(
                "resolve_context",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
            )
        })
    }

    // ---- precise, language-server backed tools ---------------------------
    // These answer the questions the graph cannot: name-matched call edges
    // recall 66% on TypeScript and 18% on Python, so "where is this symbol
    // referenced" has to come from a real language server. Each forwards to
    // `precise_tools`, which reports an actionable install hint (and
    // `confidence: unknown`) when no server is available — never an empty
    // result that reads like "there are no references".
    #[tool(
        description = "精确引用查询：由语言服务器解析，图谱的名字匹配不足以支撑改写决策。REQUIRED for any rename/rewrite decision — grep matches text, not bindings."
    )]
    pub(crate) fn find_references(
        &self,
        Parameters(params): Parameters<crate::precise_tools::FindReferencesParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_find_references(
            params,
            self.precise.as_ref(),
        ))
    }

    #[tool(
        description = "精确定义跳转：语言服务器绑定解析；不要用 find_symbol 代替（那是句法名字匹配）。The sanctioned way to resolve a definition — not Read/grep."
    )]
    pub(crate) fn goto_definition(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GotoDefinitionParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_goto_definition(
            params,
            self.precise.as_ref(),
        ))
    }

    #[tool(
        description = "文件诊断：语法与类型错误，用于改写后确认代码仍然成立。MUST run after editing a file to confirm it still holds."
    )]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GetDiagnosticsParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_get_diagnostics(
            params,
            self.precise.as_ref(),
        ))
    }

    #[tool(
        description = "符号语义信息（LSP hover：docstring/类型/签名，Serena include_info 对应物）。Docstrings without reading the file — pair with find_symbol anchors."
    )]
    pub(crate) fn get_symbol_info(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GetSymbolInfoParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_get_symbol_info(
            params,
            self.precise.as_ref(),
        ))
    }

    #[tool(
        description = "生成重命名计划，只产出待改位置与置信度，不写盘。Always plan before apply_rename; NEVER hand-edit occurrences for a rename."
    )]
    pub(crate) fn plan_rename(
        &self,
        Parameters(params): Parameters<crate::precise_tools::PlanRenameParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_plan_rename(
            params,
            self.precise.as_ref(),
        ))
    }

    #[tool(
        description = "应用重命名（不可逆）。默认仅 exact/scoped 写盘；低置信度需 force=true。写后重解析，失败整笔回滚。The only sanctioned way to rename; never hand-edit occurrences."
    )]
    pub(crate) fn apply_rename(
        &self,
        Parameters(params): Parameters<crate::precise_tools::ApplyRenameParams>,
    ) -> CallToolResult {
        self.finish(crate::precise_tools::run_apply_rename(
            params,
            self.precise.as_ref(),
            &self.root,
        ))
    }

    #[tool(
        description = "返回按 import 图中心性排名的仓库骨架与公开符号。Prefer over reading files for a first map of the repo."
    )]
    pub(crate) fn get_repo_skeleton(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let centrality = compute_centrality(&index.graph);
            let mut ids: Vec<_> = index.graph.files.iter().map(|file| file.id).collect();
            ids.sort_by(|a, b| {
                centrality
                    .by_file
                    .get(b)
                    .copied()
                    .unwrap_or_default()
                    .partial_cmp(&centrality.by_file.get(a).copied().unwrap_or_default())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let body = self.ranked_files(index, ids, params.budget_tokens, Confidence::Scoped);
            self.result(
                "get_repo_skeleton",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
            )
        })
    }

    #[tool(
        description = "按名称或子串定位符号，返回签名和 path:line 锚点。Prefer over Read for locating symbols — no whole-file reads needed."
    )]
    pub(crate) fn find_symbol(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            // "Class/method" 路径查询（含 "/"）走层级匹配器（Serena
            // NamePathMatcher 对应物：后缀匹配/绝对路径/kind 过滤/depth 展开）。
            if params.query.contains('/') {
                return self.find_symbol_by_path(index, &params);
            }
            let needle = params.query.to_lowercase();
            // 命中对先收集 (锚点行, symbol 下标)，与展示列表同一排序源——
            // include_body 只对"用户实际看到的" kept 前几个附体（审查 major：
            // 原实现按 graph 构建序取前 5，可能对应被 budget 裁掉的符号）。
            let mut hits: Vec<(String, usize)> = Vec::new();
            for (idx, symbol) in index.graph.symbols.iter().enumerate() {
                if symbol.name.to_lowercase().contains(&needle) {
                    if let Some(file) = index.file(symbol.file) {
                        hits.push((symbol_line(symbol, &file.path), idx));
                    }
                }
            }
            hits.sort_by(|a, b| a.0.cmp(&b.0));
            let lines: Vec<String> = hits.iter().map(|(line, _)| line.clone()).collect();
            let (kept, omitted) =
                truncate_ranked(&lines, params.budget_tokens, |line| format!("{line}\n"));
            let kept_count = kept.len();
            let truncated = omitted > 0;
            let mut body = "confidence: syntactic (符号名称匹配；请用锚点核验)\n".to_string();
            body.push_str(&render_budget_status(
                kept_count,
                omitted,
                params.budget_tokens,
            ));
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            // 零命中时显式声明（对齐 find_references 空结果约定），并跳过
            // include_body 附加，避免误导性的"无可提取的符号体"。
            if hits.is_empty() {
                body.push_str("(未找到匹配的符号)\n");
            }
            if params.include_body && !hits.is_empty() {
                self.attach_include_bodies(
                    index,
                    &mut body,
                    &hits,
                    kept_count,
                    params.budget_tokens,
                );
            }
            self.result(
                "find_symbol",
                body,
                json!({
                    "confidence": "syntactic",
                    "matches": lines.len(),
                    "shown": kept_count,
                    "omitted": omitted,
                    "truncated": truncated,
                    "budget_tokens": params.budget_tokens
                }),
            )
        })
    }

    #[tool(
        description = "Search indexed source for a literal substring (default) or a regex when regex=true. Returns matching lines as path:line anchors. Query is literal unless regex=true — A|B without that flag searches for a vertical bar, not an alternation. Truncation is declared in the result (shown/omitted/truncated); raise budget_tokens or tighten path_filter for more hits — there is no pagination. Discovery only; edits and renames must be driven by find_references."
    )]
    pub(crate) fn search_code(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let regex = if params.regex {
                match regex::Regex::new(&params.query) {
                    Ok(regex) => Some(regex),
                    Err(error) => {
                        return text_error(format!("无效正则表达式：{error}"));
                    }
                }
            } else {
                None
            };
            let needle = params.query.to_lowercase();
            let mut matches = Vec::new();
            let mut hits = 0u64;
            let mut misses = 0u64;
            for file in &index.graph.files {
                if params
                    .path_filter
                    .as_ref()
                    .is_some_and(|filter| !file.path.as_str().contains(filter))
                {
                    continue;
                }
                let Some((lines, hit)) = index.cached_lines(&self.file_cache, file.path.as_str())
                else {
                    continue;
                };
                if hit {
                    hits += 1;
                } else {
                    misses += 1;
                }
                for (line_no, line) in lines.iter().enumerate() {
                    let matched = if let Some(regex) = &regex {
                        regex.is_match(line)
                    } else {
                        line.to_lowercase().contains(&needle)
                    };
                    if matched {
                        matches.push(format!("@{}:{} {}", file.path, line_no + 1, line.trim()));
                    }
                }
            }
            self.cache_hits.fetch_add(hits, Ordering::Relaxed);
            self.cache_misses.fetch_add(misses, Ordering::Relaxed);
            tracing::debug!(
                hits,
                misses,
                weighted_size = self.file_cache.weighted_size(),
                budget_bytes = self.cache_budget_bytes,
                "search_code file cache"
            );
            let (kept, omitted) =
                truncate_ranked(&matches, params.budget_tokens, |line| format!("{line}\n"));
            let shown = kept.len();
            let truncated = omitted > 0;
            let mode = if params.regex { "regex" } else { "literal" };
            let mut body = format!(
                "confidence: exact (磁盘源码字面匹配)\n{}",
                render_search_mode_header(params.regex, &params.query)
            );
            body.push_str(&render_budget_status(shown, omitted, params.budget_tokens));
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            // 零命中时显式声明，区分"正常无匹配"与"服务故障/吞内容"
            // （对齐 find_references 的 `未找到对 … 的引用。` 空结果约定）。
            if matches.is_empty() {
                body.push_str("(未找到匹配的代码行)\n");
            }
            self.result(
                "search_code",
                body,
                json!({
                    "confidence": "exact",
                    "mode": mode,
                    "matches": matches.len(),
                    "shown": shown,
                    "omitted": omitted,
                    "truncated": truncated,
                    "budget_tokens": params.budget_tokens
                }),
            )
        })
    }

    #[tool(
        description = "沿 scope-aware import 图查询文件的依赖或被依赖关系。Replaces manual grep for \"who imports this file\" — scoped from the import graph."
    )]
    pub(crate) fn get_dependents(
        &self,
        Parameters(params): Parameters<DependentsParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let Some(target) = index.file_id(&params.target) else {
                return self.result(
                    "get_dependents",
                    format!("未找到目标路径：{}\nconfidence: exact", params.target),
                    json!({"confidence":"exact","found":false,"budget_tokens":params.budget_tokens}),
                );
            };
            let ids = if params.direction == "dependencies" {
                dependencies(&index.graph, target)
            } else {
                dependents(&index.graph, target)
            };
            let body = self.ranked_files(index, ids, params.budget_tokens, Confidence::Scoped);
            self.result(
                "get_dependents",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
            )
        })
    }

    #[tool(
        description = "追踪名字匹配得到的调用者/被调用者；结果是 syntactic，必须核验动态分派。For the precise set use find_references."
    )]
    pub(crate) fn trace_calls(
        &self,
        Parameters(params): Parameters<TraceParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            // depth>1：多跳树（对齐 openvisio trace.ts；环检测 + 缩进渲染）。
            if params.depth > 1 {
                let body = self.trace_calls_tree_body(index, &params);
                return self.result(
                    "trace_calls",
                    body,
                    json!({"confidence":"syntactic","depth":params.depth.clamp(1, 6),"budget_tokens":params.budget_tokens}),
                );
            }
            let wanted: BTreeSet<_> = index
                .graph
                .symbols
                .iter()
                .filter(|symbol| symbol.name.eq_ignore_ascii_case(&params.symbol))
                .map(|symbol| symbol.id.0)
                .collect();
            let mut lines = BTreeSet::new();
            for edge in index.graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Call) {
                let include = match params.direction.as_str() {
                    "callers" => wanted.contains(&edge.to),
                    "callees" => wanted.contains(&edge.from),
                    _ => wanted.contains(&edge.from) || wanted.contains(&edge.to),
                };
                if !include {
                    continue;
                }
                let from = index.graph.symbols.iter().find(|symbol| symbol.id.0 == edge.from);
                let to = index.graph.symbols.iter().find(|symbol| symbol.id.0 == edge.to);
                if let (Some(from), Some(to), Some(from_file), Some(to_file)) = (
                    from,
                    to,
                    from.and_then(|symbol| index.file(symbol.file)),
                    to.and_then(|symbol| index.file(symbol.file)),
                ) {
                    lines.insert(format!(
                        "{} -> {}",
                        symbol_line(from, &from_file.path),
                        symbol_line(to, &to_file.path)
                    ));
                }
            }
            let lines: Vec<_> = lines.into_iter().collect();
            let (kept, omitted) =
                truncate_ranked(&lines, params.budget_tokens, |line| format!("{line}\n"));
            let mut body = format!(
                "confidence: syntactic {}\n警告：调用图基于名字匹配，可能漏掉动态分派或包含同名误报。\n",
                confidence_note(Confidence::Syntactic)
            );
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
            }
            self.result(
                "trace_calls",
                body,
                json!({"confidence":"syntactic","budget_tokens":params.budget_tokens}),
            )
        })
    }

    #[tool(
        description = "多跳 import 邻域（BFS 1-3 跳，对齐 openvisio get_neighborhood）：文件在依赖图上的 k 跳可达集。Replaces chained get_dependents calls for impact analysis."
    )]
    pub(crate) fn get_neighborhood(
        &self,
        Parameters(params): Parameters<NeighborhoodParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let Some(target_id) = index.file_id(&params.target) else {
                return self.result(
                    "get_neighborhood",
                    format!("未找到目标文件：{}\n", params.target),
                    json!({"confidence":"unknown","budget_tokens":params.budget_tokens}),
                );
            };
            let direction = match params.direction.as_str() {
                "dependencies" => astrolabe_core::neighborhood::NeighborhoodDirection::Dependencies,
                "dependents" => astrolabe_core::neighborhood::NeighborhoodDirection::Dependents,
                _ => astrolabe_core::neighborhood::NeighborhoodDirection::Both,
            };
            let depth = params.depth.clamp(1, u16::from(astrolabe_core::neighborhood::MAX_NEIGHBORHOOD_DEPTH)) as u8;
            let entries = astrolabe_core::neighborhood::neighborhood(
                &index.graph,
                target_id,
                direction,
                depth,
            );
            let mut header = format!(
                "confidence: scoped {} (import 图 BFS；depth 1-3)\n",
                confidence_note(Confidence::Scoped)
            );
            if entries.is_empty() {
                header.push_str("无邻域（目标孤立或超深度域）。\n");
                return self.result(
                    "get_neighborhood",
                    header,
                    json!({"confidence":"scoped","entries":0,"depth":depth,"budget_tokens":params.budget_tokens}),
                );
            }
            let lines: Vec<String> = entries
                .iter()
                .filter_map(|entry| {
                    index.file(entry.file).map(|file| {
                        format!(
                            "depth={} @{}:1 ({} LOC)",
                            entry.depth, file.path, file.loc
                        )
                    })
                })
                .collect();
            let remaining = params
                .budget_tokens
                .saturating_sub(estimate_tokens(&header));
            let (kept, omitted) =
                truncate_ranked(&lines, remaining, |line| format!("{line}\n"));
            let mut body = header;
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
            }
            self.result(
                "get_neighborhood",
                body,
                json!({"confidence":"scoped","entries":entries.len(),"depth":depth,"budget_tokens":params.budget_tokens}),
            )
        })
    }

    // ----- 项目记忆（Serena memories 对应物：跨会话知识留存，名称寻址）-----
    #[tool(
        description = "列出项目记忆（名称 + 首行摘要）。Read relevant memories before starting a task in this repo — they persist across sessions."
    )]
    pub(crate) fn list_memories(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        let entries = crate::memory::list_memories(&self.root);
        let header = if entries.is_empty() {
            "（无项目记忆；用 write_memory 沉淀跨会话知识）\n".to_string()
        } else {
            format!(
                "共 {} 条项目记忆（<root>/.astrolabe/memories/）：\n",
                entries.len()
            )
        };
        if entries.is_empty() {
            return self.result(
                "list_memories",
                header,
                json!({"confidence":"exact","count":0,"budget_tokens":params.budget_tokens}),
            );
        }
        let lines: Vec<String> = entries
            .iter()
            .map(|(name, summary)| format!("- {name}: {summary}"))
            .collect();
        let remaining = params
            .budget_tokens
            .saturating_sub(estimate_tokens(&header));
        let (kept, omitted) = truncate_ranked(&lines, remaining, |line| format!("{line}\n"));
        let mut body = header;
        for line in kept {
            body.push_str(line);
            body.push('\n');
        }
        if omitted > 0 {
            body.push_str(&format!("{omitted} 条记忆因 budget_tokens 被省略。\n"));
        }
        self.result(
            "list_memories",
            body,
            json!({"confidence":"exact","count":entries.len(),"budget_tokens":params.budget_tokens}),
        )
    }

    #[tool(
        description = "读单条项目记忆全文（Serena read_memory）。Check list_memories for names."
    )]
    pub(crate) fn read_memory(
        &self,
        Parameters(params): Parameters<MemoryNameParams>,
    ) -> CallToolResult {
        match crate::memory::read_memory(&self.root, &params.name) {
            Some(content) => self.result(
                "read_memory",
                truncate_body_text(&content, params.budget_tokens),
                json!({"confidence":"exact","name":params.name,"budget_tokens":params.budget_tokens}),
            ),
            None => self.result(
                "read_memory",
                format!("记忆 `{}` 不存在（先 list_memories 查名）。confidence: exact\n", params.name),
                json!({"confidence":"exact","found":false,"budget_tokens":params.budget_tokens}),
            ),
        }
    }

    #[tool(
        description = "写/覆写项目记忆（Serena write_memory）：跨会话沉淀任务结论、坑、架构决策。原子写。"
    )]
    pub(crate) fn write_memory(
        &self,
        Parameters(params): Parameters<MemoryContentParams>,
    ) -> CallToolResult {
        match crate::memory::write_memory(&self.root, &params.name, &params.content) {
            Ok(()) => self.result(
                "write_memory",
                format!("记忆 `{}` 已写入。\n", params.name),
                json!({"confidence":"exact","name":params.name,"budget_tokens":params.budget_tokens}),
            ),
            Err(error) => self.finish(text_error(format!("写入失败：{error}"))),
        }
    }

    #[tool(description = "删除项目记忆（Serena delete_memory）。Irreversible.")]
    pub(crate) fn delete_memory(
        &self,
        Parameters(params): Parameters<MemoryNameParams>,
    ) -> CallToolResult {
        let deleted = crate::memory::delete_memory(&self.root, &params.name);
        if deleted {
            self.result(
                "delete_memory",
                format!("记忆 `{}` 已删除。confidence: exact\n", params.name),
                json!({"confidence":"exact","deleted":true,"budget_tokens":params.budget_tokens}),
            )
        } else {
            self.finish(text_error(format!(
                "删除失败：记忆 `{}` 不存在或名字非法",
                params.name
            )))
        }
    }

    #[tool(
        description = "文件夹级架构图（对齐 openvisio toGroupGraph）：顶层目录为节点、聚合 import 为加权边。The macro architecture view — start here for module-boundary decisions."
    )]
    pub(crate) fn get_group_graph(
        &self,
        Parameters(params): Parameters<GroupGraphParams>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let centrality = compute_centrality(&index.graph);
            let depth = params.depth.clamp(1, 3) as u8;
            let (nodes, edges) =
                astrolabe_core::group_graph::group_graph(&index.graph, &centrality, depth);
            let header = format!(
                "confidence: scoped {} (import 图文件夹聚合；depth={depth})\n\nnodes:\n",
                confidence_note(Confidence::Scoped)
            );
            let mut lines: Vec<String> = nodes
                .iter()
                .map(|node| {
                    format!(
                        "{}  files={} centrality={:.4}",
                        node.id, node.files, node.centrality
                    )
                })
                .collect();
            lines.push(String::new());
            lines.push("edges (from -> to, weight):".to_string());
            if edges.is_empty() {
                lines.push("(无跨组边)".to_string());
            } else {
                for edge in &edges {
                    lines.push(format!(
                        "{} -> {} weight={}",
                        edge.from, edge.to, edge.weight
                    ));
                }
            }
            let remaining = params
                .budget_tokens
                .saturating_sub(estimate_tokens(&header));
            let (kept, omitted) =
                truncate_ranked(&lines, remaining, |line| format!("{line}\n"));
            let mut body = header;
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
            }
            self.result(
                "get_group_graph",
                body,
                json!({"confidence":"scoped","groups":nodes.len(),"edges":edges.len(),"depth":depth,"budget_tokens":params.budget_tokens}),
            )
        })
    }

    /// include_body：对展示列表（kept）前 5 个符号附源码体，头带
    /// path:line 锚点区分同名符号；附加体的 token 计入 budget。
    /// 预算不够时按行优雅截断而非整块丢弃：首个符号强制保留最小
    /// 展示空间，杜绝"找到符号却零字节 + 无可提取"的自相矛盾输出。
    #[allow(clippy::too_many_lines)]
    fn attach_include_bodies(
        &self,
        index: &RepoIndex,
        body: &mut String,
        hits: &[(String, usize)],
        kept_count: usize,
        budget_tokens: usize,
    ) {
        let mut attached = 0usize;
        let mut failed = 0usize;
        let mut spent = estimate_tokens(body);
        for (line, idx) in hits.iter().take(kept_count).take(5) {
            let symbol = &index.graph.symbols[*idx];
            let Some(file) = index.file(symbol.file) else {
                failed += 1;
                continue;
            };
            match astrolabe_core::body::symbol_body(&self.root.join(file.path.as_str()), symbol) {
                Ok(text) if !text.is_empty() => {
                    let section = format!("\n----- body @ {line} -----\n{text}");
                    let section_tokens = estimate_tokens(&section);
                    if spent + section_tokens > budget_tokens {
                        // 整体放不下：不丢弃，按剩余预算逐行截断。
                        let mut available_tokens = budget_tokens.saturating_sub(spent);
                        if attached == 0 {
                            // 首个符号即使预算紧迫也强制保留展示空间。
                            available_tokens = available_tokens.max(250);
                        }
                        if available_tokens < 150 {
                            // 前面的符号已展示过、剩余空间过小：省略收尾。
                            body.push_str("\n(include_body: 剩余符号体因 budget_tokens 被省略)\n");
                            break;
                        }
                        let header = format!("\n----- body @ {line} -----\n");
                        // 截断说明约需 40 tokens，先从预算里扣除。
                        let body_budget = available_tokens
                            .saturating_sub(40)
                            .saturating_sub(estimate_tokens(&header));
                        let mut shown_lines = 0usize;
                        let mut partial = String::new();
                        for src_line in text.lines() {
                            let candidate = format!("{partial}{src_line}\n");
                            if estimate_tokens(&candidate) > body_budget
                                // 首个符号至少给出一行，避免零字节输出。
                                && !(attached == 0 && shown_lines == 0)
                            {
                                break;
                            }
                            partial = candidate;
                            shown_lines += 1;
                        }
                        if shown_lines > 0 {
                            let total_lines = symbol.end_line.saturating_sub(symbol.start_line) + 1;
                            let note = format!(
                                "\n... [符号共 {total_lines} 行，受 budget_tokens 限制截断展示前 {shown_lines} 行；查看内部局部代码请使用 Read(offset, limit)] ...\n"
                            );
                            let truncated = format!("{header}{partial}{note}");
                            body.push_str(&truncated);
                            spent += estimate_tokens(&truncated);
                            attached += 1;
                        }
                        body.push_str("\n(include_body: 剩余符号体因 budget_tokens 被省略)\n");
                        break;
                    }
                    spent += section_tokens;
                    body.push_str(&section);
                    attached += 1;
                }
                _ => {
                    failed += 1;
                }
            }
        }
        // 只有真正一个符号体都没附加（含截断附加）时才提示无可提取。
        let _ = spent;
        if attached == 0 && failed == 0 {
            body.push_str("\n(include_body: 无可提取的符号体)\n");
        }
        if failed > 0 {
            body.push_str(&format!(
                "\n(include_body: {failed} 个符号体读取失败已降级为锚点)\n"
            ));
        }
    }

    /// "Class/method" 路径查询分支：core::name_path 层级匹配 + 渲染。
    /// 与名称匹配分支一致地支持 include_body（kept → top5 → symbol_body/truncate）。
    fn find_symbol_by_path(&self, index: &RepoIndex, params: &QueryParams) -> CallToolResult {
        let kind = match params.kind.as_deref() {
            None => None,
            Some(name) => match kind_from_name(name) {
                Ok(kind) => Some(kind),
                Err(message) => return self.finish(text_error(message)),
            },
        };
        let kind = astrolabe_core::name_path::KindFilter(kind);
        let matches = astrolabe_core::name_path::match_name_path(
            &index.graph.symbols,
            &params.query,
            params.substring,
            kind,
            params.depth,
        );
        // 命中对先收集 (锚点行, symbol 下标)，与名称分支同一 include_body 排序源。
        let mut hits: Vec<(String, usize)> = Vec::new();
        for m in &matches {
            if let Some((idx, symbol)) = index
                .graph
                .symbols
                .iter()
                .enumerate()
                .find(|(_, s)| s.id == m.symbol)
            {
                if let Some(file) = index.file(symbol.file) {
                    let depth_note = if m.depth > 0 {
                        format!(" (嵌套层 d{})", m.depth)
                    } else {
                        String::new()
                    };
                    hits.push((
                        format!("{}{depth_note}", symbol_line(symbol, &file.path)),
                        idx,
                    ));
                }
            }
        }
        let lines: Vec<String> = hits.iter().map(|(line, _)| line.clone()).collect();
        let (kept, omitted) =
            truncate_ranked(&lines, params.budget_tokens, |line| format!("{line}\n"));
        let kept_count = kept.len();
        let truncated = omitted > 0;
        let mut body =
            "confidence: syntactic (层级路径匹配：行范围嵌套推断，语法容器如 impl 会形成中间层；请核验)\n"
                .to_string();
        body.push_str(&render_budget_status(
            kept_count,
            omitted,
            params.budget_tokens,
        ));
        for line in kept {
            body.push_str(line);
            body.push('\n');
        }
        // 零命中时显式声明，避免只剩 confidence 元信息的歧义输出。
        if hits.is_empty() {
            body.push_str("(未找到匹配的符号)\n");
        }
        if params.include_body && !hits.is_empty() {
            self.attach_include_bodies(index, &mut body, &hits, kept_count, params.budget_tokens);
        }
        self.result(
            "find_symbol",
            body,
            json!({
                "confidence": "syntactic",
                "matches": lines.len(),
                "shown": kept_count,
                "omitted": omitted,
                "truncated": truncated,
                "path_query": true,
                "budget_tokens": params.budget_tokens
            }),
        )
    }

    /// 多跳调用树（depth 1-6）：对每个同名符号各跑一棵 trace_tree，
    /// 缩进渲染 + 环标记。同名多根时各树以根锚点行分隔。
    #[allow(clippy::too_many_lines)]
    fn trace_calls_tree_body(&self, index: &RepoIndex, params: &TraceParams) -> String {
        let direction = match params.direction.as_str() {
            "callers" => astrolabe_core::trace_tree::TraceDirection::Callers,
            "callees" => astrolabe_core::trace_tree::TraceDirection::Callees,
            _ => astrolabe_core::trace_tree::TraceDirection::Both,
        };
        let depth = params.depth.clamp(1, 6) as u8;
        let mut body = format!(
            "confidence: syntactic {}\n警告：调用图基于名字匹配，可能漏掉动态分派或包含同名误报。depth={depth}，(cycle) = 路径成环已截断。\n",
            confidence_note(Confidence::Syntactic)
        );
        // 同名符号多根一次建图 + 全森林硬上限（审查 major #3：逐根重建邻接、
        // 无节点上限，depth=6 稠密图组合爆炸）。
        let roots: Vec<_> = index
            .graph
            .symbols
            .iter()
            .filter(|symbol| symbol.name.eq_ignore_ascii_case(&params.symbol))
            .map(|symbol| symbol.id)
            .collect();
        let forest =
            astrolabe_core::trace_tree::trace_forest(&index.graph, &roots, direction, depth, 2000);
        if forest.entries.is_empty() {
            body.push_str("未找到同名符号或无可展开调用边。\n");
            return body;
        }
        // 预渲染 SymbolId → 锚点行，避免逐节点线性扫符号表（审查 major #3）。
        let mut line_by_id: BTreeMap<_, String> = BTreeMap::new();
        for symbol in &index.graph.symbols {
            if let Some(file) = index.file(symbol.file) {
                line_by_id.insert(symbol.id, symbol_line(symbol, &file.path));
            }
        }
        for entry in &forest.entries {
            let Some(root_line) = line_by_id.get(&entry.root) else {
                continue;
            };
            body.push_str(&format!("\nroot: {root_line}\n"));
            // Both = Callees 树在前、Callers 树在后（trace_tree 语义）——第二
            // 个 depth=0 是分界，打分段头；depth=0 节点本身已由 root 头表达
            // （审查 major #4：根重复打印）。
            let mut seen_root: usize = 0;
            for node in &entry.nodes {
                if node.depth == 0 {
                    if direction == astrolabe_core::trace_tree::TraceDirection::Both
                        && seen_root == 1
                    {
                        body.push_str("--- callers ---\n");
                    }
                    seen_root += 1;
                    continue;
                }
                let Some(line) = line_by_id.get(&node.symbol) else {
                    continue;
                };
                let cycle = if node.cycle { " (cycle)" } else { "" };
                body.push_str(&format!(
                    "{}{}{cycle}\n",
                    "  ".repeat(node.depth as usize),
                    line
                ));
            }
        }
        body
    }

    #[tool(
        description = "返回 import 中心性较高的承重文件，适合风险与重构候选定位。Start here to find load-bearing files before a refactor."
    )]
    pub(crate) fn get_hotspots(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        self.with_index(|index| {
            // churn 融合（对齐 openvisio buildHotspots）：中心性 × churn 增益。
            // 无 git / git 失败 → 空表 → 增益恒 1，退化为纯中心性（向后兼容）。
            let centrality = compute_centrality(&index.graph);
            let churn = self.cached_churn();
            let mut scored: Vec<(FileId, f64)> = index
                .graph
                .files
                .iter()
                .map(|file| {
                    let c = centrality.by_file.get(&file.id).copied().unwrap_or_default();
                    let path_key = file.path.to_string();
                    (file.id, astrolabe_core::churn::fuse_hotspot_score(c, churn.get(&path_key)))
                })
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            // churn_fused = 至少一个文件真正命中 churn 键（表非空但全 miss 时
            // 不误报——审查 major：子目录口径失配场景）。
            let churn_hits = scored
                .iter()
                .filter(|(id, _)| {
                    index
                        .file(*id)
                        .map(|file| churn.contains_key(&file.path.to_string()))
                        .unwrap_or(false)
                })
                .count();
            let mut header = format!(
                "confidence: scoped {} (中心性×git-churn 融合；churn 缺失时退化为纯中心性)\n",
                confidence_note(Confidence::Scoped)
            );
            let mut rows: Vec<String> = Vec::new();
            for (id, score) in &scored {
                if rows.len() >= 25 {
                    break;
                }
                if let Some(file) = index.file(*id) {
                    let churn_note = churn
                        .get(&file.path.to_string())
                        .map(|stats| {
                            format!(" churn=90d:{}c/{}a", stats.commits_90d, stats.authors_90d)
                        })
                        .unwrap_or_default();
                    rows.push(format!(
                        "@{}:1 score={:.4}{churn_note} ({} LOC)",
                        file.path, score, file.loc
                    ));
                }
            }
            let (kept, omitted) =
                truncate_ranked(&rows, params.budget_tokens, |row| format!("{row}\n"));
            for row in kept {
                header.push_str(row);
                header.push('\n');
            }
            if omitted > 0 {
                header.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
            }
            self.result(
                "get_hotspots",
                header,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens,"churn_fused":churn_hits>0,"churn_files":churn_hits}),
            )
        })
    }

    #[tool(description = "统计已索引仓库的语言、文件数与代码行数。Index coverage check.")]
    pub(crate) fn get_languages(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        self.with_index(|index| {
            let mut totals: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            for file in &index.graph.files {
                if let Some(language) = file.language {
                    let total = totals.entry(language.name()).or_default();
                    total.0 += 1;
                    total.1 += u64::from(file.loc);
                }
            }
            let mut rows: Vec<_> = totals
                .into_iter()
                .map(|(language, (files, loc))| format!("{language}: {files} files, {loc} LOC"))
                .collect();
            rows.sort_by(|a, b| b.cmp(a));
            let (kept, omitted) =
                truncate_ranked(&rows, params.budget_tokens, |row| format!("{row}\n"));
            let cache_bytes = self.file_cache.weighted_size();
            let cache_hits = self.cache_hits.load(Ordering::Relaxed);
            let cache_misses = self.cache_misses.load(Ordering::Relaxed);
            tracing::debug!(
                weighted_size = cache_bytes,
                budget_bytes = self.cache_budget_bytes,
                cache_hits,
                cache_misses,
                "file cache occupancy"
            );
            let mut body = format!(
                "confidence: exact (来自已扫描文件元数据)\n\
                 file_cache: {cache_bytes}/{} bytes, hits={cache_hits}, misses={cache_misses}\n",
                self.cache_budget_bytes
            );
            for row in kept {
                body.push_str(row);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 种语言因 budget_tokens 被省略。\n"));
            }
            self.result(
                "get_languages",
                body,
                json!({
                    "confidence":"exact",
                    "budget_tokens":params.budget_tokens,
                    "file_cache_bytes": cache_bytes,
                    "file_cache_budget_bytes": self.cache_budget_bytes,
                    "file_cache_hits": cache_hits,
                    "file_cache_misses": cache_misses,
                }),
            )
        })
    }
}

#[tool_handler]
impl ServerHandler for AstrolabeServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_server_info(Implementation::new("astrolabe", env!("CARGO_PKG_VERSION")))
        .with_instructions(crate::instructions::connection_instructions(
            &self.context.name,
            &self.root,
        ))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2026_07_28))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // `serve_directly` skips rmcp's handshake wrapper, which would otherwise
        // overwrite peer_info with the *negotiated* version. Do that here so a
        // client that still sends `initialize` (legacy 2025-11-25 etc.) is
        // recorded as speaking the version we actually agreed, not the one it
        // asked for.
        let result = self.negotiate_initialize(&request)?;
        let mut peer_info = request;
        peer_info.protocol_version = result.protocol_version.clone();
        context.peer.set_peer_info(peer_info);
        Ok(result)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self.listed_tools();
        Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TOOL_CATALOG_TTL_MS)
            .with_cache_scope(CacheScope::Public))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let resources = self.resources.lock().expect("resource lock poisoned");
        let Some(text) = resources.get(&request.uri) else {
            return Err(ErrorData::resource_not_found(
                format!("resource not found: {}", request.uri),
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text.clone(), request.uri)]).into())
    }
}

/// Truncate free-form text by whole lines to honor `budget_tokens`.
fn truncate_body_text(text: &str, budget_tokens: usize) -> String {
    if estimate_tokens(text) <= budget_tokens {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let (kept, omitted) = truncate_ranked(&lines, budget_tokens, |line| format!("{line}\n"));
    let mut out = String::new();
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    if omitted > 0 {
        out.push_str(&format!("({omitted} 行因 budget_tokens 被省略)\n"));
    }
    out
}

fn text_error(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    CallToolResult::error(vec![ContentBlock::text(if message.trim().is_empty() {
        "未知错误".to_string()
    } else {
        message
    })])
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::{
        graph::CodeGraph, CodeFile, CodeSymbol, Language, RelPath, SymbolId, SymbolKind,
    };

    fn ready_server() -> AstrolabeServer {
        let mut server =
            AstrolabeServer::new_with_cache_budget(PathBuf::from("."), DEFAULT_MEMORY_BUDGET_BYTES);
        let graph = CodeGraph {
            files: vec![CodeFile {
                id: FileId(0),
                path: RelPath::new("src/lib.rs"),
                language: Some(Language::Rust),
                loc: 10,
                sha: String::new(),
            }],
            symbols: vec![CodeSymbol {
                id: SymbolId(0),
                file: FileId(0),
                name: "alpha".into(),
                kind: SymbolKind::Function,
                signature: "pub fn alpha()".into(),
                start_line: 3,
                end_line: 3,
                exported: true,
            }],
            edges: vec![],
        };
        *server.state.write().unwrap() = IndexState::Ready(Arc::new(RepoIndex {
            root: PathBuf::from("."),
            graph,
            report: Default::default(),
        }));
        // Tests must not inherit ASTROLABE_STRUCTURED from the environment.
        server.structured_output = false;
        server
    }

    #[test]
    fn looks_like_regex_query_catches_alternation_and_skips_dotted_names() {
        assert!(looks_like_regex_query(
            "scanForBrandHint|RewriteStreamingBody"
        ));
        assert!(looks_like_regex_query("foo.*bar"));
        assert!(looks_like_regex_query("\\bHermes\\b"));
        assert!(!looks_like_regex_query("foo.bar"));
        assert!(!looks_like_regex_query("RewriteClaudeBody"));
        assert!(!looks_like_regex_query("$"));
    }

    fn assert_first_text(result: CallToolResult) {
        let text = result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|content| content.text.trim())
            .unwrap_or_default();
        assert!(!text.is_empty(), "content[0].text must always be non-empty");
        assert!(
            text.starts_with("index_root:"),
            "tool text must declare the absolute index_root: {text}"
        );
    }

    #[test]
    fn tool_schemas_include_budget_and_tool_count_is_bounded() {
        let server = ready_server();
        let tools = server.tool_router.list_all();
        // Compare against the constant rather than a literal so adding a tool
        // fails in exactly one place.
        assert_eq!(tools.len(), crate::TOOL_COUNT);
        for tool in tools {
            let schema = serde_json::to_value(&tool.input_schema).unwrap();
            assert!(
                schema.pointer("/properties/budget_tokens").is_some(),
                "{} lacks budget_tokens",
                tool.name
            );
        }
    }

    #[test]
    fn excluded_tools_are_omitted_from_catalog() {
        let mut ctx = crate::ClientContext::default_builtin();
        ctx.excluded_tools = vec!["trace_calls".into(), "get_hotspots".into()];
        let server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            ctx,
        );
        let names: Vec<String> = server
            .listed_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert!(!names.iter().any(|name| name == "trace_calls"), "{names:?}");
        assert!(
            !names.iter().any(|name| name == "get_hotspots"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|name| name == "resolve_context"),
            "{names:?}"
        );
        assert_eq!(names.len(), crate::TOOL_COUNT - 2);
    }

    #[test]
    fn codex_list_tools_sanitizes_integer_and_null_unions() {
        let ctx = crate::ClientContext::builtin("codex").unwrap();
        assert!(ctx.openai_tool_compatible());
        let server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            ctx,
        );
        let tools = server.listed_tools();
        assert!(!tools.is_empty());
        for tool in &tools {
            let schema = Value::Object((*tool.input_schema).clone());
            let schema_str = schema.to_string();
            assert!(
                !schema_str.contains("\"integer\""),
                "{} still has integer: {schema_str}",
                tool.name
            );
            let budget = schema
                .pointer("/properties/budget_tokens")
                .unwrap_or_else(|| panic!("{} lacks budget_tokens", tool.name));
            assert_eq!(budget.get("type"), Some(&json!("number")), "{}", tool.name);
            assert_eq!(budget.get("multipleOf"), Some(&json!(1)), "{}", tool.name);
            if let Some(Value::Object(props)) = schema.get("properties") {
                for (prop_name, prop) in props {
                    assert!(
                        prop.get("type").is_some(),
                        "{} property `{prop_name}` missing type after sanitize: {prop}",
                        tool.name
                    );
                    if let Some(Value::Array(types)) = prop.get("type") {
                        assert!(
                            types.iter().all(|t| t.as_str() != Some("null")),
                            "{} property `{prop_name}` still has null in type: {prop}",
                            tool.name
                        );
                    }
                }
            }
        }

        // Default context must leave schemars integer alone.
        let default_server = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::default_builtin(),
        );
        let raw = Value::Object((*default_server.listed_tools()[0].input_schema).clone());
        let budget = raw.pointer("/properties/budget_tokens").expect("budget");
        assert_eq!(
            budget.get("type"),
            Some(&json!("integer")),
            "non-codex contexts must not sanitize"
        );
    }

    #[test]
    fn structured_output_for_respects_context_then_env_override() {
        let claude = crate::ClientContext::builtin("claude-code").unwrap();
        let cursor = crate::ClientContext::builtin("cursor").unwrap();
        assert!(!claude.structured_or_auto_off());
        assert!(cursor.structured_or_auto_off());
        match structured_output_override() {
            Some(forced) => {
                assert_eq!(structured_output_for(&claude), forced);
                assert_eq!(structured_output_for(&cursor), forced);
            }
            None => {
                assert!(!structured_output_for(&claude));
                assert!(structured_output_for(&cursor));
            }
        }
    }

    #[test]
    fn every_tool_returns_nonempty_first_text_block() {
        let server = ready_server();
        assert_first_text(server.resolve_context(Parameters(ResolveContextParams {
            task_description: "alpha".into(),
            budget_tokens: 100,
        })));
        assert_first_text(server.get_repo_skeleton(Parameters(BudgetOnly { budget_tokens: 100 })));
        assert_first_text(server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 100,
        })));
        assert_first_text(server.search_code(Parameters(SearchParams {
            query: "definitely absent".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 100,
        })));
        assert_first_text(server.get_dependents(Parameters(DependentsParams {
            target: "src/lib.rs".into(),
            direction: "dependents".into(),
            budget_tokens: 100,
        })));
        assert_first_text(server.trace_calls(Parameters(TraceParams {
            symbol: "alpha".into(),
            direction: "both".into(),
            depth: 1,
            budget_tokens: 100,
        })));
        assert_first_text(server.get_hotspots(Parameters(BudgetOnly { budget_tokens: 100 })));
        assert_first_text(server.get_languages(Parameters(BudgetOnly { budget_tokens: 100 })));
    }

    #[test]
    fn budget_parameter_reaches_core_truncation() {
        let server = ready_server();
        let result = server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 0,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("truncated=true"), "{text}");
        assert!(text.contains("这不是全集"), "{text}");
        assert!(text.contains("budget_tokens=0 被省略"), "{text}");
        assert!(
            result.structured_content.is_none(),
            "Claude Code replaces text with structuredContent; default is text-only"
        );
    }

    #[test]
    fn structured_output_opt_in_carries_root_and_full_text() {
        let mut server = ready_server();
        server.structured_output = true;
        let result = server.get_languages(Parameters(BudgetOnly { budget_tokens: 100 }));
        let text = result.content[0].as_text().unwrap().text.clone();
        let structured = result.structured_content.expect("opt-in structured");
        assert_eq!(structured["root"], json!(server.root.display().to_string()));
        assert_eq!(structured["text"], json!(text));
        assert_eq!(
            structured["file_cache_budget_bytes"],
            json!(DEFAULT_MEMORY_BUDGET_BYTES)
        );
        assert!(text.starts_with("index_root:"), "{text}");
    }

    #[test]
    fn large_results_keep_text_first_then_resource_link() {
        let server = ready_server();
        let result = server.result(
            "large",
            "x".repeat((RESOURCE_THRESHOLD_TOKENS + 1) * 4),
            json!({}),
        );
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_resource_link().is_some());
    }

    fn unique_temp_dir() -> PathBuf {
        // A timestamp alone is not unique: macOS resolves `SystemTime::now`
        // to roughly a microsecond, so two tests starting in the same instant
        // get the same path, and whichever finishes first deletes the other's
        // files mid-run. The counter makes collisions impossible.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-mcp-{}-{}-{}-{}",
            "server",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn ready_server_with_disk() -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'cache-me'\n",
        )
        .unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    #[test]
    fn cache_budget_env_parsing() {
        assert_eq!(parse_cache_budget_mb(None), DEFAULT_MEMORY_BUDGET_BYTES);
        assert_eq!(parse_cache_budget_mb(Some("8")), 8 * 1024 * 1024);
        assert_eq!(parse_cache_budget_mb(Some("256")), 256 * 1024 * 1024);
        assert_eq!(parse_cache_budget_mb(Some(" 4 ")), 4 * 1024 * 1024);
        assert_eq!(
            parse_cache_budget_mb(Some("nope")),
            DEFAULT_MEMORY_BUDGET_BYTES
        );
        assert_eq!(parse_cache_budget_mb(Some("0")), 0);
    }

    #[test]
    fn get_languages_reports_file_cache_occupancy() {
        let server = ready_server();
        let result = server.get_languages(Parameters(BudgetOnly { budget_tokens: 100 }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("file_cache:"),
            "get_languages should expose cache occupancy: {text}"
        );
        assert!(
            text.contains(&format!("/{} bytes", DEFAULT_MEMORY_BUDGET_BYTES)),
            "cache budget should appear in text: {text}"
        );
        assert!(result.structured_content.is_none());
    }

    #[test]
    fn search_code_uses_file_cache_and_records_hits() {
        let (server, dir) = ready_server_with_disk();
        let params = SearchParams {
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1000,
        };

        let first = server.search_code(Parameters(params.clone()));
        let first_text = first.content[0].as_text().unwrap().text.as_str();
        assert!(first_text.contains("cache-me"), "{first_text}");
        assert!(server.cache_misses.load(Ordering::Relaxed) >= 1);
        assert_eq!(server.cache_hits.load(Ordering::Relaxed), 0);

        let second = server.search_code(Parameters(params));
        let second_text = second.content[0].as_text().unwrap().text.as_str();
        assert!(second_text.contains("cache-me"), "{second_text}");
        assert!(
            server.cache_hits.load(Ordering::Relaxed) >= 1,
            "second search of an unchanged file must hit"
        );

        let languages = server.get_languages(Parameters(BudgetOnly { budget_tokens: 200 }));
        let languages_text = languages.content[0].as_text().unwrap().text.as_str();
        assert!(languages_text.contains("hits="), "{languages_text}");
        assert!(
            languages_text.contains(&format!("/{} bytes", 8 * 1024 * 1024)),
            "{languages_text}"
        );
        assert!(languages.structured_content.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_file_cache_stays_within_byte_budget() {
        let dir = unique_temp_dir();
        let payload = format!("payload = '{}'\n", "y".repeat(2_000));
        for index in 0..80 {
            std::fs::write(dir.join(format!("f{index}.py")), &payload).unwrap();
        }
        const BUDGET: u64 = 32 * 1024;
        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), BUDGET);
        let index = crate::index::build(&dir).expect("index");
        *server.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        let _ = server.search_code(Parameters(SearchParams {
            query: "payload".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 200,
        }));

        let weighted = server.file_cache.weighted_size();
        assert!(
            weighted > 0,
            "enough inserts should flush moka maintenance so occupancy is visible"
        );
        assert!(
            weighted <= BUDGET,
            "file cache occupancy {weighted} exceeded budget {BUDGET}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_sees_edits_after_mtime_change() {
        let (server, dir) = ready_server_with_disk();
        let _ = server.search_code(Parameters(SearchParams {
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1000,
        }));

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'fresh-content'\n",
        )
        .unwrap();

        let after = server.search_code(Parameters(SearchParams {
            query: "fresh-content".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1000,
        }));
        let text = after.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("fresh-content"), "{text}");
        assert!(!text.contains("cache-me"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        let result = server.search_code(Parameters(SearchParams {
            query: "totally-absent-needle".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1000,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        // 零命中必须显式声明，避免"只剩元信息、疑似服务故障"的歧义输出。
        assert!(text.contains("(未找到匹配的代码行)"), "{text}");
        // confidence 元信息保留，供宿主客户端区分索引根上下文。
        assert!(text.contains("confidence: exact"), "{text}");
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("shown=0 omitted=0 truncated=false"), "{text}");
        // 空结果不应误报截断省略。
        assert!(!text.contains("被省略"), "{text}");
        assert!(!text.contains("这不是全集"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_warns_when_regex_metacharacters_are_literal() {
        let (server, dir) = ready_server_with_disk();
        let result = server.search_code(Parameters(SearchParams {
            query: "scanForBrandHint|RewriteStreamingBody".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1000,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("regex=false"), "{text}");
        assert!(
            text.contains("scanForBrandHint|RewriteStreamingBody"),
            "{text}"
        );
        assert!(text.contains("(未找到匹配的代码行)"), "{text}");
        assert!(!text.contains("truncated=true"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_truncation_is_declared_in_text() {
        let dir = unique_temp_dir();
        let mut source = String::from("needle = 0\n");
        for i in 1..40 {
            source.push_str(&format!("needle = {i}\n"));
        }
        std::fs::write(dir.join("hits.py"), source).unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        let result = server.search_code(Parameters(SearchParams {
            query: "needle".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 20,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(text.contains("这不是全集"), "{text}");
        assert!(text.contains("本工具没有翻页"), "{text}");
        assert!(!text.contains("regex=false，已按字面搜索"), "{text}");
        let structured = result.structured_content.expect("opt-in structured");
        assert_eq!(structured["truncated"], json!(true));
        assert_eq!(structured["mode"], json!("literal"));
        assert!(structured["omitted"].as_u64().unwrap() > 0, "{structured}");
        assert_eq!(structured["matches"], json!(40));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_schema_describes_literal_default_and_regex_flag() {
        let server = ready_server();
        let tools = server.listed_tools();
        let search = tools
            .iter()
            .find(|tool| tool.name == "search_code")
            .expect("search_code in catalog");
        let schema = Value::Object((*search.input_schema).clone());
        let query = schema.pointer("/properties/query/description").unwrap();
        let regex = schema.pointer("/properties/regex/description").unwrap();
        let path_filter = schema
            .pointer("/properties/path_filter/description")
            .unwrap();
        let budget = schema
            .pointer("/properties/budget_tokens/description")
            .unwrap();
        let tool_desc = search.description.as_deref().unwrap_or("");
        assert!(query.as_str().unwrap().contains("regex=true"), "{query}");
        assert!(regex.as_str().unwrap().contains("Default false"), "{regex}");
        assert!(
            path_filter.as_str().unwrap().contains("case-sensitive"),
            "{path_filter}"
        );
        assert!(
            budget.as_str().unwrap().contains("no page or cursor"),
            "{budget}"
        );
        assert!(tool_desc.contains("regex=true"), "{tool_desc}");
        assert!(tool_desc.contains("no pagination"), "{tool_desc}");
    }

    #[test]
    fn find_symbol_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        // 名称匹配分支：include_body=true 时零命中也不得出现
        // 误导性的"无可提取的符号体"。
        let named = server.find_symbol(Parameters(QueryParams {
            query: "NoSuchSymbolAnywhere".into(),
            include_body: true,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
        }));
        let named_text = named.content[0].as_text().unwrap().text.as_str();
        assert!(named_text.contains("(未找到匹配的符号)"), "{named_text}");
        assert!(!named_text.contains("无可提取的符号体"), "{named_text}");

        // 层级路径分支（query 含 '/'）同样需要显式零命中声明。
        let by_path = server.find_symbol(Parameters(QueryParams {
            query: "NoSuch/Path".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
        }));
        let path_text = by_path.content[0].as_text().unwrap().text.as_str();
        assert!(path_text.contains("(未找到匹配的符号)"), "{path_text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_large_body_is_gracefully_truncated() {
        let dir = unique_temp_dir();
        // 62 行的函数体：全文远超 300 token 预算，必然触发截断路径。
        let mut source = String::from("fn spacious() {\n");
        for i in 0..60 {
            source.push_str(&format!("    let v{i} = {i};\n"));
        }
        source.push_str("}\n");
        std::fs::write(dir.join("big.rs"), source).unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        let result = server.find_symbol(Parameters(QueryParams {
            query: "spacious".into(),
            include_body: true,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 300,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();

        // 不是整块丢弃：符号的前几行代码确实被展示。
        assert!(text.contains("let v0 = 0;"), "{text}");
        assert!(text.contains("let v1 = 1;"), "{text}");
        // 截断说明带总行数/已展示行数与 Read 指引。
        let note = regex::Regex::new(
            r"\[符号共 \d+ 行，受 budget_tokens 限制截断展示前 \d+ 行；查看内部局部代码请使用 Read\(offset, limit\)\]",
        )
        .unwrap();
        assert!(note.is_match(text), "truncation note missing: {text}");
        // 截断真的发生了：尾部行不在输出里。
        assert!(!text.contains("let v59 = 59;"), "{text}");
        // 已成功截断附加符号体，绝不能输出自相矛盾的"无可提取"。
        assert!(!text.contains("(include_body: 无可提取的符号体)"), "{text}");
        assert!(!text.contains("符号体读取失败"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_watch_mode_matrix() {
        assert_eq!(parse_watch_mode(None), Some(WatchMode::Events));
        assert_eq!(parse_watch_mode(Some("")), Some(WatchMode::Events));
        assert_eq!(parse_watch_mode(Some("0")), None);
        assert_eq!(parse_watch_mode(Some(" 0 ")), None);
        assert_eq!(
            parse_watch_mode(Some("5")),
            Some(WatchMode::Poll(Duration::from_secs(5)))
        );
        assert_eq!(
            parse_watch_mode(Some(" 10 ")),
            Some(WatchMode::Poll(Duration::from_secs(10)))
        );
        assert_eq!(parse_watch_mode(Some("invalid")), Some(WatchMode::Events));
    }
}
