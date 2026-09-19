# mcp_misc — NORMAL fixes (astrolabe-mcp)

No git commit.

## cli.rs
- Empty `ASTROLABE_ROOT` (set-but-blank) treated like unset → fallback `.`
- Reject empty/whitespace `--client=` under `hooks remind`
- `-h` / `--help` under `hooks` (any position) → `Launch::Help`
- Tests: `empty_env_root_falls_back_to_dot`, `empty_client_flag_errors`, `hooks_help_flags`

## context.rs
- Unknown YAML list keys: skip list items (no startup brick); unknown empty keys can own following `-` items for skip
- `excluded_tools` re-key clears then fills (last block wins)
- `load_key` (builtin name / file stem) stored; `openai_tool_compatible` checks `name` **or** `load_key`
- Flow list `[a, b]` accepted for `excluded_tools`; clearer error otherwise
- Tests for skip / rekey / flow / path-stem oaicompat

## memory.rs
- Windows-safe overwrite: on `rename` fail, `remove` destination then `rename` again
- `MAX_CONTENT_BYTES` = 1 MiB; oversized content → clear `Err`
- Test: `test_8_content_size_limit_rejected`

## openai_schema.rs
- `oneOf`/`anyOf` type|null fold: merge **full** non-null object fields (via `walk`), not type alone
- `multipleOf: 1` only when `had_integer` (not pure `number` unions)
- Tests: `null_oneof_merges_full_non_null_fields`, `pure_number_union_does_not_get_multiple_of`

## live_backend.rs
- Distinguishes missing `path` vs unmapped extension (`unknown_language` note)
- definition/diagnostics with unmapped path no longer claim “需要 path”
- Removed unused `language_of`

## main.rs
- Comment updated from “Thirteen tools…” to twenty-one tools matching `TOOL_COUNT` (10 graph + 6 precise + instructions + 4 memory)

## Verification
`cargo test -p astrolabe-mcp --lib` — 138 passed.
