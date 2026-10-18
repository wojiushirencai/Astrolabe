//! Precise (LSP-backed) query tools: references, definitions, diagnostics.
//!
//! Rendering lives here; `server.rs` mounts `#[tool]` methods that call
//! [`run_find_references`], [`run_goto_definition`], and
//! [`run_get_diagnostics`] through `LiveBackend`.
//! [`crate::TOOL_COUNT`] includes [`crate::PRECISE_TOOL_COUNT`] (3).
#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;

use astrolabe_core::{
    budget::{estimate_tokens, truncate_ranked},
    lsp::{queries::AMBIGUOUS_DEFINITIONS_MARK, Diagnostic, Location, Position, Precise, Severity},
    render::{anchor, confidence_note},
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

/// Same ceiling `ASTROLABE_MAX_BUDGET_TOKENS` puts on the other tools
/// (default 32000); `budget_tokens` above it is lowered, never honoured.
const MAX_BUDGET_TOKENS: usize = 32_000;

fn effective_budget(raw: usize) -> usize {
    let max = std::env::var("ASTROLABE_MAX_BUDGET_TOKENS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(MAX_BUDGET_TOKENS);
    raw.min(max)
}

/// Stated once per result that lists positions.
const COORD_NOTE: &str =
    "坐标：行、列均从 1 起，列按 UTF-16 单位；作为 line/character 入参时用 0 起的值（各减 1）。\n";

/// One page of `items` starting at `offset`, cut by `budget`. Always keeps at
/// least one item when any remain, so `next_offset` advances. Returns the kept
/// items and how many remain after them.
fn page<'a>(items: &'a [String], offset: usize, budget: usize) -> (Vec<&'a String>, usize) {
    let rest = items.get(offset..).unwrap_or(&[]);
    let (mut kept, mut omitted) = truncate_ranked(rest, budget, |line| format!("{line}\n"));
    if kept.is_empty() && !rest.is_empty() {
        kept.push(&rest[0]);
        omitted -= 1;
    }
    (kept, omitted)
}

fn omitted_line(omitted: usize, next_offset: usize) -> String {
    format!("{omitted} 个结果因 budget_tokens 被省略；用 offset={next_offset} 取后续。\n")
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FindReferencesParams {
    #[serde(default)]
    #[schemars(description = "Multi-repo workspace: sub-repo name or absolute path.")]
    pub root: Option<String>,
    #[schemars(description = "Symbol name to find references for.")]
    pub symbol: String,
    #[serde(default)]
    #[schemars(
        required,
        description = "Repo-relative file (selects the language server)."
    )]
    pub path: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based line.")]
    pub line: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based UTF-16 column; used with line.")]
    pub character: Option<u32>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Max tokens (capped at 32000).")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "Skip first N results (paging).")]
    pub offset: Option<usize>,
    #[serde(default)]
    #[schemars(description = "New name: preview per-file edits (no write).")]
    pub dry_run_rename: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GotoDefinitionParams {
    #[serde(default)]
    #[schemars(description = "Multi-repo workspace: sub-repo name or absolute path.")]
    pub root: Option<String>,
    #[schemars(description = "Repo-relative file containing the target symbol reference.")]
    pub path: String,
    #[serde(default)]
    #[schemars(description = "Symbol name to locate definition for.")]
    pub symbol: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based line; used with character.")]
    pub line: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional 0-based UTF-16 column.")]
    pub character: Option<u32>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Max tokens (capped at 32000).")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(description = "Skip first N results (paging).")]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetDiagnosticsParams {
    #[serde(default)]
    #[schemars(description = "Multi-repo workspace: sub-repo name or absolute path.")]
    pub root: Option<String>,
    #[schemars(description = "Repo-relative file to diagnose.")]
    pub path: String,
    #[serde(default)]
    #[schemars(description = "Minimum severity: error, warning, information, or hint.")]
    pub severity: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate token budget.")]
    pub budget_tokens: usize,
}

/// Backend for the precise MCP query tools.
///
/// Production uses `LiveBackend` (on-demand language-server pool). Tests
/// inject fakes for the missing-server path.
pub(crate) trait PreciseCapability: std::fmt::Debug + Send + Sync {
    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        position: Option<Position>,
    ) -> Precise<Vec<Location>>;
    fn definition(
        &self,
        path: &str,
        symbol: Option<&str>,
        line: Option<u32>,
        character: Option<u32>,
    ) -> Precise<Vec<Location>>;
    fn diagnostics(&self, path: &str) -> Precise<Vec<Diagnostic>>;
    /// Files changed on disk; keeps running language servers in sync.
    fn files_changed(&self, _changes: &astrolabe_core::watch::ChangeSet) {}
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
    fn references(
        &self,
        symbol: &str,
        path: Option<&str>,
        _position: Option<Position>,
    ) -> Precise<Vec<Location>> {
        Precise::unknown(
            Vec::new(),
            format!(
                "{NOT_WIRED} 目标符号 `{symbol}`{}。",
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
}

#[derive(Clone)]
pub(crate) struct PreciseTools {
    backend: Arc<dyn PreciseCapability>,
    tool_router: ToolRouter<Self>,
}

impl PreciseTools {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_backend(Arc::new(Unwired))
    }

    pub(crate) fn with_backend(backend: Arc<dyn PreciseCapability>) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl PreciseTools {
    #[tool(
        description = "精确引用查询（语言服务器）。LSP-resolved call sites and usages across repository. Required for renames and caller analysis. dry_run_rename=NewName: read-only per-file rename checklist.",
        annotations(title = "精确引用", read_only_hint = true)
    )]
    pub(crate) fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> CallToolResult {
        run_find_references(params, self.backend.as_ref())
    }

    #[tool(
        description = "精确定义跳转（语言服务器绑定解析）。Resolves definition anchor via language server semantics.",
        annotations(title = "精确定义", read_only_hint = true)
    )]
    pub(crate) fn goto_definition(
        &self,
        Parameters(params): Parameters<GotoDefinitionParams>,
    ) -> CallToolResult {
        run_goto_definition(params, self.backend.as_ref())
    }

    #[tool(
        description = "文件诊断（语言服务器）。LSP diagnostics (syntax and type errors) to verify file correctness after edits.",
        annotations(title = "文件诊断", read_only_hint = true)
    )]
    pub(crate) fn get_diagnostics(
        &self,
        Parameters(params): Parameters<GetDiagnosticsParams>,
    ) -> CallToolResult {
        run_get_diagnostics(params, self.backend.as_ref())
    }
}

pub(crate) fn run_find_references(
    mut params: FindReferencesParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    params.budget_tokens = effective_budget(params.budget_tokens);
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

    let position = match (params.line, params.character) {
        (Some(line), Some(character)) => Some(Position { line, character }),
        _ => None,
    };
    let result = backend.references(&params.symbol, params.path.as_deref(), position);
    // Order is the backend's: queried file first, then path and position, so
    // budget truncation drops the least local hits last.
    let locations = result.value;

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

    let offset = params.offset.unwrap_or(0);
    let items: Vec<String> = locations.iter().map(render_location).collect();
    let (kept, omitted) = page(&items, offset, params.budget_tokens);

    let mut body = preamble;
    if !kept.is_empty() {
        body.push_str(COORD_NOTE);
    }
    if offset > 0 && offset >= items.len() && !items.is_empty() {
        body.push_str(&format!("offset={offset} 超出结果总数 {}。\n", items.len()));
    }
    for line in &kept {
        body.push_str(line);
        body.push('\n');
    }
    let next_offset = offset + kept.len();
    if omitted > 0 {
        body.push_str(&omitted_line(omitted, next_offset));
    }
    let mut preview_files = 0usize;
    if let Some(new_name) = params.dry_run_rename.as_deref() {
        // One shared budget: the preview only gets what the list left over.
        let remaining = params.budget_tokens.saturating_sub(estimate_tokens(&body));
        let ambiguous = result
            .note
            .as_deref()
            .is_some_and(|n| n.contains(AMBIGUOUS_DEFINITIONS_MARK));
        let (text, files) = render_rename_preview(
            &params.symbol,
            new_name,
            &locations,
            result.confidence,
            ambiguous,
            remaining,
        );
        body.push_str(&text);
        preview_files = files;
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
            "offset": offset,
            "returned": kept.len(),
            "omitted": omitted,
            "truncated": omitted > 0,
            "next_offset": (omitted > 0).then_some(next_offset),
            "dry_run_rename": params.dry_run_rename.is_some(),
            "rename_files": preview_files,
        }),
    )
}

/// Read-only rename preview: per-file checklist of the sites that hold the
/// old name (1-based line and column, UTF-16 units, as in the reference list). Nothing
/// is written; the caller edits file by file and verifies with
/// get_diagnostics.
fn render_rename_preview(
    symbol: &str,
    new_name: &str,
    locations: &[Location],
    confidence: Confidence,
    ambiguous: bool,
    budget_tokens: usize,
) -> (String, usize) {
    let new_name = new_name.trim();
    let valid = !new_name.is_empty()
        && !new_name.chars().next().is_some_and(|c| c.is_ascii_digit())
        && new_name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    if !valid {
        return (
            format!("改名预览未生成：dry_run_rename=`{new_name}` 不是合法标识符。\n"),
            0,
        );
    }
    if new_name == symbol {
        return ("改名预览未生成：新名与旧名相同。\n".to_string(), 0);
    }
    if confidence == Confidence::Unknown {
        return (
            format!(
                "改名预览未生成：`{symbol}` 的引用查询没有得出可信结果（confidence: Unknown），不能据此判断是否需要改动。\n"
            ),
            0,
        );
    }
    if ambiguous {
        return (
            format!(
                "改名预览未生成：`{symbol}` 在该文件有多个同名定义，引用已被合并，不是单个符号的改名清单。请用 line/character 指定其中一个再预览。\n"
            ),
            0,
        );
    }
    if locations.is_empty() {
        return (
            format!("改名预览：语言服务器未返回 `{symbol}` 的任何引用，无需改动。\n"),
            0,
        );
    }
    let mut by_file: Vec<(&str, Vec<String>)> = Vec::new();
    for loc in locations {
        let site = format!(
            "L{}:{}",
            loc.range.start.line.saturating_add(1),
            loc.range.start.character.saturating_add(1)
        );
        match by_file.last_mut() {
            Some((path, sites)) if *path == loc.path.as_str() => sites.push(site),
            _ => by_file.push((loc.path.as_str(), vec![site])),
        }
    }
    let mut out = format!(
        "改名预览（只读，未写盘）：`{symbol}` → `{new_name}`，共 {} 处 / {} 个文件。逐文件把下列位置的 `{symbol}` 替换为 `{new_name}`：\n",
        locations.len(),
        by_file.len()
    );
    let lines: Vec<String> = by_file
        .iter()
        .map(|(path, sites)| format!("  {path} ×{}: {}", sites.len(), sites.join(" ")))
        .collect();
    let (kept, omitted) = truncate_ranked(&lines, budget_tokens, |l| format!("{l}\n"));
    for line in kept {
        out.push_str(line);
        out.push('\n');
    }
    if omitted > 0 {
        out.push_str(&format!(
            "（不完整）另有 {omitted} 个文件因 budget_tokens 未列出；加大 budget_tokens 取全清单。\n"
        ));
    }
    if confidence == Confidence::Exact {
        out.push_str("清单来自语言服务器（精确）；替换完成后用 get_diagnostics 逐文件验证。\n");
    } else {
        out.push_str("注意：引用置信度低于 exact，清单可能漏报或误报，替换前请核对。\n");
    }
    (out, by_file.len())
}

pub(crate) fn run_goto_definition(
    mut params: GotoDefinitionParams,
    backend: &dyn PreciseCapability,
) -> CallToolResult {
    params.budget_tokens = effective_budget(params.budget_tokens);
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
    let locations = result.value;

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

    let offset = params.offset.unwrap_or(0);
    let items: Vec<String> = locations.iter().map(render_location).collect();
    let (kept, omitted) = page(&items, offset, params.budget_tokens);

    let mut body = preamble;
    if !kept.is_empty() {
        body.push_str(COORD_NOTE);
    }
    if offset > 0 && offset >= items.len() && !items.is_empty() {
        body.push_str(&format!("offset={offset} 超出结果总数 {}。\n", items.len()));
    }
    for line in &kept {
        body.push_str(line);
        body.push('\n');
    }
    let next_offset = offset + kept.len();
    if omitted > 0 {
        body.push_str(&omitted_line(omitted, next_offset));
    }

    text_result(
        body,
        json!({
            "confidence": confidence_json(result.confidence),
            "budget_tokens": params.budget_tokens,
            "matches": locations.len(),
            "offset": offset,
            "returned": kept.len(),
            "omitted": omitted,
            "truncated": omitted > 0,
            "next_offset": (omitted > 0).then_some(next_offset),
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
    if server_missing(note) {
        body.push_str(&unavailable_ensure_gate(language_of(path)));
        body.push_str("安装提示：\n");
        body.push_str(&install_hint(language_of(path)));
    }
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
    if server_missing(note) {
        body.push_str(&unavailable_ensure_gate(language_of(path)));
        body.push_str("安装提示：\n");
        body.push_str(&install_hint(language_of(path)));
    }
    body.push_str(DEFINITION_FALLBACK);
    body.push('\n');
    body
}

/// True when the note says the language server itself is missing or cannot
/// start (or when there is no note at all). Only then is an install hint
/// useful; a bad symbol, position, or timeout with a working server is not
/// fixed by installing anything.
fn server_missing(note: Option<&str>) -> bool {
    let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) else {
        return true;
    };
    [
        "not installed",
        "no language server available",
        "failed to start",
        "尚未接通",
        "语言服务器不可用",
    ]
    .iter()
    .any(|m| note.contains(m))
}

/// Prompt contract when a precise tool hits Unavailable: tell the model to
/// install the language server locally, or inspect `get_languages`.
fn unavailable_ensure_gate(language: Option<Language>) -> String {
    let (server, _lang_name) = match language {
        Some(lang) => (
            astrolabe_core::lsp::router::primary_server_name(lang),
            lang.name(),
        ),
        None => ("the language server", "<language>"),
    };
    format!(
        "语言服务器不可用（{server}）。请在本机安装后重试，或用 get_languages 查看 Ready / needs_install / AST-only。\n"
    )
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
                Language::Php,
                Language::Dart,
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
        Language::Swift => {
            "Swift（sourcekit-lsp）：`xcode-select --install` 或完整 Xcode；可用 ASTROLABE_LSP_SWIFT 覆盖。".into()
        }
        Language::ObjC | Language::ObjCpp => {
            "ObjC/ObjC++：优先 clangd（`brew install llvm` / `apt-get install clangd`），回退 sourcekit-lsp（`xcode-select --install`）。".into()
        }
        Language::C | Language::Cpp => {
            "C/C++（clangd）：`brew install llvm`，或设置 ASTROLABE_LSP_C。".into()
        }
        Language::Kotlin => {
            "Kotlin：优先 kotlin-lsp（JetBrains，`brew install JetBrains/utils/kotlin-lsp`），回退 kotlin-language-server；可用 ASTROLABE_LSP_KOTLIN 覆盖。".into()
        }
        Language::Shell => {
            "Shell/Bash（bash-language-server）：`npm i -g bash-language-server`，可选装 shellcheck。".into()
        }
        Language::Ruby => {
            "Ruby（ruby-lsp）：`gem install ruby-lsp`，回退 solargraph。".into()
        }
        Language::Lua => {
            "Lua（lua-language-server）：`brew install lua-language-server`。".into()
        }
        Language::Php => {
            "PHP（intelephense）：`npm i -g intelephense`。".into()
        }
        Language::Vue => {
            "Vue（Volar / vue-language-server）：`npm i -g @vue/language-server typescript-language-server typescript`，stdio 启动 `vue-language-server --stdio`。可设 ASTROLABE_LSP_VUE。2.x 单进程（hybridMode=false）；3.x 自动桥接 typescript-language-server + @vue/typescript-plugin。".into()
        }
        Language::Dart => "Dart（dart language-server）：内置于 Dart / Flutter SDK。".into(),
        Language::CSharp => {
            "C#（Microsoft.CodeAnalysis.LanguageServer）：`dotnet <dll> --stdio`；需要 .NET 10 runtime（分析目标框架可更旧）；设置 ASTROLABE_LSP_CSHARP。非 OmniSharp / csharp-ls。".into()
        }
        Language::VisualBasic => {
            "Visual Basic .NET（同一 Microsoft.CodeAnalysis.LanguageServer）：`dotnet <dll> --stdio`；与 C# 共用 ASTROLABE_LSP_CSHARP；.vb / .vbproj / .sln。非 OmniSharp。".into()
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
    // LSP positions are 0-based; every position we print is 1-based
    // (line in the anchor, UTF-16 column after `col`).
    format!(
        "{}  col {}",
        anchor(&loc.path, loc.range.start.line.saturating_add(1)),
        loc.range.start.character.saturating_add(1)
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

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Information => "information",
        Severity::Hint => "hint",
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

    #[derive(Debug)]
    struct Fake {
        references: Precise<Vec<Location>>,
        definition: Precise<Vec<Location>>,
        diagnostics: Precise<Vec<Diagnostic>>,
    }

    impl PreciseCapability for Fake {
        fn references(
            &self,
            _symbol: &str,
            _path: Option<&str>,
            _position: Option<Position>,
        ) -> Precise<Vec<Location>> {
            self.references.clone()
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
    }

    fn tools_with(fake: Fake) -> PreciseTools {
        PreciseTools::with_backend(Arc::new(fake))
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
            vec!["find_references", "get_diagnostics", "goto_definition"]
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
            required.iter().any(|f| f == "path"),
            "path is effectively required and must be listed: {required:?}"
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
            root: None,
            symbol: "Handler".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            path: "src/main.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        })));
        assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            path: "src/main.go".into(),
            severity: None,
            budget_tokens: 800,
        })));
    }

    #[test]
    fn lsp_unavailable_degrades_with_install_hint() {
        let tools = tools_with(Fake {
            references: Precise::unknown(Vec::new(), "gopls is not installed"),
            definition: Precise::unknown(Vec::new(), "gopls is not installed"),
            diagnostics: Precise::unknown(Vec::new(), "gopls is not installed"),
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "Handler".into(),
            path: Some("pkg/server.go".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&refs);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(
            text.contains(confidence_note(Confidence::Unknown)),
            "{text}"
        );
        assert!(text.contains("gopls"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("get_languages"), "{text}");
        assert!(text.contains("search_code"), "{text}");
        assert!(text.contains("误报"), "{text}");
        assert!(
            text.contains("尚未接通") || text.contains("not installed"),
            "{text}"
        );

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            path: "pkg/server.go".into(),
            severity: None,
            budget_tokens: 800,
        })));
        let text = first_text(&diags);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("go install"), "{text}");
        assert!(text.contains("get_languages"), "{text}");
        assert!(!text.contains("search_code"), "{text}");

        let defs = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            path: "pkg/server.go".into(),
            symbol: Some("Handler".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
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
        });

        let refs = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "x".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 0,
            offset: None,
            dry_run_rename: None,
        })));
        let text = first_text(&refs);
        assert!(text.contains("因 budget_tokens 被省略"), "{text}");
        let structured = refs.structured_content.unwrap();
        assert_eq!(structured["budget_tokens"], json!(0));
        assert_eq!(structured["truncated"], json!(true));
        assert!(structured["omitted"].as_u64().unwrap() >= 1);
        assert_eq!(
            structured["returned"].as_u64().unwrap() + structured["omitted"].as_u64().unwrap(),
            structured["matches"].as_u64().unwrap()
        );

        let diags = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            path: "src/lib.rs".into(),
            severity: None,
            budget_tokens: 0,
        })));
        let text = first_text(&diags);
        assert!(text.contains("因 budget_tokens 被省略"), "{text}");
    }

    #[test]
    fn exact_references_use_path_line_anchors() {
        let tools = tools_with(Fake {
            references: Precise::exact(vec![loc("src/b.rs", 9, 4), loc("src/a.rs", 2, 0)]),
            definition: Precise::exact(vec![loc("src/a.rs", 2, 0)]),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "x".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
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
        });
        let filtered = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            path: "src/a.rs".into(),
            severity: Some("error".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&filtered);
        assert!(text.contains("[error]"), "{text}");
        assert!(!text.contains("[hint]"), "{text}");
        assert!(text.contains("@src/a.rs:2"), "{text}");

        let invalid = assert_first_text(tools.get_diagnostics(Parameters(GetDiagnosticsParams {
            root: None,
            path: "src/a.rs".into(),
            severity: Some("fatal".into()),
            budget_tokens: 800,
        })));
        let text = first_text(&invalid);
        assert!(text.contains("无法识别的 severity"), "{text}");
        assert!(text.contains("confidence: Unknown"), "{text}");
    }

    #[test]
    fn dry_run_rename_lists_edits_per_file_without_writing() {
        let tools = tools_with(Fake {
            references: Precise::exact(vec![
                loc("src/a.rs", 3, 4),
                loc("src/a.rs", 9, 8),
                loc("src/b.rs", 0, 12),
            ]),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "old_name".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: Some("new_name".into()),
        })));
        let text = first_text(&result);
        assert!(text.contains("改名预览（只读，未写盘）"), "{text}");
        assert!(
            text.contains("`old_name` → `new_name`，共 3 处 / 2 个文件"),
            "{text}"
        );
        assert!(text.contains("  src/a.rs ×2: L4:5 L10:9"), "{text}");
        assert!(text.contains("  src/b.rs ×1: L1:13"), "{text}");
        assert!(text.contains("get_diagnostics"), "{text}");
        // No preview without the parameter.
        let plain = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "old_name".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
            dry_run_rename: None,
        })));
        assert!(!first_text(&plain).contains("改名预览"));
    }

    #[test]
    fn dry_run_rename_rejects_bad_names_and_flags_low_confidence() {
        let (bad, _) = render_rename_preview(
            "a",
            "1x",
            &[loc("a.rs", 0, 0)],
            Confidence::Exact,
            false,
            800,
        );
        assert!(bad.contains("不是合法标识符"), "{bad}");
        let (same, _) = render_rename_preview(
            "a",
            "a",
            &[loc("a.rs", 0, 0)],
            Confidence::Exact,
            false,
            800,
        );
        assert!(same.contains("相同"), "{same}");
        let (none, n) = render_rename_preview("a", "b", &[], Confidence::Exact, false, 800);
        assert!(none.contains("无需改动") && n == 0, "{none}");
        let (low, _) = render_rename_preview(
            "a",
            "b",
            &[loc("a.rs", 0, 0)],
            Confidence::Syntactic,
            false,
            800,
        );
        assert!(low.contains("低于 exact"), "{low}");
    }

    #[test]
    fn empty_exact_references_are_not_unknown() {
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        });
        let result = assert_first_text(tools.find_references(Parameters(FindReferencesParams {
            root: None,
            symbol: "neverDefined".into(),
            path: None,
            line: None,
            character: None,
            budget_tokens: 100,
            offset: None,
            dry_run_rename: None,
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
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            path: "src/main.rs".into(),
            symbol: Some("Widget".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        })));
        let text = first_text(&result);
        assert!(text.contains("confidence: Exact"), "{text}");
        assert!(text.contains("@src/lib.rs:12"), "{text}");
        assert!(text.contains("col 5"), "{text}");
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
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            path: "src/main.rs".into(),
            symbol: Some("neverDefined".into()),
            line: None,
            character: None,
            budget_tokens: 100,
            offset: None,
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
                root: None,
                path: "".into(),
                symbol: Some("x".into()),
                line: None,
                character: None,
                budget_tokens: 100,
                offset: None,
            })));
        let text = first_text(&missing_path);
        assert!(text.contains("confidence: Unknown"), "{text}");
        assert!(text.contains("path"), "{text}");

        let missing_locator =
            assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
                root: None,
                path: "src/main.rs".into(),
                symbol: None,
                line: None,
                character: None,
                budget_tokens: 100,
                offset: None,
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
        });
        let result = assert_first_text(tools.goto_definition(Parameters(GotoDefinitionParams {
            root: None,
            path: "src/lib.rs".into(),
            symbol: Some("x".into()),
            line: None,
            character: None,
            budget_tokens: 0,
            offset: None,
        })));
        let text = first_text(&result);
        assert!(
            text.contains("因 budget_tokens 被省略") || text.contains("confidence:"),
            "{text}"
        );
    }

    #[test]
    fn default_install_hint_includes_dart() {
        let hint = install_hint(None);
        assert!(hint.contains("Dart"));
        assert!(hint.contains("dart language-server"));
    }

    fn refs_params(
        budget: usize,
        offset: Option<usize>,
        rename: Option<&str>,
    ) -> FindReferencesParams {
        FindReferencesParams {
            symbol: "Run".into(),
            path: Some("a.go".into()),
            line: None,
            character: None,
            budget_tokens: budget,
            offset,
            dry_run_rename: rename.map(str::to_string),
        }
    }

    fn fake_refs(references: Precise<Vec<Location>>) -> PreciseTools {
        tools_with(Fake {
            references,
            definition: Precise::exact(Vec::new()),
            diagnostics: Precise::exact(Vec::new()),
        })
    }

    #[test]
    fn rename_preview_refused_when_references_unknown() {
        let tools = fake_refs(Precise::unknown(Vec::new(), "gopls is not installed"));
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("改名预览未生成"), "{text}");
        assert!(!text.contains("无需改动"), "{text}");
    }

    #[test]
    fn rename_preview_refused_for_merged_definitions_and_not_called_exact() {
        let note = format!("{AMBIGUOUS_DEFINITIONS_MARK} `Run` has 3 definitions");
        let tools = fake_refs(Precise {
            value: vec![loc("a.go", 5, 11), loc("a.go", 9, 11)],
            confidence: Confidence::Scoped,
            note: Some(note),
        });
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("多个同名定义"), "{text}");
        assert!(!text.contains("清单来自语言服务器（精确）"), "{text}");
        assert!(!text.contains("改名预览（只读"), "{text}");
        assert!(text.contains("confidence: Scoped"), "{text}");
    }

    #[test]
    fn truncation_keeps_backend_order_queried_file_first() {
        // Backend order: queried file (zzz.go) first, then aaa.go.
        let mut v = vec![loc("zzz.go", 0, 0)];
        v.extend((0..16).map(|i| loc("aaa.go", i, 0)));
        let tools = fake_refs(Precise::exact(v));
        let r = tools.find_references(Parameters(refs_params(8, None, None)));
        let text = first_text(&r);
        assert!(text.contains("@zzz.go:1"), "{text}");
    }

    #[test]
    fn budget_is_capped_and_preview_shares_it() {
        let many: Vec<Location> = (0..3000).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = fake_refs(Precise::exact(many));
        let r = tools.find_references(Parameters(refs_params(100_000_000, None, None)));
        let s = r.structured_content.unwrap();
        assert_eq!(s["budget_tokens"], json!(MAX_BUDGET_TOKENS));
        assert!(s["truncated"].as_bool().unwrap());
        let text = first_text(&r);
        assert!(
            estimate_tokens(text) <= MAX_BUDGET_TOKENS + 300,
            "{}",
            estimate_tokens(text)
        );

        // Preview: list + preview together stay within one budget.
        let r = tools.find_references(Parameters(refs_params(400, None, Some("Go"))));
        let text = first_text(&r);
        assert!(
            estimate_tokens(text) <= 400 + 120,
            "{}",
            estimate_tokens(text)
        );
    }

    #[test]
    fn find_references_pages_with_offset() {
        let many: Vec<Location> = (0..40).map(|i| loc("src/lib.rs", i, 0)).collect();
        let tools = fake_refs(Precise::exact(many));
        let first = tools.find_references(Parameters(refs_params(60, None, None)));
        let s1 = first.structured_content.clone().unwrap();
        assert_eq!(s1["truncated"], json!(true));
        let next = s1["next_offset"].as_u64().unwrap() as usize;
        assert!(next > 0);
        let t1 = first_text(&first).to_string();
        assert!(t1.contains(&format!("offset={next}")), "{t1}");
        let second = tools.find_references(Parameters(refs_params(60, Some(next), None)));
        let t2 = first_text(&second);
        assert!(t2.contains(&format!("@src/lib.rs:{}", next + 1)), "{t2}");
        assert!(!t2.contains("@src/lib.rs:1 "), "{t2}");
        // Zero budget still advances.
        let z = tools.find_references(Parameters(refs_params(0, Some(3), None)));
        assert_eq!(z.structured_content.unwrap()["next_offset"], json!(4));
    }

    #[test]
    fn goto_definition_pages_and_lists_all_definitions() {
        let defs: Vec<Location> = (0..3).map(|i| loc("a.go", i * 10, 5)).collect();
        let tools = tools_with(Fake {
            references: Precise::exact(Vec::new()),
            definition: Precise {
                value: defs,
                confidence: Confidence::Scoped,
                note: Some(format!(
                    "{AMBIGUOUS_DEFINITIONS_MARK} `Run` has 3 declarations"
                )),
            },
            diagnostics: Precise::exact(Vec::new()),
        });
        let r = tools.goto_definition(Parameters(GotoDefinitionParams {
            path: "a.go".into(),
            symbol: Some("Run".into()),
            line: None,
            character: None,
            budget_tokens: 800,
            offset: None,
        }));
        let text = first_text(&r);
        assert!(
            text.contains("@a.go:1 ") && text.contains("@a.go:11 ") && text.contains("@a.go:21 "),
            "{text}"
        );
        assert!(text.contains("3 declarations"), "{text}");
        assert!(!text.contains("confidence: Exact"), "{text}");
    }

    #[test]
    fn coordinates_are_one_based_and_stated() {
        let tools = fake_refs(Precise::exact(vec![loc("a.go", 5, 11)]));
        let r = tools.find_references(Parameters(refs_params(800, None, Some("Go"))));
        let text = first_text(&r);
        assert!(text.contains("@a.go:6  col 12"), "{text}");
        assert!(text.contains("L6:12"), "{text}");
        assert!(text.contains("UTF-16"), "{text}");
    }

    #[test]
    fn install_hint_only_when_server_is_missing() {
        let tools = fake_refs(Precise::unknown(
            Vec::new(),
            "symbol `NoSuchThing` not found in a.go; cannot provide precise references.",
        ));
        let r = tools.find_references(Parameters(refs_params(800, None, None)));
        let text = first_text(&r);
        assert!(text.contains("not found in a.go"), "{text}");
        assert!(!text.contains("go install"), "{text}");
        assert!(!text.contains("语言服务器不可用"), "{text}");
    }
}
