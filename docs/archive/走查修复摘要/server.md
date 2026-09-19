# server shard fixes

File: `crates/astrolabe-mcp/src/server.rs` (no git commit)

## SEVERE
1. **find_symbol_by_path + include_body** — Path branch now mirrors the name-match flow: collect `(line, symbol_idx)` hits → `truncate_ranked` → kept/top5 → shared `attach_include_bodies` (symbol_body + per-line budget truncate). Empty hits skip body attach.

## NORMAL
2. **Trace/GroupGraph depth serde default = 1** — Added `default_trace_or_group_depth() -> 1` and wired `#[serde(default = "default_trace_or_group_depth")]` on `TraceParams.depth` and `GroupGraphParams.depth` (was bare `#[serde(default)]` → 0).
3. **rustdoc/clippy placement** — Moved the call-tree rustdoc + `#[allow(clippy::too_many_lines)]` onto `trace_calls_tree_body`; `list_memories` only keeps the memories section comment.
4. **Unknown kind** — `kind_from_name` returns `Result`; unknown values error with `有效值：function, method, class, interface, struct, enum, trait, type, const, module, field` via `finish(text_error(...))` (no silent filter disable).
5. **Neighborhood / group_graph budget** — Both build ranked line lists and apply `truncate_ranked` against remaining `budget_tokens`, with an omission note.
6. **write_memory / delete_memory failures** — Failures use `finish(text_error(...))` (`CallToolResult::error`), not the success `result` path. Delete of missing/invalid name is an error.
7. **instructions / memories budget** — `initial_instructions` and `read_memory` run through `truncate_body_text`; `list_memories` truncates entry lines with `truncate_ranked` + note.

## Verification
- `cargo test -p astrolabe-mcp --lib server::tests` — 17 passed
- Minor: removed unused `mut` on `list_memories` header
