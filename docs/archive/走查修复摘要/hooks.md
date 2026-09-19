# hooks.rs walkthrough fixes

File: `crates/astrolabe-mcp/src/hooks.rs` (code only; no git commit)

## SEVERE

### 1. session_id sanitization
- Added `sanitize_session_id`: rejects empty, `/`, `\`, `..`, embedded `..`, and any char outside `[A-Za-z0-9_-]`.
- `get_hook_data_dir` now returns `Result` and sanitizes before path join.
- `cleanup_session` maps sanitize failure to `io::ErrorKind::InvalidInput`.
- `run_remind_impl` / `run_cleanup_impl` write the error to stderr and return exit code **2** on invalid session_id.

### 2. Counter load/decide/save locking + atomic save
- Added exclusive lock file `counter.json.lock` beside the counter (`CounterFileLock` via `create_new` + short retry).
- `with_counter_locked` serializes load → decide → optional save under that lock in `run_remind_impl`.
- `save_counter` is atomic: write `counter.json.tmp`, then `rename` onto `counter.json` (tmp removed on failure).

## NORMAL

### 3. cleanup failure propagation
- `run_cleanup_impl` no longer ignores `cleanup_session` errors or logs success on failure.
- On `Err`, writes stderr (`Failed to cleanup session data: …`) and returns **2**; success debug log only on `Ok`.

### 4. Comments synced with implementation
- Slice-read docs: require explicit `limit <= 120`; offset alone is not enough (removed stale `offset>1` wording).
- `is_astrolabe_symbolic_tool` doc now includes `memory` in the non-symbolic substring set.
- Sed print-token docs: only pure numeric `Np` / `A,Bp`; reject `$p` / `/re/p` (matches code).
- `classify_tool` / inline slice comments updated accordingly.

## Tests
- `test_sanitize_session_id_accepts_safe_ids`
- `test_sanitize_session_id_rejects_unsafe_ids`
- `test_invalid_session_id_remind_and_cleanup_exit_2`
- `test_save_counter_atomic_writes_via_tmp_rename` (atomic write + locked mutate round-trip)
- Existing callers updated for `get_hook_data_dir(...).unwrap()`

## Verification
- `cargo check -p astrolabe-mcp`: ok
- `cargo test -p astrolabe-mcp --lib hooks::`: **29 passed**
