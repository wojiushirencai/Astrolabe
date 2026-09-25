//! Serena 式防漂移 hooks：PreToolUse 计数连续 grep/read 滥用，超阈值 deny + 提醒。
//! 入口由 cli 分发；本模块只做协议与计数。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// 阈值常量（照抄 Serena 数值）
pub(crate) const GREP_THRESHOLD: u32 = 3;
pub(crate) const READ_THRESHOLD: u32 = 3;
pub(crate) const NON_SYMBOLIC_THRESHOLD: u32 = 4;

/// 重置周期（秒）：两次同类调用间隔超过该值才重置计数
pub(crate) const GREP_RESET_PERIOD_SECONDS: f64 = 1000.0;
pub(crate) const READ_RESET_PERIOD_SECONDS: f64 = 1000.0;
pub(crate) const NON_SYMBOLIC_RESET_PERIOD_SECONDS: f64 = 2000.0;

/// deny 后静默窗口（秒）：窗口内整个 hook 变为 no-op（不增计数、不发 deny）
pub(crate) const MIN_DENY_INTERVAL_SECONDS: f64 = 120.0;

/// 切片精读（slice read）最大 limit：显式带 limit 且 limit<=120 的局部阅读视为合法精读，
/// 不计 read 滥用（Serena 哲学："read a few lines" 时内置 Read 完全正当）。
/// 注意：仅有 offset 而无合法 limit 时不算切片精读（Claude Code 默认会读约 2000 行）。
pub(crate) const SLICE_READ_MAX_LIMIT: u64 = 120;

/// 非符号工具子串：Astrolabe 工具名包含这些子串时不重置 burst 计数
pub(crate) const NON_SYMBOLIC_ASTROLABE_SUBSTRINGS: &[&str] = &[
    "pattern",
    "search_code",
    "diagnostics",
    "languages",
    "ensure_language",
    "instructions",
    "memory",
];

/// 非 Claude-Code 客户端判定 read_file 的动作动词子串
pub(crate) const READ_FILE_VERB_SUBSTRINGS: &[&str] = &["read", "view", "open", "show"];

/// Codex / Grok 下的 grep 类 shell 命令
pub(crate) const GREP_SHELL_COMMANDS: &[&str] = &[
    "grep",
    "rg",
    "ag",
    "ack",
    "fgrep",
    "egrep",
    "search_for_pattern",
];

/// Codex / Grok 下的 read 类 shell 命令
pub(crate) const READ_SHELL_COMMANDS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "sed",
    "less",
    "more",
    "bat",
    "get-content",
    "gc",
];

/// 源码文件后缀名全集（照抄 Serena 58 个扩展名，只针对代码文件做 read deny）
pub(crate) const CODE_FILE_EXTENSIONS: &[&str] = &[
    "al", "bash", "c", "clj", "cljs", "cpp", "cs", "css", "dart", "elm", "ex", "exs", "fs", "fsx",
    "go", "graphql", "gql", "groovy", "h", "hcl", "hpp", "hs", "html", "java", "jl", "js", "json",
    "jsonc", "jsx", "kt", "kts", "lean", "lua", "m", "matlab", "nf", "php", "proto", "ps1", "py",
    "r", "rb", "rs", "scala", "sh", "sol", "sql", "svelte", "swift", "tf", "tfvars", "toml", "ts",
    "tsx", "vue", "yaml", "yml", "zig",
];

/// 触发 hook 的客户端类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Client {
    ClaudeCode,
    Codebuddy,
    Vscode,
    Codex,
    Grok,
    Other,
}

impl Client {
    pub(crate) fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude-code" | "claude_code" | "claudecode" => Client::ClaudeCode,
            "codebuddy" => Client::Codebuddy,
            "vscode" => Client::Vscode,
            "codex" => Client::Codex,
            "grok" => Client::Grok,
            _ => Client::Other,
        }
    }
}

/// 计数状态持久化结构体（保存在 ~/.astrolabe/hook_data/<session_id>/counter.json）
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub(crate) struct CounterState {
    pub(crate) n_grep: u32,
    pub(crate) n_read: u32,
    pub(crate) n_non_symbolic: u32,
    pub(crate) last_grep_ts: Option<f64>,
    pub(crate) last_read_ts: Option<f64>,
    pub(crate) last_non_symbolic_ts: Option<f64>,
    pub(crate) last_deny_ts: Option<f64>,
}

impl CounterState {
    /// 清零连续突发计数（保留 last_deny_ts 以维持静默窗口）
    pub(crate) fn reset_burst(&mut self) {
        self.n_grep = 0;
        self.n_read = 0;
        self.n_non_symbolic = 0;
        self.last_grep_ts = None;
        self.last_read_ts = None;
        self.last_non_symbolic_ts = None;
    }

    /// 判定当前是否处于静默窗口之外（可正常触发 hook）
    pub(crate) fn is_hook_active(&self, now: f64) -> bool {
        match self.last_deny_ts {
            None => true,
            Some(ts) => (now - ts) >= MIN_DENY_INTERVAL_SECONDS,
        }
    }
}

/// 工具分类结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKind {
    /// Astrolabe 符号工具（find_symbol, find_references 等，重置计数）
    AstrolabeSymbolic,
    /// grep 类工具
    Grep,
    /// read 类工具，附带是否为代码文件判定
    Read { is_code_file: bool },
    /// 中立工具（Edit, Write, Bash 等非 grep/read 工具，不增不减计数）
    Neutral,
}

/// deny 类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DenyKind {
    Grep,
    Read,
    Mixed,
}

/// 决策结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// 位于静默窗口内，直接放行无输出且不修改状态
    Silenced,
    /// 符号工具，计数清零并保存，无输出
    ResetSymbolic,
    /// 中立工具，直接放行不写盘
    NeutralAllow,
    /// grep/read 调用未超阈值，更新计数并保存，无输出
    Allow,
    /// 达到阈值 deny，计数清零并记录 last_deny_ts，输出对应 deny JSON
    Deny(DenyKind),
}

/// 获取秒级 UNIX 时间戳
fn current_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// 读取用户主目录
fn get_user_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().and_then(|h| {
        let trimmed = h.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    })
}

/// 校验 session_id 可作为单一路径分量使用。
///
/// 拒绝空串、`/`、`\`、`..`，以及任何不在 `[A-Za-z0-9_-]` 内的字符。
pub(crate) fn sanitize_session_id(session_id: &str) -> Result<&str, &'static str> {
    if session_id.is_empty() {
        return Err("Session ID is empty");
    }
    if session_id == "/" || session_id == "\\" || session_id == ".." {
        return Err("Session ID is invalid");
    }
    if !session_id
        .chars()
        .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'))
    {
        return Err("Session ID contains invalid characters");
    }
    // Defense in depth: reject embedded ".." even though '.' is already disallowed.
    if session_id.contains("..") {
        return Err("Session ID is invalid");
    }
    Ok(session_id)
}

/// 构造会话持久化目录路径：~/.astrolabe/hook_data/<session_id>
///
/// `session_id` 必须通过 [`sanitize_session_id`]；失败时返回错误信息。
fn get_hook_data_dir(home: &Path, session_id: &str) -> Result<PathBuf, &'static str> {
    let session_id = sanitize_session_id(session_id)?;
    Ok(home.join(".astrolabe").join("hook_data").join(session_id))
}

/// 从磁盘读取 CounterState
pub(crate) fn load_counter(path: &Path) -> CounterState {
    if let Ok(file) = std::fs::File::open(path) {
        let reader = std::io::BufReader::new(file);
        if let Ok(counter) = serde_json::from_reader(reader) {
            return counter;
        }
    }
    CounterState::default()
}

/// 写回 CounterState 到磁盘（先写 `.tmp` 再 rename，保证原子替换）
pub(crate) fn save_counter(path: &Path, counter: &CounterState) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = {
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        PathBuf::from(tmp)
    };
    let write_ok = (|| -> std::io::Result<()> {
        let file = std::fs::File::create(&tmp_path)?;
        let writer = std::io::BufWriter::new(file);
        serde_json::to_writer(writer, counter).map_err(std::io::Error::other)?;
        Ok(())
    })();
    match write_ok {
        Ok(()) => {
            if std::fs::rename(&tmp_path, path).is_err() {
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
        Err(_) => {
            let _ = std::fs::remove_file(&tmp_path);
        }
    }
}

/// 独占锁文件：与 `counter.json` 并列的 `counter.json.lock`。
struct CounterFileLock {
    lock_path: PathBuf,
}

impl CounterFileLock {
    /// 在 counter 路径旁创建独占锁；短暂重试后仍失败则返回 None。
    fn acquire(counter_path: &Path) -> Option<Self> {
        if let Some(parent) = counter_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut lock_path = counter_path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);

        for _ in 0..200 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => {
                    // Keep the file handle until drop so the lock inode stays reserved;
                    // we only need create_new exclusivity, then close is fine after drop removes it.
                    drop(file);
                    return Some(Self { lock_path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        None
    }
}

impl Drop for CounterFileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// 在文件锁保护下执行 load → 回调 →（可选）save。
///
/// 若无法获取锁，仍降级执行一次（避免 hook 永久卡住），但不保证互斥。
fn with_counter_locked<F, R>(path: &Path, f: F) -> R
where
    F: FnOnce(CounterState) -> (CounterState, bool, R),
{
    let _guard = CounterFileLock::acquire(path);
    let counter = load_counter(path);
    let (counter, should_save, result) = f(counter);
    if should_save {
        save_counter(path, &counter);
    }
    result
}

/// 判定工具名是否为 Astrolabe 符号工具
///
/// tool_name 含 "astrolabe" 且不含子串
/// {pattern, search_code, diagnostics, languages, instructions, memory}
pub(crate) fn is_astrolabe_symbolic_tool(tool_name: &str) -> bool {
    tool_name.contains("astrolabe")
        && !NON_SYMBOLIC_ASTROLABE_SUBSTRINGS
            .iter()
            .any(|sub| tool_name.contains(sub))
}

/// 判定路径是否为代码文件
pub(crate) fn is_code_file_path(path: &str) -> bool {
    let cleaned = path.trim().trim_matches(|c| c == '\'' || c == '"');
    if cleaned.is_empty() {
        return false;
    }
    if let Some(ext) = Path::new(cleaned).extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        CODE_FILE_EXTENSIONS.contains(&ext_lower.as_str())
    } else {
        false
    }
}

/// 从 shell 命令行参数中提取非选项路径参数
pub(crate) fn iter_shell_path_arguments(args_str: &str) -> Vec<String> {
    args_str
        .split_whitespace()
        .filter_map(|raw| {
            let cleaned = raw.trim().trim_matches(|c| c == '\'' || c == '"');
            if cleaned.is_empty() || cleaned.starts_with('-') {
                None
            } else {
                Some(cleaned.to_string())
            }
        })
        .collect()
}

/// 判定 shell 命令行参数是否包含写重定向（>、>>、<<、<<<、>file、>>file 等）
///
/// 注意：< 与 <file 为输入重定向（属于读），不视为写重定向；-> 为文本箭头，不视为重定向。
fn has_write_redirection(args_str: &str) -> bool {
    args_str
        .split_whitespace()
        .any(|token| token.starts_with('>') || token.starts_with("<<"))
}

/// 判定 sed 命令参数是否包含原地写参数（-i、-i''、-i.bak、--in-place 等）
fn is_sed_in_place(args_str: &str) -> bool {
    args_str.split_whitespace().any(|token| {
        let cleaned = token.trim_matches(|c| c == '\'' || c == '"');
        cleaned.starts_with("-i") || cleaned.starts_with("--in-place")
    })
}

/// 从 tool_input JSON 字段提取 u64（接受 Number 或可解析为数字的 String）
fn json_value_as_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 解析纯数字字符串为 u64（拒绝 `+N`/`-N`/混入其它字符的形式：
/// 如 tail -n +5 表示从第 5 行打印到 EOF，不是有界切片）
fn parse_digits(s: &str) -> Option<u64> {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        s.parse::<u64>().ok()
    } else {
        None
    }
}

/// 解析 head/tail 参数中的行数：-n N、-nN、--lines=N（只接受有界行数，
/// 不接受 tail 的 `+N` 起始行形式——那会打印到 EOF，不是有界切片）
fn parse_head_tail_line_count(args_str: &str) -> Option<u64> {
    let tokens: Vec<&str> = args_str.split_whitespace().collect();
    for (i, raw) in tokens.iter().enumerate() {
        let cleaned = raw.trim_matches(|c| c == '\'' || c == '"');
        if cleaned == "-n" || cleaned == "--lines" {
            if let Some(next) = tokens.get(i + 1) {
                let n = next.trim_matches(|c| c == '\'' || c == '"');
                if let Some(v) = parse_digits(n) {
                    return Some(v);
                }
            }
        } else if let Some(rest) = cleaned.strip_prefix("--lines=") {
            if let Some(v) = parse_digits(rest) {
                return Some(v);
            }
        } else if cleaned.len() > 2 && cleaned.starts_with("-n") {
            if let Some(v) = parse_digits(&cleaned[2..]) {
                return Some(v);
            }
        }
    }
    None
}

/// 判定单个 sed 脚本 token 是否为"纯数字地址 + p 打印"命令（如 300,340p / 50p / 50p;80p）
///
/// 检查 sed 脚本是否为受限的切片打印 token（如 `'300,340p'`、`'50p'` 或 `'50p;80p'`）。
/// 仅放行纯数字单行或跨度 <= SLICE_READ_MAX_LIMIT 的明确行号区间；
/// 拒绝 `$p`、`/re/p`、`1,$p` 以及超限大范围倾倒。
fn is_sed_print_script_token(cleaned: &str) -> bool {
    if cleaned.contains(';') {
        let mut parts = cleaned.split(';');
        return parts.all(|part| is_single_sed_print_script_token(part.trim()));
    }
    is_single_sed_print_script_token(cleaned)
}

fn is_single_sed_print_script_token(s_raw: &str) -> bool {
    let Some(s) = s_raw.strip_suffix('p') else {
        return false;
    };
    if s.is_empty() {
        return false;
    }
    // 单行形式：如 "50p"
    if let Some(_line) = parse_digits(s) {
        return true;
    }
    // 区间形式：如 "300,340p"
    if let Some((start_str, end_str)) = s.split_once(',') {
        if let (Some(start), Some(end)) = (parse_digits(start_str), parse_digits(end_str)) {
            return end >= start && (end - start + 1) <= SLICE_READ_MAX_LIMIT;
        }
    }
    false
}

/// 判定 sed 参数是否为切片打印：带 -n（--quiet/--silent）静默且脚本含 p 打印命令，
/// 且不含 -i 原地写（如 `sed -n '300,340p' src/a.rs`）
fn is_sed_slice_print(args_str: &str) -> bool {
    if args_str.trim().is_empty() || is_sed_in_place(args_str) {
        return false;
    }
    let mut has_quiet = false;
    let mut has_print_script = false;
    for raw in args_str.split_whitespace() {
        let cleaned = raw.trim_matches(|c| c == '\'' || c == '"');
        if cleaned == "-n" || cleaned == "--quiet" || cleaned == "--silent" {
            has_quiet = true;
        } else if is_sed_print_script_token(cleaned) {
            has_print_script = true;
        }
    }
    has_quiet && has_print_script
}

/// 判定调用是否为局部切片阅读（slice read）——合法精读，不应计入 read 滥用
///
/// - 直接 read 类工具（tool_name 为 "read" 或含 "read_file"）：
///   - 必须显式带 limit 且 limit <= 120 → 切片精读；
///   - 仅有 offset、无合法 limit 时不算切片精读；
/// - shell 命令：
///   - `sed -n 'Np'` / `sed -n 'A,Bp'`（纯数字地址，不含 -i）→ 切片打印；
///   - `head -n N` / `tail -n N` 当 N <= 120 → 切片打印。
pub(crate) fn is_slice_read(
    tool_name: &str,
    command_name: Option<&str>,
    command_args_str: Option<&str>,
    limit: Option<u64>,
    _offset: Option<u64>,
) -> bool {
    let lower_name = tool_name.to_ascii_lowercase();

    // 1. 直接 read 类工具：仅按 limit 判定（offset 忽略）
    if lower_name == "read" || lower_name.contains("read_file") {
        // 切片阅读放行规则：必须显式带 limit 且 limit <= 120；
        // 不带 limit（即使有 offset）在 Claude Code 中默认会读 2000 行，不属于小范围切片。
        if let Some(l) = limit {
            if l <= SLICE_READ_MAX_LIMIT {
                return true;
            }
        }
        return false;
    }

    // 2. shell 命令切片打印
    if let Some(cmd) = command_name {
        let args = command_args_str.unwrap_or("");
        match cmd {
            "sed" => return is_sed_slice_print(args),
            "head" | "tail" => {
                return parse_head_tail_line_count(args)
                    .map(|n| n <= SLICE_READ_MAX_LIMIT)
                    .unwrap_or(false);
            }
            _ => {}
        }
    }

    false
}

/// 判定 read 工具调用是否针对代码文件
pub(crate) fn is_read_code_file_call(
    is_read: bool,
    file_path: Option<&str>,
    client: Client,
    command_args_str: Option<&str>,
) -> bool {
    if !is_read {
        return false;
    }

    if let Some(args_str) = command_args_str {
        if has_write_redirection(args_str) || is_sed_in_place(args_str) {
            return false;
        }
    }

    if let Some(fp) = file_path {
        return is_code_file_path(fp);
    }

    if matches!(
        client,
        Client::Codex | Client::Grok | Client::ClaudeCode | Client::Codebuddy
    ) {
        if let Some(args_str) = command_args_str {
            let args = iter_shell_path_arguments(args_str);
            return args.iter().any(|arg| is_code_file_path(arg));
        }
    }

    // 保守处理：其它客户端无明确路径信息时视为代码文件
    true
}

/// 工具分类函数
///
/// `limit` / `offset` 来自 tool_input 的切片参数（数字或数字字符串）。
/// 当前切片精读判定只使用 `limit`（须存在且 <= 120）；`offset` 保留传入以兼容调用方。
pub(crate) fn classify_tool(
    tool_name: &str,
    client: Client,
    file_path: Option<&str>,
    command_name: Option<&str>,
    command_args_str: Option<&str>,
    limit: Option<u64>,
    offset: Option<u64>,
) -> ToolKind {
    // 1. 先检查是否为 Astrolabe 符号工具
    if is_astrolabe_symbolic_tool(tool_name) {
        return ToolKind::AstrolabeSymbolic;
    }

    // 2. 检查是否为 grep 类工具
    let is_grep = match client {
        Client::ClaudeCode | Client::Codebuddy => {
            tool_name == "grep"
                || tool_name.contains("search_for_pattern")
                || command_name
                    .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Grok => {
            tool_name == "grep"
                || command_name
                    .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Codex => command_name
            .map(|cmd| GREP_SHELL_COMMANDS.contains(&cmd))
            .unwrap_or(false),
        Client::Vscode | Client::Other => tool_name.contains("grep"),
    };

    if is_grep {
        return ToolKind::Grep;
    }

    // 3. 检查是否为 read 类工具
    let is_read = match client {
        Client::ClaudeCode | Client::Codebuddy => {
            tool_name == "read"
                || tool_name.contains("read_file")
                || command_name
                    .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Grok => {
            tool_name == "read_file"
                || command_name
                    .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
                    .unwrap_or(false)
        }
        Client::Codex => command_name
            .map(|cmd| READ_SHELL_COMMANDS.contains(&cmd))
            .unwrap_or(false),
        Client::Vscode | Client::Other => {
            tool_name.contains("file")
                && READ_FILE_VERB_SUBSTRINGS
                    .iter()
                    .any(|verb| tool_name.contains(verb))
        }
    };

    if is_read {
        // 切片精读（slice read）：带合法 limit 的局部读取，或 head -n / sed -n 切片打印
        // 视为合法精读（中立），不计 read 滥用、不计 non_symbolic、不触发 deny
        if is_slice_read(tool_name, command_name, command_args_str, limit, offset) {
            return ToolKind::Neutral;
        }

        let is_write_redirection = command_args_str.map(has_write_redirection).unwrap_or(false);
        let is_sed_write =
            command_name == Some("sed") && command_args_str.map(is_sed_in_place).unwrap_or(false);

        if is_write_redirection || is_sed_write {
            return ToolKind::Neutral;
        }

        let is_code = is_read_code_file_call(true, file_path, client, command_args_str);
        return ToolKind::Read {
            is_code_file: is_code,
        };
    }

    ToolKind::Neutral
}

/// 纯逻辑决策函数：按输入状态与工具类型推进计数并生成决策
pub(crate) fn decide(counter: &mut CounterState, tool_kind: ToolKind, now: f64) -> Decision {
    // 1. 静默窗内 → no-op（allow，无输出更新）
    if !counter.is_hook_active(now) {
        return Decision::Silenced;
    }

    // 2. astrolabe 符号工具 → 重置 burst 计数，保留 last_deny_ts
    if tool_kind == ToolKind::AstrolabeSymbolic {
        counter.reset_burst();
        return Decision::ResetSymbolic;
    }

    // 3. 非追踪工具（既非 grep 类也非 read 类）→ 中立：不增不重置，直接 allow
    let (is_grep, is_code_file_read, is_read) = match tool_kind {
        ToolKind::Grep => (true, false, false),
        ToolKind::Read { is_code_file } => (false, is_code_file, true),
        _ => return Decision::NeutralAllow,
    };

    // 4. grep 类/read 类 → 按间隔-period 规则更新对应计数（及 non_symbolic 计数）
    if is_grep {
        if let Some(last_ts) = counter.last_grep_ts {
            if (now - last_ts) <= GREP_RESET_PERIOD_SECONDS {
                counter.n_grep += 1;
            } else {
                counter.n_grep = 1;
            }
        } else {
            counter.n_grep = 1;
        }
        counter.last_grep_ts = Some(now);
    }

    if is_code_file_read {
        if let Some(last_ts) = counter.last_read_ts {
            if (now - last_ts) <= READ_RESET_PERIOD_SECONDS {
                counter.n_read += 1;
            } else {
                counter.n_read = 1;
            }
        } else {
            counter.n_read = 1;
        }
        counter.last_read_ts = Some(now);
    }

    if is_grep || is_read {
        if let Some(last_ts) = counter.last_non_symbolic_ts {
            if (now - last_ts) <= NON_SYMBOLIC_RESET_PERIOD_SECONDS {
                counter.n_non_symbolic += 1;
            } else {
                counter.n_non_symbolic = 1;
            }
        } else {
            counter.n_non_symbolic = 1;
        }
        counter.last_non_symbolic_ts = Some(now);
    }

    // 阈值判定
    let too_many_greps = counter.n_grep >= GREP_THRESHOLD;
    let too_many_reads = counter.n_read >= READ_THRESHOLD;
    let too_many_non_symbolic = counter.n_non_symbolic >= NON_SYMBOLIC_THRESHOLD;

    // 依次判定：
    // 当前调用是 grep 类且 n_grep≥3 → grep deny；
    // 当前是 read-代码文件 且 n_read≥3 → read deny；
    // 否则 n_grep≥3 → grep deny；
    // n_read≥3 → read deny；
    // n_non_symbolic≥4 → mixed deny。
    let deny_kind = if is_grep && too_many_greps {
        Some(DenyKind::Grep)
    } else if is_code_file_read && too_many_reads {
        Some(DenyKind::Read)
    } else if too_many_greps {
        Some(DenyKind::Grep)
    } else if too_many_reads {
        Some(DenyKind::Read)
    } else if too_many_non_symbolic {
        Some(DenyKind::Mixed)
    } else {
        None
    };

    if let Some(kind) = deny_kind {
        // 触发即：burst 计数清零、记录 last_deny_ts
        counter.reset_burst();
        counter.last_deny_ts = Some(now);
        Decision::Deny(kind)
    } else {
        Decision::Allow
    }
}

/// 按 client 与 deny 种类构建单行 JSON 输出。
///
/// additionalContext 末尾统一带一句只读声明（实测：只读探索代理被拦时会把
/// 提醒误读为"与我只读任务冲突"而选择绕过——讲清楚 astrolabe 本身只读，
/// 就不是禁止探索而是给出更省 token 的探索方式）。
pub(crate) fn build_output(client: Client, deny_kind: DenyKind) -> String {
    const READONLY_NOTE: &str = " Note: all Astrolabe tools except apply_rename are read-only and safe for exploration tasks. Also note: slice reads with limit <= 120 (e.g. Read with offset and limit) are permitted and not counted as abuses.";
    let (reason, ctx) = match deny_kind {
        DenyKind::Grep => (
            "Too many consecutive grep calls without using symbolic tools. You can continue using grep now if needed, the counter was reset.",
            "You were using many grep calls recently. Consider using Astrolabe's symbolic mcp tools instead for more code-centric search (search_code / find_references are read-only and return path:line anchors). You can continue using grep now if needed, the counter was reset.",
        ),
        DenyKind::Read => (
            "Too many consecutive read calls of files without using symbolic tools. You can continue using read now if needed, the counter was reset.",
            "You were using many read calls on files recently. Consider using Astrolabe's symbolic mcp tools instead for more targeted reads (read-only exploration included: find_symbol / search_code return exact bodies and path:line anchors without whole-file reads). You can continue using read now if needed, the counter was reset.",
        ),
        DenyKind::Mixed => (
            "Too many consecutive non-symbolic tool calls (mixed grep and read). You can continue using these tools now if needed, the counter was reset.",
            "You were alternating between grep and read file calls recently without using Astrolabe's symbolic mcp tools. Consider using symbolic search and targeted symbol reads instead for more code-centric exploration. You can continue using these tools now if needed, the counter was reset.",
        ),
    };
    let ctx = format!("{ctx}{READONLY_NOTE}");

    match client {
        Client::Grok => serde_json::json!({
            "decision": "deny",
            "reason": reason,
        })
        .to_string(),
        Client::Codex => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        })
        .to_string(),
        Client::ClaudeCode | Client::Codebuddy | Client::Vscode | Client::Other => {
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                    "additionalContext": ctx,
                }
            })
            .to_string()
        }
    }
}

/// 清除特定 session 的数据目录（幂等）
///
/// `session_id` 非法时返回 `InvalidInput`；删除失败时向上传播 IO 错误。
pub(crate) fn cleanup_session(home: &Path, session_id: &str) -> std::io::Result<()> {
    let dir = get_hook_data_dir(home, session_id)
        .map_err(|msg| std::io::Error::new(std::io::ErrorKind::InvalidInput, msg))?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// PreToolUse remind hook 纯实现（支持依赖注入方便测试）
pub(crate) fn run_remind_impl<R: Read, W: Write, E: Write>(
    client_name: &str,
    mut reader: R,
    mut stdout: W,
    mut stderr: E,
    now: Option<f64>,
    home_override: Option<&Path>,
) -> i32 {
    let mut raw = String::new();
    if let Err(err) = reader.read_to_string(&mut raw) {
        let _ = writeln!(stderr, "Failed to read hook input from stdin: {err}");
        return 2;
    }

    if raw.trim().is_empty() {
        let _ = writeln!(stderr, "Hook input data is empty");
        return 2;
    }

    let input_data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(stderr, "Invalid JSON in hook input data: {err}");
            return 2;
        }
    };

    // 提取 session_id / sessionId
    let session_id = input_data
        .get("session_id")
        .or_else(|| input_data.get("sessionId"))
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });

    let session_id = match session_id {
        Some(s) => s,
        None => {
            let _ = writeln!(stderr, "Session ID is required in the hook input data");
            return 2;
        }
    };

    if let Err(msg) = sanitize_session_id(&session_id) {
        let _ = writeln!(stderr, "{msg}");
        return 2;
    }

    // 提取 tool_name / toolName
    let raw_tool_name = input_data
        .get("tool_name")
        .or_else(|| input_data.get("toolName"))
        .and_then(|v| v.as_str());

    let tool_name = match raw_tool_name {
        Some(s) if !s.trim().is_empty() => s.trim().to_ascii_lowercase(),
        _ => {
            let _ = writeln!(stderr, "Tool name is required in the hook input data");
            return 2;
        }
    };

    let client = Client::from_str(client_name);

    // 提取 tool_input 中的 file_path、切片参数（limit/offset）与 shell cmd
    let mut file_path: Option<String> = None;
    let mut command_name: Option<String> = None;
    let mut command_args_str: Option<String> = None;
    let mut tool_input_limit: Option<u64> = None;
    let mut tool_input_offset: Option<u64> = None;

    let raw_tool_input = input_data
        .get("tool_input")
        .or_else(|| input_data.get("toolInput"));

    if let Some(serde_json::Value::Object(map)) = raw_tool_input {
        // 取 file_path / filePath / target_file / targetFile
        let fp = map
            .get("file_path")
            .or_else(|| map.get("filePath"))
            .or_else(|| map.get("target_file"))
            .or_else(|| map.get("targetFile"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(fp_str) = fp {
            file_path = Some(fp_str.to_string());
        }

        // 取 limit / offset（数字或可解析为数字的字符串），用于切片精读判定
        tool_input_limit = map.get("limit").and_then(json_value_as_u64);
        tool_input_offset = map.get("offset").and_then(json_value_as_u64);

        // 取 cmd / command
        let cmd = map
            .get("cmd")
            .or_else(|| map.get("command"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(cmd_str) = cmd {
            if let Some(idx) = cmd_str.find(char::is_whitespace) {
                let first = &cmd_str[..idx];
                let rest = cmd_str[idx..].trim();
                let basename = Path::new(first)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(first)
                    .to_ascii_lowercase();
                command_name = Some(basename);
                if !rest.is_empty() {
                    command_args_str = Some(rest.to_string());
                }
            } else {
                let basename = Path::new(cmd_str)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(cmd_str)
                    .to_ascii_lowercase();
                command_name = Some(basename);
            }
        }
    }

    let tool_kind = classify_tool(
        &tool_name,
        client,
        file_path.as_deref(),
        command_name.as_deref(),
        command_args_str.as_deref(),
        tool_input_limit,
        tool_input_offset,
    );

    tracing::debug!(
        client = %client_name,
        session_id = %session_id,
        tool_name = %tool_name,
        ?tool_kind,
        "处理 PreToolUse hook 调用"
    );

    let now_ts = now.unwrap_or_else(current_timestamp);

    // 计算持久化路径（session_id 已 sanitize）
    let home = home_override.map(PathBuf::from).or_else(get_user_home);
    let persistence_path = match home.as_ref() {
        Some(h) => match get_hook_data_dir(h, &session_id) {
            Ok(dir) => Some(dir.join("counter.json")),
            Err(msg) => {
                let _ = writeln!(stderr, "{msg}");
                return 2;
            }
        },
        None => None,
    };

    // load → decide → save 在同一把文件锁内串行化
    let decision = match persistence_path.as_ref() {
        Some(path) => with_counter_locked(path, |mut counter| {
            let decision = decide(&mut counter, tool_kind, now_ts);
            let should_save = matches!(
                decision,
                Decision::ResetSymbolic | Decision::Allow | Decision::Deny(_)
            );
            (counter, should_save, decision)
        }),
        None => {
            let mut counter = CounterState::default();
            decide(&mut counter, tool_kind, now_ts)
        }
    };

    match decision {
        Decision::Silenced | Decision::NeutralAllow => {
            // 静默窗内或中立工具：不写盘，无输出
            0
        }
        Decision::ResetSymbolic => {
            // 符号工具：已清零 burst 计数并保存，无输出
            0
        }
        Decision::Allow => {
            // 正常累加计数未超阈值：已保存，无输出
            0
        }
        Decision::Deny(deny_kind) => {
            // 超阈值触发 deny：已清零并保存，输出 JSON
            let output_str = build_output(client, deny_kind);
            tracing::info!(?deny_kind, client = %client_name, "触发防漂移 deny 提示");
            let _ = writeln!(stdout, "{output_str}");
            0
        }
    }
}

/// SessionEnd cleanup hook 纯实现（支持依赖注入方便测试）
pub(crate) fn run_cleanup_impl<R: Read, E: Write>(
    mut reader: R,
    mut stderr: E,
    home_override: Option<&Path>,
) -> i32 {
    let mut raw = String::new();
    if let Err(err) = reader.read_to_string(&mut raw) {
        let _ = writeln!(stderr, "Failed to read hook input from stdin: {err}");
        return 2;
    }

    if raw.trim().is_empty() {
        let _ = writeln!(stderr, "Hook input data is empty");
        return 2;
    }

    let input_data: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            let _ = writeln!(stderr, "Invalid JSON in hook input data: {err}");
            return 2;
        }
    };

    let session_id = input_data
        .get("session_id")
        .or_else(|| input_data.get("sessionId"))
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        });

    let session_id = match session_id {
        Some(s) => s,
        None => {
            let _ = writeln!(stderr, "Session ID is required in the hook input data");
            return 2;
        }
    };

    if let Err(msg) = sanitize_session_id(&session_id) {
        let _ = writeln!(stderr, "{msg}");
        return 2;
    }

    let home = home_override.map(PathBuf::from).or_else(get_user_home);
    if let Some(h) = home {
        match cleanup_session(&h, &session_id) {
            Ok(()) => {
                tracing::debug!(session_id = %session_id, "SessionEnd cleanup 成功");
                0
            }
            Err(err) => {
                let _ = writeln!(stderr, "Failed to cleanup session data: {err}");
                2
            }
        }
    } else {
        // 无 HOME 时无法定位状态目录；不视为成功清理，但无可清理目标
        tracing::debug!(session_id = %session_id, "SessionEnd cleanup skipped: no home dir");
        0
    }
}

/// PreToolUse remind hook：stdin 读 hook payload JSON，按 client 输出决策 JSON。
/// 返回进程退出码（0 正常，包括 allow；2 输入不合法）。
pub fn run_remind(client_name: &str) -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_remind_impl(
        client_name,
        stdin.lock(),
        stdout.lock(),
        stderr.lock(),
        None,
        None,
    )
}

/// SessionEnd cleanup hook：按 stdin payload 的 session_id 删除状态目录。返回退出码。
pub fn run_cleanup() -> i32 {
    let stdin = std::io::stdin();
    let stderr = std::io::stderr();
    run_cleanup_impl(stdin.lock(), stderr.lock(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_1_consecutive_grep_deny_and_burst_reset() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        // 第 1 次 grep
        let d1 = decide(&mut counter, ToolKind::Grep, t0);
        assert_eq!(d1, Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_non_symbolic, 1);

        // 第 2 次 grep（间隔 1s）
        let d2 = decide(&mut counter, ToolKind::Grep, t0 + 1.0);
        assert_eq!(d2, Decision::Allow);
        assert_eq!(counter.n_grep, 2);
        assert_eq!(counter.n_non_symbolic, 2);

        // 第 3 次 grep（间隔 1s）-> 触发 grep deny
        let d3 = decide(&mut counter, ToolKind::Grep, t0 + 2.0);
        assert_eq!(d3, Decision::Deny(DenyKind::Grep));
        // burst 计数必须清零，且记录 last_deny_ts
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 0);
        assert_eq!(counter.last_grep_ts, None);
        assert_eq!(counter.last_deny_ts, Some(t0 + 2.0));
    }

    #[test]
    fn test_2_astrolabe_symbolic_resets_burst_counters() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        // 前 2 次 grep
        assert_eq!(decide(&mut counter, ToolKind::Grep, t0), Decision::Allow);
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 1.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);

        // 插入 Astrolabe 符号工具调用（如 find_references）
        let d_reset = decide(&mut counter, ToolKind::AstrolabeSymbolic, t0 + 2.0);
        assert_eq!(d_reset, Decision::ResetSymbolic);
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_non_symbolic, 0);

        // 之后第 3、4 次 grep 重新累积，仍为 allow
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 3.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 4.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);
    }

    #[test]
    fn test_3_search_code_does_not_reset_counter() {
        // search_code / pattern / diagnostics 等非导航类工具不应重置符号计数
        assert!(!is_astrolabe_symbolic_tool("mcp__astrolabe__search_code"));
        assert!(!is_astrolabe_symbolic_tool("astrolabe_pattern"));
        assert!(!is_astrolabe_symbolic_tool(
            "mcp__astrolabe__get_diagnostics"
        ));
        assert!(!is_astrolabe_symbolic_tool("mcp__astrolabe__get_languages"));
        assert!(!is_astrolabe_symbolic_tool(
            "mcp__astrolabe__ensure_language_server"
        ));
        assert!(!is_astrolabe_symbolic_tool(
            "mcp__astrolabe__initial_instructions"
        ));

        // 导航类工具正常重置
        assert!(is_astrolabe_symbolic_tool("mcp__astrolabe__find_symbol"));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__find_references"
        ));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__goto_definition"
        ));
        assert!(is_astrolabe_symbolic_tool(
            "mcp__astrolabe__resolve_context"
        ));
        assert!(is_astrolabe_symbolic_tool("mcp__astrolabe__plan_rename"));
        assert!(is_astrolabe_symbolic_tool("mcp__astrolabe__apply_rename"));

        let kind = classify_tool(
            "mcp__astrolabe__search_code",
            Client::ClaudeCode,
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(kind, ToolKind::Neutral);

        let mut counter = CounterState {
            n_grep: 2,
            last_grep_ts: Some(100.0),
            ..CounterState::default()
        };

        // 调用 search_code，计数不重置
        let d = decide(&mut counter, kind, 101.0);
        assert_eq!(d, Decision::NeutralAllow);
        assert_eq!(counter.n_grep, 2);
    }

    #[test]
    fn test_4_read_non_code_file_no_read_deny_counts_non_symbolic() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        let non_code_kind = ToolKind::Read {
            is_code_file: false,
        };

        // 连续 3 次读取 .md 文件（非代码文件）
        assert_eq!(decide(&mut counter, non_code_kind, t0), Decision::Allow);
        assert_eq!(counter.n_read, 0); // 不计入 read 计数
        assert_eq!(counter.n_non_symbolic, 1); // 计入 non_symbolic

        assert_eq!(
            decide(&mut counter, non_code_kind, t0 + 1.0),
            Decision::Allow
        );
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 2);

        // 第 3 次：不触发 read deny
        assert_eq!(
            decide(&mut counter, non_code_kind, t0 + 2.0),
            Decision::Allow
        );
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 3);

        // 第 4 次：non_symbolic 达到 4 阈值，触发 mixed deny
        let d4 = decide(&mut counter, non_code_kind, t0 + 3.0);
        assert_eq!(d4, Decision::Deny(DenyKind::Mixed));
        assert_eq!(counter.n_non_symbolic, 0);
    }

    #[test]
    fn test_5_mixed_grep_and_read_burst_triggers_mixed_deny() {
        let mut counter = CounterState::default();
        let t0 = 1000.0;
        let code_read = ToolKind::Read { is_code_file: true };

        // 4 次交替调用：grep -> read -> grep -> read
        assert_eq!(decide(&mut counter, ToolKind::Grep, t0), Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 1);

        assert_eq!(decide(&mut counter, code_read, t0 + 1.0), Decision::Allow);
        assert_eq!(counter.n_grep, 1);
        assert_eq!(counter.n_read, 1);
        assert_eq!(counter.n_non_symbolic, 2);

        assert_eq!(
            decide(&mut counter, ToolKind::Grep, t0 + 2.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 2);
        assert_eq!(counter.n_read, 1);
        assert_eq!(counter.n_non_symbolic, 3);

        // 第 4 次到达 non_symbolic=4，触发 mixed deny
        let d4 = decide(&mut counter, code_read, t0 + 3.0);
        assert_eq!(d4, Decision::Deny(DenyKind::Mixed));
        assert_eq!(counter.n_grep, 0);
        assert_eq!(counter.n_read, 0);
        assert_eq!(counter.n_non_symbolic, 0);
        assert_eq!(counter.last_deny_ts, Some(t0 + 3.0));
    }

    #[test]
    fn test_6_silent_window_within_120s() {
        let mut counter = CounterState {
            last_deny_ts: Some(1000.0),
            ..CounterState::default()
        };

        // 50s 后调用处于 120s 静默窗口内，应当为 Silenced，不更新计数
        let d1 = decide(&mut counter, ToolKind::Grep, 1050.0);
        assert_eq!(d1, Decision::Silenced);
        assert_eq!(counter.n_grep, 0);

        // 120s 后调用离开静默窗口，正常放行并更新计数
        let d2 = decide(&mut counter, ToolKind::Grep, 1120.0);
        assert_eq!(d2, Decision::Allow);
        assert_eq!(counter.n_grep, 1);
    }

    #[test]
    fn test_7_interval_greater_than_1000s_resets_counter() {
        let mut counter = CounterState::default();
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, 1000.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);

        // 超过 1000s（GREP_RESET_PERIOD_SECONDS）后再次 grep，计数重置为 1 而非递增至 2
        assert_eq!(
            decide(&mut counter, ToolKind::Grep, 2001.0),
            Decision::Allow
        );
        assert_eq!(counter.n_grep, 1);
    }

    #[test]
    fn test_8_client_output_formats() {
        // 1. Claude Code
        let cc_out = build_output(Client::ClaudeCode, DenyKind::Grep);
        let cc_val: serde_json::Value = serde_json::from_str(&cc_out).unwrap();
        assert_eq!(cc_val["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(cc_val["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(cc_val["hookSpecificOutput"]["permissionDecisionReason"].is_string());
        assert!(cc_val["hookSpecificOutput"]["additionalContext"].is_string());

        // 2. Codex（无 additionalContext）
        let codex_out = build_output(Client::Codex, DenyKind::Read);
        let codex_val: serde_json::Value = serde_json::from_str(&codex_out).unwrap();
        assert_eq!(
            codex_val["hookSpecificOutput"]["hookEventName"],
            "PreToolUse"
        );
        assert_eq!(
            codex_val["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert!(codex_val["hookSpecificOutput"]["permissionDecisionReason"].is_string());
        assert!(codex_val["hookSpecificOutput"]
            .get("additionalContext")
            .is_none());

        // 3. Grok（decision / reason）
        let grok_out = build_output(Client::Grok, DenyKind::Mixed);
        let grok_val: serde_json::Value = serde_json::from_str(&grok_out).unwrap();
        assert_eq!(grok_val["decision"], "deny");
        assert!(grok_val["reason"].is_string());
        assert!(grok_val.get("hookSpecificOutput").is_none());
    }

    #[test]
    fn test_9_cleanup_session_is_idempotent() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_hook_test_{rand_id}"));
        let sess_dir = get_hook_data_dir(&temp_home, "sess_123").unwrap();
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::write(sess_dir.join("counter.json"), "{}").unwrap();
        assert!(sess_dir.exists());

        // 第一次删除
        let res1 = cleanup_session(&temp_home, "sess_123");
        assert!(res1.is_ok());
        assert!(!sess_dir.exists());

        // 第二次删除不存在的目录（幂等）
        let res2 = cleanup_session(&temp_home, "sess_123");
        assert!(res2.is_ok());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_10_missing_session_id_returns_exit_code_2() {
        let input_no_session = r#"{"tool_name": "grep"}"#;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            input_no_session.as_bytes(),
            &mut out,
            &mut err,
            None,
            None,
        );
        assert_eq!(code, 2);
        let err_str = String::from_utf8_lossy(&err);
        assert!(err_str.contains("Session ID is required"));

        // cleanup 同样要求 session_id
        let mut clean_err = Vec::new();
        let clean_code = run_cleanup_impl("{}".as_bytes(), &mut clean_err, None);
        assert_eq!(clean_code, 2);
        let clean_err_str = String::from_utf8_lossy(&clean_err);
        assert!(clean_err_str.contains("Session ID is required"));
    }

    #[test]
    fn test_sanitize_session_id_accepts_safe_ids() {
        assert_eq!(sanitize_session_id("sess_123").unwrap(), "sess_123");
        assert_eq!(sanitize_session_id("abc-XYZ_09").unwrap(), "abc-XYZ_09");
        assert_eq!(
            sanitize_session_id("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_sanitize_session_id_rejects_unsafe_ids() {
        assert!(sanitize_session_id("").is_err());
        assert!(sanitize_session_id("/").is_err());
        assert!(sanitize_session_id("\\").is_err());
        assert!(sanitize_session_id("..").is_err());
        assert!(sanitize_session_id("../etc").is_err());
        assert!(sanitize_session_id("a/b").is_err());
        assert!(sanitize_session_id("a\\b").is_err());
        assert!(sanitize_session_id("has space").is_err());
        assert!(sanitize_session_id("dot.dot").is_err());
        assert!(get_hook_data_dir(Path::new("/tmp"), "../x").is_err());
    }

    #[test]
    fn test_invalid_session_id_remind_and_cleanup_exit_2() {
        let payload = r#"{"session_id":"../evil","tool_name":"grep","tool_input":{}}"#;
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            None,
            None,
        );
        assert_eq!(code, 2);
        let err_str = String::from_utf8_lossy(&err);
        assert!(
            err_str.contains("invalid")
                || err_str.contains("Invalid")
                || err_str.contains("characters"),
            "stderr={err_str}"
        );

        let mut clean_err = Vec::new();
        let clean_code = run_cleanup_impl(&br#"{"session_id":"a/b"}"#[..], &mut clean_err, None);
        assert_eq!(clean_code, 2);
    }

    #[test]
    fn test_save_counter_atomic_writes_via_tmp_rename() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("astrolabe_atomic_save_{rand_id}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("counter.json");

        let state = CounterState {
            n_grep: 7,
            n_read: 2,
            ..CounterState::default()
        };
        save_counter(&path, &state);
        assert!(path.exists());
        assert!(!PathBuf::from(format!("{}.tmp", path.display())).exists());

        let loaded = load_counter(&path);
        assert_eq!(loaded.n_grep, 7);
        assert_eq!(loaded.n_read, 2);

        // Locked load→mutate→save round-trip
        with_counter_locked(&path, |mut c| {
            c.n_grep += 1;
            (c, true, ())
        });
        let loaded2 = load_counter(&path);
        assert_eq!(loaded2.n_grep, 8);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_end_to_end_remind_flow_with_tempdir() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_e2e_test_{rand_id}"));
        let sess_id = "e2e_sess";

        let payload_grep = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "grep",
            "tool_input": {}
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st grep
        let code1 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(code1, 0);
        assert!(out.is_empty());

        // 2nd grep
        out.clear();
        let code2 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(code2, 0);
        assert!(out.is_empty());

        // 3rd grep -> Deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));

        // 4th grep at 150.0 (in silent window) -> Allow / Silenced (no output)
        out.clear();
        let code4 = run_remind_impl(
            "claude-code",
            payload_grep.as_bytes(),
            &mut out,
            &mut err,
            Some(150.0),
            Some(&temp_home),
        );
        assert_eq!(code4, 0);
        assert!(out.is_empty());

        // cleanup
        let clean_payload = serde_json::json!({ "session_id": sess_id }).to_string();
        let mut clean_err = Vec::new();
        let clean_code =
            run_cleanup_impl(clean_payload.as_bytes(), &mut clean_err, Some(&temp_home));
        assert_eq!(clean_code, 0);

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_11_claude_code_bash_grep_denies_on_3rd() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_grep_{rand_id}"));
        let sess_id = "cc_bash_grep_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "grep -rn foo src/"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st call
        let c1 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(c1, 0);
        assert!(out.is_empty());

        // 2nd call
        out.clear();
        let c2 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(c2, 0);
        assert!(out.is_empty());

        // 3rd call -> deny
        out.clear();
        let c3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(c3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_12_claude_code_bash_rg_path_prefix() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_rg_{rand_id}"));
        let sess_id = "cc_bash_rg_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "/usr/bin/rg pattern"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 1st call
        let c1 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(100.0),
            Some(&temp_home),
        );
        assert_eq!(c1, 0);
        assert!(out.is_empty());

        // 2nd call
        out.clear();
        let c2 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(101.0),
            Some(&temp_home),
        );
        assert_eq!(c2, 0);
        assert!(out.is_empty());

        // 3rd call -> deny
        out.clear();
        let c3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(c3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_13_claude_code_bash_neutral_command_no_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_neutral_{rand_id}"));
        let sess_id = "cc_bash_neutral_sess";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "ls -la"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 5 次中性命令
        for i in 0..5 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "第 {} 次调用不应产生 deny 输出", i + 1);
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_14_claude_code_bash_cat_code_vs_non_code() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_bash_cat_{rand_id}"));

        // Case 1: cat src/main.rs (代码文件) 连续 3 次 -> 第 3 次 read deny
        let code_payload = serde_json::json!({
            "session_id": "sess_cat_code",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat src/main.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                code_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次触发 read deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            code_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        // Case 2: cat README.md (非代码文件) 连续 3 次 -> 不触发 read deny
        let doc_payload = serde_json::json!({
            "session_id": "sess_cat_doc",
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat README.md"
            }
        })
        .to_string();

        for i in 0..3 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                doc_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "README.md 读取不应触发 read deny");
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_15_codex_grok_bash_regression() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_codex_grok_{rand_id}"));

        // Codex: grep shell command 3 times -> codex deny format
        let codex_payload = serde_json::json!({
            "session_id": "sess_codex",
            "tool_name": "bash",
            "tool_input": {
                "command": "grep pattern lib.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            assert_eq!(
                run_remind_impl(
                    "codex",
                    codex_payload.as_bytes(),
                    &mut out,
                    &mut err,
                    Some(100.0 + i as f64),
                    Some(&temp_home)
                ),
                0
            );
            assert!(out.is_empty());
        }

        out.clear();
        assert_eq!(
            run_remind_impl(
                "codex",
                codex_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(102.0),
                Some(&temp_home)
            ),
            0
        );
        let codex_out_str = String::from_utf8_lossy(&out);
        let codex_val: serde_json::Value = serde_json::from_str(&codex_out_str).unwrap();
        assert_eq!(
            codex_val["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert!(codex_val["hookSpecificOutput"]
            .get("additionalContext")
            .is_none());

        // Grok: cat code file 3 times -> grok deny format
        let grok_payload = serde_json::json!({
            "session_id": "sess_grok",
            "tool_name": "bash",
            "tool_input": {
                "command": "cat src/lib.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            assert_eq!(
                run_remind_impl(
                    "grok",
                    grok_payload.as_bytes(),
                    &mut out,
                    &mut err,
                    Some(200.0 + i as f64),
                    Some(&temp_home)
                ),
                0
            );
            assert!(out.is_empty());
        }

        out.clear();
        assert_eq!(
            run_remind_impl(
                "grok",
                grok_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(202.0),
                Some(&temp_home)
            ),
            0
        );
        let grok_out_str = String::from_utf8_lossy(&out);
        let grok_val: serde_json::Value = serde_json::from_str(&grok_out_str).unwrap();
        assert_eq!(grok_val["decision"], "deny");
        assert!(grok_val["reason"].is_string());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_16_has_write_redirection_and_sed_in_place_unit() {
        // 写重定向 token 判定
        assert!(has_write_redirection("> src/main.rs <<'EOF'"));
        assert!(has_write_redirection("x >> src/a.rs"));
        assert!(has_write_redirection(">file.txt"));
        assert!(has_write_redirection(">>file.txt"));
        assert!(has_write_redirection("<<EOF"));
        assert!(has_write_redirection("<<'EOF'"));
        assert!(has_write_redirection("<<<\"test\""));

        // 输入重定向与普通参数不应判定为写重定向
        assert!(!has_write_redirection("< src/main.rs"));
        assert!(!has_write_redirection("<src/main.rs"));
        assert!(!has_write_redirection("src/main.rs -> other.rs"));
        assert!(!has_write_redirection("src/main.rs"));
        assert!(!has_write_redirection(""));

        // sed 原地写参数判定
        assert!(is_sed_in_place("-i 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("-i'' 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("-i.bak 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("--in-place 's/a/b/' src/a.rs"));
        assert!(is_sed_in_place("--in-place=.bak 's/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("'s/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("-e 's/a/b/' src/a.rs"));
        assert!(!is_sed_in_place("-n 's/a/b/' src/a.rs"));
    }

    #[test]
    fn test_17_shell_write_redirection_cat_heredoc_no_deny() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cat_heredoc_{rand_id}"));
        let sess_id = "sess_cat_heredoc";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat > src/main.rs <<'EOF'"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 4 次 heredoc 写操作，不应触发 read deny 或 mixed deny（视为中性命令）
        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次 heredoc 写操作不应产生 deny 输出",
                i + 1
            );
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_18_shell_write_redirection_echo_append_no_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_echo_append_{rand_id}"));
        let sess_id = "sess_echo_append";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "echo x >> src/a.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        // 连续 4 次 echo 追加写操作，不计 read，不触发 deny
        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次 echo 追加操作不应产生 deny 输出",
                i + 1
            );
        }

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_19_shell_input_redirection_cat_counts_as_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cat_input_{rand_id}"));
        let sess_id = "sess_cat_input";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "cat < src/main.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次输入重定向 cat 读取代码文件 -> 触发 read deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_20_shell_sed_inplace_vs_stdout_read() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_sed_test_{rand_id}"));

        // Case 1: sed -i 原地修改（含变体 -i''、-i.bak、--in-place）不计 read
        let sed_i_payload = serde_json::json!({
            "session_id": "sess_sed_i",
            "tool_name": "Bash",
            "tool_input": {
                "command": "sed -i 's/a/b/' src/a.rs"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..4 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                sed_i_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "sed -i 不应计为 read");
        }

        // sed 变体：-i''、-i.bak、--in-place 均不触发 deny
        for (i, cmd) in [
            "sed -i'' 's/a/b/' src/a.rs",
            "sed -i.bak 's/a/b/' src/a.rs",
            "sed --in-place 's/a/b/' src/a.rs",
            "sed --in-place=.bak 's/a/b/' src/a.rs",
        ]
        .into_iter()
        .enumerate()
        {
            let payload = serde_json::json!({
                "session_id": "sess_sed_variants",
                "tool_name": "Bash",
                "tool_input": { "command": cmd }
            })
            .to_string();
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty(), "sed 原地变体不应计为 read: {cmd}");
        }

        // Case 2: sed 's/a/b/' src/a.rs（无 -i，stdout 读）连续 3 次 -> 触发 read deny
        let sed_read_payload = serde_json::json!({
            "session_id": "sess_sed_read",
            "tool_name": "Bash",
            "tool_input": {
                "command": "sed 's/a/b/' src/a.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                sed_read_payload.as_bytes(),
                &mut out,
                &mut err,
                Some(300.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            sed_read_payload.as_bytes(),
            &mut out,
            &mut err,
            Some(302.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_21_shell_grep_with_redirection_still_counts_grep() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_grep_redir_{rand_id}"));
        let sess_id = "sess_grep_redir";

        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Bash",
            "tool_input": {
                "command": "grep -n foo src/a.rs > out.txt"
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        // 第 3 次 grep（即使命令行含重定向 > out.txt）仍照常触发 grep deny
        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive grep calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_claude_code_read_with_limit_is_neutral() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_read_slice_{rand_id}"));
        let sess_id = "cc_read_slice_sess";

        // 单元层：Read + offset/limit 局部切片 → Neutral
        let kind = classify_tool(
            "read",
            Client::ClaudeCode,
            Some("src/main.rs"),
            None,
            None,
            Some(40),
            Some(300),
        );
        assert_eq!(kind, ToolKind::Neutral);

        // e2e：连续 5 次 Read(offset=300, limit=40) 全部 NeutralAllow，无 deny 输出
        let payload = serde_json::json!({
            "session_id": sess_id,
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs",
                "offset": 300,
                "limit": 40
            }
        })
        .to_string();

        let mut out = Vec::new();
        let mut err = Vec::new();

        for i in 0..5 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(
                out.is_empty(),
                "第 {} 次切片 Read 不应产生 deny 输出",
                i + 1
            );
        }

        // 计数不增加：NeutralAllow 不写盘，counter.json 保持缺省（全 0、无 deny）
        let counter_path = get_hook_data_dir(&temp_home, sess_id)
            .unwrap()
            .join("counter.json");
        let counter = load_counter(&counter_path);
        assert_eq!(counter, CounterState::default());

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_claude_code_read_without_limit_still_denies() {
        let rand_id = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_home = std::env::temp_dir().join(format!("astrolabe_cc_read_full_{rand_id}"));

        let mut out = Vec::new();
        let mut err = Vec::new();

        // Case 1: 未传 limit/offset（整文件盲读）连续 3 次 -> 第 3 次 read deny
        let payload_full = serde_json::json!({
            "session_id": "cc_read_full_sess",
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs"
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_full.as_bytes(),
                &mut out,
                &mut err,
                Some(100.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code3 = run_remind_impl(
            "claude-code",
            payload_full.as_bytes(),
            &mut out,
            &mut err,
            Some(102.0),
            Some(&temp_home),
        );
        assert_eq!(code3, 0);
        let out_str = String::from_utf8_lossy(&out);
        assert!(out_str.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str.contains("Too many consecutive read calls"));

        // Case 2: limit=500 > 120（大块读取）连续 3 次 -> 依然触发 read deny
        let payload_big = serde_json::json!({
            "session_id": "cc_read_big_sess",
            "tool_name": "Read",
            "tool_input": {
                "file_path": "src/main.rs",
                "offset": 1,
                "limit": 500
            }
        })
        .to_string();

        for i in 0..2 {
            out.clear();
            let code = run_remind_impl(
                "claude-code",
                payload_big.as_bytes(),
                &mut out,
                &mut err,
                Some(200.0 + i as f64),
                Some(&temp_home),
            );
            assert_eq!(code, 0);
            assert!(out.is_empty());
        }

        out.clear();
        let code6 = run_remind_impl(
            "claude-code",
            payload_big.as_bytes(),
            &mut out,
            &mut err,
            Some(202.0),
            Some(&temp_home),
        );
        assert_eq!(code6, 0);
        let out_str6 = String::from_utf8_lossy(&out);
        assert!(out_str6.contains("\"permissionDecision\":\"deny\""));
        assert!(out_str6.contains("Too many consecutive read calls"));

        let _ = std::fs::remove_dir_all(&temp_home);
    }

    #[test]
    fn test_shell_slice_print_and_is_slice_read_unit() {
        let code_read = ToolKind::Read { is_code_file: true };

        // head/tail -n N（N <= 120）切片打印 → Neutral
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n 40 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("tail"),
                Some("-n 120 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n40 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("--lines=50 src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );

        // N > 120、无 -n、tail -n +N（打印到 EOF，非有界切片）→ 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("-n 500 src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("head"),
                Some("src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("tail"),
                Some("-n +5 src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // sed -n '地址p' 切片打印 → Neutral
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n '300,340p' src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n '50p;80p' src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n 300,340p src/a.rs"),
                None,
                None
            ),
            ToolKind::Neutral
        );

        // sed 无 -n 或脚本无 p 打印命令 → 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("'s/a/b/' src/a.rs"),
                None,
                None
            ),
            code_read
        );
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("sed"),
                Some("-n 's/a/b/' src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // cat 不属于切片命令 → 仍计 Read
        assert_eq!(
            classify_tool(
                "bash",
                Client::ClaudeCode,
                None,
                Some("cat"),
                Some("src/a.rs"),
                None,
                None
            ),
            code_read
        );

        // is_slice_read 直接工具判定：必须显式带 limit 且 limit <= 120
        assert!(is_slice_read("Read", None, None, Some(120), None));
        assert!(is_slice_read("read", None, None, Some(40), Some(300)));
        assert!(!is_slice_read("read", None, None, Some(121), None));
        assert!(!is_slice_read("read_file", None, None, None, Some(2))); // 无 limit 不放行
        assert!(is_slice_read("read_file", None, None, Some(40), Some(2)));
        assert!(!is_slice_read("read", None, None, None, Some(1)));
        assert!(!is_slice_read("read", None, None, None, None));
        assert!(!is_slice_read("read", None, None, Some(500), Some(300)));

        // json_value_as_u64：Number / 数字字符串 / 非数字
        assert_eq!(json_value_as_u64(&serde_json::json!(40)), Some(40));
        assert_eq!(json_value_as_u64(&serde_json::json!("40")), Some(40));
        assert_eq!(json_value_as_u64(&serde_json::json!(" 120 ")), Some(120));
        assert_eq!(json_value_as_u64(&serde_json::json!("abc")), None);
        assert_eq!(json_value_as_u64(&serde_json::json!(true)), None);
        assert_eq!(json_value_as_u64(&serde_json::json!(null)), None);
    }
}
