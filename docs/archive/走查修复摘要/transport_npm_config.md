# transport_npm_config — NORMAL fixes

Shard covers: `transport.rs`, `root.rs` + `stdio_smoke.rs`, `download.js`, `readonly.yml`.
No git commit.

## 1. `crates/astrolabe-core/src/lsp/transport.rs`

- **ready_confirmed sticky latch:** on `$/progress` `begin`, clear `ready_confirmed` so a later indexing wave is waited on again (fast-path no longer sticks forever after the first quiet period).
- **diagnostics Timeout:** pull-diagnostic failure fallback no longer rewrites `LspError::Timeout` into the “pull unsupported…” `Protocol` error; empty push fallback preserves `Timeout`. `Protocol` path unchanged.
- **ASTROLABE_LSP_TIMEOUT parse:** invalid integer now `tracing::warn!`s and falls back to default (was silent `None`).

## 2. `crates/astrolabe-mcp/src/root.rs` + `tests/stdio_smoke.rs`

- **Smoke assertion message:** no longer claims “no .git here”; `unique_temp_dir` creates `.git`, so the message now says the temp project root / `.git` fixture.
- **Ready gate:** loop waits until the Building message is gone (`索引正在后台构建` / English variants), then asserts python — does not treat “contains python” as the sole readiness signal (empty / non-Python workspaces would otherwise spin to timeout).
- **Sentinel `.` tests:** added `cwd_sentinel_dot_walks_up_to_git` and `cwd_sentinel_dot_walks_up_to_serena` covering `resolve_index_root(Path::new("."))` walk-up from a nested cwd.

## 3. `npm/packages/astrolabe/lib/download.js`

- PowerShell `Expand-Archive` no longer interpolates paths into a double-quoted `-Command` string.
- Paths passed via `ASTROLABE_ZIP_SRC` / `ASTROLABE_ZIP_DST` env vars; `-Command` uses `$env:…` only.

## 4. `crates/astrolabe-mcp/contexts/readonly.yml`

- Comments + `notes` state clearly: `excluded_tools` only hides from `tools/list`; it does **not** block `tools/call`.
