# graph_core fixes (NORMAL)

Scope: `crates/astrolabe-core` graph/path/body/churn helpers. No git commit.

## 1. `neighborhood.rs` — Both = single bidirectional BFS
- Build import adjacency once (`import_adjacency_pair`: fwd + rev).
- `Dependencies` / `Dependents` / `Both` all go through `bfs_layers`; Both expands **in+out per layer** (OpenVisio `gatherNeighborhood`), not two one-way BFS union.
- Test `both_bidirectional_reaches_co_dependents_via_shared_neighbor`: A→B←C from A, Both depth=2 includes C@2.

## 2. `trace_tree.rs` — truncation signal + less dispatch dup
- New `TraceForest { entries, truncated, omitted_roots }`.
- `truncated`: budget cut a root short; `omitted_roots`: known roots skipped after budget=0.
- Shared `expand_root` for `trace_tree` / `trace_forest`; `walk` returns whether budget truncated.
- MCP `server.rs` updated to use `forest.entries`.

## 3. `name_path.rs` — absolute + nested same-name
- Absolute outermost ancestor segment continues past inner same-name matches until chain top.
- Test: `/Dup/leaf` with nested Dup accepts leaf under top-level Dup.

## 4. `body.rs` — inverted range still opens file
- `start > end` still `File::open` then `Ok("")`; missing file → `Err`.
- Test covers inverted + missing via `extract_lines` (not `symbol_body`, which `debug_assert`s inverted ranges).

## 5. `group_graph.rs` — weight accumulation
- Group edge weight `+= edge.weight.max(1)` (not `+1` per record).
- Test: weights 5 + 0 → group weight 6.

## 6. `churn.rs` — shared deadline
- `show-prefix` + `log` share one `Instant` deadline (`GIT_TIMEOUT` total, worst case ≤10s).
- `run_git` takes `deadline: Instant`.

## Verification
- Affected unit tests green (neighborhood / name_path / body / group_graph / trace_tree; churn unit parsers/fuse/rebase).
- `cargo check -p astrolabe-mcp` OK.
- Env noise (not from these edits): `compute_churn_on_this_repo` (workspace has no `.git`), unrelated lsp discovery/transport flakes.
