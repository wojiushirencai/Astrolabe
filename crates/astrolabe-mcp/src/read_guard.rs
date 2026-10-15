//! Opt-in `Read` guard for Claude Code (`astrolabe hooks read-guard`).
//!
//! The MCP server appends every code range it returns (symbol bodies from
//! `search_code` / `explain_symbol` / `find_symbol include_body`) to a served
//! log under `~/.astrolabe/served/`. The PreToolUse hook on `Read` checks a
//! requested range against that log and, when the file is unchanged and the
//! whole range was already sent, answers with a pointer instead of letting the
//! model pay for the same lines again.
//!
//! Safety (Edit requires a prior Read):
//! - `deny-once` (default when enabled): the first matching Read is denied with
//!   a pointer; repeating the *same* Read in the same session is always
//!   allowed, so a model that needs the Read to unlock `Edit` loses one turn,
//!   never the ability to edit.
//! - `warn`: never denies, only adds context.
//! - Fails open on anything unexpected: no log, unreadable file, changed file,
//!   several live servers for the same root, partial coverage, short ranges.
//!
//! Enabled only when `ASTROLABE_READ_HOOK` is set (`1`/`once` or `warn`);
//! the MCP server writes the served log under the same switch.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};

pub(crate) const ENV_SWITCH: &str = "ASTROLABE_READ_HOOK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Off,
    DenyOnce,
    Warn,
}

pub(crate) fn mode_from(value: Option<&str>) -> Mode {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("0") | Some("off") | Some("false") => Mode::Off,
        Some("warn") => Mode::Warn,
        Some(_) => Mode::DenyOnce,
    }
}

pub(crate) fn mode_from_env() -> Mode {
    mode_from(std::env::var(ENV_SWITCH).ok().as_deref())
}

fn served_dir(home: &Path) -> PathBuf {
    home.join(".astrolabe").join("served")
}

#[derive(Debug, Serialize, Deserialize)]
struct Header {
    root: String,
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Entry {
    path: String,
    start: u32,
    end: u32,
    mtime_ns: u128,
    size: u64,
    call: u64,
    tool: String,
}

fn stamp(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let ns = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((ns, meta.len()))
}

/// Append-only log of ranges this MCP server process has returned.
#[derive(Debug)]
pub(crate) struct ServedLog {
    enabled: bool,
    dir: Option<PathBuf>,
    root: PathBuf,
    file: Mutex<Option<PathBuf>>,
}

impl ServedLog {
    pub(crate) fn new(root: &Path) -> Self {
        let enabled = mode_from_env() != Mode::Off;
        let dir = enabled
            .then(crate::hooks::get_user_home)
            .flatten()
            .map(|h| served_dir(&h));
        Self::with_dir(root, dir.filter(|_| enabled))
    }

    pub(crate) fn with_dir(root: &Path, dir: Option<PathBuf>) -> Self {
        Self {
            enabled: dir.is_some(),
            dir,
            root: root.to_path_buf(),
            file: Mutex::new(None),
        }
    }

    /// Record that `abs_path` lines `start..=end` were returned by `tool`.
    pub(crate) fn record(&self, abs_path: &Path, start: u32, end: u32, tool: &str, call: u64) {
        if !self.enabled || start == 0 || end < start {
            return;
        }
        let Some(dir) = self.dir.as_ref() else { return };
        let Some((mtime_ns, size)) = stamp(abs_path) else {
            return;
        };
        let mut slot = self.file.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let _ = std::fs::create_dir_all(dir);
            sweep_old(dir);
            let path = dir.join(format!("{}.jsonl", std::process::id()));
            let header = Header {
                root: self.root.display().to_string(),
                pid: std::process::id(),
            };
            let Ok(line) = serde_json::to_string(&header) else {
                return;
            };
            if std::fs::write(&path, format!("{line}\n")).is_err() {
                return;
            }
            *slot = Some(path);
        }
        let Some(path) = slot.as_ref() else { return };
        let entry = Entry {
            path: abs_path.display().to_string(),
            start,
            end,
            mtime_ns,
            size,
            call,
            tool: tool.to_string(),
        };
        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

impl Drop for ServedLog {
    fn drop(&mut self) {
        if let Some(path) = self.file.lock().ok().and_then(|g| g.clone()) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn sweep_old(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 3600));
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Live served logs whose root contains `file`. `None` when ambiguous.
fn logs_for(dir: &Path, file: &Path) -> Option<Vec<(Header, Vec<Entry>)>> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut lines = text.lines();
        let Some(header) = lines
            .next()
            .and_then(|l| serde_json::from_str::<Header>(l).ok())
        else {
            continue;
        };
        if !file.starts_with(&header.root) {
            continue;
        }
        let fresh = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(2 * 3600));
        let alive = if Path::new("/proc/self").exists() {
            Path::new(&format!("/proc/{}", header.pid)).exists()
        } else {
            fresh
        };
        if !alive {
            continue;
        }
        let entries = lines
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
            .collect();
        found.push((header, entries));
    }
    // Two live sessions on one root: cannot tell whose results the model saw.
    (found.len() == 1).then_some(found)
}

fn covered(
    entries: &[Entry],
    file: &Path,
    stamp_now: (u128, u64),
    s: u32,
    e: u32,
) -> Option<Entry> {
    let file = file.display().to_string();
    let mut spans: Vec<&Entry> = entries
        .iter()
        .filter(|x| x.path == file && (x.mtime_ns, x.size) == stamp_now)
        .collect();
    spans.sort_by_key(|x| x.start);
    let mut reach = s;
    let mut first: Option<Entry> = None;
    for x in spans {
        if x.start > reach {
            continue;
        }
        if x.end >= reach {
            first.get_or_insert_with(|| x.clone());
            reach = x.end.saturating_add(1);
        }
        if reach > e {
            return first;
        }
    }
    None
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GuardState {
    denied: Vec<String>,
}

fn min_lines() -> u32 {
    std::env::var("ASTROLABE_READ_HOOK_MIN_LINES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10)
}

fn as_u64(v: Option<&serde_json::Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

/// Hook body. Returns the process exit code (always 0: fail open).
pub(crate) fn run_read_guard_impl<R: Read, W: Write>(
    mode: Mode,
    mut reader: R,
    mut stdout: W,
    home: Option<&Path>,
) -> i32 {
    if mode == Mode::Off {
        return 0;
    }
    let mut raw = String::new();
    if reader.read_to_string(&mut raw).is_err() {
        return 0;
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return 0;
    };
    if input.get("tool_name").and_then(|v| v.as_str()) != Some("Read") {
        return 0;
    }
    let Some(tool_input) = input.get("tool_input") else {
        return 0;
    };
    let Some(file_path) = tool_input.get("file_path").and_then(|v| v.as_str()) else {
        return 0;
    };
    let Some(home) = home
        .map(Path::to_path_buf)
        .or_else(crate::hooks::get_user_home)
    else {
        return 0;
    };
    let Ok(file) = std::fs::canonicalize(file_path) else {
        return 0;
    };
    let Some(now_stamp) = stamp(&file) else {
        return 0;
    };
    if now_stamp.1 > 8 * 1024 * 1024 {
        return 0;
    }
    let Ok(text) = std::fs::read_to_string(&file) else {
        return 0;
    };
    let total = text.lines().count() as u64;
    let start = as_u64(tool_input.get("offset")).unwrap_or(1).max(1);
    let limit = as_u64(tool_input.get("limit")).unwrap_or(2000).max(1);
    if start > total {
        return 0;
    }
    let end = (start + limit - 1).min(total);
    if (end - start + 1) < u64::from(min_lines()) {
        return 0;
    }
    let Some(logs) = logs_for(&served_dir(&home), &file) else {
        return 0;
    };
    let Some(hit) = logs
        .iter()
        .find_map(|(_, entries)| covered(entries, &file, now_stamp, start as u32, end as u32))
    else {
        return 0;
    };

    let key = format!("{}:{start}-{end}", file.display());
    let session = ["session_id", "sessionId"]
        .iter()
        .find_map(|k| input.get(*k).and_then(|v| v.as_str()))
        .and_then(|s| {
            crate::hooks::sanitize_session_id(s)
                .ok()
                .map(str::to_string)
        });
    let state_path = session.as_deref().and_then(|s| {
        crate::hooks::get_hook_data_dir(&home, s, None)
            .ok()
            .map(|d| d.join("read_guard.json"))
    });
    let mut state: GuardState = state_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let pointer = format!(
        "{}:{start}-{end} 已由 Astrolabe 的 {} 在本会话第 {} 次调用中完整返回，文件自那以后未改动，结果就在上文，直接使用即可。",
        file.display(),
        hit.tool,
        hit.call
    );
    let out = match mode {
        Mode::Warn => serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": format!("{pointer}（提示：这次 Read 重复了已返回的内容。）"),
            }
        }),
        Mode::DenyOnce => {
            if state.denied.contains(&key) {
                // Second identical Read: let it through (needed to unlock Edit).
                return 0;
            }
            state.denied.push(key);
            if let Some(p) = state_path.as_ref() {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(t) = serde_json::to_string(&state) {
                    let _ = std::fs::write(p, t);
                }
            } else {
                // Without a session id we cannot remember the denial, so never deny.
                return 0;
            }
            serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": format!(
                        "{pointer}如果你确实需要这次 Read（例如要编辑该文件，Edit 需要先 Read），原样重发同一个 Read 即可，第二次会放行。"
                    ),
                }
            })
        }
        Mode::Off => return 0,
    };
    let _ = writeln!(stdout, "{out}");
    0
}

pub fn run_read_guard() -> i32 {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_read_guard_impl(mode_from_env(), stdin.lock(), stdout.lock(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "astro-rg-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn setup() -> (PathBuf, PathBuf, PathBuf, ServedLog) {
        let home = tmp("home");
        let root = std::fs::canonicalize(tmp("repo")).unwrap();
        let file = root.join("a.rs");
        let body: String = (1..=60).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, body).unwrap();
        let log = ServedLog::with_dir(&root, Some(served_dir(&home)));
        (home, root, file, log)
    }

    fn payload(file: &Path, offset: Option<u64>, limit: Option<u64>, sid: &str) -> String {
        let mut ti = serde_json::json!({"file_path": file.display().to_string()});
        if let Some(o) = offset {
            ti["offset"] = o.into();
        }
        if let Some(l) = limit {
            ti["limit"] = l.into();
        }
        serde_json::json!({"tool_name":"Read","session_id":sid,"tool_input":ti}).to_string()
    }

    fn run(mode: Mode, home: &Path, p: &str) -> String {
        let mut out = Vec::new();
        assert_eq!(
            run_read_guard_impl(mode, p.as_bytes(), &mut out, Some(home)),
            0
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(mode_from(None), Mode::Off);
        assert_eq!(mode_from(Some("0")), Mode::Off);
        assert_eq!(mode_from(Some("1")), Mode::DenyOnce);
        assert_eq!(mode_from(Some("once")), Mode::DenyOnce);
        assert_eq!(mode_from(Some("WARN")), Mode::Warn);
    }

    #[test]
    fn denies_once_then_allows_same_read_so_edit_still_works() {
        let (home, _root, file, log) = setup();
        log.record(&file, 10, 40, "explain_symbol", 3);
        let p = payload(&file, Some(12), Some(20), "s1");
        let first = run(Mode::DenyOnce, &home, &p);
        assert!(first.contains("\"permissionDecision\":\"deny\""), "{first}");
        assert!(first.contains("第 3 次调用"), "{first}");
        assert!(first.contains("原样重发"), "{first}");
        // The identical retry must pass (silent allow).
        assert_eq!(run(Mode::DenyOnce, &home, &p), "");
        // Still allowed afterwards.
        assert_eq!(run(Mode::DenyOnce, &home, &p), "");
    }

    #[test]
    fn partial_coverage_changed_file_and_short_ranges_are_allowed() {
        let (home, _root, file, log) = setup();
        log.record(&file, 10, 40, "search_code", 1);
        // Extends past what was served.
        assert_eq!(
            run(
                Mode::DenyOnce,
                &home,
                &payload(&file, Some(30), Some(30), "s")
            ),
            ""
        );
        // Whole-file read (no offset/limit) is not covered.
        assert_eq!(
            run(Mode::DenyOnce, &home, &payload(&file, None, None, "s")),
            ""
        );
        // Short slice.
        assert_eq!(
            run(
                Mode::DenyOnce,
                &home,
                &payload(&file, Some(12), Some(5), "s")
            ),
            ""
        );
        // Covered, then the file changes: allowed.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&file, "x\n".repeat(80)).unwrap();
        assert_eq!(
            run(
                Mode::DenyOnce,
                &home,
                &payload(&file, Some(12), Some(20), "s")
            ),
            ""
        );
    }

    #[test]
    fn adjacent_ranges_merge_and_cover() {
        let (home, _root, file, log) = setup();
        log.record(&file, 1, 20, "search_code", 1);
        log.record(&file, 21, 45, "find_symbol", 2);
        let out = run(
            Mode::DenyOnce,
            &home,
            &payload(&file, Some(5), Some(35), "s2"),
        );
        assert!(out.contains("deny"), "{out}");
    }

    #[test]
    fn warn_mode_never_denies_and_off_is_silent() {
        let (home, _root, file, log) = setup();
        log.record(&file, 10, 40, "explain_symbol", 3);
        let p = payload(&file, Some(12), Some(20), "s3");
        let out = run(Mode::Warn, &home, &p);
        assert!(
            out.contains("additionalContext") && !out.contains("deny"),
            "{out}"
        );
        assert_eq!(run(Mode::Off, &home, &p), "");
    }

    #[test]
    fn two_live_servers_on_one_root_fail_open() {
        let (home, root, file, log) = setup();
        log.record(&file, 10, 40, "explain_symbol", 3);
        let other = served_dir(&home).join("999999999.jsonl");
        std::fs::write(
            &other,
            format!(
                "{{\"root\":\"{}\",\"pid\":{}}}\n",
                root.display(),
                std::process::id()
            ),
        )
        .unwrap();
        let p = payload(&file, Some(12), Some(20), "s4");
        assert_eq!(run(Mode::DenyOnce, &home, &p), "");
    }

    #[test]
    fn missing_session_or_non_read_tools_are_allowed() {
        let (home, _root, file, log) = setup();
        log.record(&file, 10, 40, "explain_symbol", 3);
        let mut v: serde_json::Value =
            serde_json::from_str(&payload(&file, Some(12), Some(20), "x")).unwrap();
        v.as_object_mut().unwrap().remove("session_id");
        assert_eq!(run(Mode::DenyOnce, &home, &v.to_string()), "");
        v["tool_name"] = "Edit".into();
        assert_eq!(run(Mode::DenyOnce, &home, &v.to_string()), "");
        assert_eq!(run(Mode::DenyOnce, &home, "not json"), "");
    }

    #[test]
    fn dropping_the_log_removes_its_file() {
        let (home, _root, file, log) = setup();
        log.record(&file, 1, 20, "search_code", 1);
        let n = || std::fs::read_dir(served_dir(&home)).unwrap().count();
        assert_eq!(n(), 1);
        drop(log);
        assert_eq!(n(), 0);
    }
}
