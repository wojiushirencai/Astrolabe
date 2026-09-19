# precise_tools NORMAL fixes

File: `crates/astrolabe-mcp/src/precise_tools.rs`  
No git commit.

## Changes

1. **Module comment / PRECISE_TOOL_COUNT**  
   Doc comment said `(5)`; updated to `(6)` and listed `run_get_symbol_info`.  
   `lib.rs` already has `PRECISE_TOOL_COUNT = 6` — no change needed.

2. **Empty symbol / new_name validation**  
   `run_plan_rename` and `run_apply_rename` empty-param paths no longer append `DO_NOT_APPLY` / `APPLY_REFUSED` (or force guidance). They still return unknown + the empty-field message only. Low-confidence refuse paths still use those markers.

3. **`run_get_symbol_info` budget truncation**  
   Hover body is line-ranked via `truncate_ranked` against remaining `budget_tokens` (after the confidence preamble). Omitted lines get an explicit budget note.

4. **`GetSymbolInfoParams.symbol` required**  
   Changed from `Option<String>` (`#[serde(default)]`) to `String` so the JSON schema marks it required, matching runtime empty-check behavior.

5. **`apply_plan_with_reparse` pre-read path escape**  
   Pre-read now mirrors `transaction::resolve_dest`: canonicalize repo root, canonicalize each joined path, refuse if not under root (`canonical path escapes the repository root`), then read.

## Verification

`cargo test -p astrolabe-mcp --lib precise_tools` — 16 passed.

Note: two unrelated `hooks.rs` syntax/test-slice issues blocked the crate compile; fixed minimally so tests could run (`"\\"` literal; `&br#..#[..]`). Not part of this shard’s scope.
