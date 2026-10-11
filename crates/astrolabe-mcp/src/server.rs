use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked, TokenBudget},
    cache::DEFAULT_MEMORY_BUDGET_BYTES,
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
        ResourceContents, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, ErrorData, RoleServer, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    context::ClientContext,
    index::RepoIndex,
    knobs::McpKnobs,
    openai_schema::sanitize_for_openai_tools,
    session::{
        self, append_stale_note, run_barrier, start_idle_ticker, start_root_indexing, teardown_all,
        BarrierEffect, DispatchState, RootSession,
    },
    TOOL_CATALOG_TTL_MS,
};

pub(crate) use crate::session::IndexState;

const DEFAULT_BUDGET: usize = 2_500;
/// 资源卸载阈值基线：正文 token 估算超过阈值（含 SLACK）时转存为
/// MCP resource。`ASTROLABE_RESOURCE_THRESHOLD` 可调，格式错误回退默认。
pub(crate) const DEFAULT_RESOURCE_THRESHOLD_TOKENS: usize = 4_000;
/// budget_tokens 硬上限：动态阈值取 `max(基线, clamp(caller_budget))` 时
/// 防止异常大的 budget 把卸载阈值顶到天上。`ASTROLABE_MAX_BUDGET_TOKENS`
/// 可调，格式错误回退默认。
pub(crate) const DEFAULT_MAX_BUDGET_TOKENS: usize = 32_000;
/// 阈值判定双保险：`chars/4` 估算器对真实 tokenizer 的偏差容忍量。
pub(crate) const RESOURCE_SLACK_TOKENS: usize = 128;
/// 单行展示的最大列宽（字符数）：超长匹配行只展示命中点附近的安全切片。
const MAX_LINE_COLUMNS: usize = 250;
/// 切片窗口在命中点前后各带的上下文字符数。
const CONTEXT_PADDING: usize = 90;
/// 匹配段自身封顶：`.*` 类命中可能横跨上万字符，只展示前 40 字符。
const MAX_MATCH_SPAN_CHARS: usize = 40;
/// search_code 头部（confidence/mode/截断状态行）的固定 token 开销估算，
/// 从 budget 中先扣除，保证整个 body 不在阈值边界溢出。
const SEARCH_HEADER_TOKENS: usize = 35;
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
/// shown=0 且 omitted>0 是最坏情形：一条都没展示，必须专门诊断"首条
/// 结果单项超预算"，否则输出只剩元信息，读起来像服务故障/吞内容。
fn render_budget_status(shown: usize, omitted: usize, budget_tokens: usize) -> String {
    let truncated = omitted > 0;
    let mut status = format!("shown={shown} omitted={omitted} truncated={truncated}\n");
    if truncated {
        if shown == 0 {
            status.push_str(&format!(
                "这不是全集：共找到 {omitted} 条匹配，但首条结果单项超过 budget_tokens={budget_tokens} 被省略。加大 budget_tokens 或收紧 path_filter 后再查；本工具没有翻页。\n"
            ));
        } else {
            status.push_str(&format!(
                "这不是全集：{omitted} 条因 budget_tokens={budget_tokens} 被省略。加大 budget_tokens 或收紧 path_filter 后再查；本工具没有翻页。\n"
            ));
        }
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

/// Parse a `usize` env var; invalid values warn and fall back to `default`
/// so a typo cannot silently disable the knob.
fn parse_env_usize(raw: Option<&str>, var: &str, default: usize) -> usize {
    match raw {
        None => default,
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(value = %raw, var = var, "invalid env var; using default");
                default
            }
        },
    }
}

/// `ASTROLABE_RESOURCE_THRESHOLD`：资源卸载阈值基线（默认 4000）。
fn resource_threshold_base_from_env() -> usize {
    parse_env_usize(
        std::env::var("ASTROLABE_RESOURCE_THRESHOLD")
            .ok()
            .as_deref(),
        "ASTROLABE_RESOURCE_THRESHOLD",
        DEFAULT_RESOURCE_THRESHOLD_TOKENS,
    )
}

/// `ASTROLABE_MAX_BUDGET_TOKENS`：budget_tokens 硬上限（默认 32000）。
fn max_budget_tokens_from_env() -> usize {
    parse_env_usize(
        std::env::var("ASTROLABE_MAX_BUDGET_TOKENS").ok().as_deref(),
        "ASTROLABE_MAX_BUDGET_TOKENS",
        DEFAULT_MAX_BUDGET_TOKENS,
    )
}

/// 纯函数核心（max 显式传入），便于测试在并行环境下不读 env。
fn clamp_budget_within(raw: usize, max: usize) -> usize {
    raw.clamp(1, max)
}

/// 将 budget 限制在 `[1, max_budget_tokens_from_env()]`。
fn clamp_budget(raw: usize) -> usize {
    clamp_budget_within(raw, max_budget_tokens_from_env())
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

#[derive(Debug, Clone)]
pub struct AstrolabeServer {
    /// Parent (multi) or the single index root. Memory tools and
    /// `initial_instructions` use this; graph tools use the routed session.
    root: PathBuf,
    primary: Arc<RootSession>,
    dispatch: Option<Arc<DispatchState>>,
    ticker_stop: Arc<AtomicBool>,
    resources: Arc<Mutex<BTreeMap<String, String>>>,
    next_resource: Arc<AtomicU64>,
    cache_budget_bytes: u64,
    context: ClientContext,
    structured_output: bool,
    tool_router: ToolRouter<Self>,
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
        Self::assemble(root, budget_bytes, context, McpKnobs::from_env(), None)
    }

    /// Multi-project dispatch mode: do not index the parent; children activate
    /// on the first routed tool call.
    pub fn with_multi_project(
        root: PathBuf,
        children: Vec<PathBuf>,
        context: ClientContext,
    ) -> Self {
        let budget = cache_budget_from_env();
        let knobs = McpKnobs::from_env();
        let children: Vec<PathBuf> = children
            .into_iter()
            .map(|c| std::fs::canonicalize(&c).unwrap_or(c))
            .collect();
        tracing::info!(
            root = %root.display(),
            children = children.len(),
            "starting in multi-project dispatch mode"
        );
        for child in &children {
            tracing::info!(child = %child.display(), "multi-project child");
        }
        let dispatch = DispatchState::new(children, knobs.clone(), budget);
        Self::assemble(root, budget, context, knobs, Some(dispatch))
    }

    #[cfg(test)]
    pub(crate) fn with_multi_project_knobs(
        root: PathBuf,
        children: Vec<PathBuf>,
        knobs: McpKnobs,
        budget_bytes: u64,
    ) -> Self {
        let children: Vec<PathBuf> = children
            .into_iter()
            .map(|c| std::fs::canonicalize(&c).unwrap_or(c))
            .collect();
        let dispatch = DispatchState::new(children, knobs.clone(), budget_bytes);
        Self::assemble(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
            knobs,
            Some(dispatch),
        )
    }

    #[cfg(test)]
    pub(crate) fn new_with_knobs(root: PathBuf, knobs: McpKnobs, budget_bytes: u64) -> Self {
        Self::assemble(
            root,
            budget_bytes,
            ClientContext::default_builtin(),
            knobs,
            None,
        )
    }

    fn assemble(
        root: PathBuf,
        budget_bytes: u64,
        context: ClientContext,
        knobs: McpKnobs,
        dispatch: Option<Arc<DispatchState>>,
    ) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let structured_output = structured_output_for(&context);
        tracing::info!(
            root = %root.display(),
            context = %context.name,
            structured_output,
            cache_budget_bytes = budget_bytes,
            "configured bounded file cache"
        );
        let primary = RootSession::new(root.clone(), knobs, budget_bytes);
        Self {
            root: primary.root.clone(),
            cache_budget_bytes: budget_bytes,
            primary,
            dispatch,
            ticker_stop: Arc::new(AtomicBool::new(false)),
            resources: Arc::new(Mutex::new(BTreeMap::new())),
            next_resource: Arc::new(AtomicU64::new(1)),
            context,
            structured_output,
            tool_router: Self::tool_router(),
        }
    }

    pub fn start_indexing(&self) {
        if let Some(dispatch) = &self.dispatch {
            start_idle_ticker(Arc::clone(dispatch), Arc::clone(&self.ticker_stop));
            return;
        }
        start_root_indexing(Arc::clone(&self.primary));
    }

    /// Stop watchers and the idle-evict ticker. Tests call this to avoid
    /// leaving pollers running.
    pub fn stop_watching(&self) {
        self.ticker_stop.store(true, Ordering::SeqCst);
        self.primary.teardown();
        if let Some(dispatch) = &self.dispatch {
            teardown_all(dispatch);
        }
    }

    fn join_hint(parts: &[&str]) -> String {
        parts
            .iter()
            .copied()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn session_for(&self, hint: &str) -> Result<Arc<RootSession>, CallToolResult> {
        match &self.dispatch {
            None => Ok(Arc::clone(&self.primary)),
            Some(dispatch) => match session::route_session(dispatch, hint) {
                Ok(s) => Ok(s),
                Err(msg) => Err(self.finish(text_error(msg))),
            },
        }
    }

    fn with_index(
        &self,
        hint: &str,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        self.with_index_barrier(hint, false, f)
    }

    fn with_fresh_index(
        &self,
        hint: &str,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        self.with_index_barrier(hint, true, f)
    }

    fn with_index_barrier(
        &self,
        hint: &str,
        barrier: bool,
        f: impl FnOnce(&RootSession, &RepoIndex) -> CallToolResult,
    ) -> CallToolResult {
        let session = match self.session_for(hint) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let effect = if barrier {
            run_barrier(&session)
        } else {
            BarrierEffect::None
        };
        let result = match &*session.state.read().expect("index state lock poisoned") {
            IndexState::Building => self.finish_at(
                &session.root,
                text_error("索引正在后台构建；MCP 握手已完成，请稍后重试。confidence: unknown"),
            ),
            IndexState::Failed(error) => self.finish_at(
                &session.root,
                text_error(format!("索引构建失败：{error}\nconfidence: unknown")),
            ),
            IndexState::Ready(index) => f(&session, index),
        };
        match effect {
            BarrierEffect::None => result,
            BarrierEffect::StaleNote(note) => append_stale_note(result, &note),
        }
    }

    fn with_precise(
        &self,
        hint: &str,
        barrier: bool,
        f: impl FnOnce(&RootSession) -> CallToolResult,
    ) -> CallToolResult {
        let session = match self.session_for(hint) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let effect = if barrier {
            run_barrier(&session)
        } else {
            BarrierEffect::None
        };
        let result = self.finish_at(&session.root, f(&session));
        match effect {
            BarrierEffect::None => result,
            BarrierEffect::StaleNote(note) => append_stale_note(result, &note),
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

    fn finish(&self, result: CallToolResult) -> CallToolResult {
        self.finish_at(&self.root, result)
    }

    fn finish_at(&self, root: &Path, mut result: CallToolResult) -> CallToolResult {
        let root = root.display().to_string();
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

    /// 动态卸载阈值：基线与 caller budget（钳制后）的较大者。caller 明确
    /// 给了更大的 budget 时，正文允许留在线内而不是立刻被卸载。
    fn resource_threshold_for(&self, caller_budget: Option<usize>) -> usize {
        resource_threshold_base_from_env().max(caller_budget.map(clamp_budget).unwrap_or(0))
    }

    fn result(
        &self,
        title: &str,
        body: String,
        structured: Value,
        budget: Option<usize>,
    ) -> CallToolResult {
        self.result_on(&self.root, title, body, structured, budget)
    }

    fn result_on(
        &self,
        root: &Path,
        title: &str,
        body: String,
        structured: Value,
        budget: Option<usize>,
    ) -> CallToolResult {
        let body = if body.trim().is_empty() {
            "没有匹配结果。confidence: unknown".to_string()
        } else {
            body
        };
        // 双保险：估算器有偏差，判定线额外加 SLACK，避免在阈值边界处
        // 把刚好等于 budget 的正文误卸载。
        let threshold = self.resource_threshold_for(budget);
        if estimate_tokens(&body) > threshold.saturating_add(RESOURCE_SLACK_TOKENS) {
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
            let preview_text = resource_preview(&body);
            let mut result = CallToolResult::success(vec![
                ContentBlock::text(format!(
                    "{title} 结果较大（约 {} tokens），已转存为资源：{uri}\n\n【前瞻匹配摘要】：\n{preview_text}\n\n如需完整内容可通过 resources/read 获取。",
                    estimate_tokens(&body)
                )),
                ContentBlock::resource_link(resource),
            ]);
            result.structured_content = Some(structured);
            self.finish_at(root, result)
        } else {
            let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
            result.structured_content = Some(structured);
            self.finish_at(root, result)
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

fn top_unindexed_exts(
    counts: &std::collections::BTreeMap<String, u32>,
    limit: usize,
) -> Vec<(&str, u32)> {
    let mut list: Vec<(&str, u32)> = counts.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    list.truncate(limit);
    list
}

fn format_search_code_unindexed_hint(
    counts: &std::collections::BTreeMap<String, u32>,
    path_filter: Option<&str>,
) -> Option<String> {
    let max_count = counts.values().copied().max().unwrap_or(0);
    if max_count < 3 {
        return None;
    }
    let top = top_unindexed_exts(counts, 3);
    if top.is_empty() {
        return None;
    }
    let ext_summary = top
        .iter()
        .map(|(ext, count)| format!("{count} 个 .{ext} 文件"))
        .collect::<Vec<_>>()
        .join("、");
    let msg = match path_filter {
        Some(filter) => format!(
            "注意：path_filter '{filter}' 范围内有 {ext_summary}属于尚未支持的语言，未入代码索引——本结果只代表已索引语言中无匹配，不覆盖这些文件。请改用 Grep/Read 检索它们。\n"
        ),
        None => format!(
            "注意：仓库内有 {ext_summary}属于尚未支持的语言，未入代码索引——本结果只代表已索引语言中无匹配，不覆盖这些文件。请改用 Grep/Read 检索它们。\n"
        ),
    };
    Some(msg)
}

fn format_find_symbol_unindexed_hint(
    counts: &std::collections::BTreeMap<String, u32>,
) -> Option<String> {
    let max_count = counts.values().copied().max().unwrap_or(0);
    if max_count < 3 {
        return None;
    }
    let top = top_unindexed_exts(counts, 3);
    if top.is_empty() {
        return None;
    }
    let ext_summary = top
        .iter()
        .map(|(ext, count)| format!("{count} 个 .{ext}"))
        .collect::<Vec<_>>()
        .join("、");
    Some(format!(
        "(仓库另有 {ext_summary} 等未支持语言的源码文件未入符号索引，符号搜索不覆盖它们——如需检索请用 Grep/Read)\n"
    ))
}

fn file_extension(path: &str) -> Option<String> {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
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
            Some(params.budget_tokens),
        )
    }

    #[tool(
        description = "Session-gated language-server install. Without confirm_install (default): return needs_install plan (name, version_policy=latest, size if known) and do NOT download. With confirm_install=true: call core installer after the user agrees. Prefer this over silent downloads when precise tools report Unavailable."
    )]
    pub(crate) fn ensure_language_server(
        &self,
        Parameters(params): Parameters<crate::ensure_ls::EnsureLanguageServerParams>,
    ) -> CallToolResult {
        // Does not need the index; discovery is PATH/env based.
        self.finish(crate::ensure_ls::run_ensure_language_server(
            params,
            &crate::ensure_ls::default_installer(),
        ))
    }

    #[tool(
        description = "主入口：根据编码任务聚合最相关文件、符号、依赖与索引完整性信息。FIRST CALL for any coding task — before Read/Grep. Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart — other source files are invisible to this tool; use Grep/Read for them."
    )]
    pub(crate) fn resolve_context(
        &self,
        Parameters(params): Parameters<ResolveContextParams>,
    ) -> CallToolResult {
        self.with_fresh_index(&params.task_description, |session, index| {
            let base = compute_centrality(index.graph());
            let ranked = rank_for_task(index.graph(), &params.task_description, &base);
            let mut ids: Vec<_> = index.graph().files.iter().map(|file| file.id).collect();
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
                index.report().files_scanned,
                index.report().files_parsed,
                index.report().parse_failures.len(),
                index.report().unresolved_imports.len()
            ));
            let mut remaining =
                TokenBudget::new(params.budget_tokens.saturating_sub(estimate_tokens(&body)));
            let mut omitted_symbols = 0;
            for id in ids.into_iter().take(12) {
                if let Some(file) = index.file(id) {
                    for symbol in index
                        .graph()
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
            self.result_on(
                &session.root,
                "resolve_context",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
        description = "精确引用查询：由语言服务器解析，图谱的名字匹配不足以支撑改写决策。LSP Unavailable 时请向用户确认后调用 ensure_language_server（默认只出计划）。REQUIRED for rename/rewrite — grep matches text, not bindings."
    )]
    pub(crate) fn find_references(
        &self,
        Parameters(params): Parameters<crate::precise_tools::FindReferencesParams>,
    ) -> CallToolResult {
        let hint = Self::join_hint(&[params.path.as_deref().unwrap_or(""), &params.symbol]);
        self.with_precise(&hint, true, move |session| {
            crate::precise_tools::run_find_references(params, session.precise.as_ref())
        })
    }

    #[tool(
        description = "精确定义跳转：语言服务器绑定解析；不要用 find_symbol 代替（那是句法名字匹配）。LSP Unavailable → ask user, then ensure_language_server. The sanctioned way to resolve a definition — not Read/grep."
    )]
    pub(crate) fn goto_definition(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GotoDefinitionParams>,
    ) -> CallToolResult {
        let hint = params.path.clone();
        self.with_precise(&hint, true, move |session| {
            crate::precise_tools::run_goto_definition(params, session.precise.as_ref())
        })
    }

    #[tool(
        description = "文件诊断：语法与类型错误，用于改写后确认代码仍然成立。LSP Unavailable → ensure_language_server (user-gated). MUST run after editing a file to confirm it still holds."
    )]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GetDiagnosticsParams>,
    ) -> CallToolResult {
        let hint = params.path.clone();
        self.with_precise(&hint, false, move |session| {
            crate::precise_tools::run_get_diagnostics(params, session.precise.as_ref())
        })
    }

    #[tool(
        description = "符号语义信息（LSP hover：docstring/类型/签名）。LSP Unavailable → ensure_language_server (ask user first). Docstrings without reading the file — pair with find_symbol anchors."
    )]
    pub(crate) fn get_symbol_info(
        &self,
        Parameters(params): Parameters<crate::precise_tools::GetSymbolInfoParams>,
    ) -> CallToolResult {
        let hint = Self::join_hint(&[&params.path, &params.symbol]);
        self.with_precise(&hint, true, move |session| {
            crate::precise_tools::run_get_symbol_info(params, session.precise.as_ref())
        })
    }

    #[tool(
        description = "生成重命名计划，只产出待改位置与置信度，不写盘。Needs LSP; if Unavailable ask user then ensure_language_server. Always plan before apply_rename; NEVER hand-edit occurrences for a rename."
    )]
    pub(crate) fn plan_rename(
        &self,
        Parameters(params): Parameters<crate::precise_tools::PlanRenameParams>,
    ) -> CallToolResult {
        let hint = Self::join_hint(&[params.path.as_deref().unwrap_or(""), &params.symbol]);
        self.with_precise(&hint, true, move |session| {
            crate::precise_tools::run_plan_rename(params, session.precise.as_ref())
        })
    }

    #[tool(
        description = "应用重命名（不可逆）。默认仅 exact/scoped 写盘；低置信度需 force=true。写后重解析，失败整笔回滚。The only sanctioned way to rename; never hand-edit occurrences."
    )]
    pub(crate) fn apply_rename(
        &self,
        Parameters(params): Parameters<crate::precise_tools::ApplyRenameParams>,
    ) -> CallToolResult {
        let hint = Self::join_hint(&[params.path.as_deref().unwrap_or(""), &params.symbol]);
        self.with_precise(&hint, true, move |session| {
            crate::precise_tools::run_apply_rename(params, session.precise.as_ref(), &session.root)
        })
    }

    #[tool(
        description = "返回按 import 图中心性排名的仓库骨架与公开符号。Prefer over reading files for a first map of the repo."
    )]
    pub(crate) fn get_repo_skeleton(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        self.with_index("", |session, index| {
            let centrality = compute_centrality(index.graph());
            let mut ids: Vec<_> = index.graph().files.iter().map(|file| file.id).collect();
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
            self.result_on(
                &session.root,
                "get_repo_skeleton",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
            )
        })
    }

    #[tool(
        description = "按名称或子串定位符号，返回签名和 path:line 锚点。Prefer over Read for locating symbols — no whole-file reads needed. Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart — other source files are invisible to this tool; use Grep/Read for them."
    )]
    pub(crate) fn find_symbol(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> CallToolResult {
        self.with_fresh_index(&params.query, |session, index| {
            // "Class/method" 路径查询（含 "/"）走层级匹配器（Serena
            // NamePathMatcher 对应物：后缀匹配/绝对路径/kind 过滤/depth 展开）。
            if params.query.contains('/') {
                return self.find_symbol_by_path(&session.root, index, &params);
            }
            let needle = params.query.to_lowercase();
            // 命中对先收集 (锚点行, symbol 下标)，与展示列表同一排序源——
            // include_body 只对"用户实际看到的" kept 前几个附体（审查 major：
            // 原实现按 graph 构建序取前 5，可能对应被 budget 裁掉的符号）。
            let mut hits: Vec<(String, usize)> = Vec::new();
            for (idx, symbol) in index.graph().symbols.iter().enumerate() {
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
                let unindexed = index.unindexed_code_exts(None);
                if let Some(hint) = format_find_symbol_unindexed_hint(&unindexed) {
                    body.push_str(&hint);
                }
            }
            if params.include_body && !hits.is_empty() {
                self.attach_include_bodies(
                    &session.root,
                    index,
                    &mut body,
                    &hits,
                    kept_count,
                    params.budget_tokens,
                );
            }
            self.result_on(
                &session.root,
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
                Some(params.budget_tokens),
            )
        })
    }

    #[tool(
        description = "Search indexed source for a literal substring (default) or a regex when regex=true. Returns matching lines as path:line anchors. Query is literal unless regex=true — A|B without that flag searches for a vertical bar, not an alternation. Truncation is declared in the result (shown/omitted/truncated); raise budget_tokens or tighten path_filter for more hits — there is no pagination. Discovery only; edits and renames must be driven by find_references. Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart — other source files are invisible to this tool; use Grep/Read for them."
    )]
    pub(crate) fn search_code(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> CallToolResult {
        self.with_fresh_index(
            &Self::join_hint(&[params.path_filter.as_deref().unwrap_or(""), &params.query]),
            |session, index| {
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
                for file in &index.graph().files {
                    if params
                        .path_filter
                        .as_ref()
                        .is_some_and(|filter| !file.path.as_str().contains(filter))
                    {
                        continue;
                    }
                    let Some((lines, hit)) =
                        index.cached_lines(&session.file_cache, file.path.as_str())
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
                            // P0：超长行先做单行安全切片（Unicode 边界安全），
                            // 单条 match 不会把 budget 一口气吃光，也不会在
                            // 大小写折叠差异上 panic。
                            matches.push(format!(
                                "@{}:{} {}",
                                file.path,
                                line_no + 1,
                                clip_match_line(line, &params.query, regex.as_ref())
                            ));
                        }
                    }
                }
                session.cache_hits.fetch_add(hits, Ordering::Relaxed);
                session.cache_misses.fetch_add(misses, Ordering::Relaxed);
                tracing::debug!(
                    hits,
                    misses,
                    weighted_size = session.file_cache.weighted_size(),
                    budget_bytes = session.cache_budget_bytes,
                    "search_code file cache"
                );
                // 头部（confidence/mode/截断状态）约 35 tokens 的固定开销先
                // 扣除，保证整个 body 不会在阈值边界处溢出。
                let clamped_budget = clamp_budget(params.budget_tokens);
                let remaining = clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS);
                let (kept, omitted) =
                    truncate_ranked(&matches, remaining, |line| format!("{line}\n"));
                let shown = kept.len();
                let truncated = omitted > 0;
                let mode = if params.regex { "regex" } else { "literal" };
                let mut body = format!(
                    "confidence: exact (磁盘源码字面匹配)\n{}",
                    render_search_mode_header(params.regex, &params.query)
                );
                body.push_str(&render_budget_status(shown, omitted, clamped_budget));
                for line in kept {
                    body.push_str(line);
                    body.push('\n');
                }
                // 零命中时显式声明，区分"正常无匹配"与"服务故障/吞内容"
                // （对齐 find_references 的 `未找到对 … 的引用。` 空结果约定）。
                let mut unindexed_hint = false;
                if matches.is_empty() {
                    body.push_str("(未找到匹配的代码行)\n");
                    let counts = index.unindexed_code_exts(params.path_filter.as_deref());
                    if let Some(hint) =
                        format_search_code_unindexed_hint(&counts, params.path_filter.as_deref())
                    {
                        body.push_str(&hint);
                        unindexed_hint = true;
                    }
                }
                self.result_on(
                    &session.root,
                    "search_code",
                    body,
                    json!({
                        "confidence": "exact",
                        "mode": mode,
                        "matches": matches.len(),
                        "shown": shown,
                        "omitted": omitted,
                        "truncated": truncated,
                        "budget_tokens": params.budget_tokens,
                        "unindexed_hint": unindexed_hint
                    }),
                    Some(params.budget_tokens),
                )
            },
        )
    }

    #[tool(
        description = "沿 scope-aware import 图查询文件的依赖或被依赖关系。Replaces manual grep for \"who imports this file\" — scoped from the import graph."
    )]
    pub(crate) fn get_dependents(
        &self,
        Parameters(params): Parameters<DependentsParams>,
    ) -> CallToolResult {
        self.with_fresh_index(&params.target, |session, index| {
            let Some(target) = index.file_id(&params.target) else {
                let unindexed = index.unindexed_code_exts(None);
                let target_ext = file_extension(&params.target);
                let message = if let Some(ext) = target_ext.filter(|e| unindexed.contains_key(e)) {
                    format!(
                        "目标 {} 是 .{} 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。\nconfidence: exact",
                        params.target, ext
                    )
                } else {
                    format!("未找到目标路径：{}\nconfidence: exact", params.target)
                };
                return self.result_on(
                    &session.root,
                    "get_dependents",
                    message,
                    json!({"confidence":"exact","found":false,"budget_tokens":params.budget_tokens}),
                    Some(params.budget_tokens),
                );
            };
            let ids = if params.direction == "dependencies" {
                dependencies(index.graph(), target)
            } else {
                dependents(index.graph(), target)
            };
            let body = self.ranked_files(index, ids, params.budget_tokens, Confidence::Scoped);
            self.result_on(
                &session.root,
                "get_dependents",
                body,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
        self.with_fresh_index(&params.symbol, |session, index| {
            // depth>1：多跳树（对齐 openvisio trace.ts；环检测 + 缩进渲染）。
            if params.depth > 1 {
                let body = self.trace_calls_tree_body(index, &params);
                return self.result_on(
                    &session.root,
                    "trace_calls",
                    body,
                    json!({"confidence":"syntactic","depth":params.depth.clamp(1, 6),"budget_tokens":params.budget_tokens}),
                    Some(params.budget_tokens),
                );
            }
            let wanted: BTreeSet<_> = index
                .graph()
                .symbols
                .iter()
                .filter(|symbol| symbol.name.eq_ignore_ascii_case(&params.symbol))
                .map(|symbol| symbol.id.0)
                .collect();
            let mut lines = BTreeSet::new();
            for edge in index.graph().edges.iter().filter(|edge| edge.kind == EdgeKind::Call) {
                let include = match params.direction.as_str() {
                    "callers" => wanted.contains(&edge.to),
                    "callees" => wanted.contains(&edge.from),
                    _ => wanted.contains(&edge.from) || wanted.contains(&edge.to),
                };
                if !include {
                    continue;
                }
                let from = index.graph().symbols.iter().find(|symbol| symbol.id.0 == edge.from);
                let to = index.graph().symbols.iter().find(|symbol| symbol.id.0 == edge.to);
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
            self.result_on(
                &session.root,
                "trace_calls",
                body,
                json!({"confidence":"syntactic","budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
        self.with_fresh_index(&params.target, |session, index| {
            let Some(target_id) = index.file_id(&params.target) else {
                let unindexed = index.unindexed_code_exts(None);
                let target_ext = file_extension(&params.target);
                let message = if let Some(ext) = target_ext.filter(|e| unindexed.contains_key(e)) {
                    format!(
                        "目标 {} 是 .{} 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。\n",
                        params.target, ext
                    )
                } else {
                    format!("未找到目标文件：{}\n", params.target)
                };
                return self.result_on(
                    &session.root,
                    "get_neighborhood",
                    message,
                    json!({"confidence":"unknown","budget_tokens":params.budget_tokens}),
                    Some(params.budget_tokens),
                );
            };
            let direction = match params.direction.as_str() {
                "dependencies" => astrolabe_core::neighborhood::NeighborhoodDirection::Dependencies,
                "dependents" => astrolabe_core::neighborhood::NeighborhoodDirection::Dependents,
                _ => astrolabe_core::neighborhood::NeighborhoodDirection::Both,
            };
            let depth = params.depth.clamp(1, u16::from(astrolabe_core::neighborhood::MAX_NEIGHBORHOOD_DEPTH)) as u8;
            let entries = astrolabe_core::neighborhood::neighborhood(
                index.graph(),
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
                return self.result_on(
                    &session.root,
                    "get_neighborhood",
                    header,
                    json!({"confidence":"scoped","entries":0,"depth":depth,"budget_tokens":params.budget_tokens}),
                    Some(params.budget_tokens),
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
            self.result_on(
                &session.root,
                "get_neighborhood",
                body,
                json!({"confidence":"scoped","entries":entries.len(),"depth":depth,"budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
                Some(params.budget_tokens),
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
            Some(params.budget_tokens),
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
                Some(params.budget_tokens),
            ),
            None => self.result(
                "read_memory",
                format!("记忆 `{}` 不存在（先 list_memories 查名）。confidence: exact\n", params.name),
                json!({"confidence":"exact","found":false,"budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
                Some(params.budget_tokens),
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
                Some(params.budget_tokens),
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
        self.with_index("", |session, index| {
            let centrality = compute_centrality(index.graph());
            let depth = params.depth.clamp(1, 3) as u8;
            let (nodes, edges) =
                astrolabe_core::group_graph::group_graph(index.graph(), &centrality, depth);
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
            self.result_on(
                &session.root,
                "get_group_graph",
                body,
                json!({"confidence":"scoped","groups":nodes.len(),"edges":edges.len(),"depth":depth,"budget_tokens":params.budget_tokens}),
                Some(params.budget_tokens),
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
        root: &Path,
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
            let symbol = &index.graph().symbols[*idx];
            let Some(file) = index.file(symbol.file) else {
                failed += 1;
                continue;
            };
            match astrolabe_core::body::symbol_body(&root.join(file.path.as_str()), symbol) {
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
    fn find_symbol_by_path(
        &self,
        root: &Path,
        index: &RepoIndex,
        params: &QueryParams,
    ) -> CallToolResult {
        let kind = match params.kind.as_deref() {
            None => None,
            Some(name) => match kind_from_name(name) {
                Ok(kind) => Some(kind),
                Err(message) => return self.finish_at(root, text_error(message)),
            },
        };
        let kind = astrolabe_core::name_path::KindFilter(kind);
        let matches = astrolabe_core::name_path::match_name_path(
            &index.graph().symbols,
            &params.query,
            params.substring,
            kind,
            params.depth,
        );
        // 命中对先收集 (锚点行, symbol 下标)，与名称分支同一 include_body 排序源。
        let mut hits: Vec<(String, usize)> = Vec::new();
        for m in &matches {
            if let Some((idx, symbol)) = index
                .graph()
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
            let unindexed = index.unindexed_code_exts(None);
            if let Some(hint) = format_find_symbol_unindexed_hint(&unindexed) {
                body.push_str(&hint);
            }
        }
        if params.include_body && !hits.is_empty() {
            self.attach_include_bodies(
                root,
                index,
                &mut body,
                &hits,
                kept_count,
                params.budget_tokens,
            );
        }
        self.result_on(
            root,
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
            Some(params.budget_tokens),
        )
    }

    /// 多跳调用树（depth 1-6）：对每个同名符号各跑一棵 trace_tree，
    /// 缩进渲染 + 环标记。同名多根时各树以根锚点行分隔。
    /// M3：整树渲染受 budget_tokens 截断保护——超大森林只保留预算内的
    /// 前缀行并显式声明省略数，不无限内联。
    #[allow(clippy::too_many_lines)]
    fn trace_calls_tree_body(&self, index: &RepoIndex, params: &TraceParams) -> String {
        let direction = match params.direction.as_str() {
            "callers" => astrolabe_core::trace_tree::TraceDirection::Callers,
            "callees" => astrolabe_core::trace_tree::TraceDirection::Callees,
            _ => astrolabe_core::trace_tree::TraceDirection::Both,
        };
        let depth = params.depth.clamp(1, 6) as u8;
        let mut header = format!(
            "confidence: syntactic {}\n警告：调用图基于名字匹配，可能漏掉动态分派或包含同名误报。depth={depth}，(cycle) = 路径成环已截断。\n",
            confidence_note(Confidence::Syntactic)
        );
        // 同名符号多根一次建图 + 全森林硬上限（审查 major #3：逐根重建邻接、
        // 无节点上限，depth=6 稠密图组合爆炸）。
        let roots: Vec<_> = index
            .graph()
            .symbols
            .iter()
            .filter(|symbol| symbol.name.eq_ignore_ascii_case(&params.symbol))
            .map(|symbol| symbol.id)
            .collect();
        let forest =
            astrolabe_core::trace_tree::trace_forest(index.graph(), &roots, direction, depth, 2000);
        if forest.entries.is_empty() {
            header.push_str("未找到同名符号或无可展开调用边。\n");
            return header;
        }
        // 预渲染 SymbolId → 锚点行，避免逐节点线性扫符号表（审查 major #3）。
        let mut line_by_id: BTreeMap<_, String> = BTreeMap::new();
        for symbol in &index.graph().symbols {
            if let Some(file) = index.file(symbol.file) {
                line_by_id.insert(symbol.id, symbol_line(symbol, &file.path));
            }
        }
        let mut lines: Vec<String> = Vec::new();
        for entry in &forest.entries {
            let Some(root_line) = line_by_id.get(&entry.root) else {
                continue;
            };
            lines.push(String::new());
            lines.push(format!("root: {root_line}"));
            // Both = Callees 树在前、Callers 树在后（trace_tree 语义）——第二
            // 个 depth=0 是分界，打分段头；depth=0 节点本身已由 root 头表达
            // （审查 major #4：根重复打印）。
            let mut seen_root: usize = 0;
            for node in &entry.nodes {
                if node.depth == 0 {
                    if direction == astrolabe_core::trace_tree::TraceDirection::Both
                        && seen_root == 1
                    {
                        lines.push("--- callers ---".to_string());
                    }
                    seen_root += 1;
                    continue;
                }
                let Some(line) = line_by_id.get(&node.symbol) else {
                    continue;
                };
                let cycle = if node.cycle { " (cycle)" } else { "" };
                lines.push(format!(
                    "{}{}{cycle}",
                    "  ".repeat(node.depth as usize),
                    line
                ));
            }
        }
        // M3 树保护：树行纳入 budget_tokens，超出部分整行丢弃并声明。
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
            body.push_str(&format!(
                "{omitted} 个树节点因 budget_tokens 被省略（调用树过大；可减小 depth 或改用 find_references 后重查）。\n"
            ));
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
        self.with_index("", |session, index| {
            // churn 融合（对齐 openvisio buildHotspots）：中心性 × churn 增益。
            // 无 git / git 失败 → 空表 → 增益恒 1，退化为纯中心性（向后兼容）。
            let centrality = compute_centrality(index.graph());
            let churn = session.cached_churn();
            let mut scored: Vec<(FileId, f64)> = index
                .graph()
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
            self.result_on(
                &session.root,
                "get_hotspots",
                header,
                json!({"confidence":"scoped","budget_tokens":params.budget_tokens,"churn_fused":churn_hits>0,"churn_files":churn_hits}),
                Some(params.budget_tokens),
            )
        })
    }

    #[tool(
        description = "统计已索引仓库的语言、文件数与代码行数，并标注 LSP 状态 Ready / needs_install / AST-only。Index coverage + session-gated install status."
    )]
    pub(crate) fn get_languages(
        &self,
        Parameters(params): Parameters<BudgetOnly>,
    ) -> CallToolResult {
        self.with_index("", |session, index| {
            let mut totals: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            for file in &index.graph().files {
                if let Some(language) = file.language {
                    let total = totals.entry(language.name()).or_default();
                    total.0 += 1;
                    total.1 += u64::from(file.loc);
                }
            }
            let installer = crate::ensure_ls::default_installer();
            let status_by_name: std::collections::BTreeMap<&str, _> = installer
                .probed_statuses()
                .into_iter()
                .map(|(lang, status)| (lang.name(), status))
                .collect();
            let mut rows: Vec<_> = totals
                .into_iter()
                .map(|(language, (files, loc))| {
                    let suffix = status_by_name
                        .get(language)
                        .map(|s| s.summary_suffix())
                        .unwrap_or_else(|| {
                            " — AST-only (language server probe not wired)".to_string()
                        });
                    format!("{language}: {files} files, {loc} LOC{suffix}")
                })
                .collect();
            rows.sort_by(|a, b| b.cmp(a));
            let (kept, omitted) =
                truncate_ranked(&rows, params.budget_tokens, |row| format!("{row}\n"));
            let cache_bytes = session.file_cache.weighted_size();
            let cache_hits = session.cache_hits.load(Ordering::Relaxed);
            let cache_misses = session.cache_misses.load(Ordering::Relaxed);
            tracing::debug!(
                weighted_size = cache_bytes,
                budget_bytes = session.cache_budget_bytes,
                cache_hits,
                cache_misses,
                "file cache occupancy"
            );
            let mut body = format!(
                "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart\n\
                 confidence: exact (来自已扫描文件元数据 + LSP discovery)\n\
                 file_cache: {cache_bytes}/{} bytes, hits={cache_hits}, misses={cache_misses}\n\
                 LSP status: Ready = server on PATH; needs_install = ask user then \
                 ensure_language_server; AST-only = parse/index only (server not wired).\n\
                 Install is session-gated: ensure_language_server without confirm returns a \
                 plan only (latest, no download).\n",
                session.cache_budget_bytes
            );
            for row in kept {
                body.push_str(row);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 种语言因 budget_tokens 被省略。\n"));
            }
            let unindexed = index.unindexed_code_exts(None);
            if unindexed.is_empty() {
                body.push_str("unindexed source files: none\n");
            } else {
                let mut list: Vec<(&String, &u32)> = unindexed.iter().collect();
                list.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
                let items_str = list
                    .into_iter()
                    .map(|(ext, count)| format!(".{ext}×{count}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(
                    "unindexed source files: {items_str} (语言暂不支持；这些文件不在符号/import 索引内，search_code/find_symbol 对其不可见，请用 Grep/Read)\n"
                ));
            }
            self.result_on(
                &session.root,
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
                Some(params.budget_tokens),
            )
        })
    }
}

#[tool_handler]
impl ServerHandler for AstrolabeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
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

/// P0 单行安全切片：超长匹配行只展示命中点附近的窗口。
///
/// 严禁用 `to_lowercase().find(..)` 拿字节偏移切原串——`İ`/`ẞ` 等字符的
/// 大小写折叠会改变字节长度， lowered 串上的偏移在原串上漂移，切片直接
/// panic。这里统一经 `regex::Match`（regex 保证字节偏移落在字符边界上）
/// 定位命中；字面量搜索用 `regex::escape(needle)` 构建 case-insensitive
/// regex 在原串上查找。找不到 Match（极端 Unicode 折叠差异，或命中只落
/// 在被 trim 掉的空白里）时退回行首固定窗口兜底，绝不 unwrap/panic。
fn clip_match_line(line: &str, needle: &str, regex: Option<&regex::Regex>) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() <= MAX_LINE_COLUMNS {
        return trimmed.to_string();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let total = chars.len();

    let found = match regex {
        Some(regex) => regex.find(trimmed),
        None => regex::RegexBuilder::new(&regex::escape(needle))
            .case_insensitive(true)
            .build()
            .ok()
            .and_then(|literal| literal.find(trimmed)),
    };
    let Some(found) = found else {
        // 安全兜底：行首 250 字符窗口 + 省略号。
        let head: String = chars.into_iter().take(MAX_LINE_COLUMNS).collect();
        return format!("{head}…");
    };

    // Match 的字节偏移保证在字符边界上；换算成字符下标再开窗口。
    let start = trimmed[..found.start()].chars().count();
    let span = found.as_str().chars().count();
    let end = start + span;

    let mut out = String::new();
    let window_start = start.saturating_sub(CONTEXT_PADDING);
    if window_start > 0 {
        out.push('…');
    }
    out.extend(chars[window_start..start].iter());
    if span > MAX_MATCH_SPAN_CHARS {
        // 匹配段封顶：`.*` 命中上万字符时只展示前 40 字符。
        out.extend(chars[start..start + MAX_MATCH_SPAN_CHARS].iter());
        out.push_str(&format!("…[匹配段共 {span} 字符，超出部分已省略]"));
    } else {
        out.extend(chars[start..end].iter());
    }
    let after_end = (end + CONTEXT_PADDING).min(total);
    out.extend(chars[end..after_end].iter());
    if after_end < total {
        out.push('…');
    }
    out
}

/// P1 前瞻摘要：资源卸载时从正文提取前 5 行、总量约 200 tokens 以内的
/// 预览，塞进首个 TextContent——客户端不读 resource 也能判断"这次卸载
/// 的内容是不是我要的"。首行单行超预算时按字符截断，保证 content[0]
/// 始终有可读文本。
fn resource_preview(body: &str) -> String {
    const PREVIEW_LINES: usize = 5;
    const PREVIEW_MAX_TOKENS: usize = 200;
    let mut preview = String::new();
    let mut cut = false;
    for (index, line) in body.lines().enumerate() {
        if index >= PREVIEW_LINES {
            cut = true;
            break;
        }
        let candidate = format!("{preview}{line}\n");
        if estimate_tokens(&candidate) > PREVIEW_MAX_TOKENS {
            if index == 0 {
                // 单行就超预算：按字符截到 200 tokens（约 800 字符）内。
                let clipped: String = line.chars().take(PREVIEW_MAX_TOKENS * 4).collect();
                preview.push_str(&clipped);
                preview.push_str("…\n");
            }
            cut = true;
            break;
        }
        preview = candidate;
    }
    if cut {
        preview.push_str("…（前瞻摘要已截断，完整内容见资源）\n");
    }
    preview.trim_end().to_string()
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
    use std::time::Duration;

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
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(
            RepoIndex::from_graph(PathBuf::from("."), graph, Default::default()),
        ));
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
        // SLACK 双保险：必须超过 基线+SLACK 才卸载，恰好超基线 1 token
        // 仍保持内联（见 dynamic_threshold_boundary_keeps_exact_budget_inline）。
        let result = server.result(
            "large",
            "x".repeat((DEFAULT_RESOURCE_THRESHOLD_TOKENS + RESOURCE_SLACK_TOKENS + 1) * 4),
            json!({}),
            None,
        );
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_resource_link().is_some());
    }

    #[test]
    fn dynamic_threshold_boundary_keeps_exact_budget_inline() {
        let server = ready_server();
        // caller budget=6000 且正文恰好 6000 tokens：<= threshold+SLACK，
        // 必须保持内联，不被卸载（kept 恰好等于 budget 的边界）。
        let inline = server.result("boundary", "x".repeat(6_000 * 4), json!({}), Some(6_000));
        assert_eq!(
            inline.content.len(),
            1,
            "exact-budget body must stay inline"
        );
        assert!(inline.content[0].as_text().is_some());

        // 超过 budget + SLACK 后才卸载为资源。
        let offloaded = server.result(
            "boundary",
            "x".repeat((6_000 + RESOURCE_SLACK_TOKENS + 1) * 4),
            json!({}),
            Some(6_000),
        );
        assert!(offloaded.content[0].as_text().is_some());
        assert!(offloaded.content[1].as_resource_link().is_some());
    }

    #[test]
    fn budget_clamp_hard_cap_and_floor() {
        // 硬上限：异常大的 budget 被压回 max。
        assert_eq!(clamp_budget_within(100_000, 32_000), 32_000);
        assert_eq!(clamp_budget_within(32_000, 32_000), 32_000);
        // 下限：0/1 钳到 1，预算永远非零正数。
        assert_eq!(clamp_budget_within(0, 32_000), 1);
        assert_eq!(clamp_budget_within(1, 32_000), 1);
        // 常规值原样通过。
        assert_eq!(clamp_budget_within(5_000, 32_000), 5_000);
        assert_eq!(clamp_budget_within(9, 8), 8);
        // env 解析：未设/空白数字/格式错误回退默认。
        assert_eq!(parse_env_usize(None, "X", 4_000), 4_000);
        assert_eq!(parse_env_usize(Some(" 123 "), "X", 4_000), 123);
        assert_eq!(parse_env_usize(Some("nope"), "X", 4_000), 4_000);
        assert_eq!(parse_env_usize(Some("0"), "X", 4_000), 0);
    }

    #[test]
    fn resource_threshold_takes_max_of_base_and_clamped_budget() {
        let server = ready_server();
        let base = resource_threshold_base_from_env();
        // 无 budget / 预算低于基线：阈值就是基线。
        assert_eq!(server.resource_threshold_for(None), base);
        assert_eq!(server.resource_threshold_for(Some(0)), base);
        assert_eq!(server.resource_threshold_for(Some(100)), base.max(100));
        // 预算高于基线：阈值抬到钳制后的 budget。
        assert_eq!(
            server.resource_threshold_for(Some(6_000)),
            base.max(clamp_budget(6_000))
        );
        // 异常大的 budget 被硬上限压住，阈值不会无限抬升。
        assert_eq!(clamp_budget(usize::MAX), max_budget_tokens_from_env());
        assert_eq!(
            server.resource_threshold_for(Some(usize::MAX)),
            base.max(max_budget_tokens_from_env())
        );
    }

    #[test]
    fn offloaded_result_carries_lookahead_preview() {
        let server = ready_server();
        let mut body = String::new();
        for i in 0..30 {
            body.push_str(&format!("line-{i:02} {}\n", "z".repeat(660)));
        }
        // 30 行 × ~168 tokens ≈ 5040 tokens > 4000 + SLACK → 卸载。
        let result = server.result("search_code", body.clone(), json!({}), Some(2_500));
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_resource_link().is_some());
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("已转存为资源"), "{text}");
        assert!(text.contains("【前瞻匹配摘要】"), "{text}");
        assert!(text.contains("line-00"), "{text}");
        assert!(text.contains("resources/read"), "{text}");
        // 摘要封顶 ~200 tokens：第 2 行就放不下，不能把整份 body 塞进摘要。
        assert!(!text.contains("line-01 "), "{text}");
        assert!(!text.contains("line-05"), "{text}");
        // 资源里保存的是完整 body。
        let stored = server.resources.lock().unwrap();
        assert!(
            stored.values().any(|v| v == &body),
            "offloaded resource must hold the full body"
        );
    }

    #[test]
    fn clip_match_line_short_lines_are_returned_trimmed() {
        assert_eq!(clip_match_line("  let x = 1;  ", "x", None), "let x = 1;");
        // 正好 250 字符：边界内原样返回。
        let exact: String = "a".repeat(MAX_LINE_COLUMNS);
        assert_eq!(clip_match_line(&exact, "a", None), exact);
        // 超一个字符才进入切片路径。
        let over = format!("{exact}b");
        let clipped = clip_match_line(&over, "a", None);
        assert!(clipped.contains('…'), "{clipped}");
        assert!(clipped.chars().count() <= MAX_LINE_COLUMNS + 1);
    }

    #[test]
    fn clip_match_line_long_ascii_centers_on_needle() {
        let line = format!("padding {} NEEDLE {}", "z".repeat(400), "y".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.contains("NEEDLE"), "{clipped}");
        assert!(
            clipped.starts_with('…') && clipped.ends_with('…'),
            "{clipped}"
        );
        assert!(clipped.chars().count() < 2 * MAX_LINE_COLUMNS, "{clipped}");
    }

    #[test]
    fn clip_match_line_match_at_start_and_end_avoids_false_ellipses() {
        // 行首命中：无前导省略号（窗口起点就是 0）。
        let line = format!("NEEDLE{}", "z".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.starts_with("NEEDLE"), "{clipped}");
        assert!(!clipped.starts_with('…'), "{clipped}");
        assert!(clipped.ends_with('…'), "{clipped}");

        // 行尾命中：无尾随省略号（窗口终点贴齐行尾）。
        let line = format!("{}NEEDLE", "z".repeat(400));
        let clipped = clip_match_line(&line, "needle", None);
        assert!(clipped.ends_with("NEEDLE"), "{clipped}");
        assert!(clipped.starts_with('…'), "{clipped}");
        assert!(!clipped.ends_with("NEEDLE…"), "{clipped}");
    }

    #[test]
    fn clip_match_line_unicode_case_folding_never_panics() {
        // ẞ (U+1E9E) 与 ß (U+00DF) 是 simple-case-folding 同组：regex 能在
        // 原串上定位命中（naive 的 to_lowercase 字节偏移在这里会漂移 panic）。
        let line = format!("{}ẞ{}", "x".repeat(300), "y".repeat(300));
        let clipped = clip_match_line(&line, "ß", None);
        assert!(clipped.contains('ẞ'), "{clipped}");

        // İ (U+0130) 的完整小写是两个码点（i + U+0307），simple folding 不同
        // 组：regex 找不到 Match，必须走行首安全窗口兜底而不是 panic。
        let line = format!("{}İ tail", "İ".repeat(300));
        let clipped = clip_match_line(&line, "i\u{307}", None);
        assert!(clipped.chars().count() <= MAX_LINE_COLUMNS + 1, "{clipped}");
        assert!(clipped.ends_with('…'), "{clipped}");
    }

    #[test]
    fn clip_match_line_caps_huge_match_spans() {
        // 纯 CJK 超长单行、命中整行（字面量 needle 覆盖全行）：匹配段封顶。
        let line = "中".repeat(600);
        let clipped = clip_match_line(&line, &line, None);
        assert!(clipped.contains("匹配段"), "{clipped}");
        assert!(clipped.contains('…'), "{clipped}");
        assert!(clipped.chars().count() < MAX_LINE_COLUMNS, "{clipped}");
        assert!(
            !clipped.contains(&"中".repeat(MAX_MATCH_SPAN_CHARS + 1)),
            "{clipped}"
        );

        // regex `.*` 类命中横跨整行：同样封顶。
        let regex = regex::Regex::new("a.*b").unwrap();
        let line = format!("a{}b", "m".repeat(500));
        let clipped = clip_match_line(&line, "", Some(&regex));
        assert!(clipped.contains("匹配段"), "{clipped}");
        assert!(clipped.chars().count() < MAX_LINE_COLUMNS, "{clipped}");
    }

    #[test]
    fn search_code_clips_overlong_matching_lines() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("long.py"),
            format!("marker = '{}'\n", "needle".repeat(300)),
        )
        .unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        let result = server.search_code(Parameters(SearchParams {
            query: "marker".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 2_500,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        let rendered = text
            .lines()
            .find(|line| line.contains("@long.py:1"))
            .expect("match line must exist");
        assert!(rendered.contains("marker"), "{rendered}");
        assert!(rendered.contains('…'), "{rendered}");
        assert!(
            rendered.chars().count() < MAX_LINE_COLUMNS + 100,
            "{rendered}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_zero_shown_diagnoses_single_item_overflow() {
        let dir = unique_temp_dir();
        let mut source = String::from("needle = 0\n");
        for i in 1..10 {
            source.push_str(&format!("needle = {i}\n"));
        }
        std::fs::write(dir.join("hits.py"), source).unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;

        // budget=1：扣掉头部 35 tokens 后剩余 0，首条匹配单项就超预算。
        let result = server.search_code(Parameters(SearchParams {
            query: "needle".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 1,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("shown=0"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(
            text.contains("首条结果单项超过 budget_tokens=1 被省略"),
            "{text}"
        );
        assert!(text.contains("本工具没有翻页"), "{text}");
        // 一条都没展示时，正文绝不能出现匹配行内容本身。
        assert!(!text.contains("@hits.py:1 "), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trace_calls_tree_respects_budget_tokens() {
        let mut server =
            AstrolabeServer::new_with_cache_budget(PathBuf::from("."), DEFAULT_MEMORY_BUDGET_BYTES);
        // 30 个 caller → alpha：depth=2 的树有 30 个节点行，预算只给 10。
        let mut symbols = vec![CodeSymbol {
            id: SymbolId(0),
            file: FileId(0),
            name: "alpha".into(),
            kind: SymbolKind::Function,
            signature: "fn alpha()".into(),
            start_line: 1,
            end_line: 1,
            exported: true,
        }];
        let mut edges = Vec::new();
        for i in 1..=30u32 {
            symbols.push(CodeSymbol {
                id: SymbolId(i),
                file: FileId(0),
                name: format!("caller{i}"),
                kind: SymbolKind::Function,
                signature: format!("fn caller{i}()"),
                start_line: i + 1,
                end_line: i + 1,
                exported: false,
            });
            edges.push(astrolabe_core::CodeEdge {
                from: i,
                to: 0,
                kind: EdgeKind::Call,
                weight: 1,
                confidence: Confidence::Syntactic,
            });
        }
        let graph = CodeGraph {
            files: vec![CodeFile {
                id: FileId(0),
                path: RelPath::new("src/lib.rs"),
                language: Some(Language::Rust),
                loc: 40,
                sha: String::new(),
            }],
            symbols,
            edges,
        };
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(
            RepoIndex::from_graph(PathBuf::from("."), graph, Default::default()),
        ));
        server.structured_output = false;

        let result = server.trace_calls(Parameters(TraceParams {
            symbol: "alpha".into(),
            direction: "callers".into(),
            depth: 2,
            budget_tokens: 10,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        assert!(text.contains("个树节点因 budget_tokens 被省略"), "{text}");
        assert!(!text.contains("caller25"), "{text}");
        // 大预算下同一棵树完整内联（保护不改变正常路径）。
        let full = server.trace_calls(Parameters(TraceParams {
            symbol: "alpha".into(),
            direction: "callers".into(),
            depth: 2,
            budget_tokens: 4_000,
        }));
        let full_text = full.content[0].as_text().unwrap().text.clone();
        assert!(full_text.contains("caller25"), "{full_text}");
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
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
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
        assert!(server.primary.cache_misses.load(Ordering::Relaxed) >= 1);
        assert_eq!(server.primary.cache_hits.load(Ordering::Relaxed), 0);

        let second = server.search_code(Parameters(params));
        let second_text = second.content[0].as_text().unwrap().text.as_str();
        assert!(second_text.contains("cache-me"), "{second_text}");
        assert!(
            server.primary.cache_hits.load(Ordering::Relaxed) >= 1,
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
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        let _ = server.search_code(Parameters(SearchParams {
            query: "payload".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 200,
        }));

        let weighted = server.primary.file_cache.weighted_size();
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
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
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
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
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

    fn first_text(result: CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|c| c.text.clone())
            .unwrap_or_default()
    }

    fn wait_ready(server: &AstrolabeServer, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            match &*server.primary.state.read().unwrap() {
                IndexState::Ready(_) => return,
                IndexState::Failed(e) => panic!("index failed: {e}"),
                IndexState::Building => {}
            }
            if start.elapsed() > timeout {
                panic!("timeout waiting for Ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_session_ready(session: &crate::session::RootSession, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            match &*session.state.read().unwrap() {
                IndexState::Ready(_) => return,
                IndexState::Failed(e) => panic!("index failed: {e}"),
                IndexState::Building => {}
            }
            if start.elapsed() > timeout {
                panic!("timeout waiting for child Ready");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn git_marker(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
    }

    fn test_knobs() -> crate::knobs::McpKnobs {
        crate::knobs::McpKnobs {
            safety_interval: Duration::ZERO,
            reindex_throttle: Duration::from_secs(60),
            watch_mode: Some(crate::knobs::WatchMode::Events),
            ..crate::knobs::McpKnobs::default()
        }
    }

    #[test]
    fn cold_build_edit_is_visible_at_ready() {
        let dir = unique_temp_dir();
        for i in 0..40 {
            std::fs::write(
                dir.join(format!("f{i}.py")),
                format!("def f{i}():\n    return {i}\n"),
            )
            .unwrap();
        }
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();

        // The watch handle is published before the baseline scan finishes.
        // Writing in that gap used to be sealed into the snapshot (no event)
        // while the cold walk missed the file, so Ready never contained it.
        // Wait until the baseline is sealed and the build is still running.
        let mut wrote = false;
        let write_deadline = std::time::Instant::now() + Duration::from_secs(20);
        while std::time::Instant::now() < write_deadline {
            let seeded = server
                .primary
                .freshness
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|log| log.is_seeded());
            let building = matches!(*server.primary.state.read().unwrap(), IndexState::Building);
            if seeded && building {
                std::fs::write(
                    dir.join("during.py"),
                    "def during_build_xyz():\n    return 1\n",
                )
                .unwrap();
                wrote = true;
                break;
            }
            if matches!(*server.primary.state.read().unwrap(), IndexState::Ready(_)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            wrote,
            "never observed Building after watcher baseline; cold build was too fast"
        );
        wait_ready(&server, Duration::from_secs(30));
        // Notify delivery and the 500ms debounce can land the changeset just
        // after the Ready publish. The edit must still show up (pending drain
        // or the query barrier), not be dropped. Poll instead of asserting
        // on the first query, which raced that handoff on CI.
        let visible_deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut text = String::new();
        while std::time::Instant::now() < visible_deadline {
            text = first_text(server.find_symbol(Parameters(QueryParams {
                query: "during_build_xyz".into(),
                include_body: false,
                substring: false,
                kind: None,
                depth: 0,
                budget_tokens: 500,
            })));
            if text.contains("during_build_xyz") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("during_build_xyz"),
            "Ready graph must include the in-flight edit, got {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_fresh_answers_without_staleness_note() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        })));
        assert!(text.contains("alpha"), "{text}");
        assert!(
            !text.contains("confidence: unknown") || text.contains("syntactic"),
            "{text}"
        );
        assert!(
            !text.contains("索引可能过期"),
            "Fresh must not append staleness: {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repair_inlines_suspects_into_graph() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));

        std::fs::write(dir.join("a.py"), "def alpha_v2():\n    return 2\n").unwrap();
        std::thread::sleep(Duration::from_millis(120));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            query: "alpha_v2".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        })));
        assert!(
            text.contains("alpha_v2"),
            "inline Repair must update the graph, got {text}"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_degrade_appends_staleness_and_rebuilds() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut knobs = test_knobs();
        knobs.barrier_max = 0;
        let mut server = AstrolabeServer::new_with_knobs(dir.clone(), knobs, 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));

        std::fs::write(dir.join("a.py"), "def alpha_v3():\n    return 3\n").unwrap();
        std::thread::sleep(Duration::from_millis(120));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        })));
        assert!(
            text.contains("索引可能过期") && text.contains("confidence: unknown"),
            "Degrade must append staleness_note, got {text}"
        );
        let start = std::time::Instant::now();
        loop {
            let probe = first_text(server.find_symbol(Parameters(QueryParams {
                query: "alpha_v3".into(),
                include_body: false,
                substring: false,
                kind: None,
                depth: 0,
                budget_tokens: 500,
            })));
            if probe.contains("alpha_v3") {
                break;
            }
            if start.elapsed() > Duration::from_secs(20) {
                panic!("background rebuild did not land alpha_v3: {probe}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repair_empty_requests_verification() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let mut server =
            AstrolabeServer::new_with_knobs(dir.clone(), test_knobs(), 8 * 1024 * 1024);
        server.structured_output = false;
        server.start_indexing();
        wait_ready(&server, Duration::from_secs(20));
        let log = server
            .primary
            .freshness
            .lock()
            .unwrap()
            .clone()
            .expect("watcher must publish a freshness log");
        // Age the watermark past FRESH_WINDOW with no suspects. Waiting for
        // the condition (instead of a fixed sleep) matters on loaded CI
        // runners: the watcher thread can be starved past `wait_ready`, so
        // the baseline check that pushes the watermark may land seconds
        // late and a fixed sleep leaves the watermark "fresh" at query
        // time — the barrier then answers Fresh and never requests a
        // verification. atime noise is one-shot (relatime), so once the
        // watermark has aged past the window no further bump races the query.
        let aged = astrolabe_core::freshness::FRESH_WINDOW + Duration::from_millis(200);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while log.watermark().elapsed() < aged {
            if std::time::Instant::now() > deadline {
                panic!("watermark never aged past FRESH_WINDOW");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let before = log.watermark();
        let _ = server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        }));
        let start = std::time::Instant::now();
        while log.watermark() <= before {
            // Generous bound: on 2-vCPU CI runners the watch thread can be
            // starved by the parallel test suite for seconds before it
            // honors the verification request (STOP_SLICE is 50ms of idle).
            if start.elapsed() > Duration::from_secs(10) {
                panic!("request_verification did not advance the watermark");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dispatch_routes_hint_and_stores_in_child() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def alpha_unique():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def beta_unique():\n    return 2\n").unwrap();

        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        knobs.resident_roots = 2;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        server.start_indexing();

        // Child name is a word-boundary hint; "alpha" also substring-matches
        // the symbol `alpha_unique` without taking the path-query branch.
        let priming = server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        }));
        let priming_text = first_text(priming);
        if priming_text.contains("索引正在后台构建") {
            let session = {
                let map = server.dispatch.as_ref().unwrap().sessions.lock().unwrap();
                map.get(&alpha.canonicalize().unwrap_or(alpha.clone()))
                    .cloned()
                    .expect("alpha session after route")
            };
            wait_session_ready(&session, Duration::from_secs(20));
        }
        let session = {
            let map = server.dispatch.as_ref().unwrap().sessions.lock().unwrap();
            map.get(&alpha.canonicalize().unwrap_or(alpha.clone()))
                .cloned()
                .expect("alpha session")
        };
        wait_session_ready(&session, Duration::from_secs(20));
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        })));
        assert!(
            text.starts_with(&format!("index_root: {}", session.root.display())),
            "dispatch must declare the child root, got {text}"
        );
        assert!(text.contains("alpha_unique"), "{text}");
        assert!(
            astrolabe_core::index::store_path(&session.root)
                .parent()
                .unwrap()
                .starts_with(&session.root),
            "store must live under the child"
        );
        assert!(
            !parent.join(".astrolabe").join("index.redb").exists(),
            "parent must not grow an index store"
        );
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn dispatch_evict_releases_child_lock() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def alpha_fn():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def beta_fn():\n    return 2\n").unwrap();

        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        knobs.resident_roots = 1;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        server.start_indexing();

        let _ = server.find_symbol(Parameters(QueryParams {
            query: "alpha".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 200,
        }));
        let alpha_key = alpha.canonicalize().unwrap_or(alpha.clone());
        let session = server
            .dispatch
            .as_ref()
            .unwrap()
            .sessions
            .lock()
            .unwrap()
            .get(&alpha_key)
            .cloned()
            .expect("alpha");
        wait_session_ready(&session, Duration::from_secs(20));
        assert!(
            astrolabe_core::elect::IndexerLock::try_acquire(&alpha_key)
                .unwrap()
                .is_none(),
            "resident child must hold the indexer lock"
        );

        let _ = server.find_symbol(Parameters(QueryParams {
            query: "beta".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 200,
        }));
        let start = std::time::Instant::now();
        loop {
            if astrolabe_core::elect::IndexerLock::try_acquire(&alpha_key)
                .unwrap()
                .is_some()
            {
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                panic!("evicted child did not release IndexerLock");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn dispatch_ambiguous_hint_lists_children() {
        let parent = unique_temp_dir();
        let alpha = parent.join("alpha");
        let beta = parent.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git_marker(&alpha);
        git_marker(&beta);
        std::fs::write(alpha.join("a.py"), "def a():\n    return 1\n").unwrap();
        std::fs::write(beta.join("b.py"), "def b():\n    return 2\n").unwrap();
        let mut knobs = test_knobs();
        knobs.watch_mode = None;
        let mut server = AstrolabeServer::with_multi_project_knobs(
            parent.clone(),
            vec![alpha.clone(), beta.clone()],
            knobs,
            8 * 1024 * 1024,
        );
        server.structured_output = false;
        let text = first_text(server.find_symbol(Parameters(QueryParams {
            query: "compare alpha and beta".into(),
            include_body: false,
            substring: false,
            kind: None,
            depth: 0,
            budget_tokens: 500,
        })));
        assert!(text.contains("alpha") && text.contains("beta"), "{text}");
        assert!(text.contains("confidence: unknown"), "{text}");
        server.stop_watching();
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn unindexed_languages_end_to_end_hints_and_dart_contrast() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("main.py"),
            "def main():\n    print('hello from python')\n",
        )
        .unwrap();

        // 3 个 .kt 文件 (未支持语言)
        std::fs::write(dir.join("A.kt"), "class A { val x = 1 }\n").unwrap();
        std::fs::write(dir.join("B.kt"), "class B { val y = 2 }\n").unwrap();
        std::fs::write(dir.join("C.kt"), "class C { val z = 3 }\n").unwrap();

        // 1 个 .dart 文件 (支持语言对照)
        std::fs::write(
            dir.join("service.dart"),
            "void runService() {\n  print('dart service running');\n}\n",
        )
        .unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        // 1. search_code 零命中且无 path_filter，含 .kt×3 (N>=3) 时提示尚未支持语言且带计数
        let search_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent_code_pattern".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 2500,
        }));
        let search_text = search_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains("(未找到匹配的代码行)"),
            "{search_text}"
        );
        assert!(search_text.contains("尚未支持的语言"), "{search_text}");
        assert!(search_text.contains("3 个 .kt 文件"), "{search_text}");
        assert!(search_text.contains("未入代码索引"), "{search_text}");
        assert!(
            search_text.contains("请改用 Grep/Read 检索它们"),
            "{search_text}"
        );
        assert!(
            search_text.contains("注意：仓库内有 3 个 .kt 文件属于尚未支持的语言"),
            "{search_text}"
        );
        let structured = search_miss.structured_content.expect("structured");
        assert_eq!(structured["unindexed_hint"], json!(true));

        // 1b. 带 path_filter 的 search_code 零命中：filter 范围内 N>=3
        let search_filter_hit_hint = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some(".kt".into()),
            budget_tokens: 2500,
        }));
        let filter_hint_text = search_filter_hit_hint.content[0]
            .as_text()
            .unwrap()
            .text
            .as_str();
        assert!(
            filter_hint_text
                .contains("注意：path_filter '.kt' 范围内有 3 个 .kt 文件属于尚未支持的语言"),
            "{filter_hint_text}"
        );
        let filter_hint_structured = search_filter_hit_hint
            .structured_content
            .expect("structured");
        assert_eq!(filter_hint_structured["unindexed_hint"], json!(true));

        // 1c. 带 path_filter 的 search_code 零命中：filter 范围内 N<3 (仅 A.kt 匹配，计 1)
        let search_filter_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some("A.kt".into()),
            budget_tokens: 2500,
        }));
        let filter_miss_text = search_filter_miss.content[0]
            .as_text()
            .unwrap()
            .text
            .as_str();
        assert!(
            filter_miss_text.contains("(未找到匹配的代码行)"),
            "{filter_miss_text}"
        );
        assert!(
            !filter_miss_text.contains("尚未支持的语言"),
            "{filter_miss_text}"
        );
        let filter_structured = search_filter_miss.structured_content.expect("structured");
        assert_eq!(filter_structured["unindexed_hint"], json!(false));

        // 2. find_symbol 零命中提示
        let symbol_miss = server.find_symbol(Parameters(QueryParams {
            query: "NonExistentSymbol".into(),
            budget_tokens: 2500,
            depth: 0,
            substring: false,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(symbol_text.contains("(未找到匹配的符号)"), "{symbol_text}");
        assert!(
            symbol_text.contains("(仓库另有 3 个 .kt 等未支持语言的源码文件未入符号索引，符号搜索不覆盖它们——如需检索请用 Grep/Read)"),
            "{symbol_text}"
        );

        // 3. get_dependents target=A.kt 精确文案
        let dep_res = server.get_dependents(Parameters(DependentsParams {
            target: "A.kt".into(),
            direction: "dependents".into(),
            budget_tokens: 2500,
        }));
        let dep_text = dep_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            dep_text.contains("目标 A.kt 是 .kt 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。"),
            "{dep_text}"
        );

        // 3b. get_neighborhood target=A.kt 精确文案
        let neigh_res = server.get_neighborhood(Parameters(NeighborhoodParams {
            target: "A.kt".into(),
            direction: "both".into(),
            depth: 2,
            budget_tokens: 2500,
        }));
        let neigh_text = neigh_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            neigh_text.contains("目标 A.kt 是 .kt 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。"),
            "{neigh_text}"
        );

        // 4. get_languages 首行 supported、末尾 unindexed 行
        let langs_res = server.get_languages(Parameters(BudgetOnly {
            budget_tokens: 2500,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        let first_body_line = langs_text
            .lines()
            .find(|l| !l.starts_with("index_root:"))
            .expect("first line after index_root");
        assert_eq!(
            first_body_line,
            "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart"
        );
        assert!(
            langs_text.contains("unindexed source files: .kt×3"),
            "{langs_text}"
        );
        assert!(
            langs_text.contains("语言暂不支持；这些文件不在符号/import 索引内"),
            "{langs_text}"
        );

        // 5. 对照验证：.dart 是支持语言
        // 5a. .dart 决不出现在 unindexed 统计中
        assert!(!langs_text.contains(".dart×"), "{langs_text}");
        // 5b. .dart 源码能被 search_code 命中
        let search_dart = server.search_code(Parameters(SearchParams {
            query: "dart service running".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 2500,
        }));
        let dart_text = search_dart.content[0].as_text().unwrap().text.as_str();
        assert!(dart_text.contains("@service.dart:2"), "{dart_text}");
        assert!(!dart_text.contains("(未找到匹配的代码行)"), "{dart_text}");
        let dart_structured = search_dart.structured_content.expect("structured");
        assert_eq!(dart_structured["unindexed_hint"], json!(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_languages_threshold_under_three_does_not_hint() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def main(): pass\n").unwrap();
        // 仅 2 个 .kt 文件 (N = 2 < 3)
        std::fs::write(dir.join("A.kt"), "class A\n").unwrap();
        std::fs::write(dir.join("B.kt"), "class B\n").unwrap();

        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        // search_code 零命中：N<3 不应触发提示
        let search_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: None,
            budget_tokens: 2500,
        }));
        let search_text = search_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains("(未找到匹配的代码行)"),
            "{search_text}"
        );
        assert!(!search_text.contains("尚未支持的语言"), "{search_text}");

        // find_symbol 零命中：N<3 不应触发提示
        let symbol_miss = server.find_symbol(Parameters(QueryParams {
            query: "NonExistent".into(),
            budget_tokens: 2500,
            depth: 0,
            substring: false,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(symbol_text.contains("(未找到匹配的符号)"), "{symbol_text}");
        assert!(!symbol_text.contains("未支持语言"), "{symbol_text}");

        // get_languages: >=1 就显示统计，这里有 2 个 .kt 仍会显示
        let langs_res = server.get_languages(Parameters(BudgetOnly {
            budget_tokens: 2500,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            langs_text.contains("unindexed source files: .kt×2"),
            "{langs_text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_languages_none_when_all_supported() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def main(): pass\n").unwrap();
        std::fs::write(dir.join("app.dart"), "void main() {}\n").unwrap();

        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        let langs_res = server.get_languages(Parameters(BudgetOnly {
            budget_tokens: 2500,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            langs_text.contains("unindexed source files: none"),
            "{langs_text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_descriptions_declare_indexed_languages_boundary() {
        let server = ready_server();
        let tools = server.listed_tools();
        for name in &["search_code", "find_symbol", "resolve_context"] {
            let tool = tools.iter().find(|t| t.name == *name).expect(name);
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(
                desc.contains("Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart"),
                "{name} description missing indexed languages notice: {desc}"
            );
        }
    }
}
