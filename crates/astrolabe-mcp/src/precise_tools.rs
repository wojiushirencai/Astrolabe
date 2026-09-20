//! Precise (LSP-backed) query and rewrite-planning tools.
//!
//! These tools answer questions the import graph cannot: exact references,
//! definitions, file diagnostics, symbol hover info, a rename *plan*, and a
//! gated rename *apply*. Rendering lives here; `server.rs` mounts `#[tool]`
//! methods that call [`run_find_references`], [`run_goto_definition`],
//! [`run_get_diagnostics`], [`run_get_symbol_info`], [`run_plan_rename`], and
//! [`run_apply_rename`] through `LiveBackend`.
//! [`crate::TOOL_COUNT`] includes [`crate::PRECISE_TOOL_COUNT`] (6).
//!
//! [`run_apply_rename`] is irreversible and gated: it recomputes a plan, then
//! writes only when [`RewritePlan::is_auto_applicable`] is true **or** the
//! caller sets `force=true` (loud warning in the result). Below `Scoped`
//! without `force`, it returns an Unknown refusal (`isError` false) and does
//! not touch disk. Writes go through `rewrite::transaction::apply` with
//! post-write reparse verification; failure rolls the whole transaction back.
#![cfg_attr(not(test), allow(dead_code))]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked},
    lsp::{Diagnostic, Location, Precise, Severity},
    render::{anchor, confidence_note},
    rewrite::{
        transaction::{self, ApplyReport},
        verify as rewrite_verify, Evidence, RewriteError, RewritePlan, Site,
    },
    Confidence, Language, RelPath,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_BUDGET: usize = 2_500;

/// Marker agents (and tests) look for when a plan must not be written.
const DO_NOT_APPLY: &str =
    "不可自动应用：置信度低于 scoped（只有 exact/scoped 才允许不经确认落盘）。\
     MCP 客户端没有统一的用户确认机制，因此本工具只产出计划，不会写盘。";

const SEARCH_CODE_FALLBACK: &str =
    "可改用 search_code 做字面搜索，但那是句法匹配：同名标识符、字符串和注释都会算作命中，\
     既会误报也会漏掉经别名或重导出的引用。它不能代替精确引用。";

const DEFINITION_FALLBACK: &str =
    "可改用 find_symbol（句法/图谱名字匹配）或 search_code（字面搜索），\
     但二者都不能代替语言服务器的绑定解析。";

const NOT_WIRED: &str =
    "该能力尚未接通：语言服务器查询层与改写引擎仍在接入中，当前不能给出精确结果。";

fn default_budget() -> usize {
    DEFAULT_BUDGET
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FindReferencesParams {
    #[schemars(description = "Symbol name whose references should be resolved")]
    pub symbol: String,
    #[serde(default)]
    #[schemars(description = "Optional repo-relative file that scopes the query")]
    pub path: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GotoDefinitionParams {
    #[schemars(
        description = "Repo-relative file that scopes the query and selects the language server"
    )]
    pub path: String,
    #[serde(default)]
    #[schemars(
        description = "Symbol name; uses the first identifier occurrence in path. Provide this or line+character"
    )]
    pub symbol: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based LSP line; use together with character")]
    pub line: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based UTF-16 column; use together with line")]
    pub character: Option<u32>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetDiagnosticsParams {
    #[schemars(description = "Repo-relative file to diagnose")]
    pub path: String,
    #[serde(default)]
    #[schemars(
        description = "Optional minimum severity filter: error, warning, information, or hint"
    )]
    pub severity: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct PlanRenameParams {
    #[schemars(description = "Symbol to rename")]
    pub symbol: String,
    #[schemars(description = "Replacement identifier; not applied to disk")]
    pub new_name: String,
    #[serde(default)]
    #[schemars(description = "Optional repo-relative file that scopes planning")]
    pub path: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ApplyRenameParams {
    #[schemars(description = "Symbol to rename (plan is recomputed, then gated)")]
    pub symbol: String,
    #[schemars(description = "Replacement identifier")]
    pub new_name: String,
    #[serde(default)]
    #[schemars(description = "Optional repo-relative file that scopes planning")]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "When true, write even if confidence is below scoped; result includes a loud warning. Default false: refuse non-auto-applicable plans without writing"
    )]
    pub force: bool,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

/// Backend for the precise MCP query/plan tools (apply recomputes via plan_rename).
///
/// Production uses `LiveBackend` (on-demand language-server pool). Tests
/// inject fakes for the missing-server path.
pub(crate) trait PreciseCapability: std::fmt::Debug + Send + Sync {
    fn references(&self, symbol: &str, path: Option<&str>) -> Precise<Vec<Location>>;
    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>>;
    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>>;
    /// Hover 信息（docstring/类型/签名，Serena include_info 对应物）。
    fn info(&self, symbol: &str, path: Option<&str>) -> Precise<Option<String>>;
    fn plan_rename(&self, symbol: &str, new_name: &str, path: Option<&str>)
        -> Precise<RewritePlan>;
}

/// Backend with no language server behind it.
///
/// Production uses `LiveBackend`; this remains as the test double for the
/// "capability absent" path, which still has to answer rather than vanish —
/// an agent needs to learn what to install.
#[cfg(test)]
#[derive(Debug)]
struct Unwired;

#[cfg(test)]
impl PreciseCapability for Unwired {
    fn references(&self, symbol: &str, path: Option<&str>) -> Precise<Vec<Location>> {
        Precise::unknown(
            Vec::new(),
            format!(
                "{NOT_WIRED} 目标符号 `{symbol}`{}。",
                path.map(|p| format!("（限定 {p}）")).unwrap_or_default()
            ),
        )
    }

    fn info(&self, symbol: &str, path: Option<&str>) -> Precise<Option<String>> {
        Precise::unknown(
            None,
            format!(
                "{NOT_WIRED} 目标符号 `{symbol}`{} 的 hover 信息。",
                path.map(|p| format!("（限定 {p}）")).unwrap_or_default()
            ),
        )
    }

    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>> {
        let target = match (symbol, line, character) {
            (_, Some(l), Some(c)) => format!("@{path}:{l}:{c}"),
            (Some(s), _, _) => format!("`{s}`（限定 {path}）"),
            _ => format!("（限定 {path}）"),
        };
        Precise::unknown(Vec::new(), format!("{NOT_WIRED} 目标 {target}。"))
    }

    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>> {
        Precise::unknown(Vec::new(), format!("{NOT_WIRED} 目标文件 `{path}`。"))
    }

    fn plan_rename(
        &self,
        symbol: &str,
        new_name: &str,
        path: Option<&str>,
    ) -> Precise<RewritePlan> {
        Precise::unknown(
            empty_plan(symbol, new_name),
            format!(
                "{NOT_WIRED} 目标 `{symbol}` → `{new_name}`{}。",
                path.map(|p| format!("（限定 {p}）")).unwrap_or_default()
            ),
        )
    }
}

fn empty_plan(symbol: &str, new_name: &str) -> RewritePlan {
    RewritePlan {
        symbol: symbol.to_string(),
        new_name: new_name.to_string(),
        sites: Vec::new(),
        evidence: Evidence::NameMatch,
        confidence: Confidence::Unknown,
        excluded: Vec::new(),
    }
}

#[derive(Clone)]
pub(crate) struct PreciseTools {
    backend: Arc<dyn PreciseCapability>,
    /// Repository root used by [`Self::apply_rename`]. Query tools ignore it.
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl PreciseTools {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_backend(Arc::new(Unwired), PathBuf::from("."))
    }

    pub(crate) fn with_backend(backend: Arc<dyn PreciseCapability>, root: PathBuf) -> Self {
        Self {
            backend,
            root,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl PreciseTools {
    #[tool(
        description = "精确引用查询（语言服务器）。LSP 不可用时说明缺什么、怎么装，并建议 search_code（有误报）",
        annotations(title = "精确引用", read_only_hint = true)
    )]
    pub(crate) fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        run_find_references(params, self.backend.as_ref())
    }

    #[tool(
        description = "精确定义跳转（语言服务器绑定解析）。不要用 find_symbol 代替；LSP 不可用时说明缺什么、怎么装",
        annotations(title = "精确定义", read_only_hint = true)
    )]
    pub(crate) fn goto_definition(
        &self,
        Parameters(params): Parameters<GotoDefinitionParams>,
    ) -> CallToolResult {
        run_goto_definition(params, self.backend.as_ref())
    }

    #[tool(
        description = "文件诊断（语言服务器）。按严重程度过滤；LSP 不可用时返回安装提示而不是空的干净结果",
        annotations(title = "文件诊断", read_only_hint = true)
    )]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(params): Parameters<GetDiagnosticsParams>,
    ) -> CallToolResult {
        run_get_diagnostics(params, self.backend.as_ref())
    }

    #[tool(
        description = "符号语义信息（LSP hover：docstring/类型/签名，Serena include_info 对应物）",
        annotations(title = "符号信息", read_only_hint = true)
    )]
    pub(crate) fn get_symbol_info(
        &self,
        Parameters(params): Parameters<GetSymbolInfoParams>,
    ) -> CallToolResult {
        run_get_symbol_info(params, self.backend.as_ref())
    }

    #[tool(
        description = "生成重命名计划但不落盘。低置信度会明确标注不可自动应用；落盘请用 gated 的 apply_rename",
        annotations(title = "重命名计划", read_only_hint = true, destructive_hint = false)
    )]
    pub(crate) fn plan_rename(
        &self,
        Parameters(params): Parameters<PlanRenameParams>,
    ) -> CallToolResult {
        run_plan_rename(params, self.backend.as_ref())
    }

    #[tool(
        description = "应用重命名计划（不可逆）。默认仅在 exact/scoped 时写盘；否则需 force=true。写后重解析，失败整笔回滚",
        annotations(title = "应用重命名", read_only_hint = false, destructive_hint = true)
    )]
    pub(crate) fn apply_rename(
        &self,
        Parameters(params): Parameters<ApplyRenameParams>,
    ) -> CallToolResult {
        run_apply_rename(params, self.backend.as_ref(), &self.root)
    }
}

pub(crate) fn run_find_references(
    params: FindReferencesParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    if params.symbol.trim().is_empty() {
        return text_result(
            unknown_preamble(Some("symbol 不能为空。"), params.path.as_deref(), true),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let result = backend.references(&params.symbol, params.path.as_deref());
    let mut locations = result.value;
    locations.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.cmp(&b.range)));

    // Keep the preamble out of ranked truncation so a zero budget still
    // yields non-empty text (the Cursor `content[0]` contract).
    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble(result.note.as_deref(), params.path.as_deref(), true)
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if locations.is_empty() {
            header.push_str(&format!("未找到对 `{}` 的引用。\n", params.symbol));
        }
        header
    };

    let items: Vec<String> = locations.iter().map(render_location).collect();
    let (kept, omitted) = truncate_ranked(&items, params.budget_tokens, |line| format!("{line}\n"));

    let mut body = preamble;
    for line in kept {
        body.push_str(line);
        body.push('\n');
    }
    if omitted > 0 {
        body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
        }),
    )
}

/// get_symbol_info（hover）参数：语言服务器语义级符号信息。
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetSymbolInfoParams {
    #[schemars(
        description = "Repo-relative file that scopes the query and selects the language server"
    )]
    pub path: String,
    #[schemars(description = "Symbol name; anchors on its first identifier occurrence in path")]
    pub symbol: String,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the rendered result")]
    pub budget_tokens: usize,
}

/// get_symbol_info：LSP hover（docstring/类型/签名，Serena include_info 对应物）。
pub(crate) fn run_get_symbol_info(
    params: GetSymbolInfoParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    if params.path.trim().is_empty() {
        return text_result(
            "path 不能为空。confidence: unknown".to_string(),
            json!({"confidence":"unknown","budget_tokens":params.budget_tokens}),
        );
    }
    if params.symbol.trim().is_empty() {
        return text_result(
            "symbol 不能为空（hover 需要名字定位）。confidence: unknown".to_string(),
            json!({"confidence":"unknown","budget_tokens":params.budget_tokens}),
        );
    }
    let symbol = params.symbol.trim();
    let outcome = backend.info(symbol, Some(params.path.as_str()));
    let confidence = outcome.confidence;
    let note = outcome
        .note
        .unwrap_or_else(|| confidence_note(confidence).to_string());
    let mut body = format!("confidence: {confidence:?} {note}\n");
    match outcome.value.as_deref() {
        Some(info) if !info.trim().is_empty() => {
            body.push('\n');
            let spent = estimate_tokens(&body);
            let remaining = params.budget_tokens.saturating_sub(spent);
            let lines: Vec<&str> = info.lines().collect();
            let (kept, omitted) = truncate_ranked(&lines, remaining, |line| format!("{line}\n"));
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!(
                    "{omitted} 行 hover 内容因 budget_tokens 被省略。\n"
                ));
            }
        }
        _ => {
            body.push_str("\n(语言服务器未返回 hover 信息；退回 find_symbol 锚点)\n");
        }
    }
    text_result(
        body,
        json!({"confidence":format!("{confidence:?}"),"budget_tokens":params.budget_tokens}),
    )
}

pub(crate) fn run_goto_definition(
    params: GotoDefinitionParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    if params.path.trim().is_empty() {
        return text_result(
            unknown_preamble_definition(Some("path 不能为空。"), None),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let has_position = params.line.is_some() && params.character.is_some();
    let symbol = params
        .symbol
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if !has_position && symbol.is_none() {
        let hint = if params.line.is_some() ^ params.character.is_some() {
            "需要同时提供 line 与 character，或提供非空 symbol。"
        } else {
            "需要提供 symbol，或同时提供 line 与 character。"
        };
        return text_result(
            unknown_preamble_definition(Some(hint), Some(params.path.as_str())),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let result = backend.definition(
        &params.path,
        symbol,
        if has_position { params.line } else { None },
        if has_position { params.character } else { None },
    );
    let mut locations = result.value;
    locations.sort_by(|a, b| a.path.cmp(&b.path).then(a.range.cmp(&b.range)));

    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble_definition(result.note.as_deref(), Some(params.path.as_str()))
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if locations.is_empty() {
            let label = symbol.unwrap_or("该位置");
            header.push_str(&format!("未找到 `{label}` 的定义。\n"));
        }
        header
    };

    let items: Vec<String> = locations.iter().map(render_location).collect();
    let (kept, omitted) = truncate_ranked(&items, params.budget_tokens, |line| format!("{line}\n"));

    let mut body = preamble;
    for line in kept {
        body.push_str(line);
        body.push('\n');
    }
    if omitted > 0 {
        body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
        }),
    )
}

pub(crate) fn run_get_diagnostics(
    params: GetDiagnosticsParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    if params.path.trim().is_empty() {
        return text_result(
            unknown_preamble(Some("path 不能为空。"), None, false),
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "matches": 0,
            }),
        );
    }

    let min_severity = match params.severity.as_deref() {
        None => None,
        Some(raw) => match parse_severity(raw) {
            Some(severity) => Some(severity),
            None => {
                return text_result(
                    unknown_preamble(
                        Some(&format!(
                            "无法识别的 severity：{raw}。允许值：error, warning, information, hint。"
                        )),
                        Some(params.path.as_str()),
                        false,
                    ),
                    json!({
                        "confidence": "unknown",
                        "budget_tokens": params.budget_tokens,
                        "matches": 0,
                    }),
                );
            }
        },
    };

    let result = backend.diagnostics(&params.path);
    let mut diagnostics = result.value;
    if let Some(min) = min_severity {
        diagnostics.retain(|d| d.severity <= min);
    }
    diagnostics.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.range.cmp(&b.range))
            .then(a.severity.cmp(&b.severity))
            .then(a.message.cmp(&b.message))
    });

    let preamble = if result.confidence == Confidence::Unknown {
        unknown_preamble(result.note.as_deref(), Some(params.path.as_str()), false)
    } else {
        let mut header = confidence_header(result.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        if diagnostics.is_empty() {
            header.push_str(&format!("`{}` 没有匹配的诊断。\n", params.path));
        }
        header
    };

    let items: Vec<String> = diagnostics.iter().map(render_diagnostic).collect();
    let (kept, omitted) = truncate_ranked(&items, params.budget_tokens, |line| format!("{line}\n"));
    let mut body = preamble;
    for line in kept {
        body.push_str(line);
        body.push('\n');
    }
    if omitted > 0 {
        body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": diagnostics.len(),
        }),
    )
}

pub(crate) fn run_plan_rename(
    params: PlanRenameParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    if params.symbol.trim().is_empty() || params.new_name.trim().is_empty() {
        let body = unknown_preamble(
            Some("symbol 和 new_name 都不能为空。"),
            params.path.as_deref(),
            false,
        );
        return text_result(
            body,
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "auto_applicable": false,
            }),
        );
    }

    let result = backend.plan_rename(&params.symbol, &params.new_name, params.path.as_deref());
    let mut plan = result.value;
    // Ord: Exact < Scoped < Syntactic < Unknown. The weaker (greater) label wins.
    plan.confidence = result.confidence.max(plan.confidence);

    let auto = plan.is_auto_applicable();
    let mut preamble = if plan.confidence == Confidence::Unknown {
        unknown_preamble(result.note.as_deref(), params.path.as_deref(), false)
    } else {
        let mut header = confidence_header(plan.confidence);
        if let Some(note) = &result.note {
            header.push_str(note);
            if !note.ends_with('\n') {
                header.push('\n');
            }
        }
        header
    };
    preamble.push_str(&format!(
        "plan: `{}` -> `{}`  evidence: {} ({})\n",
        plan.symbol,
        plan.new_name,
        evidence_label(plan.evidence),
        evidence_json(plan.evidence)
    ));
    if !auto {
        preamble.push_str(DO_NOT_APPLY);
        preamble.push('\n');
    }

    let mut items: Vec<String> = plan
        .sites
        .iter()
        .map(|site| render_site(site, None))
        .collect();
    for (site, reason) in &plan.excluded {
        items.push(render_site(site, Some(reason)));
    }

    let (kept, omitted) = truncate_ranked(&items, params.budget_tokens, |line| format!("{line}\n"));
    let mut body = preamble;
    for line in kept {
        body.push_str(line);
        body.push('\n');
    }
    if omitted > 0 {
        body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(plan.confidence),
            "budget_tokens": params.budget_tokens,
            "sites": plan.sites.len(),
            "excluded": plan.excluded.len(),
            "auto_applicable": auto,
        }),
    )
}

/// Marker agents look for when apply refuses to write without `force`.
const APPLY_REFUSED: &str =
    "拒绝落盘：置信度低于 scoped（只有 exact/scoped 才允许不经 force 写盘）。\
     请先用 plan_rename 复核，或在确认风险后设置 force=true。\
     apply_rename 不可逆；失败会整笔回滚。";

const FORCE_WARNING: &str = "⚠ FORCE：置信度低于 scoped，仍按调用方要求强制落盘。\
     这可能改坏无关符号；写后已做重解析校验，失败会回滚，但语义错误仍可能漏网。";

pub(crate) fn run_apply_rename(
    params: ApplyRenameParams,
    backend: &dyn PreciseCapability,
    root: &Path,
) -> CallToolResult {
    if params.symbol.trim().is_empty() || params.new_name.trim().is_empty() {
        let body = unknown_preamble(
            Some("symbol 和 new_name 都不能为空。"),
            params.path.as_deref(),
            false,
        );
        return text_result(
            body,
            json!({
                "confidence": "unknown",
                "budget_tokens": params.budget_tokens,
                "applied": false,
                "auto_applicable": false,
                "forced": false,
            }),
        );
    }

    let result = backend.plan_rename(&params.symbol, &params.new_name, params.path.as_deref());
    let mut plan = result.value;
    // Ord: Exact < Scoped < Syntactic < Unknown. The weaker (greater) label wins.
    plan.confidence = result.confidence.max(plan.confidence);
    let auto = plan.is_auto_applicable();

    if !auto && !params.force {
        let mut body = if plan.confidence == Confidence::Unknown {
            unknown_preamble(result.note.as_deref(), params.path.as_deref(), false)
        } else {
            let mut header = confidence_header(plan.confidence);
            if let Some(note) = &result.note {
                header.push_str(note);
                if !note.ends_with('\n') {
                    header.push('\n');
                }
            }
            header
        };
        body.push_str(&format!(
            "plan: `{}` -> `{}`  sites: {}  auto_applicable: false\n",
            plan.symbol,
            plan.new_name,
            plan.sites.len()
        ));
        body.push_str(APPLY_REFUSED);
        body.push('\n');
        return text_result(
            body,
            json!({
                "confidence": confidence_json(plan.confidence),
                "budget_tokens": params.budget_tokens,
                "applied": false,
                "auto_applicable": false,
                "forced": false,
                "sites": plan.sites.len(),
            }),
        );
    }

    let forced = params.force && !auto;
    match apply_plan_with_reparse(root, &plan) {
        Ok(report) => {
            let mut body = confidence_header(plan.confidence);
            if let Some(note) = &result.note {
                body.push_str(note);
                if !note.ends_with('\n') {
                    body.push('\n');
                }
            }
            if forced {
                body.push_str(FORCE_WARNING);
                body.push('\n');
            }
            body.push_str(&format!(
                "applied: `{}` -> `{}`  files: {}  sites: {}\n",
                plan.symbol,
                plan.new_name,
                report.files.len(),
                report.sites_applied
            ));
            body.push_str(
                "写后已重解析校验；失败会整笔回滚。此操作不可逆（无 git 时请自行备份）。\n",
            );
            let file_lines: Vec<String> = report.files.iter().map(|p| format!("- {p}")).collect();
            let (kept, omitted) = truncate_ranked(&file_lines, params.budget_tokens, |line| {
                format!("{line}\n")
            });
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            if omitted > 0 {
                body.push_str(&format!("{omitted} 个结果因 budget_tokens 被省略。\n"));
            }
            text_result(
                body,
                json!({
                    "confidence": confidence_json(plan.confidence),
                    "budget_tokens": params.budget_tokens,
                    "applied": true,
                    "auto_applicable": auto,
                    "forced": forced,
                    "sites": report.sites_applied,
                    "files": report.files.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                }),
            )
        }
        Err(err) => {
            let mut body = confidence_header(plan.confidence);
            if forced {
                body.push_str(FORCE_WARNING);
                body.push('\n');
            }
            body.push_str(&format!("apply 失败，已回滚（或未写盘）：{err}\n"));
            body.push_str("写后重解析或事务校验未通过；工作树应已恢复到应用前状态。\n");
            text_result(
                body,
                json!({
                    "confidence": confidence_json(plan.confidence),
                    "budget_tokens": params.budget_tokens,
                    "applied": false,
                    "auto_applicable": auto,
                    "forced": forced,
                    "sites": plan.sites.len(),
                    "error": err.to_string(),
                }),
            )
        }
    }
}

/// Snapshot parse health, apply via transaction, re-parse each touched file.
/// Any verification failure rolls the whole write back.
fn apply_plan_with_reparse(root: &Path, plan: &RewritePlan) -> Result<ApplyReport, RewriteError> {
    let files = plan.files();
    let mut before: Vec<(RelPath, String)> = Vec::with_capacity(files.len());
    // Mirror `transaction::resolve_dest`: canonicalize under root and refuse escapes.
    if !files.is_empty() {
        let root_canon = std::fs::canonicalize(root).map_err(|e| {
            RewriteError::Io(
                files[0].clone(),
                format!("cannot resolve repository root: {e}"),
            )
        })?;
        for rel in &files {
            let joined = root_canon.join(rel.as_str());
            let abs = std::fs::canonicalize(&joined).map_err(|e| {
                RewriteError::Io(rel.clone(), format!("cannot read before apply: {e}"))
            })?;
            if !abs.starts_with(&root_canon) {
                return Err(RewriteError::Io(
                    rel.clone(),
                    "canonical path escapes the repository root".into(),
                ));
            }
            let source = std::fs::read_to_string(&abs).map_err(|e| {
                RewriteError::Io(rel.clone(), format!("cannot read before apply: {e}"))
            })?;
            before.push((rel.clone(), source));
        }
    }

    let verifier = rewrite_verify::Verifier::new();
    let snap = verifier.snapshot(before.iter().map(|(p, s)| (p, s.as_str())));

    transaction::apply(root, plan, move |path: &RelPath, source: &str| {
        verifier.verify(&snap, [(path, source)])
    })
}

fn text_result(body: String, structured: Value) -> CallToolResult {
    let body = if body.trim().is_empty() {
        "没有匹配结果。confidence: unknown".to_string()
    } else {
        body
    };
    let mut result = CallToolResult::success(vec![ContentBlock::text(body)]);
    result.structured_content = Some(structured);
    result
}

fn confidence_header(confidence: Confidence) -> String {
    let note = confidence_note(confidence);
    if note.is_empty() {
        format!("confidence: {confidence:?}\n")
    } else {
        format!("confidence: {confidence:?} {note}\n")
    }
}

fn unknown_preamble(note: Option<&str>, path: Option<&str>, suggest_search: bool) -> String {
    let mut body = confidence_header(Confidence::Unknown);
    if let Some(note) = note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            body.push_str(trimmed);
            body.push('\n');
        }
    }
    body.push_str("安装提示：\n");
    body.push_str(&install_hint(language_of(path)));
    if suggest_search {
        body.push_str(SEARCH_CODE_FALLBACK);
        body.push('\n');
    }
    body
}

fn unknown_preamble_definition(note: Option<&str>, path: Option<&str>) -> String {
    let mut body = confidence_header(Confidence::Unknown);
    if let Some(note) = note {
        let trimmed = note.trim();
        if !trimmed.is_empty() {
            body.push_str(trimmed);
            body.push('\n');
        }
    }
    body.push_str("安装提示：\n");
    body.push_str(&install_hint(language_of(path)));
    body.push_str(DEFINITION_FALLBACK);
    body.push('\n');
    body
}

fn language_of(path: Option<&str>) -> Option<Language> {
    path.and_then(|p| Language::from_path(&RelPath::new(p)))
}

/// Human-readable install commands, mirroring `ServerSpec.install_hint`.
///
/// `discovery` will own the authoritative strings once that workstream
/// lands; these are the fallback so an agent can still act today.
pub(crate) fn install_hint(language: Option<Language>) -> String {
    match language {
        Some(language) => format!("- {}\n", install_hint_one(language)),
        None => {
            let mut out = String::new();
            for language in [
                Language::Python,
                Language::Go,
                Language::Rust,
                Language::TypeScript,
                Language::Java,
            ] {
                out.push_str("- ");
                out.push_str(&install_hint_one(language));
                out.push('\n');
            }
            out
        }
    }
}

fn install_hint_one(language: Language) -> String {
    match language {
        Language::Python => {
            "Python（pyright）：`npm i -g pyright`，stdio 启动 `pyright-langserver --stdio`。也可用 `pip install pyright`。".into()
        }
        Language::Go => {
            "Go（gopls）：`go install golang.org/x/tools/gopls@latest`。".into()
        }
        Language::Rust => {
            "Rust（rust-analyzer）：`rustup component add rust-analyzer`。".into()
        }
        Language::TypeScript | Language::Tsx | Language::JavaScript => {
            "TypeScript/JavaScript（typescript-language-server）：`npm i -g typescript-language-server typescript`，stdio 启动 `typescript-language-server --stdio`。".into()
        }
        Language::Java => {
            "Java（Eclipse JDT LS / jdtls）：启动参数依赖发行版，Homebrew 可用 `brew install jdtls`。当前默认不自动拉起。".into()
        }
    }
}

fn parse_severity(raw: &str) -> Option<Severity> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" | "err" => Some(Severity::Error),
        "warning" | "warn" => Some(Severity::Warning),
        "information" | "info" | "informational" => Some(Severity::Information),
        "hint" => Some(Severity::Hint),
        _ => None,
    }
}

fn render_location(loc: &Location) -> String {
    // LSP positions are 0-based; graph tools emit 1-based @path:line.
    format!(
        "{}  col {}",
        anchor(&loc.path, loc.range.start.line.saturating_add(1)),
        loc.range.start.character
    )
}

fn render_diagnostic(diag: &Diagnostic) -> String {
    let code = diag
        .code
        .as_deref()
        .map(|c| format!(" {c}"))
        .unwrap_or_default();
    format!(
        "{} [{}{code}] {}",
        anchor(&diag.path, diag.range.start.line.saturating_add(1)),
        severity_label(diag.severity),
        diag.message
    )
}

fn render_site(site: &Site, excluded: Option<&str>) -> String {
    // Rewrite sites are byte ranges, not LSP lines. Without the file
    // contents we cannot recover a real line, so the jump target is :1
    // and the byte range is the authoritative span.
    let mut line = format!(
        "{}  bytes {}..{}  `{}` -> `{}`",
        anchor(&site.path, 1),
        site.range.start,
        site.range.end,
        site.current,
        site.replacement
    );
    if let Some(reason) = excluded {
        line.push_str("  excluded: ");
        line.push_str(reason);
    }
    line
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "information",
        Severity::Hint => "hint",
    }
}

fn evidence_label(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::LanguageServer => "语言服务器解析",
        Evidence::ScopeBinding => "作用域绑定",
        Evidence::NameMatch => "名字匹配（可能是无关符号）",
    }
}

fn evidence_json(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::LanguageServer => "language_server",
        Evidence::ScopeBinding => "scope_binding",
        Evidence::NameMatch => "name_match",
    }
}

fn confidence_json(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Exact => "exact",
        Confidence::Scoped => "scoped",
        Confidence::Syntactic => "syntactic",
        Confidence::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::lsp::{Position, Range};
    use astrolabe_core::rewrite::ByteRange;

    fn loc(path: &str, line: u32, character: u32) -> Location {
        Location {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character },
                end: Position {
                    line,
                    character: character + 1,
                },
            },
        }
    }

    fn diag(path: &str, line: u32, severity: Severity, message: &str) -> Diagnostic {
        Diagnostic {
            path: RelPath::new(path),
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity,
            message: message.into(),
            code: None,
        }
    }

    fn site(path: &str, start: usize, end: usize, current: &str, replacement: &str) -> Site {
        Site {
            path: RelPath::new(path),
            range: ByteRange { start, end },
            current: current.into(),
            replacement: replacement.into(),
        }
    }

    #[derive(Debug)]
    struct Fake {
        references: Precise<Vec<Location>>,
        definition: Precise<Vec<Location>>,
        diagnostics: Precise<Vec<Diagnostic>>,
        plan: Precise<RewritePlan>,
    }

    impl PreciseCapability for Fake {
        fn references(&self, _symbol: &str, _path: Option<&str>) -> Precise<Vec<Location>> {
            self.references.clone()
        }
        fn info(&self, _symbol: &str, _path: Option<&str>) -> Precise<Option<String>> {
            Precise::exact(None)
        }
        fn definition(
            &self,
            _path: &str,
            _symbol: Option<&str>,
            _line: Option<u32>,
            _character: Option<u32>,
        ) -> Precise<Vec<Location>> {
            self.definition.clone()
        }
        fn diagnostics(&self, _path: &str) -> Precise<Vec<Diagnostic>> {
            self.diagnostics.clone()
        }
        fn plan_rename(
            &self,
            _symbol: &str,
            _new_name: &str,
            _path: Option<&str>,
        ) -> Precise<RewritePlan> {
            self.plan.clone()
        }
    }

    fn tools_with(fake: Fake) -> PreciseTools {
        PreciseTools::with_backend(Arc::new(fake), PathBuf::from("."))
    }

    fn tools_with_root(fake: Fake, root: PathBuf) -> PreciseTools {
        PreciseTools::with_backend(Arc::new(fake), root)
    }

    fn first_text(result: &CallToolResult) -> &str {
        result
            .content
            .first()
            .and_then(ContentBlock::as_text)
            .map(|content| content.text.as_str())
            .unwrap_or_default()
    }

    fn assert_first_text(result: CallToolResult) -> CallToolResult {
        let text = first_text(&result);
        assert!(
            !text.trim().is_empty(),
            "content[0].text must always be non-empty"
        );
        assert!(
            !result.is_error.unwrap_or(false),
            "precise tools report missing LSP as a result, not isError"
        );
        result
    }

    fn schema_for_tool(name: &str) -> Value {
        let tools = PreciseTools::new();
        let listed = tools.tool_router.list_all();
        let tool = listed
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        serde_json::to_value(&tool.input_schema).unwrap()
    }

    fn required_fields(schema: &Value) -> Vec<String> {
        schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn tool_count_and_names_match_catalog_constant() {
        let tools = PreciseTools::new().tool_router.list_all();
        assert_eq!(tools.len(), crate::PRECISE_TOOL_COUNT);
        let mut names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "apply_rename",
                "find_references",
                "get_diagnostics",
                "get_symbol_info",
                "goto_definition",
                "plan_rename"
            ]
        );
        assert!(
            names.iter().any(|n| n == "apply_rename"),
            "apply_rename must be present (gated)"
        );
        assert!(
            !names.iter().any(|n| n == "rename_symbol"),
            "rename_symbol is not a catalog name"
        );
    }

    #[test]
    fn schemas_required_and_optional_fields() {
        let refs = schema_for_tool("find_references");
        assert!(refs.pointer("/properties/symbol").is_some());
        assert!(refs.pointer("/properties/path").is_some());
        assert!(refs.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&refs);
        assert!(required.iter().any(|f| f == "symbol"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "path"),
            "path must be optional: {required:?}"
        );

        let diags = schema_for_tool("get_diagnostics");
        assert!(diags.pointer("/properties/path").is_some());
        assert!(diags.pointer("/properties/severity").is_some());
        assert!(diags.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&diags);
        assert!(required.iter().any(|f| f == "path"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "severity"),
            "severity must be optional: {required:?}"
        );

        let rename = schema_for_tool("plan_rename");
        assert!(rename.pointer("/properties/symbol").is_some());
        assert!(rename.pointer("/properties/new_name").is_some());
        assert!(rename.pointer("/properties/path").is_some());
        assert!(rename.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&rename);
        assert!(required.iter().any(|f| f == "symbol"), "{required:?}");
        assert!(required.iter().any(|f| f == "new_name"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "path"),
            "path must be optional: {required:?}"
        );

        let apply = schema_for_tool("apply_rename");
        assert!(apply.pointer("/properties/symbol").is_some());
        assert!(apply.pointer("/properties/new_name").is_some());
        assert!(apply.pointer("/properties/path").is_some());
        assert!(apply.pointer("/properties/force").is_some());
        assert!(apply.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&apply);
        assert!(required.iter().any(|f| f == "symbol"), "{required:?}");
        assert!(required.iter().any(|f| f == "new_name"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "force"),
            "force must be optional (default false): {required:?}"
        );
        assert!(
            !required.iter().any(|f| f == "path"),
            "path must be optional: {required:?}"
        );

        let defs = schema_for_tool("goto_definition");
        assert!(defs.pointer("/properties/path").is_some());
        assert!(defs.pointer("/properties/symbol").is_some());
        assert!(defs.pointer("/properties/line").is_some());
        assert!(defs.pointer("/properties/character").is_some());
        assert!(defs.pointer("/properties/budget_tokens").is_some());
        let required = required_fields(&defs);
        assert!(required.iter().any(|f| f == "path"), "{required:?}");
        assert!(
            !required.iter().any(|f| f == "symbol"),
            "symbol must be optional: {required:?}"
        );
        assert!(
            !required.iter().any(|f| f == "line"),
            "line must be optional: {required:?}"
        );
        assert!(
            !required.iter().any(|f| f == "budget_tokens"),
            "budget_tokens must be optional: {required:?}"
        );
    }

    #[test]
    fn every_tool_returns_nonempty_first_text_when_unwired() {
        let tools = PreciseTools::new();
        assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            symbol: "Handler".into(),
            path: None,
            budget_tokens: 800,
        })));
        assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "src/main.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
        })));
        assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            path: "src/main.go".into(),
            severity: None,
            budget_tokens: 800,
        })));
        assert_first_text(tools.plan_rename(Parameters(PlanRenameParams {
            symbol: "Handler".into(),
            new_name: "onEvent".into(),
            path: None,
            budget_tokens: 800,
        })));
    }

    #[test]
    fn lsp_unavailable_degrades_with_install_hint() {
        let tools = tools_with(Fake {
            references: Precise::unknown(Vec::new(), "gopls is not installed"),
            definition: Precise::unknown(Vec::new(), "gopls is not installed"),
            diagnostics: Precise::unknown(Vec::new(), "gopls is not installed"),
            plan: Precise::unknown(empty_plan("Handler", "onEvent"), "gopls is not installed"),
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            symbol: "Handler".into(),
            path: Some("pkg/server.go".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&refs);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(
            text.contains(confidence_note(Confidence::Unknown)),
            "{text}"
        );
        assert!(text.contains("gopls"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("search_code"), "{text}");
        assert!(text.contains("误报"), "{text}");
        assert!(
            text.contains("尚未接通") || text.contains("not installed"),
            "{text}"
        );

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            path: "pkg/server.go".into(),
            severity: None,
            budget_tokens: 800,
        })));
        let text = first_text(&diags);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(!text.contains("search_code"), "{text}");

        let plan = assert_first_text(tools.plan_rename(Parameters(PlanRenameParams {
            symbol: "Handler".into(),
            new_name: "onEvent".into(),
            path: Some("pkg/server.go".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&plan);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("不可自动应用"), "{text}");

        let defs = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "pkg/server.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
        })));
        let text = first_text(&defs);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(
            text.contains("find_symbol") || text.contains("绑定解析"),
            "{text}"
        );
    }

    #[test]
    fn plan_rename_low_confidence_is_not_auto_applicable() {
        let plan = RewritePlan {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            sites: vec![site("a.ts", 0, 7, "handler", "onEvent")],
            evidence: Evidence::NameMatch,
            confidence: Confidence::Syntactic,
            excluded: vec![(
                site("a.ts", 40, 47, "handler", "onEvent"),
                "字符串字面量".into(),
            )],
        };
        assert!(!plan.is_auto_applicable());
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise {
                value: plan,
                confidence: Confidence::Syntactic,
                note: Some("名字匹配召回不足，禁止自动落盘".into()),
            },
        });
        let result = assert_first_text(tools.plan_rename(Parameters(PlanRenameParams {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            path: Some("a.ts".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("不可自动应用"), "{text}");
        assert!(text.contains("confidence: Syntactic"), "{text}");
        assert!(text.contains("@a.ts:1"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["auto_applicable"],
            json!(false)
        );
    }

    #[test]
    fn budget_truncation_keeps_nonempty_text() {
        let many: Vec<Location> = (0..20).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(many.clone()),
            definition: Precise::exact(many),
            diagnostics: Precise::exact(
                (0..20)
                    .map(|i| diag("src/lib.rs", i, Severity::Error, "boom"))
                    .collect(),
            ),
            plan: Precise::exact(RewritePlan {
                symbol: "x".into(),
                new_name: "y".into(),
                sites: (0..20)
                    .map(|i| site("src/lib.rs", i * 2, i * 2 + 1, "x", "y"))
                    .collect(),
                evidence: Evidence::LanguageServer,
                confidence: Confidence::Exact,
                excluded: Vec::new(),
            }),
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            symbol: "x".into(),
            path: None,
            budget_tokens: 0,
        })));
        let text = first_text(&refs);
        assert!(text.contains("因 budget_tokens 被省略"), "{text}");
        assert_eq!(refs.structured_content.unwrap()["budget_tokens"], json!(0));

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            path: "src/lib.rs".into(),
            severity: None,
            budget_tokens: 0,
        })));
        let text = first_text(&diags);
        assert!(text.contains("因 budget_tokens 被省略"), "{text}");

        let plan = assert_first_text(tools.plan_rename(Parameters(PlanRenameParams {
            symbol: "x".into(),
            new_name: "y".into(),
            path: None,
            budget_tokens: 0,
        })));
        let text = first_text(&plan);
        assert!(text.contains("因 budget_tokens 被省略"), "{text}");
    }

    #[test]
    fn exact_references_use_path_line_anchors() {
        let tools = tools_with(Fake {
            references: Precise::exact(vec![loc("src/b.rs", 9, 4), loc("src/a.rs", 2, 0)]),
            definition: Precise::exact(vec![loc("src/a.rs", 2, 0)]),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            symbol: "x".into(),
            path: None,
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("@src/a.rs:3"), "{text}");
        assert!(text.contains("@src/b.rs:10"), "{text}");
        let a = text.find("@src/a.rs:3").unwrap();
        let b = text.find("@src/b.rs:10").unwrap();
        assert!(a < b, "locations must be sorted by path: {text}");
    }

    #[test]
    fn diagnostics_severity_filter_and_invalid_value() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(vec![
                diag("src/a.rs", 1, Severity::Error, "broken"),
                diag("src/a.rs", 2, Severity::Hint, "style"),
            ]),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let filtered = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            path: "src/a.rs".into(),
            severity: Some("error".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&filtered);
        assert!(text.contains("[error]"), "{text}");
        assert!(!text.contains("[hint]"), "{text}");
        assert!(text.contains("@src/a.rs:2"), "{text}");

        let invalid = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            path: "src/a.rs".into(),
            severity: Some("fatal".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&invalid);
        assert!(text.contains("无法识别的 severity"), "{text}");
        assert!(text.contains("confidence: Unknown"), "{text}");
    }

    #[test]
    fn empty_exact_references_are_not_unknown() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            symbol: "neverDefined".into(),
            path: None,
            budget_tokens: 100,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("未找到"), "{text}");
        assert!(!text.contains("search_code"), "{text}");
        assert!(!text.contains("尚未接通"), "{text}");
    }

    #[test]
    fn goto_definition_exact_hits_use_anchors() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(vec![loc("src/lib.rs", 11, 4)]),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "src/main.rs".into(),
            symbol: Some("Widget".into()),
            line: None,
            character: None,
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("@src/lib.rs:12"), "{text}");
        assert!(text.contains("col 4"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["matches"],
            json!(1)
        );
    }

    #[test]
    fn empty_exact_definitions_are_not_unknown() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "src/main.rs".into(),
            symbol: Some("neverDefined".into()),
            line: None,
            character: None,
            budget_tokens: 100,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("未找到"), "{text}");
        assert!(!text.contains("尚未接通"), "{text}");
        assert!(!text.contains("search_code"), "{text}");
    }

    #[test]
    fn goto_definition_missing_path_or_locator_is_unknown() {
        let tools = PreciseTools::new();
        let missing_path =
            assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
                path: "".into(),
                symbol: Some("x".into()),
                line: None,
                character: None,
                budget_tokens: 100,
            })));
        let text = first_text(&missing_path);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("path"), "{text}");

        let missing_locator =
            assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
                path: "src/main.rs".into(),
                symbol: None,
                line: None,
                character: None,
                budget_tokens: 100,
            })));
        let text = first_text(&missing_locator);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("symbol"), "{text}");
        assert!(
            text.contains("find_symbol") || text.contains("search_code"),
            "{text}"
        );
    }

    #[test]
    fn goto_definition_budget_zero_keeps_nonempty_text() {
        let many: Vec<Location> = (0..20).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(many),
            diagnostics: Precise::exact(Vec::new()),
            plan: Precise::unknown(empty_plan("x", "y"), "unused"),
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "src/lib.rs".into(),
            symbol: Some("x".into()),
            line: None,
            character: None,
            budget_tokens: 0,
        })));
        let text = first_text(&result);
        assert!(
            text.contains("因 budget_tokens 被省略") || text.contains("confidence:"),
            "{text}"
        );
    }

    #[test]
    fn apply_rename_refuses_low_confidence_without_force() {
        let dir = tempfile_dir();
        let file = "a.ts";
        std::fs::write(dir.join(file), "const handler = 1;\n").unwrap();
        let plan = RewritePlan {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            sites: vec![site(file, 6, 13, "handler", "onEvent")],
            evidence: Evidence::NameMatch,
            confidence: Confidence::Syntactic,
            excluded: Vec::new(),
        };
        assert!(!plan.is_auto_applicable());
        let tools = tools_with_root(
            Fake {
                references: Precise::exact(Vec::new()),
                definition: Precise::exact(Vec::new()),
                diagnostics: Precise::exact(Vec::new()),
                plan: Precise {
                    value: plan,
                    confidence: Confidence::Syntactic,
                    note: Some("名字匹配不足".into()),
                },
            },
            dir.clone(),
        );
        let result = assert_first_text(tools.apply_rename(Parameters(ApplyRenameParams {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            path: Some(file.into()),
            force: false,
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("拒绝落盘"), "{text}");
        assert!(text.contains("force=true"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["applied"],
            json!(false)
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).unwrap(),
            "const handler = 1;\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rename_writes_when_auto_applicable() {
        let dir = tempfile_dir();
        let file = "a.ts";
        let original = "const handler = 1;\n";
        std::fs::write(dir.join(file), original).unwrap();
        let start = original.find("handler").unwrap();
        let end = start + "handler".len();
        let plan = RewritePlan {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            sites: vec![site(file, start, end, "handler", "onEvent")],
            evidence: Evidence::ScopeBinding,
            confidence: Confidence::Scoped,
            excluded: Vec::new(),
        };
        assert!(plan.is_auto_applicable());
        let tools = tools_with_root(
            Fake {
                references: Precise::exact(Vec::new()),
                definition: Precise::exact(Vec::new()),
                diagnostics: Precise::exact(Vec::new()),
                plan: Precise {
                    value: plan,
                    confidence: Confidence::Scoped,
                    note: None,
                },
            },
            dir.clone(),
        );
        let result = assert_first_text(tools.apply_rename(Parameters(ApplyRenameParams {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            path: Some(file.into()),
            force: false,
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("applied:"), "{text}");
        assert!(text.contains("重解析"), "{text}");
        assert!(!text.contains("FORCE"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["applied"],
            json!(true)
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).unwrap(),
            "const onEvent = 1;\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_rename_force_writes_with_warning() {
        let dir = tempfile_dir();
        let file = "a.ts";
        let original = "const handler = 1;\n";
        std::fs::write(dir.join(file), original).unwrap();
        let start = original.find("handler").unwrap();
        let end = start + "handler".len();
        let plan = RewritePlan {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            sites: vec![site(file, start, end, "handler", "onEvent")],
            evidence: Evidence::NameMatch,
            confidence: Confidence::Syntactic,
            excluded: Vec::new(),
        };
        let tools = tools_with_root(
            Fake {
                references: Precise::exact(Vec::new()),
                definition: Precise::exact(Vec::new()),
                diagnostics: Precise::exact(Vec::new()),
                plan: Precise {
                    value: plan,
                    confidence: Confidence::Syntactic,
                    note: None,
                },
            },
            dir.clone(),
        );
        let result = assert_first_text(tools.apply_rename(Parameters(ApplyRenameParams {
            symbol: "handler".into(),
            new_name: "onEvent".into(),
            path: Some(file.into()),
            force: true,
            budget_tokens: 800,
        })));
        let text = first_text(&result);
        assert!(text.contains("FORCE"), "{text}");
        assert!(text.contains("applied:"), "{text}");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["forced"],
            json!(true)
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(file)).unwrap(),
            "const onEvent = 1;\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-apply-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
