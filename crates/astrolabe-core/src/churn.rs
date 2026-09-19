//! Git 提交频率统计（churn）与热点融合评分。
//!
//! 对齐 OpenVisio 的 `computeChurn`/`buildHotspots`：热点 = 「承重」×「正在
//! 变化」。承重由 [`crate::graph::compute_centrality`] 的 PageRank 静态中心性
//! 给出（可缓存、确定性）；正在变化由本模块从**本地** git 历史读取（无网络、
//! 无新依赖，子进程调 `git` 而非 git2）。
//!
//! 两条设计契约：
//!
//! 1. **优雅降级，绝不 Err 阻流。** `root` 不是 git 仓库 / `git` 二进制不存
//!    在 / 命令退出非 0 / 超过 10 秒超时，[`compute_churn`] 一律返回空表，
//!    调用方（如 `get_hotspots`）退回纯中心性排序——与 OpenVisio「无 git
//!    checkout 时 hotspots 退化为纯 centrality」一致。
//! 2. **churn 天然带时间窗。** 中心性对同一份仓库字节是确定的；churn 随历
//!    史推进逐日变化，热点排序因此可能日日不同——这是特性而非缺陷，公开记
//!    录在此。
//!
//! 路径口径：表键是 **astrolabe root 相对路径**。`git log --name-only` 输出
//! 的是 git 顶层相对路径；当 root 是仓库的子目录时，[`compute_churn`] 先用
//! `git rev-parse --show-prefix` 取得 root 在仓库内的前缀，把每个文件路径
//! 剥成 root 相对（不属于 root 子树的文件直接丢弃），与调用方（server 侧
//! `file.path`，同为 root 相对）的查表口径一致。

use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 提交头与各字段之间的分隔符（`\x1f`，git `%x1f` 展开；文件名里不可能
/// 合法出现，可安全当记录边界）。
const UNIT_SEP: char = '\u{1f}';
const DAY_SECS: i64 = 86400;
const WINDOW_30D_SECS: i64 = 30 * DAY_SECS;
const WINDOW_90D_SECS: i64 = 90 * DAY_SECS;
/// git 子进程硬超时。大仓库 `git log` 也不应超过这个量级；超时按失败降级。
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// try_wait 轮询间隔。20ms 对 10s 超时的精度足够，轮询开销可忽略。
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 单文件的 git 变更统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChurnStats {
    pub commits_30d: u32,
    pub commits_90d: u32,
    /// 90 天窗口内触过该文件的独立作者数。
    pub authors_90d: u32,
}

/// churn 表（astrolabe root 相对路径 → 统计）。
pub type ChurnTable = HashMap<String, ChurnStats>;

/// 统计过去 90 天每个文件的提交次数与作者数。
///
/// 子进程共两次，复用同一超时/降级管道：
/// 1. `git -C <root> rev-parse --show-prefix` —— root 在 git 顶层内的相对
///    前缀（root 即顶层时为空串）；
/// 2. `git -C <root> log --since=90.days --name-only
///    --pretty=format:%x1f%at%x1f%an`（`\x1f` 分隔 unix 时间戳与作者名，
///    `--name-only` 随后逐行给出该提交**git 顶层相对**的文件路径）。
///
/// log 给出的路径随后按前缀换算成 root 相对（[`rebase_path`]）作为表键，
/// 不属于 root 子树的文件丢弃——否则 root 为子目录时 server 侧按 root 相
/// 对 `file.path` 查表会因前缀不一致永远 miss。30/90 天分窗在 [`parse_log`]
/// 内按时间戳自行完成，因此解析器可以脱离 git 单测。
///
/// 降级契约（返回空表、绝不 Err，见模块文档）：
/// * `root` 非 git 仓库或 `git` 不存在 → show-prefix 的 spawn/退出码失败
///   → 空表（show-prefix 先于 log 跑，非 git root 在第一步即降级）；
/// * 两次子进程共享同一 10 秒 deadline（合计最坏 ≤10s）→ 超时 kill → 空表；
/// * 同一提交内同名文件只计 1 次 commit（先按提交去重再累计）。
///
/// 已知简化：git 对含特殊字符的路径会做 C 风格加引号转义（`core.quotePath`），
/// 这里只剥掉首尾双引号、跳过内部仍含引号/反斜杠的行；非 UTF-8 字节经
/// `from_utf8_lossy` 有损转换。
pub fn compute_churn(root: &Path) -> ChurnTable {
    // show-prefix 与 log 共用一个 deadline，最坏合计 ≤ GIT_TIMEOUT，而非各 10s。
    let deadline = Instant::now() + GIT_TIMEOUT;
    let Some(prefix_raw) = run_git(root, &["rev-parse", "--show-prefix"], deadline) else {
        return ChurnTable::new();
    };
    let prefix = prefix_raw.trim();

    let args = [
        "log",
        "--since=90.days",
        "--name-only",
        "--pretty=format:%x1f%at%x1f%an",
    ];
    let Some(raw) = run_git(root, &args, deadline) else {
        return ChurnTable::new();
    };
    // 表键换算是单射（前缀 + root 相对键 = 原路径），留下的键互不碰撞。
    parse_log(&raw, unix_now())
        .into_iter()
        .filter_map(|(path, stats)| rebase_path(&path, prefix).map(|key| (key, stats)))
        .collect()
}

/// 把 git 顶层相对路径换算成 astrolabe root 相对路径（表键口径）。
///
/// `prefix` 是 `git rev-parse --show-prefix` 的输出：root 位于仓库顶层时为
/// 空串（原样放行），否则是形如 `crates/mcp/` 的相对前缀。匹配按 `/` 分隔
/// 的完整路径组件进行：不以 `/` 结尾的非空前缀（防御性分支，git 正常输出
/// 总带 `/`）先补上组件边界再比较，避免 `a/b` 假命中 `a/bc.rs`；剥完只剩
/// 空串（路径恰好等于前缀目录本身）与不以该前缀开头的路径（同仓库但不在
/// root 子树内）一样返回 `None` 丢弃。
fn rebase_path(path: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(path.to_string());
    }
    let rest = match path.strip_prefix(prefix) {
        Some(rest) if prefix.ends_with('/') => rest,
        Some(rest) => rest.strip_prefix('/')?,
        None => return None,
    };
    if rest.is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// 融合分：中心性 × churn 增益。可解释公式：
/// `score = centrality_rank_score * (1 + (commits_90d as f64).ln_1p())`。
///
/// 对数增益让 churn 只做乘性放大而不抢占排序主导权——对齐 OpenVisio 用
/// 饱和函数 `commits/(commits+K)` 压制小计数噪声的意图：年轻仓库里最热
/// 文件也只有几次提交时，中心性排序仍成立，只有真正高频变更才显著加分。
///
/// * churn 缺失（表里没有该文件，`None`）→ 增益 1，原样返回中心性，
///   向后兼容无 git 环境；
/// * `commits_90d == 0` → `ln_1p(0) = 0`，同样原样返回；
/// * `centrality_rank_score` 就是调用方传入的中心性值本身，本函数只做乘法。
pub fn fuse_hotspot_score(centrality: f64, churn: Option<&ChurnStats>) -> f64 {
    let Some(stats) = churn else {
        return centrality;
    };
    centrality * (1.0 + f64::ln_1p(stats.commits_90d as f64))
}

/// 解析 `git log --pretty=format:%x1f%at%x1f%an --name-only` 的原始输出。
///
/// 记录形态（`\x1f` 显示为 `^_`）：
///
/// ```text
/// ^_<unix ts>^_<author>
/// <空行>
/// path/to/file.rs
/// ...
/// <空行，之后是下一提交的头>
/// ```
///
/// 以 `\x1f` 开头的行是提交头（时间戳 + 作者），其后到下一个头之间的非空行
/// 视为文件；头之前或解析失败的头（缺时间戳/作者）之后的行全部跳过。窗口
/// 判定用作者时间（`%at`）对 `now` 作差：`now - ts <= 30/90 天` 即入窗
/// （边界含端点；超出 90 天的提交完全不计）。`--since` 由 git 按提交时间
/// 预过滤过一遍，这里是按作者时间的二次兜底。
fn parse_log(raw: &str, now: i64) -> ChurnTable {
    let mut acc: HashMap<String, Acc> = HashMap::new();
    // 当前提交：(时间戳, 作者)。头解析失败时为 None，其后文件行被忽略。
    let mut current: Option<(i64, String)> = None;
    // 同一提交内的文件集合：BTreeSet 顺便完成去重（同名文件只计 1 次）。
    let mut files: BTreeSet<String> = BTreeSet::new();

    for line in raw.lines() {
        if let Some(header) = line.strip_prefix(UNIT_SEP) {
            if let Some((ts, author)) = current.take() {
                apply_commit(&mut acc, ts, &author, &files, now);
            }
            files.clear();
            let mut fields = header.split(UNIT_SEP);
            let ts = fields.next().and_then(|s| s.trim().parse::<i64>().ok());
            let author = fields.next().map(str::trim).filter(|a| !a.is_empty());
            current = match (ts, author) {
                (Some(ts), Some(author)) => Some((ts, author.to_string())),
                _ => None,
            };
        } else if current.is_some() {
            if let Some(path) = clean_path(line) {
                files.insert(path);
            }
        }
    }
    if let Some((ts, author)) = current.take() {
        apply_commit(&mut acc, ts, &author, &files, now);
    }

    acc.into_iter()
        .map(|(path, a)| {
            (
                path,
                ChurnStats {
                    commits_30d: a.commits_30d,
                    commits_90d: a.commits_90d,
                    authors_90d: a.authors_90d.len() as u32,
                },
            )
        })
        .collect()
}

/// 聚合中间态：作者需要按文件去重，折叠成 [`ChurnStats`] 时再取长度。
#[derive(Default)]
struct Acc {
    commits_30d: u32,
    commits_90d: u32,
    authors_90d: BTreeSet<String>,
}

/// 把一个提交的文件集累计进表。90 天窗口之外整条丢弃。
fn apply_commit(
    acc: &mut HashMap<String, Acc>,
    ts: i64,
    author: &str,
    files: &BTreeSet<String>,
    now: i64,
) {
    if now - ts > WINDOW_90D_SECS {
        return;
    }
    let in_30d = now - ts <= WINDOW_30D_SECS;
    for path in files {
        let entry = acc.entry(path.clone()).or_default();
        entry.commits_90d += 1;
        if in_30d {
            entry.commits_30d += 1;
        }
        entry.authors_90d.insert(author.to_string());
    }
}

/// 清洗一行文件路径：trim、剥外层双引号、丢掉无法可靠还原的行。
///
/// git 对特殊字符路径输出 C 风格转义（如 `"\350\276\255.rs"`），其中反斜杠
/// 与八进制/`\u` 转义无法在不引入解码器的情况下还原——保守跳过（丢一行
/// 只少计一次，错解则会产生永远匹配不上的键，代价不对称）。
fn clean_path(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.contains(UNIT_SEP) {
        return None;
    }
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        if inner.contains('"') || inner.contains('\\') {
            return None;
        }
        return Some(inner.to_string());
    }
    if trimmed.contains('"') {
        // 不带外层引号却含引号：不是 git 的正常输出形态，保守跳过。
        return None;
    }
    Some(trimmed.to_string())
}

/// 跑 `git -C <root> <args>`，成功且退出码为 0 时返回 stdout。
///
/// `deadline` 由调用方设定（[`compute_churn`] 让 show-prefix 与 log 共享同一
/// 截止时刻）。无第三方依赖的超时实现：spawn 后主线程以 [`POLL_INTERVAL`]
/// 轮询 `try_wait`，越过 deadline 即 kill。stdout 的读取放在独立线程——管道
/// 缓冲（常见 64 KB）写满而无人读时子进程会阻塞，轮询方就会把正常执行误判
/// 成超时；读线程在 kill 后随 EOF 自然退出。超时与 `try_wait` 返回 Err 两条
/// 失败路径都 kill + wait 收尸，绝不遗留僵尸子进程。
fn run_git(root: &Path, args: &[&str], deadline: Instant) -> Option<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    });

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    // 非 git 仓库（"fatal: not a git repository"）也走这里。
                    return None;
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None; // 读线程 detach，随 kill 后的 EOF 自行结束
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                // try_wait 出错（IO 失败等）：与超时分支同样 kill 后 wait 收
                // 尸再返回，否则子进程可能滞留成僵尸。
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let bytes = reader.join().ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86400;

    // ---------- fuse_hotspot_score ----------

    #[test]
    fn fuse_without_churn_returns_centrality() {
        for c in [0.0, 0.001, 0.25, 1.0, 42.0] {
            assert_eq!(fuse_hotspot_score(c, None), c);
        }
    }

    #[test]
    fn fuse_with_zero_churn_returns_centrality() {
        // ln_1p(0) = 0 → 增益 1。表里“有记录但 90 天 0 提交”与缺失等价。
        let stats = ChurnStats::default();
        assert_eq!(fuse_hotspot_score(0.25, Some(&stats)), 0.25);
        // commits_30d / authors_90d 不参与公式。
        let stats = ChurnStats {
            commits_30d: 7,
            commits_90d: 0,
            authors_90d: 3,
        };
        assert_eq!(fuse_hotspot_score(0.25, Some(&stats)), 0.25);
    }

    #[test]
    fn fuse_amplifies_high_churn() {
        let stats = ChurnStats {
            commits_90d: 100,
            ..Default::default()
        };
        let expected = 0.25 * (1.0 + 100f64.ln_1p());
        assert!((fuse_hotspot_score(0.25, Some(&stats)) - expected).abs() < 1e-12);
        assert!(fuse_hotspot_score(0.25, Some(&stats)) > 0.25);
    }

    #[test]
    fn fuse_is_monotonic_in_churn() {
        let mut prev = f64::NEG_INFINITY;
        for commits in [0u32, 1, 3, 10, 50, 200, 5000] {
            let stats = ChurnStats {
                commits_90d: commits,
                ..Default::default()
            };
            let score = fuse_hotspot_score(0.3, Some(&stats));
            assert!(score > prev, "commits={commits} score={score} prev={prev}");
            prev = score;
        }
    }

    // ---------- parse_log ----------

    fn header(ts: i64, author: &str) -> String {
        format!("\u{1f}{ts}\u{1f}{author}\n")
    }

    #[test]
    fn parse_log_multiple_commits_files_and_authors() {
        let now = 1_700_000_000i64;
        let raw = format!(
            "{}src/a.rs\nsrc/b.rs\n\n{}src/a.rs\nsrc/c.rs\n\n{}docs/x.md\n",
            header(now - DAY, "alice"),
            header(now - 40 * DAY, "bob"),
            header(now - DAY, "alice"),
        );
        let table = parse_log(&raw, now);
        assert_eq!(table.len(), 4);
        // a.rs 只出现在 alice(30 天内) 与 bob(40 天前) 的提交里：
        // 90 天 2 次、30 天 1 次、两个作者。
        assert_eq!(
            table.get("src/a.rs"),
            Some(&ChurnStats {
                commits_30d: 1,
                commits_90d: 2,
                authors_90d: 2,
            })
        );
        assert_eq!(
            table.get("src/b.rs"),
            Some(&ChurnStats {
                commits_30d: 1,
                commits_90d: 1,
                authors_90d: 1,
            })
        );
        // bob 的提交在 30 天窗外、90 天窗内。
        assert_eq!(
            table.get("src/c.rs"),
            Some(&ChurnStats {
                commits_30d: 0,
                commits_90d: 1,
                authors_90d: 1,
            })
        );
        assert_eq!(
            table.get("docs/x.md"),
            Some(&ChurnStats {
                commits_30d: 1,
                commits_90d: 1,
                authors_90d: 1,
            })
        );
    }

    #[test]
    fn parse_log_dedups_repeated_file_within_one_commit() {
        let now = 1_700_000_000i64;
        let raw = format!("{}dup.rs\ndup.rs\ndup.rs\nother.rs\n", header(now, "alice"));
        let table = parse_log(&raw, now);
        assert_eq!(
            table.get("dup.rs"),
            Some(&ChurnStats {
                commits_30d: 1,
                commits_90d: 1,
                authors_90d: 1,
            })
        );
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn parse_log_window_boundaries() {
        let now = 1_700_000_000i64;
        let raw = format!(
            "{}at-now.rs\n{}at-30d.rs\n{}at-30d-minus-1s.rs\n{}at-90d.rs\n{}at-90d-minus-1s.rs\n",
            header(now, "a"),
            header(now - 30 * DAY, "a"),
            header(now - 30 * DAY - 1, "a"),
            header(now - 90 * DAY, "a"),
            header(now - 90 * DAY - 1, "a"),
        );
        let table = parse_log(&raw, now);
        // 恰好 30 天：两端窗口都含端点。
        assert_eq!(table["at-now.rs"].commits_30d, 1);
        assert_eq!(table["at-30d.rs"].commits_30d, 1);
        // 30 天 + 1 秒：只落 90 天窗。
        let just_outside = table["at-30d-minus-1s.rs"];
        assert_eq!((just_outside.commits_30d, just_outside.commits_90d), (0, 1));
        // 恰好 90 天：计入；90 天 + 1 秒：整条丢弃，不产生键。
        assert_eq!(table["at-90d.rs"].commits_90d, 1);
        assert!(!table.contains_key("at-90d-minus-1s.rs"));
        assert_eq!(table.len(), 4);
    }

    #[test]
    fn parse_log_quoted_escaped_and_garbage_lines() {
        let now = 1_700_000_000i64;
        // 引号内的转义形态按字面写：r#""we ird.rs""# 的字符序列是 "we ird.rs"。
        let raw = format!(
            "\n\n完全不在提交里的噪声行\n{}plain.rs\n\"we ird.rs\"\n\"esc\\\"aped.rs\"\n\"\\350\\256\\270.rs\"\n\n{}next.rs\n",
            header(now, "a"),
            header(now, "b"),
        );
        let table = parse_log(&raw, now);
        // 剥掉外层引号后还原成功。
        assert!(table.contains_key("we ird.rs"));
        // 内部仍含引号/反斜杠（C 风格转义残留）：跳过。
        assert!(!table.contains_key("esc\"aped.rs"));
        assert!(!table.contains_key("\\350\\256\\270.rs"));
        assert!(table.len() == 3); // plain.rs, we ird.rs, next.rs
        assert_eq!(table["plain.rs"].commits_30d, 1);
        assert_eq!(table["next.rs"].authors_90d, 1);
    }

    #[test]
    fn parse_log_malformed_header_skips_until_next_header() {
        let now = 1_700_000_000i64;
        // 头缺作者（第二个字段为空）→ 该提交连同其文件行全部忽略。
        let raw = format!(
            "\u{1f}{}\u{1f}\norphan.rs\n{}kept.rs\n",
            now,
            header(now, "a")
        );
        let table = parse_log(&raw, now);
        assert_eq!(table.len(), 1);
        assert!(table.contains_key("kept.rs"));
        // 空串输入 → 空表。
        assert!(parse_log("", now).is_empty());
    }

    // ---------- rebase_path ----------

    #[test]
    fn rebase_path_strips_prefix_and_drops_outside_subtree() {
        assert_eq!(rebase_path("a/b/c.rs", "a/b/"), Some("c.rs".to_string()));
        assert_eq!(
            rebase_path("a/b/nested/d.rs", "a/b/"),
            Some("nested/d.rs".to_string())
        );
        // 同仓库但不属于本子树：丢弃。
        assert_eq!(rebase_path("x/y.rs", "a/b/"), None);
        assert_eq!(rebase_path("a/c.rs", "a/b/"), None);
    }

    #[test]
    fn rebase_path_empty_prefix_is_passthrough() {
        // root 即 git 顶层：前缀为空串，路径原样放行。
        assert_eq!(
            rebase_path("src/foo.rs", ""),
            Some("src/foo.rs".to_string())
        );
        assert_eq!(rebase_path("x/y.rs", ""), Some("x/y.rs".to_string()));
    }

    #[test]
    fn rebase_path_matches_on_component_boundary() {
        // 不以 '/' 结尾的非空前缀（防御性分支）：按完整组件匹配，
        // `a/b` 不得命中 `a/bc.rs`。
        assert_eq!(rebase_path("a/b/c.rs", "a/b"), Some("c.rs".to_string()));
        assert_eq!(rebase_path("a/bc.rs", "a/b"), None);
        assert_eq!(rebase_path("a/b", "a/b"), None);
        // 带斜杠前缀同理：半组件命中不算。
        assert_eq!(rebase_path("a/bc.rs", "a/b/"), None);
        // 剥完只剩空串（路径恰为前缀目录本身）：无意义的空键，丢弃。
        assert_eq!(rebase_path("a/b/", "a/b/"), None);
    }

    // ---------- compute_churn（本仓真跑 + 非 git 目录） ----------

    #[test]
    fn compute_churn_on_this_repo() {
        // CARGO_MANIFEST_DIR = <workspace>/crates/astrolabe-core，是本仓的
        // **子目录**：show-prefix = "crates/astrolabe-core/"，表键应为 root
        // 相对（本模块即 src/churn.rs 形态），而非 git 顶层相对
        // （crates/astrolabe-core/src/churn.rs）。
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
        let table = compute_churn(&root);
        // 宽松断言（真实 git 数据，防 flaky）：表非空，且存在不带 crates/
        // 前缀的键（前缀已被剥掉，口径是 root 相对）；有提交的文件必然
        // authors_90d >= 1，且 30d 计数 <= 90d。
        assert!(!table.is_empty(), "churn table should not be empty");
        assert!(
            table.keys().any(|k| !k.starts_with("crates/")),
            "expected a root-relative key without the crates/ prefix, got {:?}",
            table.keys().collect::<Vec<_>>()
        );
        for stats in table.values() {
            assert!(stats.commits_30d <= stats.commits_90d);
            if stats.commits_90d > 0 {
                assert!(stats.authors_90d >= 1);
            }
        }
    }

    #[test]
    fn compute_churn_on_non_git_dir_returns_empty() {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-churn-nogit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let table = compute_churn(&dir);
        assert!(table.is_empty(), "non-git dir must yield an empty table");
        std::fs::remove_dir_all(&dir).ok();
    }
}
