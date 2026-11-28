//! Backward-compatible hooks module.
//!
//! Legacy hook subcommands (`astrolabe hooks ...`) are deprecated no-ops that
//! exit with status `0` without writing to stdout or stderr. Only `remind` and
//! `cleanup` drain up to 1 MiB of piped stdin (when not attached to a
//! terminal); other subcommands (such as `gc`, `session-start`, or bare
//! `hooks`) return `0` immediately without reading stdin.
//!
//! [`sanitize_agent_component`] is retained for `server.rs` per-agent
//! deduplication shard naming.

use std::io::{IsTerminal, Read, Write};

const MAX_NOOP_STDIN_BYTES: u64 = 1024 * 1024;

/// 校验 session_id 可作为单一路径分量使用。
///
/// 拒绝空串、`/`、`\`、`..`，以及任何不在 `[A-Za-z0-9_-]` 内的字符。
fn sanitize_session_id(session_id: &str) -> Result<&str, &'static str> {
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

/// FNV-1a 64 位哈希：把任意 agent_id 收敛为固定长度、路径安全的十六进制分量
fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// 将 agent_id 收敛为单一安全路径分量。
///
/// trim 后非空、长度 ≤ 128、且能通过 [`sanitize_session_id`] 校验（`[A-Za-z0-9_-]`）
/// 时原样返回；否则返回 `ag-<fnv1a64 十六进制>`（防路径穿越，兼容带冒号/点/斜杠的 agent 名）。
pub fn sanitize_agent_component(raw: &str) -> String {
    let trimmed = raw.trim();
    if !trimmed.is_empty() && trimmed.len() <= 128 && sanitize_session_id(trimmed).is_ok() {
        trimmed.to_string()
    } else {
        format!("ag-{:016x}", fnv1a64(trimmed))
    }
}

fn run_noop_with<R: Read, W: Write, E: Write>(
    reader: R,
    is_terminal: bool,
    drain_stdin: bool,
    _stdout: W,
    _stderr: E,
) -> i32 {
    if drain_stdin && !is_terminal {
        let mut limited = reader.take(MAX_NOOP_STDIN_BYTES);
        let _ = std::io::copy(&mut limited, &mut std::io::sink());
    }
    0
}

/// Deprecated hook entrypoint kept for backward compatibility.
///
/// When `drain_stdin` is `true` (used by legacy `remind` / `cleanup` hooks),
/// drains up to 1 MiB from stdin when piped (avoiding EPIPE in callers) without
/// blocking on interactive terminals. When `drain_stdin` is `false` (e.g. `gc`,
/// `session-start`, bare `hooks`), exits `0` immediately without locking or
/// reading stdin.
pub fn run_noop(drain_stdin: bool) -> i32 {
    if !drain_stdin {
        return 0;
    }
    let stdin = std::io::stdin();
    let is_terminal = stdin.is_terminal();
    run_noop_with(
        stdin.lock(),
        is_terminal,
        drain_stdin,
        std::io::stdout(),
        std::io::stderr(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_unsafe_agent_id_falls_back_to_hashed_shard() {
        // FNV-1a 64 标准测试向量（写死常数，防算法实现写错而期望值同错仍绿）
        assert_eq!(fnv1a64(""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64("a"), 0xaf63dc4c8601ec8c);
        assert_eq!(sanitize_agent_component("agent-a"), "agent-a");
        let expected_evil = format!("ag-{:016x}", fnv1a64("../evil"));
        assert_eq!(sanitize_agent_component("../evil"), expected_evil);
        let long_id = "a".repeat(200);
        let expected_long = format!("ag-{:016x}", fnv1a64(&long_id));
        assert_eq!(sanitize_agent_component(&long_id), expected_long);
        // 边界钉死：`..`/`/`/绝对路径/unicode/空白串全部收敛为哈希分量（不以输入原样出现）；
        // 空白串 trim 后为空 → 哈希的即 offset 基值；恰好 128 字节仍原样、129 字节走哈希
        for bad in ["..", "/", "/etc/passwd", "café", "   "] {
            let got = sanitize_agent_component(bad);
            assert_eq!(
                got.len(),
                3 + 16,
                "should collapse to ag-<16hex> hash component: {bad} -> {got}"
            );
            assert!(
                got.starts_with("ag-"),
                "should collapse to ag-<16hex> hash component: {bad} -> {got}"
            );
        }
        assert_eq!(
            sanitize_agent_component("   "),
            format!("ag-{:016x}", 0xcbf29ce484222325u64)
        );
        let id_128 = "a".repeat(128);
        assert_eq!(sanitize_agent_component(&id_128), id_128);
        let id_129 = "a".repeat(129);
        assert_eq!(
            sanitize_agent_component(&id_129),
            format!("ag-{:016x}", fnv1a64(&id_129))
        );
    }

    #[test]
    fn run_noop_with_piped_json_drains_silently() {
        let payload = br#"{"session_id":"s1","tool_name":"Grep"}"#;
        let mut reader = Cursor::new(payload.as_slice());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_noop_with(&mut reader, false, true, &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert_eq!(reader.position(), payload.len() as u64);
    }

    #[test]
    fn run_noop_with_caps_stdin_at_one_mib() {
        let total = 1024 * 1024 + 256;
        let buf = vec![b'a'; total];
        let mut reader = Cursor::new(buf);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_noop_with(&mut reader, false, true, &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert_eq!(reader.position(), 1024 * 1024);
    }

    #[test]
    fn run_noop_with_tty_does_not_read() {
        let mut reader = Cursor::new(b"should not be read".as_slice());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_noop_with(&mut reader, true, true, &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn run_noop_with_drain_false_never_reads() {
        struct PanicOnRead;
        impl Read for PanicOnRead {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("read() must not be called when drain_stdin is false");
            }
        }

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_noop_with(PanicOnRead, false, false, &mut out, &mut err);
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn run_noop_returns_zero() {
        assert_eq!(run_noop(false), 0);
        assert_eq!(run_noop(true), 0);
    }
}
