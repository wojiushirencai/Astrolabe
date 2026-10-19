//! Per-root runtime: index, watcher, lock, LSP, and the query-time barrier.
//!
//! The MCP server holds either one [`RootSession`] (Repo / Single) or a
//! [`DispatchState`] of them (MultiProject). Watcher spawn happens *before* the
//! cold build so edits during `IncrementalIndex::build` arrive as channel
//! events instead of being folded into a post-build baseline.

use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use astrolabe_core::cache::{CacheConfig, FileCache};
use astrolabe_core::freshness::{barrier_decision, staleness_note, BarrierDecision, FRESH_WINDOW};
use astrolabe_core::watch::{ChangeSet, FreshnessLog, WatchHandle, Watcher};
use astrolabe_core::RelPath;
use rmcp::model::{CallToolResult, ContentBlock};

use crate::index::{self, RepoIndex};
use crate::knobs::{McpKnobs, WatchMode};
use crate::live_backend::LiveBackend;
use crate::precise_tools::PreciseCapability;
use crate::roots::{DispatcherConfig, RootDispatcher};

/// Serving snapshot for one index root.
#[derive(Debug)]
pub(crate) enum IndexState {
    Building,
    Ready(Arc<RepoIndex>),
    Failed(String),
}

/// `FileCache` is not `Debug`; wrap it so session structs can keep a derive.
#[derive(Clone)]
pub(crate) struct SharedFileCache(pub Arc<FileCache>);

impl Deref for SharedFileCache {
    type Target = FileCache;

    fn deref(&self) -> &FileCache {
        &self.0
    }
}

impl fmt::Debug for SharedFileCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileCache")
            .field("weighted_size", &self.weighted_size())
            .finish()
    }
}

/// Everything that must be created, torn down, and isolated per index root.
pub(crate) struct RootSession {
    pub root: PathBuf,
    pub state: Arc<RwLock<IndexState>>,
    pub watch: Arc<Mutex<Option<WatchHandle>>>,
    pub freshness: Arc<Mutex<Option<Arc<FreshnessLog>>>>,
    pub indexer_lock: Arc<Mutex<Option<astrolabe_core::elect::IndexerLock>>>,
    pub precise: Arc<dyn PreciseCapability>,
    pub pending: Arc<Mutex<ChangeSet>>,
    pub file_cache: SharedFileCache,
    pub cache_hits: Arc<AtomicU64>,
    pub cache_misses: Arc<AtomicU64>,
    pub cache_budget_bytes: u64,
    pub rebuild_inflight: Arc<AtomicBool>,
    /// Changesets applied to the live index while a background rebuild runs;
    /// replayed onto the rebuilt index so the rebuild cannot erase them.
    pub rebuild_journal: Arc<Mutex<ChangeSet>>,
    /// Set by [`RootSession::teardown`]; a bootstrap that races with an
    /// eviction checks it so it never leaves a watcher running. It is also
    /// the cancel token of this session's cold build and background
    /// rebuilds: an evicted root or a shutting-down server stops parsing
    /// instead of finishing a build nobody will serve.
    pub closed: Arc<AtomicBool>,
    /// Full builds (cold or background rebuild) currently running for this
    /// session; [`RootSession::wait_builds_idle`] waits for it to reach 0.
    pub builds_active: Arc<std::sync::atomic::AtomicUsize>,
    /// Last coverage note ("N 个文件有语法错误…") shown in full in this
    /// session. The note is repeated only when it changes, a result is
    /// empty, or a hit lands in an error file.
    pub coverage_note_shown: Arc<Mutex<Option<String>>>,
    pub knobs: McpKnobs,
}

impl fmt::Debug for RootSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootSession")
            .field("root", &self.root)
            .field("cache_budget_bytes", &self.cache_budget_bytes)
            .finish_non_exhaustive()
    }
}

impl RootSession {
    pub(crate) fn new(root: PathBuf, knobs: McpKnobs, cache_budget_bytes: u64) -> Arc<Self> {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let state = Arc::new(RwLock::new(IndexState::Building));
        let syntax_state = Arc::clone(&state);
        let precise =
            LiveBackend::new(root.clone()).with_syntax_index(Arc::new(
                move || match &*syntax_state.read().ok()? {
                    IndexState::Ready(index) => Some(Arc::clone(index)),
                    _ => None,
                },
            ));
        Arc::new(Self {
            precise: Arc::new(precise),
            root,
            state,
            watch: Arc::new(Mutex::new(None)),
            freshness: Arc::new(Mutex::new(None)),
            indexer_lock: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(ChangeSet::default())),
            file_cache: SharedFileCache(Arc::new(FileCache::new(CacheConfig {
                budget_bytes: cache_budget_bytes,
                ..CacheConfig::default()
            }))),
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
            cache_budget_bytes,
            rebuild_inflight: Arc::new(AtomicBool::new(false)),
            rebuild_journal: Arc::new(Mutex::new(ChangeSet::default())),
            closed: Arc::new(AtomicBool::new(false)),
            builds_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            coverage_note_shown: Arc::new(Mutex::new(None)),
            knobs,
        })
    }

    /// Wait (bounded) until no full build of this session is running.
    /// Returns `true` when idle. Used after [`Self::teardown`] at shutdown so
    /// a cancelled build reaches its safe point before the process exits.
    pub(crate) fn wait_builds_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.builds_active.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stop the watcher and drop the indexer lock. Store files stay on disk.
    pub(crate) fn teardown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(handle) = self.watch.lock().expect("watch lock poisoned").take() {
            handle.stop();
        }
        *self.indexer_lock.lock().expect("indexer lock poisoned") = None;
        *self.freshness.lock().expect("freshness lock poisoned") = None;
    }
}

/// Multi-project dispatcher plus the live per-child sessions.
#[derive(Debug)]
pub(crate) struct DispatchState {
    pub dispatcher: Mutex<RootDispatcher>,
    pub sessions: Mutex<HashMap<PathBuf, Arc<RootSession>>>,
    pub children: Vec<PathBuf>,
    pub knobs: McpKnobs,
    pub cache_budget_bytes: u64,
}

impl DispatchState {
    pub(crate) fn new(
        children: Vec<PathBuf>,
        knobs: McpKnobs,
        cache_budget_bytes: u64,
    ) -> Arc<Self> {
        let cfg = DispatcherConfig {
            idle: knobs.idle,
            resident_roots: knobs.resident_roots,
        };
        Arc::new(Self {
            dispatcher: Mutex::new(RootDispatcher::new(children.clone(), cfg)),
            sessions: Mutex::new(HashMap::new()),
            children,
            knobs,
            cache_budget_bytes,
        })
    }
}

/// Counts one running full build in [`RootSession::builds_active`].
struct BuildActive(Arc<std::sync::atomic::AtomicUsize>);

impl BuildActive {
    fn enter(session: &RootSession) -> Self {
        session.builds_active.fetch_add(1, Ordering::SeqCst);
        BuildActive(Arc::clone(&session.builds_active))
    }
}

impl Drop for BuildActive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Full build for `session` that stops early once the session is closed
/// (teardown on eviction / shutdown). `None` = cancelled; nothing to publish.
pub(crate) fn build_session_index(session: &RootSession, persist: bool) -> Option<IndexState> {
    let _active = BuildActive::enter(session);
    let started = Instant::now();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index::build_cancellable(
            &session.root,
            persist,
            session.knobs.parse_cache_bytes,
            &session.closed,
        )
    }));
    let state = match outcome {
        Err(_) => IndexState::Failed("底层索引能力尚未实现或发生 panic".to_string()),
        Ok(None) => {
            tracing::info!(
                root = %session.root.display(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index build cancelled (session closed)"
            );
            return None;
        }
        Ok(Some(index)) => IndexState::Ready(Arc::new(index)),
    };
    Some(state)
}

/// Index once, converting a panic in the engine into a reportable failure.
/// Production paths use the cancellable [`build_session_index`].
#[cfg(test)]
pub(crate) fn build_index(root: &Path, persist: bool, parse_cache_bytes: u64) -> IndexState {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index::build_with_options(root, persist, parse_cache_bytes)
    }))
    .map_err(|_| "底层索引能力尚未实现或发生 panic".to_string())
    .and_then(|result| result.map_err(|error| error.to_string()))
    .map(Arc::new)
    .map(IndexState::Ready)
    .unwrap_or_else(IndexState::Failed)
}

/// Spawn the cold-build thread. Watcher is started *inside* that thread
/// before `IncrementalIndex::build`.
pub(crate) fn start_root_indexing(session: Arc<RootSession>) {
    std::thread::Builder::new()
        .name("astrolabe-index".into())
        .spawn(move || bootstrap_root(session))
        .expect("failed to spawn index thread");
}

fn bootstrap_root(session: Arc<RootSession>) {
    if session.closed.load(Ordering::SeqCst) {
        return;
    }
    let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);

    // Spawn the watcher (and take its baseline) BEFORE the cold build.
    // Edits during IncrementalIndex::build must arrive as channel events, not
    // be absorbed into a post-build baseline; the reindex thread buffers them
    // until Ready and applies once.
    //
    // `start_watching` publishes the handle before the watch thread finishes
    // its first `check`. Waiting here closes that gap: a file created after
    // the handle exists but before the scan returns would otherwise be sealed
    // into the baseline (no event) while `build_index` walks past it.
    if let Some(mode) = session.knobs.watch_mode {
        start_watching(&session, mode);
        wait_for_watcher_baseline(&session);
    }

    if session.closed.load(Ordering::SeqCst) {
        // Evicted before the cold build started: skip the whole build.
        session.teardown();
        return;
    }
    // Cancelled when the session closes mid-build (eviction / shutdown).
    let Some(outcome) = build_session_index(&session, persist) else {
        session.teardown();
        return;
    };
    publish_ready(&session, outcome);
}

/// Block the cold build until the watcher thread has sealed its baseline.
///
/// No-op when watching is disabled. Gives up after 60s so a stuck watch
/// thread cannot pin the server in `Building` forever.
fn wait_for_watcher_baseline(session: &RootSession) {
    let log = {
        let guard = session.freshness.lock().expect("freshness lock poisoned");
        guard.as_ref().map(Arc::clone)
    };
    let Some(log) = log else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !log.is_seeded() {
        if Instant::now() >= deadline {
            tracing::warn!(
                root = %session.root.display(),
                "watcher baseline was not ready before the cold build; edits during startup may be missed"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Watch the repository and incrementally rebuild the graph when files change.
///
/// The previous index keeps serving for the whole rebuild: dropping back to
/// `Building` would make every tool call fail for a second because someone
/// saved a file. A stale answer beats no answer here, and the window is short.
/// A failed rebuild is also discarded rather than published, so one bad edit
/// cannot replace a working index with an error.
///
/// Reindex loop:
///   recv → coalesce (throttle window, merge every queued ChangeSet) →
///   language-filter (non-source added/modified dropped; removed kept) →
///   skip if empty → if not Ready, merge into pending; if Ready, apply.
fn start_watching(session: &Arc<RootSession>, mode: WatchMode) {
    let watcher = Watcher::new(session.root.clone())
        .safety_interval(session.knobs.safety_interval)
        .storm_paths(session.knobs.storm_paths);
    let (rx, handle, log) = match mode {
        WatchMode::Events => {
            tracing::info!(root = %session.root.display(), "watching for changes (native OS events)");
            watcher.spawn_with_log()
        }
        WatchMode::Poll(interval) => {
            tracing::info!(
                root = %session.root.display(),
                interval_secs = interval.as_secs(),
                "watching for changes (forced polling)"
            );
            watcher
                .poll_interval(interval)
                .events(false)
                .spawn_with_log()
        }
    };
    *session.watch.lock().expect("watch lock poisoned") = Some(handle);
    *session.freshness.lock().expect("freshness lock poisoned") = Some(log);
    if session.closed.load(Ordering::SeqCst) {
        // Evicted while the watcher was starting: teardown already ran and
        // missed the handle stored above, so stop it here instead of leaking.
        session.teardown();
        return;
    }

    let session = Arc::clone(session);
    let throttle = session.knobs.reindex_throttle;
    std::thread::Builder::new()
        .name("astrolabe-reindex".into())
        .spawn(move || {
            crate::reindex::for_each_reindex_batch(&session.root, rx, throttle, |changes| {
                tracing::info!(
                    added = changes.added.len(),
                    modified = changes.modified.len(),
                    removed = changes.removed.len(),
                    "reindexing after file changes"
                );
                // Before reindexing: the LSP sync is cheap and a query racing
                // the rebuild should already see the new text.
                session.precise.files_changed(changes);
                apply_or_buffer(&session, changes);
            });
        })
        .expect("failed to spawn reindex thread");
}

/// Lock order: pending, then state. Matches [`publish_ready`] and [`inline_repair`].
fn apply_or_buffer(session: &RootSession, changes: &ChangeSet) {
    let mut pending = session.pending.lock().expect("pending lock poisoned");
    pending.merge(changes.clone());
    apply_pending(session, &mut pending);
}

fn publish_ready(session: &RootSession, mut outcome: IndexState) {
    let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
    let opts = crate::reindex::mcp_index_options(persist, session.knobs.parse_cache_bytes);

    let mut pending = session.pending.lock().expect("pending lock poisoned");

    if let IndexState::Ready(index) = &outcome {
        let drained = std::mem::take(&mut *pending);
        if !drained.is_empty() {
            if let Ok(inc) =
                crate::reindex::apply_reindex(&session.root, index.incremental(), &drained, &opts)
            {
                outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                    session.root.clone(),
                    inc,
                )));
            }
        }
    }
    if session.closed.load(Ordering::SeqCst) {
        // Evicted during the cold build: do not publish into a dead session.
        session.teardown();
        return;
    }
    if let IndexState::Ready(index) = &outcome {
        index.prewarm(true);
    }
    *session.state.write().unwrap_or_else(|e| e.into_inner()) = outcome;
}

/// Apply buffered changes when the index is Ready. The caller holds the
/// `pending` lock, which serialises every writer of `state` (apply, publish),
/// so the heavy `apply_reindex` runs on a snapshot *outside* the state lock and
/// the write lock is held only for the `Arc` swap. Queries keep reading the old
/// index meanwhile instead of queueing behind the rebuild.
fn apply_pending(session: &RootSession, pending: &mut ChangeSet) {
    let previous = match &*session.state.read().unwrap_or_else(|e| e.into_inner()) {
        IndexState::Ready(index) => Arc::clone(index),
        _ => return,
    };
    let changes = std::mem::take(pending);
    if changes.is_empty() {
        return;
    }
    let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
    let opts = crate::reindex::mcp_index_options(persist, session.knobs.parse_cache_bytes);
    match crate::reindex::apply_reindex(&session.root, previous.incremental(), &changes, &opts) {
        Ok(inc) => {
            let next = Arc::new(index::RepoIndex::from_incremental(
                session.root.clone(),
                inc,
            ));
            next.prewarm(false);
            *session.state.write().unwrap_or_else(|e| e.into_inner()) = IndexState::Ready(next);
            if session.rebuild_inflight.load(Ordering::SeqCst) {
                session
                    .rebuild_journal
                    .lock()
                    .expect("rebuild journal lock poisoned")
                    .merge(changes);
            }
        }
        Err(error) => {
            tracing::warn!(%error, "reindex failed; keeping previous index");
        }
    }
}

pub(crate) enum BarrierEffect {
    None,
    StaleNote(String),
}

/// Query-time freshness barrier. No-ops when watching is disabled (no log).
pub(crate) fn run_barrier(session: &Arc<RootSession>) -> BarrierEffect {
    let log = {
        let g = session.freshness.lock().expect("freshness lock poisoned");
        match g.as_ref() {
            Some(log) => Arc::clone(log),
            None => return BarrierEffect::None,
        }
    };

    let started = Instant::now();
    // Suspects noted up to here are covered by whatever repair this call does.
    let observed_at = started;
    let watermark = log.watermark();
    let watermark_age = watermark.elapsed();
    let suspects = log.suspect_paths(watermark);
    let distrusted = log.distrusted();
    let max_inline = session.knobs.barrier_max;
    let decision = barrier_decision(
        watermark_age,
        fresh_window_for(&session.knobs, log.poll_cycle()),
        distrusted,
        &suspects,
        max_inline,
    );
    tracing::debug!(
        ?decision,
        elapsed_ms = started.elapsed().as_millis() as u64,
        suspects = suspects.len(),
        "freshness barrier"
    );

    match decision {
        BarrierDecision::Fresh => BarrierEffect::None,
        BarrierDecision::Repair(paths) if paths.is_empty() => {
            let before = log.watermark();
            log.request_verification();
            if wait_watermark_advance(&log, before, BARRIER_VERIFY_WAIT) {
                // The verification may have found changes that are not in
                // the index yet; they are suspects after the new watermark.
                let observed = Instant::now();
                let found = log.suspect_paths(log.watermark());
                if log.distrusted() || found.len() > max_inline {
                    trigger_background_rebuild_arc(session);
                    BarrierEffect::StaleNote(staleness_note(found.len()))
                } else {
                    if !found.is_empty() {
                        inline_repair(session, found);
                        log.clear_suspects_through(observed);
                    }
                    BarrierEffect::None
                }
            } else {
                // The verification scan did not finish in time: we answer
                // from an unverified index, so say so instead of staying silent.
                BarrierEffect::StaleNote(verification_timeout_note())
            }
        }
        BarrierDecision::Repair(paths) => {
            inline_repair(session, paths);
            log.clear_suspects_through(observed_at);
            BarrierEffect::None
        }
        BarrierDecision::Degrade { suspects } => {
            trigger_background_rebuild_arc(session);
            BarrierEffect::StaleNote(staleness_note(suspects))
        }
    }
}

/// Freshness window for the barrier. With native events and a safety scan,
/// the watermark only advances on that scan while idle, but every change
/// arrives as a suspect, so an idle watermark older than `FRESH_WINDOW` does
/// not need a full-tree verification per query (M1). In that mode the window
/// is the safety interval plus slack; otherwise it is `FRESH_WINDOW`.
///
/// `poll_cycle` is the watcher's published polling cycle (forced polling, or
/// events that fell back to polling at runtime). A poller advances the
/// watermark once per cycle and records every change it finds as a suspect,
/// so the window is that cycle plus slack: a fixed 5 s window made every
/// query in the second half of a 10 s cycle force an extra full scan (and
/// a 2 s wait / stale note on big trees), while a 30 min events window after
/// a fallback would never notice a stalled poller. The barrier still forces a
/// verification once the poller is overdue.
pub(crate) fn fresh_window_for(
    knobs: &crate::knobs::McpKnobs,
    poll_cycle: Option<Duration>,
) -> Duration {
    if let Some(cycle) = poll_cycle {
        return (cycle + (cycle / 4).max(Duration::from_secs(1))).max(FRESH_WINDOW);
    }
    match knobs.watch_mode {
        Some(crate::knobs::WatchMode::Events) if !knobs.safety_interval.is_zero() => {
            (knobs.safety_interval + Duration::from_secs(60)).max(FRESH_WINDOW)
        }
        _ => FRESH_WINDOW,
    }
}

/// How long a `Repair([])` barrier waits for the requested full check.
const BARRIER_VERIFY_WAIT: Duration = Duration::from_secs(2);

pub(crate) fn verification_timeout_note() -> String {
    "索引可能过期：变更校验未在 2 秒内完成，本结果基于未确认的索引。confidence: unknown".to_string()
}

/// Returns `true` when the watermark advanced before `timeout`.
fn wait_watermark_advance(log: &FreshnessLog, before: Instant, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if log.watermark() != before {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn inline_repair(session: &RootSession, paths: Vec<RelPath>) {
    // Suspects are raw event paths: classify them with the scan gates so a
    // gitignored / deleted / oversized path is removed instead of being
    // blindly parsed into the index.
    let scan_opts = astrolabe_core::scan::ScanOptions::default();
    let mut repair = ChangeSet::default();
    for path in paths {
        match astrolabe_core::scan::path_admission(&session.root, &path, &scan_opts) {
            astrolabe_core::scan::PathAdmission::Indexed => repair.modified.push(path),
            _ => repair.removed.push(path),
        }
    }
    let repair = crate::reindex::filter_changeset(&session.root, repair);
    if repair.is_empty() {
        return;
    }
    let mut pending = session.pending.lock().expect("pending lock poisoned");
    pending.merge(repair);
    apply_pending(session, &mut pending);
}

/// Kick a full `IncrementalIndex::build` without dropping to `Building`.
pub(crate) fn trigger_background_rebuild_arc(session: &Arc<RootSession>) {
    if session
        .rebuild_inflight
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    session
        .rebuild_journal
        .lock()
        .expect("rebuild journal lock poisoned")
        .clone_from(&ChangeSet::default());
    let session = Arc::clone(session);
    std::thread::Builder::new()
        .name("astrolabe-rebuild".into())
        .spawn(move || {
            let started = Instant::now();
            let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
            let outcome = build_session_index(&session, persist);
            if let Some(outcome @ IndexState::Ready(_)) = outcome {
                publish_rebuilt(&session, outcome);
                // The rebuilt index covers every suspect noted before it began.
                let log = session
                    .freshness
                    .lock()
                    .expect("freshness lock poisoned")
                    .clone();
                if let Some(log) = log {
                    log.clear_suspects_through(started);
                }
            }
            session.rebuild_inflight.store(false, Ordering::SeqCst);
        })
        .ok();
}

/// Publish a background rebuild without losing changes that were applied to
/// the old index (journal) or buffered (pending) while it was building.
fn publish_rebuilt(session: &RootSession, mut outcome: IndexState) {
    let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
    let opts = crate::reindex::mcp_index_options(persist, session.knobs.parse_cache_bytes);
    let mut pending = session.pending.lock().expect("pending lock poisoned");
    let mut replay = std::mem::take(
        &mut *session
            .rebuild_journal
            .lock()
            .expect("rebuild journal lock poisoned"),
    );
    replay.merge(std::mem::take(&mut *pending));
    if let IndexState::Ready(index) = &outcome {
        if !replay.is_empty() {
            if let Ok(inc) =
                crate::reindex::apply_reindex(&session.root, index.incremental(), &replay, &opts)
            {
                outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                    session.root.clone(),
                    inc,
                )));
            }
        }
    }
    if let IndexState::Ready(index) = &outcome {
        index.prewarm(true);
    }
    *session.state.write().unwrap_or_else(|e| e.into_inner()) = outcome;
}

/// Append a staleness note to the first text block of an already-finished result.
pub(crate) fn append_stale_note(result: CallToolResult, note: &str) -> CallToolResult {
    let mut result = result;
    if let Some(first) = result.content.first_mut() {
        if let Some(existing) = first.as_text() {
            let new_text = format!("{}\n{note}", existing.text.trim_end());
            result.content[0] = ContentBlock::text(new_text);
        }
    }
    result
}

pub(crate) fn unroutable_message(children: &[PathBuf]) -> String {
    let mut out = String::from(
        "多项目工作区：无法路由到唯一子仓。请加 root 参数（子仓名或绝对路径），或让 path 以子仓名/绝对路径开头。候选：\n",
    );
    for child in children {
        out.push_str(&format!("  {}\n", child.display()));
    }
    if let Some(name) = children
        .first()
        .and_then(|c| c.file_name())
        .and_then(|n| n.to_str())
    {
        out.push_str(&format!("例如 root=\"{name}\"\n"));
    }
    out.push_str("confidence: unknown");
    out
}

pub(crate) fn route_session(
    dispatch: &DispatchState,
    hint: &str,
) -> Result<Arc<RootSession>, String> {
    let (path, evicted) = {
        let mut disp = dispatch
            .dispatcher
            .lock()
            .expect("dispatcher lock poisoned");
        match disp.route(hint) {
            Some(path) => {
                let path = path.to_path_buf();
                let evicted = disp.activate(&path, Instant::now());
                (path, evicted)
            }
            None => return Err(unroutable_message(&dispatch.children)),
        }
    };
    for evicted in evicted {
        teardown_child(dispatch, &evicted);
    }
    Ok(ensure_session(dispatch, &path))
}

fn ensure_session(dispatch: &DispatchState, root: &Path) -> Arc<RootSession> {
    let mut map = dispatch.sessions.lock().expect("sessions lock poisoned");
    if let Some(existing) = map.get(root) {
        return Arc::clone(existing);
    }
    let session = RootSession::new(
        root.to_path_buf(),
        dispatch.knobs.clone(),
        dispatch.cache_budget_bytes,
    );
    map.insert(root.to_path_buf(), Arc::clone(&session));
    drop(map);
    start_root_indexing(Arc::clone(&session));
    session
}

pub(crate) fn teardown_child(dispatch: &DispatchState, root: &Path) {
    let session = dispatch
        .sessions
        .lock()
        .expect("sessions lock poisoned")
        .remove(root);
    if let Some(session) = session {
        session.teardown();
    }
}

pub(crate) fn start_idle_ticker(dispatch: Arc<DispatchState>, stop: Arc<AtomicBool>) {
    let idle = dispatch.knobs.idle;
    let interval = {
        let quarter = idle / 4;
        let capped = quarter.min(Duration::from_secs(30));
        if capped.is_zero() {
            Duration::from_millis(50)
        } else {
            capped
        }
    };
    std::thread::Builder::new()
        .name("astrolabe-idle-evict".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let evicted = dispatch
                    .dispatcher
                    .lock()
                    .expect("dispatcher lock poisoned")
                    .evict_idle(Instant::now());
                for path in evicted {
                    teardown_child(&dispatch, &path);
                }
            }
        })
        .expect("failed to spawn idle-evict thread");
}

pub(crate) fn teardown_all(dispatch: &DispatchState) {
    let sessions: Vec<Arc<RootSession>> = dispatch
        .sessions
        .lock()
        .expect("sessions lock poisoned")
        .drain()
        .map(|(_, s)| s)
        .collect();
    for session in sessions {
        session.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astrolabe_core::index::DEFAULT_PARSE_CACHE_BYTES;
    use std::sync::atomic::AtomicU64;

    fn unique_temp_dir() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-session-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn fresh_window_wide_only_for_events_with_safety_scan() {
        let mut k = McpKnobs::default();
        assert_eq!(fresh_window_for(&k, None), Duration::from_secs(1860));
        k.safety_interval = Duration::ZERO;
        assert_eq!(fresh_window_for(&k, None), FRESH_WINDOW);
        k.safety_interval = Duration::from_secs(1800);
        k.watch_mode = Some(crate::knobs::WatchMode::Poll(Duration::from_secs(10)));
        assert_eq!(fresh_window_for(&k, None), FRESH_WINDOW);
    }

    #[test]
    fn fresh_window_follows_the_published_poll_cycle() {
        let mut k = McpKnobs {
            watch_mode: Some(crate::knobs::WatchMode::Poll(Duration::from_secs(10))),
            ..McpKnobs::default()
        };
        // 10 s cycle + 2.5 s slack: no forced verification inside a cycle.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_millis(10_100))),
            Duration::from_millis(12_625)
        );
        // Backed-off poller on a huge tree: the window grows with it.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_secs(40))),
            Duration::from_secs(50)
        );
        // Short cycles never drop below the default window.
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_millis(200))),
            FRESH_WINDOW
        );
        // Events mode that fell back to polling: the poller's cycle wins over
        // the 30 min events window.
        k.watch_mode = Some(crate::knobs::WatchMode::Events);
        assert_eq!(
            fresh_window_for(&k, Some(Duration::from_secs(10))),
            Duration::from_millis(12_500)
        );
    }

    #[test]
    fn poll_mode_barrier_does_not_force_scans_inside_a_cycle() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(crate::knobs::WatchMode::Poll(Duration::from_secs(8))),
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        start_root_indexing(Arc::clone(&session));
        let deadline = Instant::now() + Duration::from_secs(20);
        while !matches!(&*session.state.read().unwrap(), IndexState::Ready(_)) {
            assert!(Instant::now() < deadline, "index never ready");
            std::thread::sleep(Duration::from_millis(20));
        }
        let log = session.freshness.lock().unwrap().clone().expect("log");
        // Age the watermark past the old fixed 5 s window, still inside the
        // 8 s cycle (the poller rescans at ~8 s).
        let wm = log.watermark();
        while wm.elapsed() < FRESH_WINDOW + Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if log.watermark() == wm && wm.elapsed() < Duration::from_secs(7) {
            assert!(matches!(run_barrier(&session), BarrierEffect::None));
            assert_eq!(
                log.watermark(),
                wm,
                "a query inside the poll cycle must not force a full scan"
            );
        }
        // A real edit is still caught by the next poll and repaired inline.
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let _ = run_barrier(&session);
            let found = match &*session.state.read().unwrap() {
                IndexState::Ready(index) => index.graph().symbols.iter().any(|s| s.name == "beta"),
                _ => false,
            };
            if found {
                break;
            }
            assert!(Instant::now() < deadline, "poller never surfaced b.py");
            std::thread::sleep(Duration::from_millis(100));
        }
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verification_wait_reports_timeout() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Live watcher: a requested verification advances the watermark.
        let before = log.watermark();
        log.request_verification();
        assert!(wait_watermark_advance(&log, before, Duration::from_secs(5)));
        // Stopped watcher: nobody advances it, the wait must report timeout.
        handle.stop();
        let before = log.watermark();
        log.request_verification();
        assert!(!wait_watermark_advance(
            &log,
            before,
            Duration::from_millis(50)
        ));
        assert!(verification_timeout_note().contains("索引可能过期"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn publish_ready_applies_pending_changeset() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);

        let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
        let cold = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        match &cold {
            IndexState::Ready(index) => {
                assert!(index.graph().symbols.iter().any(|s| s.name == "alpha"));
                assert!(!index
                    .graph()
                    .symbols
                    .iter()
                    .any(|s| s.name == "during_build"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }

        std::fs::write(dir.join("b.py"), "def during_build():\n    return 2\n").unwrap();
        *session.pending.lock().unwrap() = ChangeSet {
            added: vec![RelPath::new("b.py")],
            ..ChangeSet::default()
        };
        publish_ready(&session, cold);

        match &*session.state.read().unwrap() {
            IndexState::Ready(index) => {
                assert!(
                    index
                        .graph()
                        .symbols
                        .iter()
                        .any(|s| s.name == "during_build"),
                    "pending changeset must be applied before Ready is published"
                );
                assert!(index.graph().symbols.iter().any(|s| s.name == "alpha"));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ready_session(dir: &Path) -> Arc<RootSession> {
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.to_path_buf(), knobs, 8 * 1024 * 1024);
        let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
        let cold = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        assert!(matches!(cold, IndexState::Ready(_)));
        *session.state.write().unwrap() = cold;
        session
    }

    fn has_symbol(session: &RootSession, name: &str) -> bool {
        match &*session.state.read().unwrap() {
            IndexState::Ready(index) => index.graph().symbols.iter().any(|s| s.name == name),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn inline_repair_classifies_ignored_and_deleted_suspects() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        std::fs::write(dir.join("gone.py"), "def gone_fn():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        assert!(has_symbol(&session, "gone_fn"));

        std::fs::write(dir.join(".gitignore"), "gen/\n").unwrap();
        std::fs::create_dir_all(dir.join("gen")).unwrap();
        std::fs::write(dir.join("gen/g.py"), "def ignored_fn():\n    return 1\n").unwrap();
        std::fs::write(dir.join("fresh.py"), "def fresh_fn():\n    return 1\n").unwrap();
        std::fs::remove_file(dir.join("gone.py")).unwrap();

        inline_repair(
            &session,
            vec![
                RelPath::new("gen/g.py"),
                RelPath::new("gone.py"),
                RelPath::new("fresh.py"),
            ],
        );
        assert!(has_symbol(&session, "fresh_fn"));
        assert!(
            !has_symbol(&session, "ignored_fn"),
            "gitignored must not enter"
        );
        assert!(!has_symbol(&session, "gone_fn"), "deleted must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rebuild_publish_replays_changes_applied_during_rebuild() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        // Rebuild "reads" the tree now (before the edit below).
        let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
        let rebuilt = build_index(&session.root, persist, session.knobs.parse_cache_bytes);

        // An incremental batch lands on the live index while the rebuild runs.
        session.rebuild_inflight.store(true, Ordering::SeqCst);
        std::fs::write(dir.join("b.py"), "def during_rebuild():\n    return 2\n").unwrap();
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(has_symbol(&session, "during_rebuild"));

        publish_rebuilt(&session, rebuilt);
        session.rebuild_inflight.store(false, Ordering::SeqCst);
        assert!(
            has_symbol(&session, "during_rebuild"),
            "rebuild must not erase a change applied while it ran"
        );
        assert!(has_symbol(&session, "alpha"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn barrier_repairs_batch_that_is_sent_but_not_applied() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        *session.freshness.lock().unwrap() = Some(Arc::clone(&log));
        // The receiver is never drained, so the batch is "sent, not applied".
        std::fs::write(dir.join("b.py"), "def late_fn():\n    return 2\n").unwrap();
        let before = log.watermark();
        log.request_verification();
        assert!(wait_watermark_advance(&log, before, Duration::from_secs(5)));
        // Watermark is fresh, yet the index lacks b.py.
        assert!(!has_symbol(&session, "late_fn"));
        let _ = run_barrier(&session);
        assert!(has_symbol(&session, "late_fn"), "barrier must repair it");
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unroutable_message_lists_children() {
        let msg = unroutable_message(&[PathBuf::from("/proj/alpha"), PathBuf::from("/proj/beta")]);
        assert!(msg.contains("/proj/alpha"), "{msg}");
        assert!(msg.contains("/proj/beta"), "{msg}");
        assert!(msg.contains("confidence: unknown"), "{msg}");
    }

    #[test]
    fn watcher_started_after_teardown_does_not_leak() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(WatchMode::Events),
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        // Eviction wins the race: teardown runs before the watcher starts.
        session.teardown();
        start_watching(&session, WatchMode::Events);
        assert!(
            session.watch.lock().unwrap().is_none(),
            "a watcher spawned for a torn-down session must be stopped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn evicted_session_skips_cold_build_and_never_publishes() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        session.teardown();
        bootstrap_root(Arc::clone(&session));
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Building
        ));
        // A build that finishes after eviction is dropped too.
        let persist = false;
        let built = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        publish_ready(&session, built);
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Building
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A repo big enough that a debug cold build takes a while.
    fn write_many_py(dir: &Path, n: usize) {
        for i in 0..n {
            let sub = dir.join(format!("pkg{}", i % 40));
            std::fs::create_dir_all(&sub).unwrap();
            let mut body = String::new();
            for j in 0..12 {
                body.push_str(&format!(
                    "def f{i}_{j}(x):\n    return g{i}_{j}(x) + {j}\n\n\nclass C{i}_{j}:\n    def m(self):\n        return f{i}_{j}(1)\n\n\n"
                ));
            }
            std::fs::write(sub.join(format!("m{i}.py")), body).unwrap();
        }
    }

    #[test]
    fn teardown_cancels_an_in_flight_cold_build() {
        let dir = unique_temp_dir();
        write_many_py(&dir, 1500);
        // Control: how long the full build takes on this machine.
        let t = Instant::now();
        let _ = build_index(&dir, false, DEFAULT_PARSE_CACHE_BYTES);
        let full = t.elapsed();

        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        start_root_indexing(Arc::clone(&session));
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.builds_active.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "cold build never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(full / 10);
        let asked = Instant::now();
        session.teardown();
        assert!(
            session.wait_builds_idle(Duration::from_secs(30)),
            "cancelled build never stopped"
        );
        let latency = asked.elapsed();
        assert!(
            latency < full / 2,
            "cancel took {latency:?}, a full build takes {full:?}"
        );
        assert!(
            matches!(*session.state.read().unwrap(), IndexState::Building),
            "a cancelled build must not be published"
        );
        // Cancelled before the parse finished: no stamps were committed (the
        // repo is far below the 32 MB mid-run flush).
        if let Ok(Some(store)) = astrolabe_core::store::Store::open_read_only(
            &astrolabe_core::index::store_path(&session.root),
        ) {
            assert!(store.file_stamp("pkg0/m0.py").unwrap().is_none());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelled_background_rebuild_does_not_publish() {
        let dir = unique_temp_dir();
        write_many_py(&dir, 1500);
        let knobs = McpKnobs {
            watch_mode: None,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let cold = build_index(&session.root, false, session.knobs.parse_cache_bytes);
        let IndexState::Ready(before) = cold else {
            panic!("cold build failed");
        };
        *session.state.write().unwrap() = IndexState::Ready(Arc::clone(&before));
        trigger_background_rebuild_arc(&session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.builds_active.load(Ordering::SeqCst) == 0
            && session.rebuild_inflight.load(Ordering::SeqCst)
        {
            assert!(Instant::now() < deadline, "rebuild never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        session.teardown();
        assert!(session.wait_builds_idle(Duration::from_secs(30)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while session.rebuild_inflight.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "rebuild flag never cleared");
            std::thread::sleep(Duration::from_millis(1));
        }
        match &*session.state.read().unwrap() {
            IndexState::Ready(now) => assert!(
                Arc::ptr_eq(now, &before),
                "a rebuild cancelled by teardown must not replace the index"
            ),
            other => panic!("unexpected state {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_pending_swaps_index_without_holding_state_lock_during_reindex() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let cold = build_index(&session.root, false, session.knobs.parse_cache_bytes);
        *session.state.write().unwrap() = cold;
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let mut pending = ChangeSet {
            added: vec![RelPath::new("b.py")],
            ..ChangeSet::default()
        };
        // A reader holding the read guard must not deadlock the apply (it
        // only takes the write lock for the swap, after dropping its read).
        let old = match &*session.state.read().unwrap() {
            IndexState::Ready(i) => Arc::clone(i),
            _ => panic!("ready"),
        };
        apply_pending(&session, &mut pending);
        assert!(pending.is_empty());
        match &*session.state.read().unwrap() {
            IndexState::Ready(i) => {
                assert!(i.graph().symbols.iter().any(|s| s.name == "beta"));
                assert!(!old.graph().symbols.iter().any(|s| s.name == "beta"));
            }
            _ => panic!("ready"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
