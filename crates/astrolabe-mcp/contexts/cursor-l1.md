Astrolabe provides indexed, symbol-aware code navigation for index_root: {{INDEX_ROOT}} (all 7 tools are read-only — batch independent calls in parallel in one turn).

Cursor exposes these MCP tools as `astrolabe-resolve_contx`, `astrolabe-search_code`, `astrolabe-find_symbol`, `astrolabe-find_refs`, `astrolabe-goto_def`, `astrolabe-get_diag`, and `astrolabe-get_lang`.

Route by intent (1 Astrolabe call replaces multi-turn Grep + Read):
- Unfamiliar area / multi-file task -> resolve_contx (import-graph ranked files & symbols)
- Symbol definition, signature, body, or file outline (query="*" + path_filter) -> find_symbol (returns bodies without full-file Read)
- Callers, usages, interface implementors, or rename checklist (dry_run_rename) -> find_refs (scope-aware; prefer over Grep for renames/refactors)
- Jump from a call/use site to its exact definition -> goto_def
- Literal/regex text search or counts (aggregate=top|dir|file, optional skip_comments/exclude; no scripts) -> search_code
- File syntax/type check ONLY when you cannot run build/tests -> get_diag (skip if build/tests pass, e.g. go test, cargo test)
- Check indexed languages, LSP readiness, or parse errors -> get_lang

Use built-in tools directly when appropriate:
- Read / Edit / Write: right before editing a known target file (host requires Read before Edit), for config/docs (Markdown, JSON, YAML, TOML), or for unindexed languages (empty search_code/find_symbol does not cover them — check get_lang).
- Grep / Glob: for unindexed file types or filename pattern matching.

Rules:
- When the last line says “Fully listed”, the listing is complete — answer from it; do not re-check with grep or re-query with other words.
- The index_root in results is that path; if it is not the repo you are editing, pass index_root or do not Read those paths.
- scoped comes from the import graph; syntactic from name matching.
- When a language server is missing, precise tools say so and suggest alternatives — do not install one yourself (see get_lang).
