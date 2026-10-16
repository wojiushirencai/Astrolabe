use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
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
    CodeSymbol, Confidence, EdgeKind, FileId,
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

use crate::loop_guard::{LoopGuard, Verdict};
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

/// search_code 的分页状态行：保留 `shown= omitted= truncated=` 机器可读格式，
/// 另给出总数、本页范围和下一页的精确 offset。
/// One `search_code` match before rendering.
#[derive(Debug, Clone)]
struct SearchHit {
    path: String,
    /// 1-based.
    line: usize,
    /// Clipped matching line.
    text: String,
    text_only: bool,
}

/// Upper bound on `context_lines`.
const MAX_SEARCH_CONTEXT: usize = 10;
/// Columns kept per context line.
const CONTEXT_LINE_COLUMNS: usize = 160;

/// Context lines per match: the caller's value (capped), or adaptive by
/// match count. Few hits get enclosing bodies instead (see
/// [`smart_body_max_hits`]); these lines are the fallback for them.
fn search_context_lines(requested: Option<usize>, total: usize) -> usize {
    match requested {
        Some(n) => n.min(MAX_SEARCH_CONTEXT),
        // Ablation: ASTROLABE_SEARCH_ADAPTIVE=0 → bare lines by default.
        None if matches!(
            std::env::var("ASTROLABE_SEARCH_ADAPTIVE").as_deref(),
            Ok("0")
        ) =>
        {
            0
        }
        None if total <= smart_body_max_hits() => 3,
        None if total <= 20 => 1,
        None => 0,
    }
}

/// Hits at or below this count are shown inside their enclosing symbol body
/// (`ASTROLABE_SEARCH_BODY_HITS`, default 5; 0 disables).
fn smart_body_max_hits() -> usize {
    env_usize("ASTROLABE_SEARCH_BODY_HITS").unwrap_or(5)
}

/// Above this many hits an unpaged query gets a per-file summary
/// (`ASTROLABE_SEARCH_BROAD_HITS`, default 60).
fn broad_hits() -> usize {
    env_usize("ASTROLABE_SEARCH_BROAD_HITS").unwrap_or(60)
}

/// Enclosing bodies longer than this fall back to line context.
const SMART_BODY_MAX_LINES: u32 = 160;

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse().ok()
}

fn session_dedup_enabled(param: Option<bool>) -> bool {
    param.unwrap_or(true)
        && !matches!(
            std::env::var("ASTROLABE_SESSION_DEDUP").as_deref(),
            Ok("0") | Ok("false")
        )
}

/// Data / fixture / test-vector file (json, yaml, toml …) by extension and
/// directory: a literal changed in code usually has copies here.
fn is_data_fixture(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    let ext = p.rsplit_once('.').map_or("", |(_, e)| e);
    let data_ext = matches!(
        ext,
        "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "toml"
            | "csv"
            | "tsv"
            | "xml"
            | "txt"
            | "golden"
            | "snap"
            | "ini"
            | "proto"
            | "hex"
    );
    let dirs = [
        "test",
        "tests",
        "testdata",
        "fixtures",
        "fixture",
        "vectors",
        "golden",
        "snapshots",
        "__snapshots__",
        "spec",
        "data",
        "testvectors",
    ];
    data_ext && p.split('/').any(|seg| dirs.contains(&seg))
}

/// Compact list of literal hits in data/fixture files outside `filter`.
fn data_hits_outside(
    index: &RepoIndex,
    root: &Path,
    filter: &str,
    query: &str,
    sensitive: bool,
) -> String {
    let rels: Vec<&str> = index
        .text_only_paths(None)
        .into_iter()
        .map(|p| p.as_str())
        .filter(|p| !p.contains(filter) && is_data_fixture(p))
        .collect();
    if rels.is_empty() {
        return String::new();
    }
    let Ok(matcher) = astrolabe_core::text_search::build_matcher(query, false, sensitive) else {
        return String::new();
    };
    let found = astrolabe_core::text_search::search_files(
        root,
        &rels,
        &matcher,
        TEXT_SEARCH_MAX_FILE_BYTES,
    );
    let mut files: Vec<(String, Vec<u64>)> = found
        .into_iter()
        .filter(|(_, hits)| !hits.is_empty())
        .map(|(i, hits)| (rels[i].to_string(), hits.iter().map(|h| h.line).collect()))
        .collect();
    if files.is_empty() {
        return String::new();
    }
    files.sort();
    let total: usize = files.iter().map(|(_, l)| l.len()).sum();
    let mut out = format!(
        "path_filter 之外的数据/夹具文件中另有 {total} 处字面命中（改字面量时通常要同步）：\n"
    );
    for (path, lines) in files.iter().take(8) {
        let shown: Vec<String> = lines.iter().take(5).map(u64::to_string).collect();
        let more = if lines.len() > 5 {
            format!(" …共 {} 处", lines.len())
        } else {
            String::new()
        };
        out.push_str(&format!("  {path}: 行 {}{more}\n", shown.join(",")));
    }
    if files.len() > 8 {
        out.push_str(&format!("  …另有 {} 个文件\n", files.len() - 8));
    }
    out
}

/// One explain_symbol call site.
#[derive(Debug, Clone)]
struct ExplainSite {
    path: String,
    line: usize,
    caller: String,
    code: String,
    test: bool,
    target: String,
}

impl ExplainSite {
    fn render(&self, show_target: bool) -> String {
        let target = if show_target {
            format!(" → {}", self.target)
        } else {
            String::new()
        };
        format!(
            "@{}:{}  in {}{target} │ {}",
            self.path, self.line, self.caller, self.code
        )
    }
}

/// Call sites of `defs`: language-server references when that server is
/// already running (exact), else the syntactic call graph narrowed to the
/// actual call lines and filtered by call form (method vs function).
fn explain_call_sites(
    session: &RootSession,
    index: &RepoIndex,
    defs: &[&CodeSymbol],
    params: &ExplainParams,
) -> (Vec<ExplainSite>, bool) {
    let symbols = &index.graph().symbols;
    let file_syms = |file: astrolabe_core::types::FileId| -> Vec<&CodeSymbol> {
        symbols.iter().filter(|s| s.file == file).collect()
    };
    let lines_of = |path: &str| {
        index
            .cached_lines(&session.file_cache, path)
            .map(|(l, _)| l)
    };
    let clip = |text: &str| -> String {
        let t = text.trim();
        let c: String = t.chars().take(120).collect();
        if c.len() < t.len() {
            format!("{c}…")
        } else {
            c
        }
    };
    let caller_label = |path: &str, line: usize| -> String {
        match enclosing_symbol(index, path, line) {
            Some(sym) => {
                let same = file_syms(sym.file);
                crate::explain::qualified_name(&sym, &same)
            }
            None => "(top level)".to_string(),
        }
    };
    let mut out: Vec<ExplainSite> = Vec::new();
    let mut lsp_used = false;
    let lsp_allowed = params.precise.unwrap_or(true)
        && !matches!(std::env::var("ASTROLABE_EXPLAIN_LSP").as_deref(), Ok("0"));
    let mut lsp_done = BTreeSet::new();
    if lsp_allowed {
        for def in defs {
            let Some(file) = index.file(def.file) else {
                continue;
            };
            let path = file.path.as_str();
            if !session.precise.is_warm(path) {
                continue;
            }
            let Some(lines) = lines_of(path) else {
                continue;
            };
            let decl = lines
                .get(def.start_line as usize - 1)
                .cloned()
                .unwrap_or_default();
            let Some(col) = decl.find(&def.name) else {
                continue;
            };
            let result = session.precise.references(
                &def.name,
                Some(path),
                Some(astrolabe_core::lsp::Position {
                    line: def.start_line - 1,
                    character: decl[..col].chars().count() as u32,
                }),
            );
            if result.confidence != Confidence::Exact {
                continue;
            }
            let same = file_syms(def.file);
            let target = crate::explain::qualified_name(def, &same);
            for loc in &result.value {
                let p = loc.path.as_str().to_string();
                let line = loc.range.start.line as usize + 1;
                if p == path && line == def.start_line as usize {
                    continue;
                }
                let code = lines_of(&p)
                    .and_then(|l| l.get(line - 1).cloned())
                    .unwrap_or_default();
                let caller = caller_label(&p, line);
                let test = crate::explain::is_test_caller(
                    &p,
                    caller.rsplit('.').next().unwrap_or(&caller),
                );
                out.push(ExplainSite {
                    path: p,
                    line,
                    caller,
                    code: clip(&code),
                    test,
                    target: target.clone(),
                });
            }
            lsp_done.insert(def.id.0);
            lsp_used = true;
        }
    }
    let pending: Vec<&&CodeSymbol> = defs
        .iter()
        .filter(|d| !lsp_done.contains(&d.id.0))
        .collect();
    if !pending.is_empty() {
        let wanted: BTreeSet<u32> = pending.iter().map(|d| d.id.0).collect();
        let mut callers: Vec<&CodeSymbol> = Vec::new();
        let mut seen_caller = BTreeSet::new();
        for edge in index
            .graph()
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Call && wanted.contains(&e.to))
        {
            if seen_caller.insert(edge.from) {
                if let Some(from) = symbols.iter().find(|s| s.id.0 == edge.from) {
                    callers.push(from);
                }
            }
        }
        let def_info: Vec<(&CodeSymbol, String, bool, String)> = pending
            .iter()
            .filter_map(|d| {
                let file = index.file(d.file)?;
                let same = file_syms(d.file);
                Some((
                    **d,
                    crate::explain::qualified_name(d, &same),
                    crate::explain::is_method(d, &same),
                    file.path.as_str().to_string(),
                ))
            })
            .collect();
        let mut seen_site = BTreeSet::new();
        for caller in callers {
            let Some(file) = index.file(caller.file) else {
                continue;
            };
            let path = file.path.as_str();
            let Some(lines) = lines_of(path) else {
                continue;
            };
            let same = file_syms(caller.file);
            let label = crate::explain::qualified_name(caller, &same);
            let test = crate::explain::is_test_caller(path, &caller.name);
            let skip = (caller.name == params.name
                || def_info.iter().any(|(d, ..)| d.id == caller.id))
            .then_some(caller.start_line as usize);
            let name = &def_info
                .first()
                .map(|(d, ..)| d.name.clone())
                .unwrap_or_default();
            for (line, form) in crate::explain::call_sites(
                &lines,
                caller.start_line as usize,
                caller.end_line as usize,
                name,
                skip,
            ) {
                for (def, target, method, def_path) in &def_info {
                    let same_dir = crate::explain::dir_of(def_path) == crate::explain::dir_of(path);
                    let go = def_path.ends_with(".go");
                    if !crate::explain::form_matches(form, *method, same_dir, go) {
                        continue;
                    }
                    if !seen_site.insert((path.to_string(), line, def.id.0)) {
                        continue;
                    }
                    out.push(ExplainSite {
                        path: path.to_string(),
                        line,
                        caller: label.clone(),
                        code: clip(&lines[line - 1]),
                        test,
                        target: target.clone(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.test
            .cmp(&b.test)
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });
    (out, lsp_used)
}

/// Smallest symbol in `file` that encloses `line` with at least 3 lines.
fn enclosing_symbol(index: &RepoIndex, path: &str, line: usize) -> Option<CodeSymbol> {
    let file = index.file_id(path)?;
    let line = line as u32;
    index
        .graph()
        .symbols
        .iter()
        .filter(|s| s.file == file && s.start_line <= line && line <= s.end_line)
        .filter(|s| s.end_line >= s.start_line + 2)
        .min_by_key(|s| s.end_line - s.start_line)
        .cloned()
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    ["test", "spec", "mock", "fixture", "__snapshots__", "e2e"]
        .iter()
        .any(|m| lower.contains(m))
        || [
            "docs/",
            "doc/",
            "examples/",
            "example/",
            "scripts/",
            "probe",
            "vendor/",
            "third_party/",
        ]
        .iter()
        .any(|m| lower.starts_with(m) || lower.contains(&format!("/{m}")))
}

/// Order broad-query hits for the preview: definition lines first, other
/// non-test hits next, test/docs/example files last (stable within each class).
fn rank_broad_hits<'a>(index: &RepoIndex, hits: &'a [SearchHit]) -> Vec<&'a SearchHit> {
    let defs: std::collections::HashSet<(FileId, u32)> = index
        .graph()
        .symbols
        .iter()
        .filter(|s| {
            !matches!(
                s.kind,
                astrolabe_core::SymbolKind::Variable
                    | astrolabe_core::SymbolKind::Field
                    | astrolabe_core::SymbolKind::Const
            )
        })
        .map(|s| (s.file, s.start_line))
        .collect();
    let mut ranked: Vec<(u8, usize, &SearchHit)> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let class = if is_test_path(&h.path) {
                2
            } else if !h.text_only
                && index
                    .file_id(&h.path)
                    .is_some_and(|f| defs.contains(&(f, h.line as u32)))
            {
                0
            } else {
                1
            };
            (class, i, h)
        })
        .collect();
    ranked.sort_by_key(|(c, i, _)| (*c, *i));
    ranked.into_iter().map(|(_, _, h)| h).collect()
}

const BROAD_PREVIEW_HITS: usize = 12;

fn name_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            tokens.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase();
        cur.push(c.to_ascii_lowercase());
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Symbols whose names are close to a zero-hit literal query: substring /
/// prefix / shared word tokens / small edit distance. Best first, max `limit`.
fn closest_symbols(
    index: &RepoIndex,
    query: &str,
    path_filter: Option<&str>,
    limit: usize,
) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.len() < 3 || q.len() > 80 {
        return Vec::new();
    }
    let q_tokens = name_tokens(query);
    let mut scored: Vec<(u32, String)> = Vec::new();
    for sym in &index.graph().symbols {
        let name = sym.name.to_lowercase();
        let score = if name == q {
            0
        } else if name.starts_with(&q) || q.starts_with(&name) && name.len() >= 4 {
            1
        } else if name.contains(&q) || q.contains(&name) && name.len() >= 4 {
            2
        } else if edit_distance(&name, &q) <= (q.len() / 5).clamp(1, 2) {
            3
        } else {
            let shared = name_tokens(&sym.name)
                .iter()
                .filter(|t| t.len() >= 3 && q_tokens.contains(t))
                .count();
            if shared >= 1 && shared >= q_tokens.len().min(2) {
                4
            } else {
                continue;
            }
        };
        let Some(file) = index.file(sym.file) else {
            continue;
        };
        if let Some(f) = path_filter {
            if !file.path.as_str().contains(f) {
                continue;
            }
        }
        let line = symbol_line(sym, &file.path);
        let line = match line.rsplit_once("  @") {
            Some((sig, loc)) => format!("{sig}  {loc}"),
            None => line,
        };
        scored.push((score, line));
    }
    scored.sort();
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(limit).map(|(_, l)| l).collect()
}

/// Parent of the first path segment run of a filter: `a/b/c` -> `a/b`.
fn loosened_filter(filter: &str) -> Option<String> {
    let trimmed = filter.trim_end_matches('/');
    trimmed
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
}

/// Role of a hit line. Tests: assertion vs plain reference; code: load/read,
/// definition, or other use. Heuristic, text-based.
fn hit_role(is_test: bool, text: &str) -> &'static str {
    let t = text.to_lowercase();
    let any = |keys: &[&str]| keys.iter().any(|k| t.contains(k));
    if is_test {
        if any(&[
            "assert", "expect", "require.", "require(", "want", "equal", "eq!(", "should", "tobe",
            "toequal", "t.error", "t.fatal", "== ", "!= ", "verify(",
        ]) {
            "断言"
        } else {
            "引用"
        }
    } else if any(&[
        "load",
        "read",
        "open",
        "getenv",
        "environ",
        "env::var",
        "lookup",
        "parse",
        "config",
        "fetch",
        "get(",
        ".get ",
        "unmarshal",
        "decode",
        "from_str",
        "require(",
        "import ",
    ]) {
        "加载"
    } else if any(&[
        "const ", "static ", "#define", " := ", "let ", "var ", "= \"", "=\"", ": \"",
    ]) {
        "定义"
    } else {
        "使用"
    }
}

/// `group=true`: hits grouped per file, production first then tests.
fn render_grouped_hits(index: &RepoIndex, hits: &[SearchHit], limit: usize) -> (String, usize) {
    struct Row {
        line: usize,
        role: &'static str,
        sym: String,
        text: String,
    }
    let mut files: BTreeMap<&str, Vec<Row>> = BTreeMap::new();
    for hit in hits {
        let sym = if hit.text_only {
            None
        } else {
            enclosing_symbol(index, &hit.path, hit.line)
        };
        let sym_name = sym.as_ref().map(|s| s.name.clone()).unwrap_or_default();
        files.entry(hit.path.as_str()).or_default().push(Row {
            line: hit.line,
            role: "",
            sym: sym_name,
            text: hit.text.clone(),
        });
    }
    let mut prod: Vec<(&str, Vec<Row>)> = Vec::new();
    let mut tests: Vec<(&str, Vec<Row>)> = Vec::new();
    for (path, mut rows) in files {
        let file_is_test = crate::explain::is_test_caller(path, "");
        for r in &mut rows {
            let test = file_is_test || crate::explain::is_test_caller(path, &r.sym);
            r.role = hit_role(test, &r.text);
        }
        if file_is_test {
            tests.push((path, rows));
        } else {
            prod.push((path, rows));
        }
    }
    prod.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    tests.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let mut out = format!(
        "分组：{} 处命中，{} 个文件（生产代码 {} 个，测试 {} 个）。命中行标注：测试 [断言]/[引用]，代码 [加载]/[定义]/[使用]（文本启发式，需核对）。\n",
        hits.len(),
        prod.len() + tests.len(),
        prod.len(),
        tests.len()
    );
    let mut shown = 0usize;
    let mut stopped = false;
    for (title, group) in [("生产代码", &prod), ("测试", &tests)] {
        if group.is_empty() {
            continue;
        }
        let n: usize = group.iter().map(|(_, r)| r.len()).sum();
        out.push_str(&format!("## {title}（{n} 处 / {} 个文件）\n", group.len()));
        for (path, rows) in group {
            let mut block = format!("{path} ({})\n", rows.len());
            for r in rows.iter().take(6) {
                let sym = if r.sym.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", r.sym)
                };
                let text: String = r.text.trim().chars().take(140).collect();
                block.push_str(&format!("  L{} [{}]{sym}: {text}\n", r.line, r.role));
            }
            if rows.len() > 6 {
                block.push_str(&format!("  …另有 {} 处\n", rows.len() - 6));
            }
            if estimate_tokens(&out) + estimate_tokens(&block) > limit {
                stopped = true;
                break;
            }
            shown += rows.len().min(6);
            out.push_str(&block);
        }
        if stopped {
            break;
        }
    }
    if stopped {
        out.push_str(
            "…其余文件因 budget_tokens 被省略；用 path_filter 收窄或加大 budget_tokens。\n",
        );
    }
    (out, shown)
}

/// Per-file and per-directory hit counts for a broad query.
fn render_search_summary(hits: &[SearchHit], path_filter: Option<&str>) -> String {
    let mut files: BTreeMap<&str, usize> = BTreeMap::new();
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for hit in hits {
        *files.entry(hit.path.as_str()).or_default() += 1;
        let dir: Vec<&str> = hit.path.split('/').collect();
        let key = if dir.len() > 2 {
            dir[..2].join("/")
        } else if dir.len() == 2 {
            dir[0].to_string()
        } else {
            ".".into()
        };
        *dirs.entry(key).or_default() += 1;
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut dirs: Vec<_> = dirs.into_iter().collect();
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out = format!(
        "宽查询：{} 处命中分布在 {} 个文件。下面只列最相关的几处，不是全集；请用 path_filter 收窄（或换更具体的字面量 / 用 find_symbol），确需逐条时传 page=true（再用 offset 翻页）。\n",
        hits.len(),
        files.len()
    );
    out.push_str("按目录：\n");
    for (dir, n) in dirs.iter().take(10) {
        out.push_str(&format!("  {n:>5}  {dir}/\n"));
    }
    out.push_str("按文件（前 15）：\n");
    for (file, n) in files.iter().take(15) {
        out.push_str(&format!("  {n:>5}  {file}\n"));
    }
    if files.len() > 15 {
        out.push_str(&format!("  …其余 {} 个文件\n", files.len() - 15));
    }
    if let Some((dir, _)) = dirs.first() {
        let suggest = if path_filter.is_some_and(|f| dir.starts_with(f) && dir.len() <= f.len()) {
            files
                .first()
                .map(|(f, _)| f.to_string())
                .unwrap_or_default()
        } else {
            dir.clone()
        };
        out.push_str(&format!("建议：path_filter=\"{suggest}\"\n"));
    }
    out
}

/// Render few hits inside their enclosing symbol bodies. Hits in the same
/// symbol share one body; a body sent earlier this session (`seen` returns
/// the earlier call ordinal) is referenced instead of resent. Hits without a
/// usable symbol get `fallback` lines of context. `None` when it does not fit
/// in `limit` tokens.
fn render_symbol_hits(
    hits: &[SearchHit],
    limit: usize,
    fallback: usize,
    enclosing: &dyn Fn(&SearchHit) -> Option<CodeSymbol>,
    file_lines: &mut dyn FnMut(&SearchHit) -> Option<Arc<Vec<String>>>,
    seen: &mut dyn FnMut(&str, u32, u32) -> Option<u64>,
) -> Option<(String, bool)> {
    let mut out = String::new();
    let mut used_symbol = false;
    let mut i = 0;
    while i < hits.len() {
        let hit = &hits[i];
        let symbol = if hit.text_only { None } else { enclosing(hit) }
            .filter(|s| s.end_line - s.start_line < SMART_BODY_MAX_LINES);
        let Some(symbol) = symbol else {
            let (text, _) = render_search_hits(&hits[i..=i], fallback, usize::MAX, file_lines);
            out.push_str(&text);
            i += 1;
            continue;
        };
        let (start, end) = (symbol.start_line as usize, symbol.end_line as usize);
        let mut j = i + 1;
        while j < hits.len()
            && hits[j].path == hit.path
            && hits[j].line >= start
            && hits[j].line <= end
        {
            j += 1;
        }
        let group = &hits[i..j];
        used_symbol = true;
        let kind = format!("{:?}", symbol.kind).to_lowercase();
        if let Some(call) = seen(&hit.path, symbol.start_line, symbol.end_line) {
            for h in group {
                out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
            }
            out.push_str(&format!(
                "  （所在 {kind} {} {}:{start}-{end} 已在本会话第 {call} 次调用中返回，文件未改动，不再重发；需要时传 dedup=false）\n  签名：{}\n",
                symbol.name,
                hit.path,
                symbol.signature.lines().next().unwrap_or("").trim()
            ));
        } else if let Some(lines) = file_lines(hit) {
            out.push_str(&format!(
                "── {kind} {} · {}:{start}-{end} ──\n",
                symbol.name, hit.path
            ));
            for n in start..=end.min(lines.len()) {
                if let Some(h) = group.iter().find(|h| h.line == n) {
                    out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
                } else {
                    render_context_line(&mut out, n, &lines[n - 1]);
                }
            }
        } else {
            for h in group {
                out.push_str(&format!("@{}:{} {}\n", h.path, h.line, h.text));
            }
        }
        i = j;
    }
    (estimate_tokens(&out) <= limit).then_some((out, used_symbol))
}

fn render_context_line(out: &mut String, line_no: usize, text: &str) {
    let text = text.trim_end();
    let clipped: String = text.chars().take(CONTEXT_LINE_COLUMNS).collect();
    let ellipsis = if clipped.len() < text.len() {
        "…"
    } else {
        ""
    };
    out.push_str(&format!("  {line_no}| {clipped}{ellipsis}\n"));
}

/// Render hits in order within `limit` tokens. Each hit keeps its
/// `@path:line text` anchor line; with `context > 0` the surrounding lines
/// follow as `  N| source`, merged per file so overlapping windows print each
/// line once (a later match inside an earlier window is still printed as its
/// own anchor line). Returns the text and how many hits it covers.
fn render_search_hits(
    hits: &[SearchHit],
    context: usize,
    limit: usize,
    file_lines: &mut dyn FnMut(&SearchHit) -> Option<Arc<Vec<String>>>,
) -> (String, usize) {
    let mut budget = TokenBudget::new(limit);
    let mut out = String::new();
    let mut current: Option<(String, Option<Arc<Vec<String>>>)> = None;
    // Last line (1-based) already printed for the current file.
    let mut printed = 0usize;
    for (i, hit) in hits.iter().enumerate() {
        let anchor = if hit.text_only {
            format!("@{}:{} [text-only] {}\n", hit.path, hit.line, hit.text)
        } else {
            format!("@{}:{} {}\n", hit.path, hit.line, hit.text)
        };
        let same_file = current.as_ref().is_some_and(|(p, _)| *p == hit.path);
        let lines = if context == 0 {
            None
        } else if same_file {
            current.as_ref().and_then(|(_, l)| l.clone())
        } else {
            file_lines(hit)
        };
        let mut chunk = String::new();
        let mut last = if same_file { printed } else { 0 };
        if let Some(lines) = &lines {
            let start = hit.line.saturating_sub(context).max(1).max(last + 1);
            if same_file && start > last + 1 && last > 0 {
                chunk.push_str("  …\n");
            }
            for n in start..hit.line {
                if let Some(text) = lines.get(n - 1) {
                    render_context_line(&mut chunk, n, text);
                }
            }
            chunk.push_str(&anchor);
            // Stop before the next match in this file: it prints its own anchor.
            let next_same = hits
                .get(i + 1)
                .filter(|n| n.path == hit.path)
                .map(|n| n.line);
            let mut end = (hit.line + context).min(lines.len());
            if let Some(next) = next_same {
                end = end.min(next.saturating_sub(1));
            }
            for n in hit.line + 1..=end {
                render_context_line(&mut chunk, n, &lines[n - 1]);
            }
            last = end.max(hit.line);
        } else {
            chunk.push_str(&anchor);
            last = hit.line;
        }
        if !budget.try_add(&chunk) {
            // A window too big for what is left: fall back to the bare line
            // so the page is not cut short by context alone.
            if context > 0 && budget.try_add(&anchor) {
                out.push_str(&anchor);
                current = Some((hit.path.clone(), lines));
                printed = hit.line;
                continue;
            }
            return (out, i);
        }
        out.push_str(&chunk);
        current = Some((hit.path.clone(), lines));
        printed = last;
    }
    (out, hits.len())
}

fn render_search_page_status(
    total: usize,
    offset: usize,
    shown: usize,
    omitted: usize,
    budget_tokens: usize,
    requested_offset: usize,
) -> String {
    let truncated = omitted > 0;
    let mut status = format!(
        "shown={shown} omitted={omitted} truncated={truncated} total={total} offset={offset}\n"
    );
    if requested_offset > total {
        status.push_str(&format!(
            "offset={requested_offset} 超出总数 {total}，本页为空。\n"
        ));
    }
    if shown > 0 && (offset > 0 || truncated) {
        status.push_str(&format!(
            "本页为第 {}–{} 条，共 {total} 条。\n",
            offset + 1,
            offset + shown
        ));
    }
    if truncated {
        let next = offset + shown;
        if shown == 0 {
            status.push_str(&format!(
                "这不是全集：剩余 {omitted} 条中首条结果单项超过 budget_tokens={budget_tokens} 被省略。加大 budget_tokens 或收紧 path_filter 后再查。\n"
            ));
        } else {
            status.push_str(&format!(
                "这不是全集：还有 {omitted} 条未显示。用相同参数加 offset={next} 获取下一页（next_offset={next}）。\n"
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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct QueryParams {
    #[schemars(
        description = "Symbol name, substring, or hierarchical path like 'Class/method', 'Type.method', 'A::b' or 'pkg.Func' ('/Abs/Path' anchors at file top level). Results rank exact name > prefix > substring."
    )]
    pub query: String,
    /// 对展示列表前 5 个命中符号附带源码体（include_body，对齐 Serena）。
    #[serde(default)]
    #[schemars(
        description = "Include source bodies for up to 5 displayed matches (bodies count against budget_tokens). Use this instead of a follow-up Read when you need the implementation of a named function, method or class."
    )]
    pub include_body: bool,
    /// 子串开关。名称查询：省略=子串匹配（精确名优先排序），false=只要精确名，
    /// true=子串；路径查询：最后段子串匹配（省略/false=精确）。
    #[serde(default)]
    #[schemars(
        description = "Name queries: omit = substring match ranked exact>prefix>substring; false = exact name only; true = substring. Path queries: substring-match the last segment (default exact)"
    )]
    pub substring: Option<bool>,
    /// kind 过滤（Function/Method/Class/Struct/Trait/...），名称与路径查询均生效。
    #[serde(default)]
    #[schemars(
        description = "Filter results by SymbolKind name (function, method, class, interface, struct, enum, trait, type, const, module, field)"
    )]
    pub kind: Option<String>,
    /// 仓库相对路径子串过滤（与 search_code 相同语义）。
    #[serde(default)]
    #[schemars(
        description = "Optional case-sensitive substring of the repo-relative path (e.g. src/server). Not a glob. A filter matching no indexed path is an error that lists the closest indexed paths."
    )]
    pub path_filter: Option<String>,
    /// 路径查询：展开匹配者的嵌套子符号层数（0=不展开）。
    #[serde(default)]
    #[schemars(
        description = "Path queries: expand nested children of matches to this depth (0 = matches only)"
    )]
    pub depth: u8,
    #[serde(default = "default_budget")]
    #[schemars(
        description = "Maximum approximate tokens in the rendered result (default 2500). Overflow drops later matches and sets truncated=true; call again with offset=next_offset to fetch the next page."
    )]
    pub budget_tokens: usize,
    /// 0 起始的翻页偏移（与 search_code 一致）。
    #[serde(default)]
    #[schemars(
        description = "0-based index of the first match to return (default 0). When a result is truncated it reports total and next_offset; pass that value here to fetch the next page. Ordering is stable (exact > prefix > substring, then anchor)."
    )]
    pub offset: usize,
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
pub(crate) struct ExplainParams {
    #[schemars(description = "Exact symbol name (case-insensitive fallback).")]
    pub name: String,
    #[serde(default)]
    #[schemars(
        description = "Optional repo-relative path substring to pick among same-named definitions."
    )]
    pub path_filter: Option<String>,
    #[serde(default = "default_budget")]
    #[schemars(description = "Maximum approximate tokens in the result (default 2500).")]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(
        description = "Default true: bodies already returned this session are referenced, not resent."
    )]
    pub dedup: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "List call sites inside test code too (default false: tests are collapsed to a count)."
    )]
    pub include_tests: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Default true: when the language server is already running, callers come from its references (exact)."
    )]
    pub precise: Option<bool>,
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
    #[serde(default)]
    #[schemars(
        description = "Optional. Omitted: literal search is case-insensitive, regex search is case-sensitive. true forces case-sensitive matching; false forces case-insensitive (also for regex)."
    )]
    pub case_sensitive: Option<bool>,
    #[serde(default = "default_budget")]
    #[schemars(
        description = "Maximum approximate tokens in the rendered result (default 2500). Overflow drops later hits and sets truncated=true; call again with offset=next_offset to fetch the next page, or raise this / tighten path_filter."
    )]
    pub budget_tokens: usize,
    #[serde(default)]
    #[schemars(
        description = "0-based index of the first match to return (default 0). When a result is truncated it reports total and next_offset; pass that value here to fetch the next page. Ordering is stable: code hits (index file order) then [text-only] hits."
    )]
    pub offset: usize,
    #[serde(default)]
    #[schemars(
        description = "Lines of surrounding source shown above and below each match (0-10). Omitted: adaptive — 3 when the query has at most 15 matches, 1 up to 60, 0 (bare lines) beyond that. Overlapping windows in one file are merged. Use the context to answer directly instead of a follow-up Read; for a whole function or class use find_symbol include_body=true."
    )]
    pub context_lines: Option<usize>,
    #[serde(default)]
    #[schemars(
        description = "Force a match page even when the query is broad (more than 60 hits). Default false: a broad query returns per-file / per-directory hit counts so you can narrow with path_filter."
    )]
    pub page: bool,
    #[serde(default)]
    #[schemars(
        description = "Group hits per file (production code first, then tests; each hit tagged [断言]/[引用] or [加载]/[定义]/[使用] with its enclosing function). Use it BEFORE editing or renaming a literal, or before asserting on it, to see who loads and who asserts it. Automatic (when omitted) if more than 5 hits span more than 3 files; false forces the plain list."
    )]
    pub group: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Default true: a symbol body already returned earlier in this session (file unchanged) is referenced by location instead of resent. false resends it."
    )]
    pub dedup: Option<bool>,
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
    loop_guard: Arc<Mutex<LoopGuard<CallToolResult>>>,
    tool_router: ToolRouter<Self>,
    /// Code ranges (symbol bodies) already returned in this MCP session,
    /// keyed by root+path+range, with the index generation they were read at
    /// and the ordinal of the call that sent them.
    sent_ranges: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    call_seq: Arc<AtomicU64>,
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
        let loop_guard_config = knobs.loop_guard;
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
            loop_guard: Arc::new(Mutex::new(LoopGuard::new(loop_guard_config))),
            tool_router: Self::tool_router(),
            sent_ranges: Arc::new(Mutex::new(HashMap::new())),
            call_seq: Arc::new(AtomicU64::new(0)),
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
            IndexState::Ready(index) => {
                let result = f(&session, index);
                match index.coverage_note() {
                    Some(note) if result.is_error != Some(true) => append_stale_note(result, &note),
                    _ => result,
                }
            }
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
        if crate::slim::explain_disabled() {
            tools.retain(|tool| tool.name != "explain_symbol");
        }
        if self.context.slim() {
            return crate::slim::slim_catalog(tools);
        }
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

    /// Current index generation for the session `hint` routes to, after the
    /// freshness barrier has absorbed pending edits. `None` when the index is
    /// not ready (the call then just runs and is never counted).
    fn index_stamp(&self, hint: &str) -> Option<u64> {
        let session = self.session_for(hint).ok()?;
        let _ = run_barrier(&session);
        let state = session.state.read().expect("index state lock poisoned");
        match &*state {
            IndexState::Ready(index) => Some(index.generation()),
            _ => None,
        }
    }

    /// Run a search tool through the per-connection [`LoopGuard`].
    ///
    /// Only successful executions are counted; the key includes every
    /// argument and the index generation, so edits reset the count. A blocked
    /// call returns the cached previous result (index unchanged) with a
    /// `blocked: true` marker.
    fn guarded(
        &self,
        tool: &str,
        hint: &str,
        signature: &str,
        run: impl FnOnce() -> CallToolResult,
    ) -> CallToolResult {
        let Some(stamp) = self.index_stamp(hint) else {
            return run();
        };
        let verdict = self
            .loop_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .check(tool, signature, stamp);
        let note = match verdict {
            Verdict::Block { note, cached } => {
                tracing::warn!(tool, "loop guard blocked repeated query");
                return mark_blocked(cached, &note);
            }
            Verdict::Run => None,
            Verdict::RunWithWarning(note) => Some(note),
        };
        let mut result = run();
        if result.is_error != Some(true) {
            // Re-read the stamp: the run itself may have absorbed edits.
            if let Some(after) = self.index_stamp(hint) {
                self.loop_guard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .record(tool, signature, after, result.clone());
            }
        }
        if let Some(note) = note {
            if let Some(first) = result.content.first_mut() {
                if let Some(t) = first.as_text() {
                    *first = ContentBlock::text(format!("{}\n\n{note}", t.text));
                }
            }
        }
        result
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
            "注意：path_filter '{filter}' 范围内有 {ext_summary}属于尚未支持的语言，未入代码索引——已按纯文本一并搜索，同样无匹配；它们没有符号/引用信息。\n"
        ),
        None => format!(
            "注意：仓库内有 {ext_summary}属于尚未支持的语言，未入代码索引——已按纯文本一并搜索，同样无匹配；它们没有符号/引用信息。\n"
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
        description = "主入口：根据编码任务聚合最相关文件、符号、依赖与索引完整性信息。FIRST CALL for any coding task — before Read/Grep. Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart, c#, vb, kotlin, shell, ruby, lua — other source files are invisible to this tool; use Grep/Read for them."
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
        description = "Prefer this over Grep/Read for code. 精确引用查询：由语言服务器解析，图谱的名字匹配不足以支撑改写决策。LSP Unavailable 时请向用户确认后调用 ensure_language_server（默认只出计划）。REQUIRED for rename/rewrite — grep matches text, not bindings. Also the right tool for who-calls / who-uses-X questions: one call returns every call site, instead of search_code on the name and Reading each hit."
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
        description = "Prefer this over Grep/Read for code. 精确定义跳转：语言服务器绑定解析；不要用 find_symbol 代替（那是句法名字匹配）。LSP Unavailable → ask user, then ensure_language_server. The sanctioned way to resolve a definition — not Read/grep."
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
        description = "Prefer this over Grep/Read for code. 符号语义信息（LSP hover：docstring/类型/签名）。LSP Unavailable → ensure_language_server (ask user first). Docstrings without reading the file — pair with find_symbol anchors."
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
        description = "按名称或子串定位符号，返回签名和 path:line 锚点。Prefer this over Grep/Read for code. Results are exhaustive for indexed languages: every symbol matching the name is listed (total/next_offset declare truncation), no need to re-grep the same name. Prefer over Read for locating symbols — no whole-file reads needed. Need the code itself? Pass include_body=true to get the full body of each hit (function, method, class) in the same call instead of search_code + Read. Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart, c#, vb, kotlin, shell, ruby, lua — other source files are invisible to this tool; use Grep/Read for them. Truncation is declared (total/shown range/next_offset); call again with offset=next_offset to page through all matches."
    )]
    pub(crate) fn find_symbol(
        &self,
        Parameters(params): Parameters<QueryParams>,
    ) -> CallToolResult {
        let signature = format!("{params:?}");
        let hint = params.query.clone();
        self.guarded("find_symbol", &hint, &signature, || {
            self.find_symbol_inner(params)
        })
    }

    fn find_symbol_inner(&self, params: QueryParams) -> CallToolResult {
        self.with_fresh_index(&params.query, |session, index| {
            if let Some(message) = path_filter_error(index, params.path_filter.as_deref()) {
                return self.finish_at(&session.root, text_error(message));
            }
            let kind = match params.kind.as_deref() {
                None => None,
                Some(name) => match kind_from_name(name) {
                    Ok(kind) => Some(kind),
                    Err(message) => return self.finish_at(&session.root, text_error(message)),
                },
            };
            let normalized = normalize_symbol_query(&params.query);
            // "Class/method"（及 Type.method / A::b / pkg.Func）路径查询走层级
            // 匹配器（Serena NamePathMatcher 对应物）。
            // A symbol literally named `a::b` / `a.b` (shell `bats::on_failure`,
            // Ruby `self.x`) wins over the hierarchical path reading.
            let literal = params.query.trim();
            let literal_hit = normalized.query != literal
                && !literal.contains('/')
                && index.graph().symbols.iter().any(|s| s.name == literal);
            if normalized.query.contains('/') && !literal_hit {
                let mut params = params.clone();
                params.query = normalized.query;
                return self.find_symbol_by_path(
                    &session.root,
                    index,
                    &params,
                    normalized.ignored.as_deref(),
                );
            }
            let needle = if literal_hit {
                literal.to_lowercase()
            } else {
                normalized.query.to_lowercase()
            };
            // 命中对先收集 (锚点行, symbol 下标)，与展示列表同一排序源——
            // include_body 只对"用户实际看到的" kept 前几个附体（审查 major：
            // 原实现按 graph 构建序取前 5，可能对应被 budget 裁掉的符号）。
            // 排序：精确名 > 前缀 > 子串，同档再按锚点行——避免真正的定义
            // 因按路径排序排在后面而被 budget 截掉。
            let exact_only = params.substring == Some(false);
            let mut ranked: Vec<(u8, String, usize)> = Vec::new();
            for (idx, symbol) in index.graph().symbols.iter().enumerate() {
                let Some(tier) = name_match_tier(&symbol.name, &needle) else {
                    continue;
                };
                if exact_only && tier > 0 {
                    continue;
                }
                if kind.is_some_and(|k| symbol.kind != k) {
                    continue;
                }
                if let Some(file) = index.file(symbol.file) {
                    if params
                        .path_filter
                        .as_deref()
                        .is_some_and(|f| !file.path.as_str().contains(f))
                    {
                        continue;
                    }
                    ranked.push((tier, symbol_line(symbol, &file.path), idx));
                }
            }
            ranked.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            let hits: Vec<(String, usize)> = ranked
                .into_iter()
                .map(|(_, line, idx)| (line, idx))
                .collect();
            let lines: Vec<String> = hits.iter().map(|(line, _)| line.clone()).collect();
            let total = lines.len();
            let offset = params.offset.min(total);
            let (kept, omitted) =
                truncate_ranked(&lines[offset..], params.budget_tokens, |line| format!("{line}\n"));
            let kept_count = kept.len();
            let truncated = omitted > 0;
            let next_offset = truncated.then_some(offset + kept_count);
            let mut body = "confidence: syntactic (符号名称匹配；请用锚点核验)\n".to_string();
            if let Some(ignored) = &normalized.ignored {
                body.push_str(&format!("注意：已忽略查询中的关键字 {ignored}\n"));
            }
            body.push_str(&render_search_page_status(total, offset, kept_count, omitted, params.budget_tokens, params.offset));
            for line in kept {
                body.push_str(line);
                body.push('\n');
            }
            // 零命中时显式声明（对齐 find_references 空结果约定），并跳过
            // include_body 附加，避免误导性的"无可提取的符号体"。
            if hits.is_empty() {
                body.push_str("(未找到匹配的符号)\n");
                body.push_str(&symbol_suggestions(index, &needle));
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
                    &hits[offset..],
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
                    "matches": lines.len(), "total": total, "offset": offset, "next_offset": next_offset,
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
        description = "Prefer this over Grep/Read for code. Results are exhaustive: every match of the literal in the scanned files is counted (total=; truncation is declared), so do not re-grep the same literal. Search repository text for a literal substring (default) or a regex when regex=true. Returns each match as a path:line anchor plus a few surrounding source lines (context_lines; adaptive default, overlapping windows merged), so the answer is usually visible without a follow-up Read. Best for literals (strings, error messages, config keys, routes): set path_filter to the directory you care about. Before editing or renaming a literal, or asserting on it, use group=true (also automatic when more than 5 hits span more than 3 files) to see who loads and who asserts it. For a named function/class body use find_symbol include_body=true; for callers/usages use find_references. Query is literal unless regex=true — A|B without that flag searches for a vertical bar, not an alternation. Case rules: literal is case-insensitive, regex is case-sensitive; pass case_sensitive to override. Covers indexed source (python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart, c#, vb, kotlin, shell, ruby, lua) first, then every other scanned text file (md, sql, yaml, json, xml, gradle, templates …, honoring .gitignore, size and binary limits; lock files skipped); those hits are tagged [text-only] and listed after code hits. Truncation is declared in the result (total/shown range/next_offset); call again with offset=next_offset to page through the rest, or raise budget_tokens / tighten path_filter. Discovery only; edits and renames must be driven by find_references."
    )]
    pub(crate) fn search_code(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> CallToolResult {
        let signature = format!("{params:?}");
        let hint = Self::join_hint(&[params.path_filter.as_deref().unwrap_or(""), &params.query]);
        self.guarded("search_code", &hint, &signature, || {
            self.search_code_inner(params)
        })
    }

    fn search_code_inner(&self, params: SearchParams) -> CallToolResult {
        self.with_fresh_index(
            &Self::join_hint(&[params.path_filter.as_deref().unwrap_or(""), &params.query]),
            |session, index| {
                // A filter that only hits non-code text files (docs/, *.toml …)
                // is valid here: those files are searched as [text-only].
                if index
                    .text_only_paths(params.path_filter.as_deref())
                    .is_empty()
                {
                    if let Some(message) = path_filter_error(index, params.path_filter.as_deref()) {
                        return text_error(message);
                    }
                }
                let matcher = match build_search_matcher(
                    &params.query,
                    params.regex,
                    params.case_sensitive,
                ) {
                    Ok(matcher) => matcher,
                    Err(error) => return text_error(format!("无效正则表达式：{error}")),
                };
                if session_dedup_enabled(params.dedup) {
                    let mut probe = params.clone();
                    probe.dedup = None;
                    let key = format!("{}|search|{probe:?}", session.root.display());
                    let generation = index.generation();
                    // Same ordinal the body bookkeeping below takes.
                    let call = self.call_seq.load(Ordering::Relaxed) + 1;
                    let earlier = {
                        let mut map = self.sent_ranges.lock().unwrap_or_else(|e| e.into_inner());
                        match map.get(&key) {
                            Some((g, c)) if *g == generation => Some(*c),
                            _ => {
                                map.insert(key, (generation, call));
                                None
                            }
                        }
                    };
                    if let Some(c) = earlier {
                        let body = format!(
                            "confidence: exact (磁盘源码字面匹配)\n{}同一查询已在本会话第 {c} 次调用返回，文件未改动，不再重发；需要时传 dedup=false。\n",
                            render_search_mode_header(params.regex, &params.query)
                        );
                        return self.result_on(
                            &session.root,
                            "search_code",
                            body,
                            json!({"confidence":"exact","deduplicated":true,"earlier_call":c,"budget_tokens":params.budget_tokens}),
                            Some(params.budget_tokens),
                        );
                    }
                }
                let mut matches: Vec<SearchHit> = Vec::new();
                let mut hits = 0u64;
                let mut misses = 0u64;
                let mut scan_file = |path: &str, matches: &mut Vec<SearchHit>| {
                    let Some((lines, hit)) = index.cached_lines(&session.file_cache, path) else {
                        return;
                    };
                    if hit {
                        hits += 1;
                    } else {
                        misses += 1;
                    }
                    for (line_no, line) in lines.iter().enumerate() {
                        if matcher.is_match(line) {
                            // P0：超长行先做单行安全切片（Unicode 边界安全），
                            // 单条 match 不会把 budget 一口气吃光，也不会在
                            // 大小写折叠差异上 panic。
                            matches.push(SearchHit {
                                path: path.to_string(),
                                line: line_no + 1,
                                text: clip_match_line(line, &params.query, Some(&matcher)),
                                text_only: false,
                            });
                        }
                    }
                };
                for file in &index.graph().files {
                    if params
                        .path_filter
                        .as_ref()
                        .is_some_and(|filter| !file.path.as_str().contains(filter))
                    {
                        continue;
                    }
                    scan_file(file.path.as_str(), &mut matches);
                }
                // 非索引文本文件（md/sql/yaml/json/xml/gradle/kt/模板…）也搜，
                // 排在代码命中之后并标注 [text-only]：它们没有符号/引用信息。
                // ripgrep 引擎（grep-searcher）并行搜索，按输入顺序回收结果。
                let code_matches = matches.len();
                let text_paths = index.text_only_paths(params.path_filter.as_deref());
                let sensitive = params.case_sensitive.unwrap_or(params.regex);
                if let Ok(text_matcher) = astrolabe_core::text_search::build_matcher(
                    &params.query,
                    params.regex,
                    sensitive,
                ) {
                    let rels: Vec<&str> = text_paths.iter().map(|p| p.as_str()).collect();
                    let found = astrolabe_core::text_search::search_files(
                        &session.root,
                        &rels,
                        &text_matcher,
                        TEXT_SEARCH_MAX_FILE_BYTES,
                    );
                    for (idx, file_hits) in found {
                        for hit in file_hits {
                            matches.push(SearchHit {
                                path: rels[idx].to_string(),
                                line: hit.line as usize,
                                text: clip_match_line(&hit.text, &params.query, Some(&matcher)),
                                text_only: true,
                            });
                        }
                    }
                }
                let text_matches = matches.len() - code_matches;
                let data_note = if params.regex {
                    String::new()
                } else {
                    params
                        .path_filter
                        .as_deref()
                        .map(|filter| data_hits_outside(index, &session.root, filter, &params.query, sensitive))
                        .unwrap_or_default()
                };
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
                let total = matches.len();
                let offset = params.offset.min(total);
                let auto_group = params.group.is_none()
                    && params.context_lines.is_none()
                    && !params.page
                    && params.offset == 0
                    && total > smart_body_max_hits()
                    && total <= broad_hits()
                    && matches
                        .iter()
                        .map(|m| m.path.as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        > 3;
                if (params.group == Some(true) || auto_group) && total > 0 {
                    let (grouped, shown) = render_grouped_hits(
                        index,
                        &matches,
                        clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS),
                    );
                    let mut body = format!(
                        "confidence: exact (磁盘源码字面匹配)\n{}",
                        render_search_mode_header(params.regex, &params.query)
                    );
                    body.push_str(&format!(
                        "grouped=true total={total} shown={shown}\n"
                    ));
                    body.push_str(&grouped);
                    body.push_str(&data_note);
                    return self.result_on(
                        &session.root,
                        "search_code",
                        body,
                        json!({
                            "confidence": "exact",
                            "grouped": true,
                            "total": total,
                            "shown": shown,
                            "budget_tokens": params.budget_tokens,
                        }),
                        Some(params.budget_tokens),
                    );
                }
                let broad = total > broad_hits()
                    && params.offset == 0
                    && !params.page
                    && params.context_lines.is_none();
                if broad {
                    let mut body = format!(
                        "confidence: exact (磁盘源码字面匹配)\n{}",
                        render_search_mode_header(params.regex, &params.query)
                    );
                    let ranked = rank_broad_hits(index, &matches);
                    let preview_budget = clamped_budget.saturating_sub(SEARCH_HEADER_TOKENS) / 2;
                    let mut preview = String::new();
                    let mut shown_preview = 0;
                    for hit in ranked.iter().take(BROAD_PREVIEW_HITS) {
                        let line = format!("@{}:{} {}\n", hit.path, hit.line, hit.text);
                        if estimate_tokens(&preview) + estimate_tokens(&line) > preview_budget {
                            break;
                        }
                        preview.push_str(&line);
                        shown_preview += 1;
                    }
                    body.push_str(&format!(
                        "shown={shown_preview} omitted={} truncated=true total={total} offset=0 summary=true\n",
                        total - shown_preview
                    ));
                    body.push_str(&render_search_summary(&matches, params.path_filter.as_deref()));
                    if shown_preview > 0 {
                        body.push_str(&format!(
                            "最相关的 {shown_preview} 处（定义行优先，测试文件靠后；不是全集）：\n{preview}"
                        ));
                    }
                    body.push_str(&data_note);
                    return self.result_on(
                        &session.root,
                        "search_code",
                        body,
                        json!({
                            "confidence": "exact",
                            "summary": true,
                            "preview_shown": shown_preview,
                            "total": total,
                            "truncated": true,
                            "next_offset": 0,
                            "budget_tokens": params.budget_tokens,
                        }),
                        Some(params.budget_tokens),
                    );
                }
                let context = search_context_lines(params.context_lines, total);
                let mut file_lines = |hit: &SearchHit| -> Option<Arc<Vec<String>>> {
                    if hit.text_only {
                        let text =
                            std::fs::read_to_string(session.root.join(&hit.path)).ok()?;
                        Some(Arc::new(text.lines().map(str::to_string).collect()))
                    } else {
                        index
                            .cached_lines(&session.file_cache, &hit.path)
                            .map(|(lines, _)| lines)
                    }
                };
                let smart = params.context_lines.is_none()
                    && offset == 0
                    && total > 0
                    && total <= smart_body_max_hits();
                let dedup = session_dedup_enabled(params.dedup);
                let generation = index.generation();
                let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
                let root_key = session.root.display().to_string();
                let mut pending: Vec<(String, u32, u32)> = Vec::new();
                let mut seen = |path: &str, start: u32, end: u32| -> Option<u64> {
                    let key = format!("{root_key}|{path}:{start}-{end}");
                    let earlier = self.lookup_sent(&key, dedup, generation);
                    if earlier.is_none() {
                        pending.push((path.to_string(), start, end));
                    }
                    earlier
                };
                let enclosing = |hit: &SearchHit| enclosing_symbol(index, &hit.path, hit.line);
                let smart_rendered = if smart {
                    render_symbol_hits(
                        &matches,
                        remaining,
                        context,
                        &enclosing,
                        &mut file_lines,
                        &mut seen,
                    )
                } else {
                    None
                };
                // Only ranges whose body actually shipped count as sent (a
                // render that overflowed the budget falls back to line context).
                if smart_rendered.is_some() {
                    for (path, start, end) in &pending {
                        self.commit_sent(
                            &session.root,
                            path,
                            (*start, *end),
                            generation,
                            call,
                        );
                    }
                }
                let bodies = smart_rendered.as_ref().is_some_and(|(_, used)| *used);
                let (rendered, shown) = match smart_rendered {
                    Some((text, _)) => (text, total),
                    None => render_search_hits(&matches[offset..], context, remaining, &mut file_lines),
                };
                let omitted = total - offset - shown;
                let truncated = omitted > 0;
                let next_offset = truncated.then_some(offset + shown);
                let mode = if params.regex { "regex" } else { "literal" };
                let mut body = format!(
                    "confidence: exact (磁盘源码字面匹配)\n{}",
                    render_search_mode_header(params.regex, &params.query)
                );
                body.push_str(&render_search_page_status(
                    total,
                    offset,
                    shown,
                    omitted,
                    clamped_budget,
                    params.offset,
                ));
                if bodies {
                    body.push_str("上下文：命中所在的函数/类型整体给出（`── kind 名字 · 路径:起-止 ──`，`  行号| 源码`，命中行为 `@路径:行`），无需再 Read。\n");
                } else if context > 0 && shown > 0 {
                    body.push_str(&format!(
                        "上下文：每处命中前后 {context} 行（`  行号| 源码`），同一文件内重叠窗口已合并。\n"
                    ));
                }
                body.push_str(&rendered);
                body.push_str(&data_note);
                // 零命中时显式声明，区分"正常无匹配"与"服务故障/吞内容"
                // （对齐 find_references 的 `未找到对 … 的引用。` 空结果约定）。
                let mut unindexed_hint = false;
                if matches.is_empty() {
                    body.push_str("(未找到匹配的代码行)\n");
                    let near = if params.regex {
                        Vec::new()
                    } else {
                        closest_symbols(index, &params.query, params.path_filter.as_deref(), 8)
                    };
                    if !near.is_empty() {
                        body.push_str("最接近的符号名（大小写/前缀/相近拼写）：\n");
                        for line in &near {
                            body.push_str(&format!("  {line}\n"));
                        }
                    }
                    if let Some(filter) = params.path_filter.as_deref() {
                        let wider = loosened_filter(filter)
                            .map(|p| format!("放宽到 path_filter=\"{p}\" 或"))
                            .unwrap_or_default();
                        body.push_str(&format!(
                            "提示：path_filter=\"{filter}\" 可能过窄，{wider}去掉后再试；也可缩短查询词。\n"
                        ));
                    } else if near.is_empty() {
                        body.push_str("提示：换更短的字面量，或用 find_symbol 按名字查。\n");
                    }
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
                        "total": total,
                        "offset": offset,
                        "next_offset": next_offset,
                        "shown": shown,
                        "omitted": omitted,
                        "truncated": truncated,
                        "budget_tokens": params.budget_tokens,
                        "unindexed_hint": unindexed_hint,
                        "text_only_matches": text_matches,
                        "context_lines": context,
                        "bodies": bodies,
                        "text_only_files_searched": text_paths.len()
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
        description = "Prefer this over Grep/Read for code. One call for a named function/method/type: its definition body (up to 3 same-named definitions, receiver/type-qualified) plus every call site as path:line with the calling function and the code line. Non-test call sites first; test call sites are collapsed to a count unless include_tests=true. Call sites come from the language server when it is already running (exact), otherwise from the syntactic call graph filtered by call form (method vs function). Use instead of find_symbol + search_code + Read; for the precise set before an edit use find_references."
    )]
    pub(crate) fn explain_symbol(
        &self,
        Parameters(params): Parameters<ExplainParams>,
    ) -> CallToolResult {
        self.with_fresh_index(&params.name, |session, index| {
            let budget = clamp_budget(params.budget_tokens);
            let in_filter = |symbol: &CodeSymbol| {
                params.path_filter.as_deref().is_none_or(|filter| {
                    index
                        .file(symbol.file)
                        .is_some_and(|file| file.path.as_str().contains(filter))
                })
            };
            let symbols = &index.graph().symbols;
            let mut defs: Vec<&CodeSymbol> = symbols
                .iter()
                .filter(|s| s.name == params.name && in_filter(s))
                .collect();
            if defs.is_empty() {
                defs = symbols
                    .iter()
                    .filter(|s| s.name.eq_ignore_ascii_case(&params.name) && in_filter(s))
                    .collect();
            }
            if defs.is_empty() {
                return self.result_on(
                    &session.root,
                    "explain_symbol",
                    format!("未找到名为 {} 的定义。可用 find_symbol 做子串查找，或 search_code 查字面量。\n", params.name),
                    json!({"definitions": 0}),
                    Some(params.budget_tokens),
                );
            }
            // Prefer exported, larger definitions (implementations over decls).
            defs.sort_by_key(|s| (!s.exported, std::cmp::Reverse(s.end_line - s.start_line)));
            let total_defs = defs.len();
            defs.truncate(3);
            let dedup = session_dedup_enabled(params.dedup);
            let generation = index.generation();
            let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let root_key = session.root.display().to_string();
            let mut body = String::from("confidence: definitions exact; callers syntactic (名字匹配)\n");
            if total_defs > 3 {
                body.push_str(&format!("同名定义 {total_defs} 个，显示前 3 个；用 path_filter 指定。\n"));
            }
            let body_budget = budget.saturating_sub(SEARCH_HEADER_TOKENS) * 3 / 4;
            for def in &defs {
                let Some(file) = index.file(def.file) else { continue };
                let path = file.path.as_str();
                let kind = format!("{:?}", def.kind).to_lowercase();
                let same_file: Vec<&CodeSymbol> = symbols.iter().filter(|s| s.file == def.file).collect();
                body.push_str(&format!(
                    "── {kind} {} · {path}:{}-{} ──\n",
                    crate::explain::qualified_name(def, &same_file),
                    def.start_line,
                    def.end_line
                ));
                let key = format!("{root_key}|{path}:{}-{}", def.start_line, def.end_line);
                let earlier = self.lookup_sent(&key, dedup, generation);
                if let Some(c) = earlier {
                    body.push_str(&format!(
                        "  （本会话第 {c} 次调用已返回此定义，文件未改动，不再重发；需要时传 dedup=false）\n  签名：{}\n",
                        def.signature.lines().next().unwrap_or("").trim()
                    ));
                    continue;
                }
                match astrolabe_core::body::symbol_body(&session.root.join(path), def) {
                    Ok(text) => {
                        let used = estimate_tokens(&body);
                        let fits = used + estimate_tokens(&text) <= body_budget.max(used + 1)
                            || body_budget == 0;
                        if fits {
                            self.commit_sent(
                                &session.root,
                                path,
                                (def.start_line, def.end_line),
                                generation,
                                call,
                            );
                        }
                        if used + estimate_tokens(&text) > body_budget.max(used + 1) && body_budget > 0 {
                            let keep = body_budget.saturating_sub(used) * 4;
                            let mut cut: String = text.chars().take(keep).collect();
                            cut.push_str("\n…(定义体超出 budget_tokens 已截断；加大 budget_tokens 查看全部)\n");
                            body.push_str(&cut);
                        } else {
                            body.push_str(&text);
                            if !text.ends_with('\n') {
                                body.push('\n');
                            }
                        }
                    }
                    Err(err) => body.push_str(&format!("  (读取定义体失败：{err})\n")),
                }
            }
            let (sites, lsp_used) = explain_call_sites(session, index, &defs, &params);
            if lsp_used {
                body = body.replacen("callers syntactic (名字匹配)", "callers exact (语言服务器引用)", 1);
            }
            let include_tests = params.include_tests.unwrap_or(false);
            let (tests, code): (Vec<_>, Vec<_>) = sites.into_iter().partition(|s| s.test);
            let code_fns: BTreeSet<_> = code.iter().map(|s| (s.path.clone(), s.caller.clone())).collect();
            let test_fns: BTreeSet<_> = tests.iter().map(|s| (s.path.clone(), s.caller.clone())).collect();
            let how = if lsp_used { "语言服务器引用" } else { "名字匹配 + 调用形式过滤" };
            body.push_str(&format!(
                "调用点（{how}，非测试 {} 处 / {} 个函数）：\n",
                code.len(),
                code_fns.len()
            ));
            let shown: Vec<&ExplainSite> = if include_tests {
                code.iter().chain(tests.iter()).collect()
            } else {
                code.iter().collect()
            };
            let lines: Vec<String> = shown.iter().map(|s| s.render(defs.len() > 1)).collect();
            let remaining = budget.saturating_sub(estimate_tokens(&body)).saturating_sub(40);
            let cap = lines.len().min(40);
            let (kept, omitted) = truncate_ranked(&lines[..cap], remaining, |l| format!("{l}\n"));
            for line in kept {
                body.push_str("  ");
                body.push_str(line);
                body.push('\n');
            }
            let hidden = omitted + lines.len().saturating_sub(cap);
            if hidden > 0 {
                body.push_str(&format!("  …另有 {hidden} 处；完整/精确集合用 find_references。\n"));
            } else if code.is_empty() && (tests.is_empty() || !include_tests) {
                body.push_str("  (未找到非测试调用点；动态分派/反射调用请用 find_references 核验)\n");
            }
            if !tests.is_empty() && !include_tests {
                body.push_str(&format!(
                    "测试调用 {} 处 / {} 个测试函数（已折叠；include_tests=true 展开）\n",
                    tests.len(),
                    test_fns.len()
                ));
            }
            self.result_on(
                &session.root,
                "explain_symbol",
                body,
                json!({
                    "definitions": total_defs,
                    "call_sites": code.len(),
                    "callers": code_fns.len(),
                    "test_call_sites": tests.len(),
                    "precise": lsp_used,
                    "budget_tokens": params.budget_tokens
                }),
                Some(params.budget_tokens),
            )
        })
    }

    #[tool(
        description = "Prefer this over Grep/Read for code. 追踪名字匹配得到的调用者/被调用者；结果是 syntactic，必须核验动态分派。For the precise set use find_references."
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

    /// Earlier call ordinal if this exact range was already returned this
    /// session at the same index generation (and dedup is on).
    fn lookup_sent(&self, key: &str, dedup: bool, generation: u64) -> Option<u64> {
        let map = self.sent_ranges.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(key) {
            Some((g, c)) if dedup && *g == generation => Some(*c),
            _ => None,
        }
    }

    /// Remember a range as fully returned (dedup table + opt-in served log).
    fn commit_sent(&self, root: &Path, path: &str, range: (u32, u32), generation: u64, call: u64) {
        let key = format!("{}|{path}:{}-{}", root.display(), range.0, range.1);
        self.sent_ranges
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, (generation, call));
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
        let dedup = session_dedup_enabled(None);
        let generation = index.generation();
        let call = self.call_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let root_key = root.display().to_string();
        for (line, idx) in hits.iter().take(kept_count).take(5) {
            let symbol = &index.graph().symbols[*idx];
            let Some(file) = index.file(symbol.file) else {
                failed += 1;
                continue;
            };
            let key = format!(
                "{root_key}|{}:{}-{}",
                file.path.as_str(),
                symbol.start_line,
                symbol.end_line
            );
            let earlier = self.lookup_sent(&key, dedup, generation);
            if let Some(c) = earlier {
                body.push_str(&format!(
                    "\n----- body @ {line} -----\n（本会话第 {c} 次调用已返回此定义体，文件未改动，不再重发）\n"
                ));
                attached += 1;
                continue;
            }
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
                    self.commit_sent(
                        root,
                        file.path.as_str(),
                        (symbol.start_line, symbol.end_line),
                        generation,
                        call,
                    );
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
        ignored: Option<&str>,
    ) -> CallToolResult {
        let kind = match params.kind.as_deref() {
            None => None,
            Some(name) => match kind_from_name(name) {
                Ok(kind) => Some(kind),
                Err(message) => return self.finish_at(root, text_error(message)),
            },
        };
        let symbols = &index.graph().symbols;
        let mut matches = astrolabe_core::name_path::match_name_path(
            symbols,
            &params.query,
            params.substring.unwrap_or(false),
            astrolabe_core::name_path::KindFilter(kind),
            params.depth,
        );
        // 行范围嵌套推不出的层级（Go 方法挂在接收者类型上、pkg.Func 的包是
        // 目录）：最后一段精确名 + 父段出现在签名词或路径段中。
        let mut receiver_fallback = false;
        if matches.is_empty() && params.depth == 0 {
            let segments: Vec<&str> = params
                .query
                .split('/')
                .filter(|seg| !seg.is_empty())
                .collect();
            if let [.., parent, last] = segments.as_slice() {
                let mut impl_sources = std::collections::HashMap::new();
                for symbol in symbols {
                    if !symbol.name.eq_ignore_ascii_case(last)
                        || kind.is_some_and(|k| symbol.kind != k)
                    {
                        continue;
                    }
                    let Some(file) = index.file(symbol.file) else {
                        continue;
                    };
                    if signature_mentions(&symbol.signature, parent)
                        || path_has_component(file.path.as_str(), parent)
                        || enclosing_impl_mentions(
                            &mut impl_sources,
                            root,
                            file.path.as_str(),
                            symbol.start_line,
                            parent,
                        )
                    {
                        matches.push(astrolabe_core::name_path::NamePathMatch {
                            symbol: symbol.id,
                            depth: 0,
                        });
                    }
                }
                receiver_fallback = !matches.is_empty();
            }
        }
        // 命中对先收集 (锚点行, symbol 下标)，与名称分支同一 include_body 排序源。
        let mut hits: Vec<(String, usize)> = Vec::new();
        for m in &matches {
            if let Some((idx, symbol)) = symbols.iter().enumerate().find(|(_, s)| s.id == m.symbol)
            {
                if let Some(file) = index.file(symbol.file) {
                    if params
                        .path_filter
                        .as_deref()
                        .is_some_and(|f| !file.path.as_str().contains(f))
                    {
                        continue;
                    }
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
        let total = lines.len();
        let offset = params.offset.min(total);
        let (kept, omitted) = truncate_ranked(&lines[offset..], params.budget_tokens, |line| {
            format!("{line}\n")
        });
        let kept_count = kept.len();
        let truncated = omitted > 0;
        let next_offset = truncated.then_some(offset + kept_count);
        let mut body =
            "confidence: syntactic (层级路径匹配：行范围嵌套推断，语法容器如 impl 会形成中间层；请核验)\n"
                .to_string();
        if let Some(ignored) = ignored {
            body.push_str(&format!("注意：已忽略查询中的关键字 {ignored}\n"));
        }
        if receiver_fallback {
            body.push_str("注意：层级由接收者类型/签名/路径段推断（非行范围嵌套）\n");
        }
        body.push_str(&render_search_page_status(
            total,
            offset,
            kept_count,
            omitted,
            params.budget_tokens,
            params.offset,
        ));
        for line in kept {
            body.push_str(line);
            body.push('\n');
        }
        // 零命中时显式声明，避免只剩 confidence 元信息的歧义输出。
        if hits.is_empty() {
            body.push_str("(未找到匹配的符号)\n");
            let last = params
                .query
                .rsplit('/')
                .find(|seg| !seg.is_empty())
                .unwrap_or_default()
                .to_lowercase();
            body.push_str(&symbol_suggestions(index, &last));
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
                &hits[offset..],
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
                "matches": lines.len(), "total": total, "offset": offset, "next_offset": next_offset,
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
                "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, visualbasic, kotlin, shell, ruby, lua\n\
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
            body.push_str(&index.coverage_detail(50));
            let unindexed = index.unindexed_code_exts(None);
            let excluded = index.excluded_source_paths();
            if !excluded.is_empty() {
                let sample = excluded
                    .iter()
                    .take(3)
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(
                    "build-output-excluded source files: {} (e.g. {sample}; 被构建输出排除规则排除，search_code/find_symbol 对其不可见，请用 Grep/Read)\n",
                    excluded.len()
                ));
            }
            if unindexed.is_empty() && excluded.is_empty() {
                body.push_str("unindexed source files: none\n");
            } else if unindexed.is_empty() {
                body.push_str("unindexed source files: none in unsupported languages\n");
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
        .with_instructions(if self.context.slim() {
            crate::slim::slim_instructions(&self.root)
        } else {
            crate::instructions::connection_instructions(&self.context.name, &self.root)
        })
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
/// Size cap for text-only search, same as the scanner's default admission
/// cap (files that grew past it after the scan are skipped).
const TEXT_SEARCH_MAX_FILE_BYTES: u64 = 1_500_000;

/// Compile the single matcher used by `search_code`.
///
/// Literal queries are escaped. Default case rules: literal is
/// case-insensitive, regex is case-sensitive; `case_sensitive` overrides
/// both.
fn build_search_matcher(
    query: &str,
    regex: bool,
    case_sensitive: Option<bool>,
) -> Result<regex::Regex, regex::Error> {
    let pattern = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let sensitive = case_sensitive.unwrap_or(regex);
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!sensitive)
        .build()
}

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

/// find_symbol 查询归一化结果。
struct NormalizedQuery {
    query: String,
    /// 被剥掉的语言关键字（如 `struct`），用于在结果中提示。
    ignored: Option<String>,
}

const QUERY_KEYWORDS: &[&str] = &[
    "struct",
    "class",
    "func",
    "fn",
    "def",
    "function",
    "method",
    "interface",
    "enum",
    "trait",
    "type",
    "impl",
    "record",
];

/// 剥掉 `Proc struct` / `func Foo` 中的关键字，并把 `::`、`.` 视作层级
/// 分隔符（`Type.method`、`A::b`、`pkg.Func` → `Type/method`）。
fn normalize_symbol_query(raw: &str) -> NormalizedQuery {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut ignored = Vec::new();
    let mut kept = Vec::new();
    if tokens.len() > 1 {
        for token in &tokens {
            if QUERY_KEYWORDS.contains(&token.to_ascii_lowercase().as_str()) {
                ignored.push(*token);
            } else {
                kept.push(*token);
            }
        }
        if kept.is_empty() {
            kept = tokens.clone();
            ignored.clear();
        }
    } else {
        kept = tokens.clone();
    }
    let mut query = kept.join(" ");
    if !query.contains('/') && (query.contains("::") || query.contains('.')) {
        let leading = query.starts_with("::");
        let replaced: Vec<&str> = query
            .split("::")
            .flat_map(|part| part.split('.'))
            .filter(|seg| !seg.is_empty())
            .collect();
        if replaced.len() > 1 {
            query = format!("{}{}", if leading { "/" } else { "" }, replaced.join("/"));
        }
    }
    NormalizedQuery {
        query,
        ignored: (!ignored.is_empty()).then(|| format!("`{}`", ignored.join(" "))),
    }
}

/// 名称匹配档位：0 精确、1 前缀、2 子串（needle 已小写）。
fn name_match_tier(name: &str, needle: &str) -> Option<u8> {
    let lower = name.to_lowercase();
    if lower == needle {
        Some(0)
    } else if lower.starts_with(needle) {
        Some(1)
    } else if lower.contains(needle) {
        Some(2)
    } else {
        None
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// 签名中是否以完整标识符出现 `word`（Go 接收者 `(c *Command)` 等）。
fn signature_mentions(signature: &str, word: &str) -> bool {
    signature
        .split(|c: char| !is_ident_char(c))
        .any(|token| token == word)
}

/// 路径中是否有目录段或文件名主干等于 `component`（pkg.Func 的 pkg）。
fn path_has_component(path: &str, component: &str) -> bool {
    path.split('/')
        .any(|seg| seg == component || seg.split('.').next().is_some_and(|stem| stem == component))
}

/// Rust `impl Type { fn m() }` / `impl Trait for Type`：向上找最近的
/// 顶格 `impl` 行，判断它是否提到父段名（impl 块本身不是索引符号）。
fn enclosing_impl_mentions(
    cache: &mut std::collections::HashMap<String, Option<String>>,
    root: &Path,
    path: &str,
    start_line: u32,
    parent: &str,
) -> bool {
    if !path.ends_with(".rs") {
        return false;
    }
    let source = cache
        .entry(path.to_string())
        .or_insert_with(|| std::fs::read_to_string(root.join(path)).ok());
    let Some(source) = source else {
        return false;
    };
    let before = usize::try_from(start_line.saturating_sub(1)).unwrap_or(0);
    source
        .lines()
        .take(before)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("impl") && trimmed.len() == line.len()
        })
        .is_some_and(|line| signature_mentions(line, parent))
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// 零命中时给出相近符号名（编辑距离 ≤2 或共享 4 字符前缀），最多 5 个。
fn symbol_suggestions(index: &RepoIndex, needle: &str) -> String {
    if needle.is_empty() {
        return String::new();
    }
    let prefix: String = needle.chars().take(4).collect();
    let mut scored: BTreeMap<(usize, String), ()> = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    for symbol in &index.graph().symbols {
        let lower = symbol.name.to_lowercase();
        if !seen.insert(lower.clone()) {
            continue;
        }
        let distance = edit_distance(&lower, needle);
        if distance <= 2 || (prefix.chars().count() >= 4 && lower.starts_with(&prefix)) {
            scored.insert((distance, symbol.name.clone()), ());
        }
    }
    if scored.is_empty() {
        return "建议：确认名字拼写；带限定的写法可用 Type/method、Type.method 或 A::b；也可改用 search_code 做全文检索。\n".to_string();
    }
    let names: Vec<String> = scored.into_keys().take(5).map(|(_, name)| name).collect();
    format!(
        "相近的符号名：{}（可直接用这些名字重查，或改用 search_code）\n",
        names.join(", ")
    )
}

/// path_filter 不匹配任何已索引路径（也不匹配未入索引的代码文件）时返回
/// 显式错误，列出最接近的已索引路径，而不是静默返回 0 条。
fn path_filter_error(index: &RepoIndex, filter: Option<&str>) -> Option<String> {
    let filter = filter.filter(|f| !f.is_empty())?;
    let files = &index.graph().files;
    if files.iter().any(|file| file.path.as_str().contains(filter)) {
        return None;
    }
    if !index.unindexed_code_exts(Some(filter)).is_empty() {
        return None;
    }
    let cleaned = filter.trim_start_matches("./").trim_start_matches('/');
    let segments: Vec<String> = cleaned
        .split('/')
        .filter(|seg| !seg.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut candidates: BTreeMap<String, ()> = BTreeMap::new();
    for file in files {
        let path = file.path.as_str();
        candidates.insert(path.to_string(), ());
        let mut dir = path;
        while let Some((parent, _)) = dir.rsplit_once('/') {
            candidates.insert(format!("{parent}/"), ());
            dir = parent;
        }
    }
    let mut scored: Vec<(usize, usize, usize, String)> = candidates
        .into_keys()
        .map(|candidate| {
            let lower = candidate.to_lowercase();
            let hits = segments
                .iter()
                .filter(|seg| lower.contains(seg.as_str()))
                .count();
            let tail = lower
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string();
            let distance = segments
                .last()
                .map_or(usize::MAX, |seg| edit_distance(&tail, seg));
            (usize::MAX - hits, distance, candidate.len(), candidate)
        })
        .collect();
    scored.sort();
    let closest: Vec<String> = scored.into_iter().take(5).map(|entry| entry.3).collect();
    let mut message = format!(
        "path_filter '{filter}' 不匹配任何已索引路径（区分大小写的仓库相对路径子串，不是 glob，不要带绝对路径或 ./ 前缀）。"
    );
    if !closest.is_empty() {
        message.push_str(&format!("最接近的已索引路径：{}", closest.join(", ")));
    }
    Some(message)
}

/// Prepend the loop-guard note to a cached result and flag it `blocked: true`.
fn mark_blocked(mut result: CallToolResult, note: &str) -> CallToolResult {
    match result.content.first_mut() {
        Some(first) if first.as_text().is_some() => {
            let text = first.as_text().map(|t| t.text.clone()).unwrap_or_default();
            *first = ContentBlock::text(format!("{note}\n\n--- 上一次执行结果 ---\n{text}"));
        }
        _ => result
            .content
            .insert(0, ContentBlock::text(note.to_string())),
    }
    if let Some(Value::Object(obj)) = result.structured_content.as_mut() {
        obj.insert("blocked".into(), json!(true));
    }
    result
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 100,
            offset: 0,
        })));
        assert_first_text(server.search_code(Parameters(SearchParams {
            query: "definitely absent".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 100,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 0,
            offset: 0,
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
            case_sensitive: None,
            budget_tokens: 2_500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            case_sensitive: None,
            budget_tokens: 1,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("shown=0"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(
            text.contains("首条结果单项超过 budget_tokens=1 被省略"),
            "{text}"
        );
        assert!(text.contains("加大 budget_tokens"), "{text}");
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

    fn smart_server(files: &[(&str, String)]) -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    fn search(query: &str) -> SearchParams {
        SearchParams {
            query: query.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: None,
            page: false,
            group: None,
            dedup: None,
        }
    }

    fn text_of(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    const CALC_PY: &str = "def compute(a, b):\n    total = a + b\n    total *= 2\n    marker_x = total - 1\n    return marker_x\n\n\ndef caller():\n    return compute(1, 2)\n";

    #[test]
    fn search_code_few_hits_expand_to_enclosing_symbol_once() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let text = text_of(&server.search_code(Parameters(search("marker_x"))));
        // Two hits in one function share a single body with both anchors.
        assert_eq!(
            text.matches("── function compute · calc.py:1-5 ──").count(),
            1,
            "{text}"
        );
        assert!(text.contains("  1| def compute(a, b):"), "{text}");
        assert!(text.contains("@calc.py:4 marker_x = total - 1"), "{text}");
        assert!(text.contains("@calc.py:5 return marker_x"), "{text}");
        assert!(text.contains("命中所在的函数/类型整体给出"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_session_dedup_references_resent_bodies() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let _ = server.search_code(Parameters(search("marker_x")));
        let again = text_of(&server.search_code(Parameters(search("total *= 2"))));
        assert!(!again.contains("  1| def compute"), "{again}");
        assert!(again.contains("@calc.py:3 total *= 2"), "{again}");
        assert!(
            again.contains("calc.py:1-5 已在本会话第 1 次调用中返回"),
            "{again}"
        );
        let mut opt_out = search("total = a");
        opt_out.dedup = Some(false);
        let full = text_of(&server.search_code(Parameters(opt_out)));
        assert!(full.contains("  1| def compute(a, b):"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_broad_query_returns_counts_and_paging_still_works() {
        let many: String = (0..70).map(|i| format!("NEEDLE_{i} = {i}\n")).collect();
        let few: String = (0..5).map(|i| format!("NEEDLE_b{i} = {i}\n")).collect();
        let (server, dir) = smart_server(&[("pkg/a/one.py", many), ("pkg/b/two.py", few)]);
        let text = text_of(&server.search_code(Parameters(search("NEEDLE_"))));
        assert!(text.contains("宽查询：75 处命中分布在 2 个文件"), "{text}");
        assert!(text.contains("     70  pkg/a/one.py"), "{text}");
        assert!(text.contains("建议：path_filter=\"pkg/a\""), "{text}");
        assert_eq!(text.matches("\n@pkg/").count(), 12, "{text}");
        let mut paged = search("NEEDLE_");
        paged.page = true;
        let page = text_of(&server.search_code(Parameters(paged)));
        assert!(page.contains("@pkg/a/one.py:1 NEEDLE_0 = 0"), "{page}");
        let mut second = search("NEEDLE_");
        second.offset = 70;
        let second = text_of(&server.search_code(Parameters(second)));
        assert!(second.contains("@pkg/b/two.py:1 NEEDLE_b0 = 0"), "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const CMD_GO: &str = "package cobra\n\ntype Command struct{}\n\nfunc (c *Command) MarkFlagRequired(name string) error {\n\treturn MarkFlagRequired(nil, name)\n}\n\nfunc MarkFlagRequired(flags *int, name string) error {\n\treturn nil\n}\n\nfunc (c *Command) enforce() {\n\tx := 1\n\t_ = c.MarkFlagRequired(\"a\")\n\t_ = x\n\t_ = c.MarkFlagRequired(\"b\")\n}\n";
    const CMD_TEST_GO: &str = "package cobra\n\nimport \"testing\"\n\nfunc TestRequired(t *testing.T) {\n\tc := &Command{}\n\t_ = c.MarkFlagRequired(\"x\")\n}\n";

    fn explain(name: &str, include_tests: Option<bool>) -> ExplainParams {
        ExplainParams {
            name: name.into(),
            path_filter: None,
            budget_tokens: 4000,
            dedup: Some(false),
            include_tests,
            precise: None,
        }
    }

    #[test]
    fn explain_symbol_call_sites_qualified_and_tests_collapsed() {
        let (server, dir) = smart_server(&[
            ("command.go", CMD_GO.into()),
            ("command_test.go", CMD_TEST_GO.into()),
        ]);
        let text = text_of(&server.explain_symbol(Parameters(explain("MarkFlagRequired", None))));
        // Receiver-qualified headers keep the method and the function apart.
        assert!(
            text.contains("(*Command).MarkFlagRequired · command.go:5-7"),
            "{text}"
        );
        assert!(
            text.contains("function MarkFlagRequired · command.go:9-11"),
            "{text}"
        );
        // One entry per call line, bound by call form: c.X( → method, X( → func.
        assert!(
            text.contains("@command.go:15  in (*Command).enforce → (*Command).MarkFlagRequired"),
            "{text}"
        );
        assert!(
            text.contains("@command.go:17  in (*Command).enforce → (*Command).MarkFlagRequired"),
            "{text}"
        );
        assert!(
            text.contains("@command.go:6  in (*Command).MarkFlagRequired → MarkFlagRequired │"),
            "{text}"
        );
        assert!(
            !text.contains("@command.go:15  in (*Command).enforce → MarkFlagRequired │"),
            "{text}"
        );
        assert!(
            !text.contains("@command.go:6  in (*Command).MarkFlagRequired → (*Command)"),
            "{text}"
        );
        // Tests collapsed by default, listed (after code) on request.
        assert!(text.contains("测试调用 1 处 / 1 个测试函数"), "{text}");
        assert!(!text.contains("command_test.go:7"), "{text}");
        let all =
            text_of(&server.explain_symbol(Parameters(explain("MarkFlagRequired", Some(true)))));
        let test_at = all.find("@command_test.go:7  in TestRequired").expect(&all);
        assert!(all.find("@command.go:15").unwrap() < test_at, "{all}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn find(query: &str) -> QueryParams {
        QueryParams {
            query: query.into(),
            include_body: true,
            substring: Some(false),
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 2500,
            offset: 0,
        }
    }

    #[test]
    fn find_symbol_bodies_and_repeat_search_are_deduplicated() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let first = text_of(&server.find_symbol(Parameters(find("compute"))));
        assert!(first.contains("total *= 2"), "{first}");
        let again = text_of(&server.find_symbol(Parameters(find("compute"))));
        assert!(again.contains("已返回此定义体"), "{again}");
        assert!(!again.contains("total *= 2"), "{again}");
        // A search whose body find_symbol already sent references it.
        let hit = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(hit.contains("已在本会话第"), "{hit}");
        // The identical query again is not resent at all.
        let repeat = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(repeat.contains("同一查询已在本会话第"), "{repeat}");
        let mut fresh = search("marker_x");
        fresh.dedup = Some(false);
        let full = text_of(&server.search_code(Parameters(fresh)));
        assert!(full.contains("@calc.py:4 marker_x = total - 1"), "{full}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_lists_data_fixture_hits_outside_path_filter() {
        let (server, dir) = smart_server(&[
            (
                "core/frame.rs",
                "pub fn code() -> &'static str {\n    \"E_SHORT\"\n}\n".into(),
            ),
            (
                "proto/vectors/frames.json",
                "[{\"expect_error\": \"E_SHORT\"},\n {\"expect_error\": \"E_SHORT\"}]\n".into(),
            ),
            ("docs/notes.md", "E_SHORT is mentioned in prose\n".into()),
        ]);
        let mut q = search("E_SHORT");
        q.path_filter = Some("core".into());
        let text = text_of(&server.search_code(Parameters(q)));
        assert!(text.contains("@core/frame.rs:2"), "{text}");
        assert!(
            text.contains("path_filter 之外的数据/夹具文件中另有 2 处"),
            "{text}"
        );
        assert!(text.contains("proto/vectors/frames.json: 行 1,2"), "{text}");
        assert!(!text.contains("docs/notes.md"), "{text}");
        let mut rx = search("E_SHORT");
        rx.path_filter = Some("core".into());
        rx.regex = true;
        let regex = text_of(&server.search_code(Parameters(rx)));
        assert!(!regex.contains("数据/夹具"), "{regex}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explain_symbol_returns_body_and_callers() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let params = || ExplainParams {
            name: "compute".into(),
            path_filter: None,
            budget_tokens: 2500,
            dedup: None,
            include_tests: None,
            precise: None,
        };
        let text = text_of(&server.explain_symbol(Parameters(params())));
        assert!(
            text.contains("── function compute · calc.py:1-5 ──"),
            "{text}"
        );
        assert!(text.contains("marker_x = total - 1"), "{text}");
        assert!(text.contains("非测试 1 处 / 1 个函数"), "{text}");
        assert!(
            text.contains("@calc.py:9  in caller │ return compute(1, 2)"),
            "{text}"
        );
        let again = text_of(&server.explain_symbol(Parameters(params())));
        assert!(again.contains("已返回此定义"), "{again}");
        assert!(!again.contains("total *= 2"), "{again}");
        let missing = text_of(&server.explain_symbol(Parameters(ExplainParams {
            name: "nope".into(),
            ..params()
        })));
        assert!(missing.contains("未找到名为 nope 的定义"), "{missing}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_code_context_lists_slim_catalog() {
        let ctx = crate::ClientContext::builtin("claude-code").unwrap();
        assert!(ctx.slim_catalog);
        let slim = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            ctx,
        );
        let tools = slim.listed_tools();
        let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
        assert_eq!(
            names,
            [
                "search_code",
                "find_symbol",
                "explain_symbol",
                "find_references",
                "goto_definition"
            ]
        );
        let full = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::default_builtin(),
        );
        let bytes = |t: &[rmcp::model::Tool]| serde_json::to_string(t).unwrap().len();
        assert!(
            bytes(&tools) * 4 < bytes(&full.listed_tools()),
            "slim catalog should be far smaller"
        );
        let search = tools.iter().find(|t| t.name == "search_code").unwrap();
        let props = search
            .input_schema
            .get("properties")
            .unwrap()
            .as_object()
            .unwrap();
        assert!(props.contains_key("path_filter") && !props.contains_key("budget_tokens"));
        let info = slim.get_info();
        let instructions = info.instructions.unwrap();
        assert!(instructions.contains("explain_symbol"), "{instructions}");
        assert!(
            !instructions.contains("initial_instructions"),
            "{instructions}"
        );
    }

    #[test]
    fn dedup_pointer_carries_signature_and_match_line() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        let _ = server.search_code(Parameters(search("marker_x")));
        let again = text_of(&server.search_code(Parameters(search("total *= 2"))));
        assert!(again.contains("@calc.py:3 total *= 2"), "{again}");
        assert!(again.contains("签名：def compute(a, b):"), "{again}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broad_query_previews_ranked_hits_definitions_first_tests_last() {
        // 70 plain hits in a test file come first alphabetically; the single
        // definition and an ordinary use must outrank them.
        let tests: String = (0..70).map(|i| format!("x{i} = widget_{i}\n")).collect();
        let (server, dir) = smart_server(&[
            ("a_tests/t.py", tests),
            (
                "src/z_def.py",
                "def widget_main():\n    return 1\n\n\ndef other():\n    return widget_main()\n"
                    .into(),
            ),
        ]);
        let text = text_of(&server.search_code(Parameters(search("widget_"))));
        assert!(text.contains("shown=12 omitted=60"), "{text}");
        assert!(text.contains("宽查询：72 处命中"), "{text}");
        let def = text.find("@src/z_def.py:1 def widget_main").expect(&text);
        let call = text.find("@src/z_def.py:6").expect(&text);
        let test = text.find("@a_tests/t.py:1 ").expect(&text);
        assert!(def < call && call < test, "{text}");
        assert!(text.matches("@a_tests/t.py:").count() == 10, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_hit_search_suggests_close_symbols_and_looser_filter() {
        let (server, dir) = smart_server(&[("pkg/sub/calc.py", CALC_PY.into())]);
        let mut p = search("computee");
        p.path_filter = Some("pkg/sub".into());
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("(未找到匹配的代码行)"), "{text}");
        assert!(text.contains("最接近的符号名"), "{text}");
        assert!(text.contains("def compute(a, b):"), "{text}");
        assert!(text.contains("放宽到 path_filter=\"pkg\""), "{text}");
        // Nothing close: no suggestion block, still the generic hint.
        let none = text_of(&server.search_code(Parameters(search("zzzqqq"))));
        assert!(!none.contains("最接近的符号名"), "{none}");
        assert!(none.contains("find_symbol"), "{none}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn name_helpers_split_and_measure() {
        assert_eq!(name_tokens("fooBar_baz"), ["foo", "bar", "baz"]);
        assert_eq!(edit_distance("compute", "computee"), 1);
        assert_eq!(loosened_filter("a/b/c/"), Some("a/b".into()));
        assert_eq!(loosened_filter("a"), None);
    }

    #[test]
    fn group_query_splits_code_and_tests_with_roles() {
        let (server, dir) = smart_server(&[
            (
                "cfg/load.go",
                "package cfg\n\nfunc LoadKey() string {\n\treturn os.Getenv(\"MAGIC_KEY\")\n}\n\nconst keyName = \"MAGIC_KEY\"\n".into(),
            ),
            (
                "cfg/load_test.go",
                "package cfg\n\nfunc TestLoadKey(t *testing.T) {\n\tassert.Equal(t, \"MAGIC_KEY\", keyName)\n}\n\nfunc helperOnly() string {\n\treturn \"MAGIC_KEY\"\n}\n".into(),
            ),
        ]);
        let mut p = search("MAGIC_KEY");
        p.group = Some(true);
        let text = text_of(&server.search_code(Parameters(p)));
        assert!(text.contains("grouped=true total=4"), "{text}");
        let prod = text.find("## 生产代码（2 处 / 1 个文件）").expect(&text);
        let tests = text.find("## 测试（2 处 / 1 个文件）").expect(&text);
        assert!(prod < tests, "{text}");
        assert!(text.contains("[加载] in LoadKey"), "{text}");
        assert!(text.contains("[定义]"), "{text}");
        assert!(text.contains("[断言] in TestLoadKey"), "{text}");
        assert!(text.contains("[引用] in helperOnly"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hit_roles_are_text_heuristics() {
        assert_eq!(hit_role(true, "require.Equal(t, a, b)"), "断言");
        assert_eq!(hit_role(true, "x := \"lit\""), "引用");
        assert_eq!(hit_role(false, "v := os.Getenv(\"K\")"), "加载");
        assert_eq!(hit_role(false, "const K = \"v\""), "定义");
        assert_eq!(hit_role(false, "log(\"v\")"), "使用");
    }

    #[test]
    fn dedup_marks_only_bodies_that_actually_shipped() {
        let (server, dir) = smart_server(&[("calc.py", CALC_PY.into())]);
        // Budget too small for the body: falls back to line context.
        let mut tiny = search("marker_x");
        tiny.budget_tokens = 1;
        let first = text_of(&server.search_code(Parameters(tiny)));
        assert!(!first.contains("── function compute"), "{first}");
        // The body never shipped, so it must not be replaced by a pointer.
        let second = text_of(&server.search_code(Parameters(search("marker_x"))));
        assert!(
            second.contains("── function compute · calc.py:1-5 ──"),
            "{second}"
        );
        assert!(!second.contains("已在本会话"), "{second}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_auto_groups_when_hits_span_more_than_three_files() {
        let files: Vec<(String, String)> = (0..4)
            .map(|i| (format!("pkg/f{i}.py"), format!("def f{i}():\n    return 'AUTO_KEY'\n\n\ndef g{i}():\n    return 'AUTO_KEY'\n")))
            .collect();
        let refs: Vec<(&str, String)> =
            files.iter().map(|(n, t)| (n.as_str(), t.clone())).collect();
        let (server, dir) = smart_server(&refs);
        let text = text_of(&server.search_code(Parameters(search("AUTO_KEY"))));
        assert!(text.contains("grouped=true total=8"), "{text}");
        // Explicit false forces the plain list.
        let mut plain = search("AUTO_KEY");
        plain.group = Some(false);
        let plain = text_of(&server.search_code(Parameters(plain)));
        assert!(!plain.contains("grouped=true"), "{plain}");
        // Few files: no auto grouping.
        let (server2, dir2) = smart_server(&refs[..3]);
        let three = text_of(&server2.search_code(Parameters(search("AUTO_KEY"))));
        assert!(!three.contains("grouped=true"), "{three}");
        // Few hits (<=5) keep enclosing bodies even across many files.
        let (server3, dir3) = smart_server(&refs);
        let few = text_of(&server3.search_code(Parameters(search("def f"))));
        assert!(!few.contains("grouped=true"), "{few}");
        for d in [dir, dir2, dir3] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn tool_descriptions_carry_prefer_over_grep_and_exhaustive_guidance() {
        let full = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::default_builtin(),
        );
        for tool in full.listed_tools() {
            if [
                "search_code",
                "find_symbol",
                "explain_symbol",
                "find_references",
                "goto_definition",
            ]
            .contains(&tool.name.as_ref())
            {
                let d = tool.description.as_deref().unwrap_or("");
                assert!(
                    d.contains("Prefer this over Grep/Read for code"),
                    "{}: {d}",
                    tool.name
                );
            }
        }
        let slim = AstrolabeServer::new_with_cache_budget_and_context(
            PathBuf::from("."),
            DEFAULT_MEMORY_BUDGET_BYTES,
            crate::ClientContext::builtin("claude-code").unwrap(),
        );
        let tools = slim.listed_tools();
        for tool in &tools {
            let d = tool.description.as_deref().unwrap_or("");
            assert!(d.contains("over Grep/Read for code"), "{}: {d}", tool.name);
        }
        let desc = |n: &str| {
            tools
                .iter()
                .find(|t| t.name == n)
                .unwrap()
                .description
                .as_deref()
                .unwrap()
                .to_string()
        };
        assert!(desc("search_code").contains("never re-grep"));
        assert!(desc("search_code").contains("before editing/renaming a literal"));
        assert!(desc("find_symbol").contains("no need to re-grep"));
        let instructions = slim.get_info().instructions.unwrap();
        assert!(instructions.contains("don't re-grep"), "{instructions}");
        assert!(instructions.contains("group=true"), "{instructions}");
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
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: Some(false),
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
            case_sensitive: None,
            budget_tokens: 200,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let text = after.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("fresh-content"), "{text}");
        assert!(!text.contains("cache-me"), "{text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn loop_guard_server(repeat_max: usize) -> (AstrolabeServer, PathBuf) {
        let (server, dir) = ready_server_with_disk();
        *server.loop_guard.lock().unwrap() = LoopGuard::new(crate::loop_guard::LoopGuardConfig {
            repeat_max,
            ..crate::loop_guard::LoopGuardConfig::default()
        });
        (server, dir)
    }

    fn guarded_search(server: &AstrolabeServer, q: &str, budget: usize) -> String {
        let result = server.search_code(Parameters(SearchParams {
            query: q.into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: budget,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        result.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn find_symbol_pages_concatenate_to_full_match_set() {
        let dir = unique_temp_dir();
        let mut expected = std::collections::BTreeSet::new();
        for f in 0..4 {
            let mut src = String::new();
            for i in 0..40 {
                src.push_str(&format!("def pager_{f}_{i}():\n    pass\n"));
                expected.insert(format!("pager_{f}_{i}"));
            }
            std::fs::write(dir.join(format!("m{f}.py")), src).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 << 20);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        let page = |offset: usize, path: bool| {
            let r = server.find_symbol(Parameters(QueryParams {
                query: if path {
                    "/pager".into()
                } else {
                    "pager".into()
                },
                include_body: false,
                substring: Some(true),
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 300,
                offset,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        for path in [false, true] {
            let mut got = Vec::new();
            let mut offset = 0;
            let mut pages = 0;
            loop {
                let text = page(offset, path);
                pages += 1;
                assert!(text.contains("total=160"), "{text}");
                assert!(!text.contains("blocked: true"), "{text}");
                for name in text
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|w| w.starts_with("pager_"))
                {
                    got.push(name.to_string());
                }
                let Some(next) = text
                    .split("next_offset=")
                    .nth(1)
                    .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                    .and_then(|n| n.parse::<usize>().ok())
                else {
                    assert!(text.contains("truncated=false"), "{text}");
                    break;
                };
                assert!(next > offset, "{text}");
                offset = next;
                assert!(pages < 100);
            }
            assert!(pages > 2, "fixture should need several pages");
            let unique: std::collections::BTreeSet<_> = got.iter().cloned().collect();
            assert_eq!(unique, expected, "path={path}");
        }
        let past = page(10_000, false);
        assert!(past.contains("超出总数"), "{past}");
    }

    #[test]
    fn search_code_pages_concatenate_to_full_match_set() {
        let dir = unique_temp_dir();
        let mut expected = std::collections::BTreeSet::new();
        for f in 0..4 {
            let mut src = String::new();
            for i in 0..40 {
                src.push_str(&format!("needle_{f}_{i} = {i}\n"));
                expected.insert(format!("@m{f}.py:{}", i + 1));
            }
            std::fs::write(dir.join(format!("m{f}.py")), src).unwrap();
        }
        let mut notes = String::new();
        for i in 0..30 {
            notes.push_str(&format!("needle note {i}\n"));
            expected.insert(format!("@notes.md:{}", i + 1));
        }
        std::fs::write(dir.join("notes.md"), notes).unwrap();
        let (mut server, _) = (
            AstrolabeServer::new_with_cache_budget(dir.clone(), 8 << 20),
            (),
        );
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        let page = |offset: usize| {
            let r = server.search_code(Parameters(SearchParams {
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
                query: "needle".into(),
                regex: false,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 300,
                offset,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        let mut got = Vec::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let text = page(offset);
            pages += 1;
            assert!(text.contains("total=190"), "{text}");
            assert!(
                !text.contains("blocked: true"),
                "paging must not trip loop guard: {text}"
            );
            for line in text.lines().filter(|l| l.starts_with('@')) {
                got.push(line.split(' ').next().unwrap().to_string());
            }
            let Some(next) = text
                .split("next_offset=")
                .nth(1)
                .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
                .and_then(|n| n.parse::<usize>().ok())
            else {
                assert!(text.contains("truncated=false"), "{text}");
                break;
            };
            assert!(next > offset, "{text}");
            offset = next;
            assert!(pages < 100);
        }
        assert!(pages > 2, "fixture should need several pages");
        let unique: std::collections::BTreeSet<_> = got.iter().cloned().collect();
        assert_eq!(unique.len(), got.len(), "pages overlap");
        assert_eq!(unique, expected);
        // code hits come before [text-only] hits across pages
        let first_text = got.iter().position(|l| l.starts_with("@notes.md")).unwrap();
        assert!(got[first_text..].iter().all(|l| l.starts_with("@notes.md")));
        // deterministic ordering
        assert_eq!(page(0), page(0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_offset_is_part_of_signature() {
        let (server, dir) = loop_guard_server(1);
        let at = |offset: usize| {
            let r = server.search_code(Parameters(SearchParams {
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
                query: "needle-a".into(),
                regex: false,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 1000,
                offset,
            }));
            r.content[0].as_text().unwrap().text.clone()
        };
        for offset in 0..4 {
            assert!(!at(offset).contains("blocked: true"));
        }
        assert!(at(0).contains("blocked: true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_loop_guard_short_circuits_repeated_query() {
        let (server, dir) = loop_guard_server(2);
        let call = |q: &str| guarded_search(&server, q, 1000);
        assert!(call("needle-a").contains("confidence: exact"));
        assert!(call("needle-b").contains("confidence: exact"));
        let first = call("needle-a");
        assert!(first.contains("confidence: exact"));
        let blocked = call("needle-a");
        assert!(blocked.contains("loop guard"), "{blocked}");
        assert!(blocked.contains("blocked: true"), "{blocked}");
        assert!(!blocked.contains("结果与之前相同"), "{blocked}");
        // the cached previous result is returned, not a bare note
        assert!(blocked.contains(&first), "{blocked}");
        assert!(call("needle-b").contains("confidence: exact"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_budget_tokens_is_part_of_signature() {
        let (server, dir) = loop_guard_server(1);
        assert!(!guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        // "truncated, raise budget_tokens" retry must execute
        assert!(!guarded_search(&server, "needle-a", 4000).contains("blocked: true"));
        assert!(guarded_search(&server, "needle-a", 4000).contains("blocked: true"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_does_not_count_index_building_responses() {
        let (server, dir) = loop_guard_server(1);
        let ready = std::mem::replace(
            &mut *server.primary.state.write().unwrap(),
            IndexState::Building,
        );
        for _ in 0..5 {
            let text = guarded_search(&server, "needle-a", 1000);
            assert!(!text.contains("blocked: true"), "{text}");
        }
        *server.primary.state.write().unwrap() = ready;
        let text = guarded_search(&server, "needle-a", 1000);
        assert!(text.contains("confidence: exact"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_does_not_count_error_responses() {
        let (server, dir) = loop_guard_server(1);
        let bad = |s: &AstrolabeServer| {
            s.search_code(Parameters(SearchParams {
                query: "(".into(),
                regex: true,
                path_filter: None,
                case_sensitive: None,
                budget_tokens: 1000,
                offset: 0,
                context_lines: Some(0),
                page: false,
                group: None,
                dedup: None,
            }))
        };
        for _ in 0..3 {
            let r = bad(&server);
            let text = r.content[0].as_text().unwrap().text.clone();
            assert!(!text.contains("blocked: true"), "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_reexecutes_after_index_changes() {
        // edit-then-requery: a new index generation means a fresh count and
        // fresh results, never the stale cached text.
        let (server, dir) = loop_guard_server(1);
        assert!(!guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        assert!(guarded_search(&server, "needle-a", 1000).contains("blocked: true"));
        std::fs::write(
            dir.join("hello.py"),
            "def greet():\n    return 'needle-a'\n",
        )
        .unwrap();
        let rebuilt = Arc::new(crate::index::build(&dir).expect("index"));
        *server.primary.state.write().unwrap() = IndexState::Ready(rebuilt);
        let text = guarded_search(&server, "needle-a", 1000);
        assert!(!text.contains("blocked: true"), "{text}");
        assert!(text.contains("hello.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loop_guard_marks_structured_blocked() {
        let blocked = mark_blocked(
            {
                let mut r = CallToolResult::success(vec![ContentBlock::text("prev")]);
                r.structured_content = Some(json!({"text": "prev"}));
                r
            },
            "blocked: true\nnote",
        );
        assert_eq!(blocked.structured_content.unwrap()["blocked"], json!(true));
        let text = blocked.content[0].as_text().unwrap().text.clone();
        assert!(
            text.starts_with("blocked: true") && text.ends_with("prev"),
            "{text}"
        );
    }

    fn hit(path: &str, line: usize) -> SearchHit {
        SearchHit {
            path: path.into(),
            line,
            text: format!("match{line}"),
            text_only: false,
        }
    }

    fn numbered(n: usize) -> Arc<Vec<String>> {
        Arc::new((1..=n).map(|i| format!("line{i}")).collect())
    }

    #[test]
    fn search_context_defaults_adapt_to_match_count() {
        assert_eq!(search_context_lines(None, 1), 3);
        assert_eq!(search_context_lines(None, 5), 3);
        assert_eq!(search_context_lines(None, 6), 1);
        assert_eq!(search_context_lines(None, 20), 1);
        assert_eq!(search_context_lines(None, 21), 0);
        assert_eq!(search_context_lines(Some(0), 1), 0);
        assert_eq!(search_context_lines(Some(99), 1), MAX_SEARCH_CONTEXT);
    }

    #[test]
    fn search_context_windows_merge_and_anchor_every_match() {
        let lines = numbered(30);
        let hits = vec![
            hit("a.rs", 5),
            hit("a.rs", 7),
            hit("a.rs", 20),
            hit("b.rs", 1),
        ];
        let (out, shown) = render_search_hits(&hits, 2, 10_000, &mut |_| Some(Arc::clone(&lines)));
        assert_eq!(shown, 4);
        let expected = "  3| line3\n  4| line4\n@a.rs:5 match5\n  6| line6\n@a.rs:7 match7\n  8| line8\n  9| line9\n  …\n  18| line18\n  19| line19\n@a.rs:20 match20\n  21| line21\n  22| line22\n@b.rs:1 match1\n  2| line2\n  3| line3\n";
        assert_eq!(out, expected);
        // Each source line at most once per file window.
        assert_eq!(out.matches("  6| ").count(), 1);
    }

    #[test]
    fn search_context_zero_is_bare_anchor_lines() {
        let hits = vec![hit("a.rs", 5), hit("a.rs", 7)];
        let (out, shown) = render_search_hits(&hits, 0, 10_000, &mut |_| panic!("not read"));
        assert_eq!(shown, 2);
        assert_eq!(out, "@a.rs:5 match5\n@a.rs:7 match7\n");
    }

    #[test]
    fn search_context_respects_budget_and_falls_back_to_bare_lines() {
        let lines = numbered(400);
        let hits: Vec<_> = (1..=40).map(|i| hit("a.rs", i * 10)).collect();
        let (out, shown) = render_search_hits(&hits, 3, 120, &mut |_| Some(Arc::clone(&lines)));
        assert!(shown > 0 && shown < 40, "{shown}");
        assert!(estimate_tokens(&out) <= 120, "{}", estimate_tokens(&out));
        assert_eq!(
            out.lines().filter(|l| l.starts_with("@a.rs:")).count(),
            shown
        );
    }

    #[test]
    fn search_code_shows_context_by_default_for_narrow_queries() {
        let (server, dir) = ready_server_with_disk();
        let params = |context_lines| SearchParams {
            query: "cache-me".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines,
            page: false,
            group: None,
            dedup: None,
        };
        let result = server.search_code(Parameters(params(None)));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(
            text.contains("  1| def greet():\n@hello.py:2 return 'cache-me'"),
            "{text}"
        );
        assert!(text.contains("上下文：每处命中前后 3 行"), "{text}");
        let bare = server.search_code(Parameters(params(Some(0))));
        let bare = bare.content[0].as_text().unwrap().text.as_str();
        assert!(!bare.contains("1| def greet"), "{bare}");
        assert!(bare.contains("@hello.py:2 return 'cache-me'"), "{bare}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        let result = server.search_code(Parameters(SearchParams {
            query: "totally-absent-needle".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            case_sensitive: None,
            budget_tokens: 1000,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            case_sensitive: None,
            budget_tokens: 20,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let text = result.content[0].as_text().unwrap().text.as_str();
        assert!(text.contains("mode: literal"), "{text}");
        assert!(text.contains("truncated=true"), "{text}");
        assert!(text.contains("这不是全集"), "{text}");
        assert!(text.contains("加大 budget_tokens"), "{text}");
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
            budget.as_str().unwrap().contains("offset=next_offset"),
            "{budget}"
        );
        assert!(tool_desc.contains("regex=true"), "{tool_desc}");
        assert!(tool_desc.contains("offset=next_offset"), "{tool_desc}");
    }

    fn server_on_files(files: &[(&str, &str)]) -> (AstrolabeServer, PathBuf) {
        let dir = unique_temp_dir();
        for (path, body) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = false;
        (server, dir)
    }

    fn fs_query(query: &str) -> QueryParams {
        QueryParams {
            query: query.into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 2500,
            offset: 0,
        }
    }

    fn fs_text(result: &CallToolResult) -> String {
        result.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn find_symbol_ranks_exact_name_before_substring_and_survives_truncation() {
        let mut many = String::new();
        for i in 0..60 {
            many.push_str(&format!("def handle_widget_{i}():\n    pass\n"));
        }
        let (server, dir) = server_on_files(&[
            ("aaa/noise.py", &many),
            ("zzz/real.py", "def widget():\n    pass\n"),
        ]);
        let mut params = fs_query("widget");
        params.budget_tokens = 120;
        let text = fs_text(&server.find_symbol(Parameters(params)));
        let first = text.lines().find(|l| l.contains('@')).unwrap_or_default();
        assert!(first.contains("zzz/real.py"), "{text}");

        // 编辑文件后重建索引再查：新增的精确同名定义同样排在最前。
        std::fs::write(
            dir.join("aaa/exact.py"),
            "def handle_widget_5x():\n    pass\nclass Gadget:\n    pass\n",
        )
        .unwrap();
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        let mut params = fs_query("gadget");
        params.substring = Some(false);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("aaa/exact.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_exact_only_kind_and_path_filter_apply_to_name_queries() {
        let (server, dir) = server_on_files(&[
            (
                "src/a.py",
                "class Item:\n    pass\ndef item():\n    pass\ndef items_all():\n    pass\n",
            ),
            ("lib/b.py", "def item():\n    pass\n"),
        ]);
        let mut params = fs_query("item");
        params.substring = Some(false);
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(!text.contains("items_all"), "{text}");

        let mut params = fs_query("item");
        params.kind = Some("class".into());
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("class Item"), "{text}");
        assert!(!text.contains("def item"), "{text}");

        let mut params = fs_query("item");
        params.path_filter = Some("lib/".into());
        let text = fs_text(&server.find_symbol(Parameters(params)));
        assert!(text.contains("lib/b.py"), "{text}");
        assert!(!text.contains("src/a.py"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_accepts_dot_and_colon_paths_and_go_receivers() {
        let (server, dir) = server_on_files(&[
            (
                "cmd/command.go",
                "package cmd\n\ntype Command struct {\n\tName string\n}\n\nfunc (c *Command) ExecuteC() error {\n\treturn nil\n}\n",
            ),
            (
                "src/lib.rs",
                "pub struct Pool;\nimpl Pool {\n    pub fn spawn(&self) {}\n}\n",
            ),
        ]);
        for query in ["Command.ExecuteC", "Command/ExecuteC", "cmd.ExecuteC"] {
            let text = fs_text(&server.find_symbol(Parameters(fs_query(query))));
            assert!(text.contains("cmd/command.go:7"), "{query}: {text}");
        }
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Pool::spawn"))));
        assert!(text.contains("src/lib.rs:3"), "{text}");
        let text = fs_text(&server.find_symbol(Parameters(fs_query("Command struct"))));
        assert!(text.contains("cmd/command.go:3"), "{text}");
        assert!(text.contains("已忽略查询中的关键字"), "{text}");
        let text = fs_text(&server.find_symbol(Parameters(fs_query("ExecuteX"))));
        assert!(
            text.contains("相近的符号名") && text.contains("ExecuteC"),
            "{text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_symbol_prefers_literal_names_containing_separators() {
        let (server, dir) = server_on_files(&[(
            "lib/hooks.sh",
            "#!/bin/bash\nbats::on_failure() {\n  :\n}\n",
        )]);
        let text = fs_text(&server.find_symbol(Parameters(fs_query("bats::on_failure"))));
        assert!(text.contains("lib/hooks.sh:2"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unmatched_path_filter_is_an_explicit_error_with_closest_paths() {
        let (server, dir) =
            server_on_files(&[("relay/channel/openai.py", "def send():\n    pass\n")]);
        let mut params = fs_query("send");
        params.path_filter = Some("relay/chanel".into());
        let result = server.find_symbol(Parameters(params));
        assert_eq!(result.is_error, Some(true));
        let text = fs_text(&result);
        assert!(text.contains("relay/channel"), "{text}");

        let result = server.search_code(Parameters(SearchParams {
            query: "send".into(),
            regex: false,
            path_filter: Some("./relay/channel".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        assert_eq!(result.is_error, Some(true));
        assert!(
            fs_text(&result).contains("relay/channel/"),
            "{}",
            fs_text(&result)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalize_symbol_query_handles_separators_and_keywords() {
        assert_eq!(normalize_symbol_query("Type.method").query, "Type/method");
        assert_eq!(normalize_symbol_query("A::b::c").query, "A/b/c");
        assert_eq!(normalize_symbol_query("::A::b").query, "/A/b");
        let n = normalize_symbol_query("Proc struct");
        assert_eq!(n.query, "Proc");
        assert!(n.ignored.is_some());
        assert_eq!(normalize_symbol_query("struct").query, "struct");
    }

    #[test]
    fn find_symbol_empty_result_states_no_match_explicitly() {
        let (server, dir) = ready_server_with_disk();
        // 名称匹配分支：include_body=true 时零命中也不得出现
        // 误导性的"无可提取的符号体"。
        let named = server.find_symbol(Parameters(QueryParams {
            query: "NoSuchSymbolAnywhere".into(),
            include_body: true,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
            offset: 0,
        }));
        let named_text = named.content[0].as_text().unwrap().text.as_str();
        assert!(named_text.contains("(未找到匹配的符号)"), "{named_text}");
        assert!(!named_text.contains("无可提取的符号体"), "{named_text}");

        // 层级路径分支（query 含 '/'）同样需要显式零命中声明。
        let by_path = server.find_symbol(Parameters(QueryParams {
            query: "NoSuch/Path".into(),
            include_body: false,
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 1000,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 300,
            offset: 0,
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
            // Polling loops in these tests legitimately repeat one query.
            loop_guard: crate::loop_guard::LoopGuardConfig {
                repeat_max: 0,
                rate_max: 0,
                ..Default::default()
            },
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
                substring: None,
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 500,
                offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
                substring: None,
                path_filter: None,
                kind: None,
                depth: 0,
                budget_tokens: 500,
                offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 200,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 200,
            offset: 0,
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
            substring: None,
            path_filter: None,
            kind: None,
            depth: 0,
            budget_tokens: 500,
            offset: 0,
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

        // 3 个 .scala 文件 (未支持语言)
        std::fs::write(dir.join("A.scala"), "class A { val x = 1 }\n").unwrap();
        std::fs::write(dir.join("B.scala"), "class B { val y = 2 }\n").unwrap();
        std::fs::write(dir.join("C.scala"), "class C { val z = 3 }\n").unwrap();

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

        // 1. search_code 零命中且无 path_filter，含 .scala×3 (N>=3) 时提示尚未支持语言且带计数
        let search_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent_code_pattern".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let search_text = search_miss.content[0].as_text().unwrap().text.as_str();
        assert!(
            search_text.contains("(未找到匹配的代码行)"),
            "{search_text}"
        );
        assert!(search_text.contains("尚未支持的语言"), "{search_text}");
        assert!(search_text.contains("3 个 .scala 文件"), "{search_text}");
        assert!(search_text.contains("未入代码索引"), "{search_text}");
        assert!(
            search_text.contains("已按纯文本一并搜索，同样无匹配"),
            "{search_text}"
        );
        assert!(
            search_text.contains("注意：仓库内有 3 个 .scala 文件属于尚未支持的语言"),
            "{search_text}"
        );
        let structured = search_miss.structured_content.expect("structured");
        assert_eq!(structured["unindexed_hint"], json!(true));

        // 1b. 带 path_filter 的 search_code 零命中：filter 范围内 N>=3
        let search_filter_hit_hint = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some(".scala".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let filter_hint_text = search_filter_hit_hint.content[0]
            .as_text()
            .unwrap()
            .text
            .as_str();
        assert!(
            filter_hint_text
                .contains("注意：path_filter '.scala' 范围内有 3 个 .scala 文件属于尚未支持的语言"),
            "{filter_hint_text}"
        );
        let filter_hint_structured = search_filter_hit_hint
            .structured_content
            .expect("structured");
        assert_eq!(filter_hint_structured["unindexed_hint"], json!(true));

        // 1c. 带 path_filter 的 search_code 零命中：filter 范围内 N<3 (仅 A.scala 匹配，计 1)
        let search_filter_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: Some("A.scala".into()),
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            offset: 0,
            depth: 0,
            substring: None,
            path_filter: None,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(symbol_text.contains("(未找到匹配的符号)"), "{symbol_text}");
        assert!(
            symbol_text.contains("(仓库另有 3 个 .scala 等未支持语言的源码文件未入符号索引，符号搜索不覆盖它们——如需检索请用 Grep/Read)"),
            "{symbol_text}"
        );

        // 3. get_dependents target=A.scala 精确文案
        let dep_res = server.get_dependents(Parameters(DependentsParams {
            target: "A.scala".into(),
            direction: "dependents".into(),
            budget_tokens: 2500,
        }));
        let dep_text = dep_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            dep_text.contains("目标 A.scala 是 .scala 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。"),
            "{dep_text}"
        );

        // 3b. get_neighborhood target=A.scala 精确文案
        let neigh_res = server.get_neighborhood(Parameters(NeighborhoodParams {
            target: "A.scala".into(),
            direction: "both".into(),
            depth: 2,
            budget_tokens: 2500,
        }));
        let neigh_text = neigh_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            neigh_text.contains("目标 A.scala 是 .scala 文件（语言暂不支持，未入 import 图）——依赖查询不覆盖它，请改用 Grep/Read 分析其引用关系。"),
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
            "supported languages: python, go, java, rust, typescript, tsx, javascript, c, cpp, objc, objcpp, swift, php, vue, dart, csharp, visualbasic, kotlin, shell, ruby, lua"
        );
        assert!(
            langs_text.contains("unindexed source files: .scala×3"),
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
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let dart_text = search_dart.content[0].as_text().unwrap().text.as_str();
        assert!(dart_text.contains("@service.dart:2"), "{dart_text}");
        assert!(!dart_text.contains("(未找到匹配的代码行)"), "{dart_text}");
        let dart_structured = search_dart.structured_content.expect("structured");
        assert_eq!(dart_structured["unindexed_hint"], json!(false));

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn search_text(
        server: &AstrolabeServer,
        query: &str,
        regex: bool,
        cs: Option<bool>,
    ) -> (String, Value) {
        let result = server.search_code(Parameters(SearchParams {
            query: query.into(),
            regex,
            path_filter: None,
            case_sensitive: cs,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
        }));
        let text = result.content[0].as_text().unwrap().text.clone();
        (text, result.structured_content.expect("structured"))
    }

    #[test]
    fn search_code_covers_non_indexed_text_files() {
        let dir = unique_temp_dir();
        std::fs::write(
            dir.join("main.py"),
            "def load_user():\n    return 'user_table'\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("README.md"),
            "Call load_user to read user_table.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("schema.sql"),
            "CREATE TABLE user_table (id int);\n",
        )
        .unwrap();
        std::fs::write(dir.join("config.yaml"), "table: user_table\n").unwrap();
        std::fs::write(dir.join("App.scala"), "val t = \"user_table\"\n").unwrap();
        std::fs::write(dir.join("package-lock.json"), "{\"user_table\": 1}\n").unwrap();

        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        let (text, structured) = search_text(&server, "user_table", false, None);
        assert!(text.contains("@main.py:2 "), "{text}");
        for anchor in [
            "@README.md:1 [text-only]",
            "@schema.sql:1 [text-only]",
            "@config.yaml:1 [text-only]",
            "@App.scala:1 [text-only]",
        ] {
            assert!(text.contains(anchor), "missing {anchor}: {text}");
        }
        assert!(
            !text.contains("package-lock.json"),
            "lock files skipped: {text}"
        );
        // Code hits rank before text-only hits.
        assert!(
            text.find("@main.py").unwrap() < text.find("@README.md").unwrap(),
            "{text}"
        );
        assert_eq!(structured["text_only_matches"], json!(4));

        // Edit a text file, then query: the new content is visible.
        std::fs::write(dir.join("README.md"), "Renamed to account_table.\n").unwrap();
        let (text, _) = search_text(&server, "account_table", false, None);
        assert!(text.contains("@README.md:1 [text-only]"), "{text}");
        let (text, _) = search_text(&server, "read user_table", false, None);
        assert!(!text.contains("README.md"), "stale text hit: {text}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_text_only_covers_common_config_types() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "x = 1\n").unwrap();
        let files = [
            "Cargo.toml",
            "pyproject.toml",
            "config.toml",
            "setup.cfg",
            "app.ini",
            "nginx.conf",
            "app.properties",
            ".env",
            ".env.local",
            "ci.yml",
            "build.gradle.kts",
            "Dockerfile",
            "Makefile",
            "run.sh",
            "api.proto",
            "schema.graphql",
            "data.csv",
        ];
        for name in files {
            std::fs::write(dir.join(name), "marker_value_42\n").unwrap();
        }
        std::fs::write(dir.join("Cargo.lock"), "marker_value_42\n").unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;
        let (text, _) = search_text(&server, "marker_value_42", false, None);
        for name in files {
            let anchor = format!("@{name}:1");
            assert!(text.contains(&anchor), "missing {anchor}: {text}");
        }
        assert!(text.contains("@Cargo.toml:1 [text-only]"), "{text}");
        assert!(
            !text.contains("Cargo.lock"),
            "Cargo.lock must be skipped: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_code_case_rules_and_override() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def LoadUser():\n    pass\n").unwrap();
        let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));
        server.structured_output = true;

        // Literal default: case-insensitive.
        assert!(search_text(&server, "loaduser", false, None)
            .0
            .contains("@main.py:1"));
        // Literal forced case-sensitive.
        assert!(!search_text(&server, "loaduser", false, Some(true))
            .0
            .contains("@main.py:1"));
        // Regex default: case-sensitive.
        assert!(!search_text(&server, "load.ser", true, None)
            .0
            .contains("@main.py:1"));
        // Regex forced case-insensitive.
        assert!(search_text(&server, "load.ser", true, Some(false))
            .0
            .contains("@main.py:1"));
        // Literal special characters are escaped, not treated as regex.
        assert!(!search_text(&server, "Load.ser", false, None)
            .0
            .contains("@main.py:1"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unindexed_languages_threshold_under_three_does_not_hint() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("main.py"), "def main(): pass\n").unwrap();
        // 仅 2 个 .scala 文件 (N = 2 < 3)
        std::fs::write(dir.join("A.scala"), "class A\n").unwrap();
        std::fs::write(dir.join("B.scala"), "class B\n").unwrap();

        let server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 * 1024 * 1024);
        let index = crate::index::build(&dir).expect("index");
        *server.primary.state.write().unwrap() = IndexState::Ready(Arc::new(index));

        // search_code 零命中：N<3 不应触发提示
        let search_miss = server.search_code(Parameters(SearchParams {
            query: "nonexistent".into(),
            regex: false,
            path_filter: None,
            case_sensitive: None,
            budget_tokens: 2500,
            offset: 0,
            context_lines: Some(0),
            page: false,
            group: None,
            dedup: None,
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
            offset: 0,
            depth: 0,
            substring: None,
            path_filter: None,
            kind: None,
            include_body: false,
        }));
        let symbol_text = symbol_miss.content[0].as_text().unwrap().text.as_str();
        assert!(symbol_text.contains("(未找到匹配的符号)"), "{symbol_text}");
        assert!(!symbol_text.contains("未支持语言"), "{symbol_text}");

        // get_languages: >=1 就显示统计，这里有 2 个 .scala 仍会显示
        let langs_res = server.get_languages(Parameters(BudgetOnly {
            budget_tokens: 2500,
        }));
        let langs_text = langs_res.content[0].as_text().unwrap().text.as_str();
        assert!(
            langs_text.contains("unindexed source files: .scala×2"),
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
        for name in &["find_symbol", "resolve_context"] {
            let tool = tools.iter().find(|t| t.name == *name).expect(name);
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(
                desc.contains("Indexed languages only: python, go, java, rust, ts/tsx/js, c/cpp, objc/objcpp/swift, php, vue, dart"),
                "{name} description missing indexed languages notice: {desc}"
            );
        }
        // search_code also covers non-indexed text files and says so.
        let tool = tools.iter().find(|t| t.name == "search_code").unwrap();
        let desc = tool.description.as_deref().unwrap_or("");
        assert!(desc.contains("[text-only]"), "{desc}");
        assert!(
            desc.contains("literal is case-insensitive, regex is case-sensitive"),
            "{desc}"
        );
    }
}
