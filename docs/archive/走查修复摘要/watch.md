# watch.rs fixes

File: `crates/astrolabe-core/src/watch.rs`  
No git commit.

## SEVERE

1. **Seeded spawn no longer discards deltas**  
   Removed the caller-thread `let _ = self.check()` on the events-success path. The first `check` now runs at the start of `event_loop` (and already did in `run_loop`): unseeded → empty baseline; already seeded → non-empty catch-up changeset is sent on the channel.

## NORMAL

2. **event_loop stop flush** — If `last_event` is still pending when the loop exits on stop, run a final `check` + send (same idea as `run_loop` pending flush). Covered by `event_loop_flushes_pending_debounce_on_stop`.

3. **First full-tree scan off caller thread** — Baseline/catch-up scan moved into the background thread (`event_loop` start); spawn no longer walks the tree on the caller.

4. **Bounded notify channel** — `sync_channel(1024)` with `try_send`; drop on overflow. Receiver drains coalesced events after each wake so the bound does not stay full. Safety-interval scan recovers missed wakes.

5. **confirm settle** — `(None, Some(_)) => meta_changed` only. First successful hash with unchanged mtime/size does not spuriously report modified (unreadable→readable / unsettled→hashed settle). Direct unit test: `confirm_settle_unreadable_to_readable_no_spurious_modified`. Note: mode `000` still drops the path from `scan` (Removed→Added), which is separate from confirm.

6. **Struct docs** — `Watcher` docs updated from “Polling watcher” to hybrid events + polling fallback.

## Tests

`cargo test -p astrolabe-core watch::` → 20 passed, 1 ignored (`bench_check_on_serena`).

New/updated: `spawn_preserves_seeded_pre_spawn_changeset`, `event_loop_flushes_pending_debounce_on_stop`, `confirm_settle_unreadable_to_readable_no_spurious_modified`, `temporary_unreadable_file_handling`.
