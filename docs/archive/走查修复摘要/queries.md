# queries.rs NORMAL fixes

File: `crates/astrolabe-core/src/lsp/queries.rs`

## Fixes

1. **Hover site-failure degrade** — `hover_site_failed` / `hover_locate_failed` sanitize via `sanitize_hover_site_reason` (strip `TEXT_SEARCH_HINT` / search_code, rewrite “precise references” → hover). Notes are hover-specific reason + `HOVER_FALLBACK_HINT` only. Named hover no longer routes through references `resolve_query_site`.

2. **Named hover definition-anchor failure** — `resolve_named_hover_site`: on `definition` `Err`, fall back to `first_identifier_position` and still issue hover (avoids stacking a second LSP timeout / losing the query).

3. **goto_definition degrade text** — `degrade_definition` + `unavailable_definition_without_server` say “precise definitions” (still keep `TEXT_SEARCH_HINT`). References helpers unchanged.

4. **guard_cold_index note merge** — when busy+empty, preserve existing finalize notes (e.g. all-external) and append the cold-index warning instead of wholesale replace.

5. **Named hover Exact / cold anchor** — unique in-repo definition is used as hover anchor only when `!server.is_busy()`; while busy/cold, stay on the first identifier so a skewed cold hit is not treated as Exact.

## Tests

`cargo test -p astrolabe-core --lib lsp::queries::` — **40 passed** (includes new coverage for all five items).

No git commit.
