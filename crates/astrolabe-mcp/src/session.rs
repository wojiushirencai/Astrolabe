//! Per-root runtime: index, watcher, lock, LSP, and the query-time barrier.
//!
//! The MCP server holds either one [`RootSession`] (Repo / Single) or a
//! [`DispatchState`] of them (MultiProject). Watcher spawn happens *before* the
//! cold build so edits during `IncrementalIndex::build` arrive as channel
//! events instead of being folded into a post-build baseline.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime};

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

/// Lock a mutex, recovering the data if a panicking thread poisoned it.
///
/// Every value guarded in a session (pending changesets, the watch handle,
/// the freshness log slot, the dispatcher maps) stays structurally valid
/// across a panic: the worst case is a partially merged changeset, which the
/// next apply or rebuild converges. Crashing every later tool call with
/// "poisoned" is strictly worse than serving from that state.
pub(crate) trait LockOrRecover<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOrRecover<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("recovering a poisoned session lock");
            self.clear_poison();
            poisoned.into_inner()
        })
    }
}

/// Changes the index failed to absorb, kept so they are retried instead of
/// silently dropped (F4). Lock order: `pending` before this.
#[derive(Debug, Default)]
pub(crate) struct ApplyHealth {
    /// Paths whose incremental update failed and is not in the index yet.
    pub unapplied: ChangeSet,
    /// Earliest next query-time retry of `unapplied` (one failing file must
    /// not cost a full apply on every call).
    pub next_inline_retry: Option<Instant>,
    /// A full background rebuild failed; do not start another before this.
    pub rebuild_backoff_until: Option<Instant>,
    /// Failed changes handed to the running background rebuild, which reads
    /// the whole tree after taking them. Cleared when it publishes, put back
    /// into `unapplied` when it fails.
    pub in_rebuild: ChangeSet,
}

/// Gap between query-time retries of a failed incremental update.
const INLINE_RETRY_GAP: Duration = Duration::from_secs(2);
/// Gap before another background rebuild after one failed.
const REBUILD_BACKOFF: Duration = Duration::from_secs(60);

/// Query-time discovery bookkeeping.
///
/// `since` is captured *before* each walk, so a file created while the walk
/// ran still lands in the next one. The small backward slack covers
/// filesystems whose directory timestamps are coarser than `SystemTime::now`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DiscoveryState {
    pub since: SystemTime,
}

impl Default for DiscoveryState {
    fn default() -> Self {
        Self {
            since: SystemTime::now(),
        }
    }
}

impl DiscoveryState {
    fn walk_since(&self) -> SystemTime {
        self.since
            .checked_sub(Duration::from_secs(2))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

/// Default hard-hibernation idle threshold (`ASTROLABE_HIBERNATE_SECS`): the
/// primary index is unloaded entirely after 30 min without a routed request.
pub(crate) const DEFAULT_HIBERNATE_SECS: u64 = 1800;
/// Default gap before a freshly woken index may hibernate again
/// (`ASTROLABE_WAKEUP_COOLDOWN_SECS`).
pub(crate) const DEFAULT_WAKEUP_COOLDOWN_SECS: u64 = 60;
/// How long a tool call waits for a hibernated index to finish its warm start
/// before answering "index is building, retry shortly".
pub(crate) const WAKE_WAIT: Duration = Duration::from_secs(5);

/// Two-stage idle policy of one primary session.
///
/// * `soft_evict`: shed lazily-derived in-memory lookup tables (they rebuild
///   from the still-resident graph on the next query). Configured through
///   `ASTROLABE_IDLE_EVICT_SECS` (the same knob the multi-root dispatcher
///   uses), default 300 s.
/// * `hibernate`: drop the whole `RepoIndex` and return its pages to the OS.
///   `ASTROLABE_HIBERNATE_SECS`, default 1800 s.
/// * `wakeup_cooldown`: a session that just woke must stay up at least this
///   long before it may hibernate again. `ASTROLABE_WAKEUP_COOLDOWN_SECS`,
///   default 60 s.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IdlePolicy {
    pub soft_evict: Duration,
    pub hibernate: Duration,
    pub wakeup_cooldown: Duration,
}

impl IdlePolicy {
    pub(crate) fn from_env(soft_evict: Duration) -> Self {
        Self {
            soft_evict,
            hibernate: crate::knobs::parse_secs_knob(
                std::env::var("ASTROLABE_HIBERNATE_SECS").ok().as_deref(),
                DEFAULT_HIBERNATE_SECS,
                "ASTROLABE_HIBERNATE_SECS",
            ),
            wakeup_cooldown: crate::knobs::parse_secs_knob(
                std::env::var("ASTROLABE_WAKEUP_COOLDOWN_SECS")
                    .ok()
                    .as_deref(),
                DEFAULT_WAKEUP_COOLDOWN_SECS,
                "ASTROLABE_WAKEUP_COOLDOWN_SECS",
            ),
        }
    }
}

/// Seconds since a process-local monotonic epoch, starting at 1 so `0` can
/// mean "never". Only differences between two values are meaningful.
pub(crate) fn current_time_secs() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    1 + EPOCH.get_or_init(Instant::now).elapsed().as_secs()
}

/// Serving snapshot for one index root.
#[derive(Debug)]
pub(crate) enum IndexState {
    Building,
    Ready(Arc<RepoIndex>),
    Failed(String),
    /// The primary index was completely unloaded during prolonged idle (hard
    /// hibernation), releasing its memory and swap back to the OS. The next
    /// tool call flips this back to `Building` and rebuilds from the on-disk
    /// store ([`RootSession::wake_if_hibernated`]).
    Hibernated,
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
    /// Wakes callers waiting on an initial cold build or warm wake to leave
    /// `IndexState::Building` (or a hibernated in-flight rebuild to publish).
    pub state_cv: Arc<(std::sync::Mutex<u64>, std::sync::Condvar)>,
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
    /// Index writers (cold/warm builds, rebuilds and incremental applies),
    /// counted through replay and publication, not just parsing.
    pub builds_active: Arc<std::sync::atomic::AtomicUsize>,
    /// Serializes shutdown with ownership acquisition and publication.
    lifecycle: Arc<Mutex<()>>,
    /// Last coverage note ("N 个文件有语法错误…") shown in full in this
    /// session. The note is repeated only when it changes, a result is
    /// empty, or a hit lands in an error file.
    pub coverage_note_shown: Arc<Mutex<Option<String>>>,
    /// Failed incremental updates awaiting retry / rebuild.
    pub apply_health: Arc<Mutex<ApplyHealth>>,
    /// Backstop for create events the watcher never delivered: the last
    /// completed query-time discovery and the mtime boundary it covers.
    pub discovery: Arc<Mutex<DiscoveryState>>,
    /// Last routed tool call for this session, in [`current_time_secs`] units.
    /// Drives the two-stage idle policy ([`IdlePolicy`]).
    pub last_active: Arc<std::sync::atomic::AtomicU64>,
    /// When the primary index was last woken from hard hibernation; the
    /// ticker refuses to hibernate again inside the wake cooldown.
    pub last_wake: Arc<std::sync::atomic::AtomicU64>,
    /// Idle thresholds (soft eviction / hard hibernation / wake cooldown).
    pub idle_policy: IdlePolicy,
    /// Set while a soft-eviction `shed_derived` already ran for the current
    /// idle spell, so the ticker does not repeat it (and the allocator trim
    /// thread is not re-queued) on every tick.
    pub soft_evicted: Arc<AtomicBool>,
    pub knobs: McpKnobs,
    /// Set on the non-git parent of a multi-project dispatcher
    /// ([`Self::mark_dormant`]): it is never indexed, woken or rebuilt.
    dormant: AtomicBool,
    /// Test hook: the next N incremental applies fail as if the engine
    /// panicked.
    #[cfg(test)]
    pub fail_applies: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    pub(crate) wake_before_publish:
        Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    pub(crate) wake_wait_started: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    #[cfg(test)]
    pub(crate) wake_wait_override: Mutex<Option<Duration>>,
    #[cfg(test)]
    writer_before_publish: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    #[cfg(test)]
    fail_rebuild_spawn: AtomicBool,
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
        Self::new_with_idle_policy(
            root,
            IdlePolicy::from_env(knobs.idle),
            knobs,
            cache_budget_bytes,
        )
    }

    pub(crate) fn new_with_idle_policy(
        root: PathBuf,
        idle_policy: IdlePolicy,
        knobs: McpKnobs,
        cache_budget_bytes: u64,
    ) -> Arc<Self> {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let state = Arc::new(RwLock::new(IndexState::Building));
        let syntax_state = Arc::clone(&state);
        let file_cache = Arc::new(FileCache::new(CacheConfig {
            budget_bytes: cache_budget_bytes,
            ..CacheConfig::default()
        }));
        let precise = LiveBackend::new(root.clone())
            .with_syntax_index(Arc::new(move || match &*syntax_state.read().ok()? {
                IndexState::Ready(index) => Some(Arc::clone(index)),
                _ => None,
            }))
            .with_file_cache(Arc::clone(&file_cache));
        Arc::new(Self {
            precise: Arc::new(precise),
            root,
            state,
            state_cv: Arc::new((std::sync::Mutex::new(0), std::sync::Condvar::new())),
            watch: Arc::new(Mutex::new(None)),
            freshness: Arc::new(Mutex::new(None)),
            indexer_lock: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(ChangeSet::default())),
            file_cache: SharedFileCache(file_cache),
            cache_hits: Arc::new(AtomicU64::new(0)),
            cache_misses: Arc::new(AtomicU64::new(0)),
            cache_budget_bytes,
            rebuild_inflight: Arc::new(AtomicBool::new(false)),
            rebuild_journal: Arc::new(Mutex::new(ChangeSet::default())),
            closed: Arc::new(AtomicBool::new(false)),
            builds_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            lifecycle: Arc::new(Mutex::new(())),
            coverage_note_shown: Arc::new(Mutex::new(None)),
            apply_health: Arc::new(Mutex::new(ApplyHealth::default())),
            discovery: Arc::new(Mutex::new(DiscoveryState::default())),
            last_active: Arc::new(AtomicU64::new(current_time_secs())),
            last_wake: Arc::new(AtomicU64::new(0)),
            idle_policy,
            soft_evicted: Arc::new(AtomicBool::new(false)),
            knobs,
            dormant: AtomicBool::new(false),
            #[cfg(test)]
            fail_applies: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            wake_before_publish: Mutex::new(None),
            #[cfg(test)]
            wake_wait_started: Mutex::new(None),
            #[cfg(test)]
            wake_wait_override: Mutex::new(None),
            #[cfg(test)]
            writer_before_publish: Mutex::new(None),
            #[cfg(test)]
            fail_rebuild_spawn: AtomicBool::new(false),
        })
    }

    pub fn notify_state_change(&self) {
        let mut gen = self.state_cv.0.lock().unwrap_or_else(|e| e.into_inner());
        *gen = gen.wrapping_add(1);
        self.state_cv.1.notify_all();
    }

    /// Block (bounded by `budget`) while `state` is [`IndexState::Building`],
    /// waking immediately on [`Self::notify_state_change`] and stopping promptly
    /// if the session closes or the current request is cancelled.
    ///
    /// Returns `true` if `state` left `IndexState::Building` (and the session is
    /// not closed), `false` if closed, cancelled, or `budget` elapsed while still
    /// `Building`.
    pub fn wait_while_building(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            let observed_gen = *self.state_cv.0.lock().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            let still_building = matches!(
                &*self.state.read().unwrap_or_else(|e| e.into_inner()),
                IndexState::Building
            );
            if !still_building {
                return true;
            }
            if crate::server::cancel::is_cancelled() {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            let slice = remaining.min(Duration::from_millis(25));
            let guard = self.state_cv.0.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == observed_gen {
                let _ = self.state_cv.1.wait_timeout(guard, slice);
            }
        }
    }

    /// Wait (bounded) until all index writers finish, including publication.
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

    /// Never index this session: cold builds, wakes and background rebuilds
    /// all refuse from now on. For the non-git parent of a dispatcher.
    pub(crate) fn mark_dormant(&self) {
        self.dormant.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_dormant(&self) -> bool {
        self.dormant.load(Ordering::SeqCst)
    }

    /// Close admission and stop the watcher. An already admitted writer keeps
    /// leadership until its final guard drops; shutdown waits for those guards.
    pub(crate) fn teardown(&self) {
        {
            let _lifecycle = self.lifecycle.lock_or_recover();
            self.closed.store(true, Ordering::SeqCst);
            if self.builds_active.load(Ordering::SeqCst) == 0 {
                *self.indexer_lock.lock_or_recover() = None;
            }
        }
        // Never join an OS thread while holding the admission/publication gate.
        let handle = self.watch.lock_or_recover().take();
        if let Some(handle) = handle {
            handle.stop();
        }
        *self.freshness.lock_or_recover() = None;
        self.notify_state_change();
    }

    /// Acquire leadership only while open. Slow parsing/replay happens outside
    /// this gate, with an active writer retaining ownership across cancellation.
    fn writer_options(&self) -> Option<astrolabe_core::index::IndexOptions> {
        let _lifecycle = self.lifecycle.lock_or_recover();
        if self.closed.load(Ordering::SeqCst) {
            return None;
        }
        let persist = crate::reindex::refresh_leader(&self.indexer_lock, &self.root);
        Some(crate::reindex::mcp_index_options(
            persist,
            self.knobs.parse_cache_bytes,
        ))
    }

    /// The only writer publication gate. Closing and swapping a snapshot are
    /// mutually exclusive; neither operation holds the gate while parsing.
    fn publish_if_open(&self, outcome: IndexState, reset_idle: bool) -> bool {
        #[cfg(test)]
        if let Some((entered, resume)) = self.writer_before_publish.lock_or_recover().take() {
            entered.wait();
            resume.wait();
        }
        let _lifecycle = self.lifecycle.lock_or_recover();
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if reset_idle {
            self.last_active
                .store(current_time_secs(), Ordering::Relaxed);
            self.soft_evicted.store(false, Ordering::Relaxed);
        }
        let old = std::mem::replace(&mut *state, outcome);
        drop(state);
        drop(_lifecycle);
        // Destructing a superseded graph can be slow; readers and shutdown
        // must not queue behind its allocator work on either gate.
        drop(old);
        self.notify_state_change();
        true
    }

    /// Record that a tool call routed to this session (idle-policy clock).
    pub(crate) fn touch(&self) {
        // Serialize activity with the hard-idle check and state swap. A call
        // either keeps Ready resident or observes Hibernated and wakes it.
        let _state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let now = current_time_secs();
        self.last_active.store(now, Ordering::Relaxed);
        // A query means the session is in use again: the next idle spell may
        // soft-evict afresh.
        self.soft_evicted.store(false, Ordering::Relaxed);
    }

    /// Seconds since the last routed call (0 while never used).
    pub(crate) fn idle_secs(&self, now: u64) -> u64 {
        let last = self.last_active.load(Ordering::Relaxed);
        if last == 0 {
            0
        } else {
            now.saturating_sub(last)
        }
    }

    /// Consume the pending soft-eviction trip: `true` once per idle spell.
    fn take_soft_eviction(&self) -> bool {
        !self.soft_evicted.swap(true, Ordering::AcqRel)
    }

    /// Soft stage: drop the lazily derived lookup tables of the live snapshot
    /// and clear the file cache, then let the allocator trim once the burst
    /// of work is over. The graph itself stays resident, so queries keep
    /// answering; they just rebuild their tables on first use.
    fn soft_evict(&self) {
        let Some(repo) = self.snapshot_ready() else {
            return;
        };
        let dropped = repo.shed_derived();
        self.file_cache.clear();
        crate::alloc::release_free_memory_when_quiet();
        tracing::info!(
            root = %self.root.display(),
            dropped,
            "primary index soft-evicted derived tables after idle"
        );
    }

    /// Hard stage: swap `Ready(_)` for `Hibernated` and drop the whole index
    /// on a helper thread, then hand every free page back to the OS. The repo
    /// must not be dropped on the ticker thread: tearing down a large graph
    /// takes tens of milliseconds.
    fn hibernate(&self, now_secs: u64, observed_activity: u64) -> bool {
        let idle_secs = now_secs.saturating_sub(observed_activity);
        // Inspect admitted writers under the state write lock. A writer
        // registers before reading Ready: either it is already counted (we
        // refuse), or it later observes Hibernated and buffers instead. This
        // excludes incremental publication as well as full rebuilds without
        // holding a gate throughout parsing.
        let old = {
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::SeqCst)
                || self.rebuild_inflight.load(Ordering::SeqCst)
                || self.builds_active.load(Ordering::SeqCst) != 0
                || self.last_active.load(Ordering::Relaxed) != observed_activity
                || self.idle_secs(now_secs) < self.idle_policy.hibernate.as_secs()
            {
                return false;
            }
            let last_wake = self.last_wake.load(Ordering::Relaxed);
            if last_wake != 0
                && now_secs.saturating_sub(last_wake) < self.idle_policy.wakeup_cooldown.as_secs()
            {
                return false;
            }
            match std::mem::replace(&mut *state, IndexState::Hibernated) {
                IndexState::Ready(index) => index,
                other => {
                    // Building / Failed / already Hibernated: put it back.
                    *state = other;
                    return false;
                }
            }
        };
        self.notify_state_change();
        self.file_cache.clear();
        self.soft_evicted.store(false, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name("astrolabe-drop".into())
            .spawn(move || {
                drop(old);
                // Only after the last page is unapped does the trim pay off.
                crate::alloc::release_free_memory();
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to spawn hibernation drop thread");
            crate::alloc::release_free_memory();
        }
        tracing::info!(
            root = %self.root.display(),
            idle_secs,
            "primary index entered hard hibernation; memory released to the OS"
        );
        true
    }

    /// The live snapshot, if the index is ready.
    pub(crate) fn snapshot_ready(&self) -> Option<Arc<RepoIndex>> {
        match &*self.state.read().unwrap_or_else(|e| e.into_inner()) {
            IndexState::Ready(index) => Some(Arc::clone(index)),
            _ => None,
        }
    }

    /// Warm wakeup: flip `Hibernated` back to `Building` and rebuild from the
    /// on-disk store in the background. Returns `true` when a wake is (or was
    /// already) in progress, so the caller may wait for `Ready`.
    ///
    /// Refuses to start another wake inside [`IdlePolicy::wakeup_cooldown`]
    /// of the previous one: a wake that failed must not be retried on every
    /// query. Also refuses while a background rebuild is already running —
    /// that rebuild publishes `Ready` by itself.
    pub(crate) fn wake_if_hibernated(self: &Arc<Self>) -> bool {
        if self.closed.load(Ordering::SeqCst) || self.is_dormant() {
            return false;
        }
        let now = current_time_secs();
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        // A concurrent winner may have moved Hibernated to Building (or
        // already published Ready). Join that wake rather than refusing it.
        match &*state {
            IndexState::Ready(_) => return true,
            IndexState::Building => return self.last_wake.load(Ordering::Relaxed) != 0,
            IndexState::Failed(_) => return false,
            IndexState::Hibernated => {}
        }
        let last_wake = self.last_wake.load(Ordering::Relaxed);
        if last_wake != 0
            && now.saturating_sub(last_wake) < self.idle_policy.wakeup_cooldown.as_secs()
        {
            return false;
        }
        // A background rebuild already in flight publishes `Ready` by itself;
        // the caller only has to wait for it.
        if self
            .rebuild_inflight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return true;
        }
        self.last_wake.store(now, Ordering::Relaxed);
        *state = IndexState::Building;
        drop(state);
        self.notify_state_change();
        self.touch();
        tracing::info!(root = %self.root.display(), "waking hibernated primary index");
        self.spawn_wake();
        true
    }

    /// Rebuild a hibernated index from the on-disk store on a background
    /// thread. Publishes through [`publish_rebuilt`] so buffered watcher
    /// changes are replayed onto the restored snapshot. A cancelled wake goes
    /// back to `Hibernated` (retryable after the cooldown); a failed one is
    /// reported as `Failed`, like a failed cold build.
    fn spawn_wake(self: &Arc<Self>) {
        // Register before spawning, and retain the guard through replay and
        // publication, not merely through parsing inside build_session_index.
        let Some(active) = BuildActive::enter_if_open(self) else {
            self.rebuild_inflight.store(false, Ordering::SeqCst);
            self.notify_state_change();
            return;
        };
        let session = Arc::clone(self);
        let slot = RebuildSlot::new(self);
        let spawned = std::thread::Builder::new()
            .name("astrolabe-wake".into())
            .spawn(move || {
                let _active = active;
                let mut slot = slot;
                let persist = {
                    let _lifecycle = session.lifecycle.lock_or_recover();
                    if session.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    crate::reindex::refresh_leader(&session.indexer_lock, &session.root)
                };
                let read_started = Instant::now();
                let outcome = build_session_index(&session, persist);
                #[cfg(test)]
                if let Some((entered, resume)) =
                    session.wake_before_publish.lock_or_recover().take()
                {
                    entered.wait();
                    resume.wait();
                }
                match outcome {
                    Some(outcome @ IndexState::Ready(_)) => {
                        if session.closed.load(Ordering::SeqCst) {
                            // Evicted/shut down while the wake built: do not
                            // publish into a dead session (same guard as
                            // `publish_ready`).
                            session.teardown();
                            return;
                        }
                        slot.published = publish_rebuilt(&session, outcome);
                        if slot.published {
                            if let Some(log) = session.freshness.lock_or_recover().clone() {
                                log.clear_suspects_through(read_started);
                            }
                        }
                        session.soft_evicted.store(false, Ordering::Relaxed);
                    }
                    Some(outcome @ IndexState::Failed(_)) => {
                        {
                            let mut state =
                                session.state.write().unwrap_or_else(|e| e.into_inner());
                            if matches!(*state, IndexState::Building) {
                                *state = outcome;
                            }
                        }
                        session.notify_state_change();
                    }
                    Some(IndexState::Building) => {}
                    None | Some(IndexState::Hibernated) => {
                        // Closed mid-wake (teardown/shutdown): stay retryable.
                        {
                            let mut state =
                                session.state.write().unwrap_or_else(|e| e.into_inner());
                            if matches!(*state, IndexState::Building) {
                                *state = IndexState::Hibernated;
                            }
                        }
                        session.notify_state_change();
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "failed to spawn wake rebuild thread");
            self.rebuild_inflight.store(false, Ordering::SeqCst);
            {
                let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
                if matches!(*state, IndexState::Building) {
                    *state = IndexState::Hibernated;
                }
            }
            self.notify_state_change();
        }
    }

    /// Wait (bounded) for a just-woken index to become `Ready`.
    fn wait_wake(&self, timeout: Duration) -> bool {
        #[cfg(test)]
        if let Some(started) = self.wake_wait_started.lock_or_recover().take() {
            let _ = started.send(());
        }
        let deadline = Instant::now() + timeout;
        loop {
            let observed_gen = *self.state_cv.0.lock().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::SeqCst) {
                return false;
            }
            let is_building = match &*self.state.read().unwrap_or_else(|e| e.into_inner()) {
                IndexState::Ready(_) => return true,
                // A failed wake is terminal for this call. `Hibernated` can
                // still mean "a background rebuild was already in flight and
                // will publish Ready", so keep waiting on it.
                IndexState::Failed(_) => return false,
                IndexState::Building => true,
                IndexState::Hibernated => false,
            };
            if crate::server::cancel::is_cancelled() {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            if remaining.is_zero() {
                return false;
            }
            if is_building {
                let _ = self.wait_while_building(remaining);
                continue;
            }
            let slice = remaining.min(Duration::from_millis(25));
            let guard = self.state_cv.0.lock().unwrap_or_else(|e| e.into_inner());
            if *guard == observed_gen {
                let _ = self.state_cv.1.wait_timeout(guard, slice);
            }
        }
    }

    /// Wake a hibernated index and wait briefly for it to serve. Returns
    /// `(is_ready, did_wait_wake)`, where `did_wait_wake` is `true` iff this
    /// call actually ran [`Self::wait_wake`].
    pub(crate) fn ensure_awake_tracked(self: &Arc<Self>) -> (bool, bool) {
        if self.closed.load(Ordering::SeqCst) || self.is_dormant() {
            return (false, false);
        }
        let needs_wake = {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            match &*state {
                IndexState::Ready(_) => return (true, false),
                IndexState::Failed(_) => return (false, false),
                IndexState::Building => self.last_wake.load(Ordering::Relaxed) != 0,
                IndexState::Hibernated => true,
            }
        };
        if !needs_wake || !self.wake_if_hibernated() {
            return (false, false);
        }
        #[cfg(test)]
        let timeout = self
            .wake_wait_override
            .lock_or_recover()
            .unwrap_or(WAKE_WAIT);
        #[cfg(not(test))]
        let timeout = WAKE_WAIT;
        let ready = self.wait_wake(timeout);
        (ready, true)
    }

    /// Wake a hibernated index and wait briefly for it to serve. Returns
    /// `true` when the session serves a snapshot (including "was never
    /// hibernated"), `false` when the index is still building after the
    /// bounded wait or the wake was refused (cooldown).
    pub(crate) fn ensure_awake(self: &Arc<Self>) -> bool {
        self.ensure_awake_tracked().0
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
struct BuildActive {
    count: Arc<std::sync::atomic::AtomicUsize>,
    lifecycle: Arc<Mutex<()>>,
    closed: Arc<AtomicBool>,
    indexer_lock: Arc<Mutex<Option<astrolabe_core::elect::IndexerLock>>>,
}

impl BuildActive {
    fn enter_if_open(session: &RootSession) -> Option<Self> {
        let _lifecycle = session.lifecycle.lock_or_recover();
        if session.closed.load(Ordering::SeqCst) {
            return None;
        }
        session.builds_active.fetch_add(1, Ordering::SeqCst);
        Some(Self {
            count: Arc::clone(&session.builds_active),
            lifecycle: Arc::clone(&session.lifecycle),
            closed: Arc::clone(&session.closed),
            indexer_lock: Arc::clone(&session.indexer_lock),
        })
    }
}

impl Drop for BuildActive {
    fn drop(&mut self) {
        let _lifecycle = self.lifecycle.lock_or_recover();
        // Release leadership BEFORE publishing count=0. Shutdown's waiter must
        // not observe idle while the cancelled writer still owns the store.
        if self.count.load(Ordering::SeqCst) == 1 && self.closed.load(Ordering::SeqCst) {
            *self.indexer_lock.lock_or_recover() = None;
        }
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Owns a claimed rebuild slot even before its OS thread exists. Failure or
/// unwind returns the failed paths to the retry queue instead of stranding them
/// in `in_rebuild`. A successfully published full snapshot covers those paths.
struct RebuildSlot {
    inflight: Arc<AtomicBool>,
    health: Arc<Mutex<ApplyHealth>>,
    published: bool,
}

impl RebuildSlot {
    fn new(session: &RootSession) -> Self {
        Self {
            inflight: Arc::clone(&session.rebuild_inflight),
            health: Arc::clone(&session.apply_health),
            published: false,
        }
    }
}

impl Drop for RebuildSlot {
    fn drop(&mut self) {
        let mut health = self.health.lock_or_recover();
        let covered = std::mem::take(&mut health.in_rebuild);
        if self.published {
            health.rebuild_backoff_until = None;
        } else {
            health.unapplied.merge(covered);
            health.rebuild_backoff_until = Some(Instant::now() + REBUILD_BACKOFF);
        }
        self.inflight.store(false, Ordering::SeqCst);
    }
}

/// Full build for `session` that stops early once the session is closed
/// (teardown on eviction / shutdown). `None` = cancelled; nothing to publish.
/// A dormant session is never built.
pub(crate) fn build_session_index(session: &RootSession, persist: bool) -> Option<IndexState> {
    if session.is_dormant() {
        tracing::warn!(root = %session.root.display(), "refusing to index a dormant non-git root");
        return None;
    }
    // The caller owns an admitted writer guard through final publication.
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
        Err(_) => IndexState::Failed(
            "Underlying index capability is not implemented or panicked".to_string(),
        ),
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
    .map_err(|_| "Underlying index capability is not implemented or panicked".to_string())
    .and_then(|result| result.map_err(|error| error.to_string()))
    .map(Arc::new)
    .map(IndexState::Ready)
    .unwrap_or_else(IndexState::Failed)
}

/// Spawn the cold-build thread. Watcher is started *inside* that thread
/// before `IncrementalIndex::build`.
pub(crate) fn start_root_indexing(session: Arc<RootSession>) {
    if session.is_dormant() {
        tracing::warn!(root = %session.root.display(), "refusing to index a dormant non-git root");
        return;
    }
    // Register before spawning, so shutdown cannot report idle between spawn
    // and the new thread first being scheduled.
    let Some(active) = BuildActive::enter_if_open(&session) else {
        return;
    };
    std::thread::Builder::new()
        .name("astrolabe-index".into())
        .spawn(move || {
            let _active = active;
            bootstrap_root(session);
        })
        .expect("failed to spawn index thread");
}

fn bootstrap_root(session: Arc<RootSession>) {
    let Some(opts) = session.writer_options() else {
        return;
    };
    let persist = opts.persist;

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
    let read_started = Instant::now();
    let Some(outcome) = build_session_index(&session, persist) else {
        session.teardown();
        return;
    };
    if publish_ready(&session, outcome) {
        if let Some(log) = session.freshness.lock_or_recover().clone() {
            log.clear_suspects_through(read_started);
        }
    }
}

/// Block the cold build until the watcher thread has sealed its baseline.
///
/// No-op when watching is disabled. Gives up after 60s so a stuck watch
/// thread cannot pin the server in `Building` forever.
fn wait_for_watcher_baseline(session: &RootSession) {
    let log = {
        let guard = session.freshness.lock_or_recover();
        guard.as_ref().map(Arc::clone)
    };
    let Some(log) = log else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(60);
    while !log.is_seeded() {
        if session.closed.load(Ordering::SeqCst) {
            return;
        }
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
    *session.watch.lock_or_recover() = Some(handle);
    *session.freshness.lock_or_recover() = Some(log);
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
            crate::reindex::for_each_reindex_batch_cancellable(
                &session.root,
                rx,
                throttle,
                &session.closed,
                |changes| {
                    let Some(_active) = BuildActive::enter_if_open(&session) else {
                        return;
                    };
                    invalidate_changed_files(&session, changes);
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
                },
            );
        })
        .expect("failed to spawn reindex thread");
}

/// Lock order: pending, then state. Matches [`publish_ready`] and [`inline_repair`].
fn apply_or_buffer(session: &RootSession, changes: &ChangeSet) {
    invalidate_changed_files(session, changes);
    let mut pending = session.pending.lock_or_recover();
    pending.merge(changes.clone());
    apply_pending(session, &mut pending);
}

/// Known changes invalidate even an entry with an identical restored mtime.
/// Repeat after publication to evict fills that raced the index update. A read
/// spanning both invalidations still needs a cache-generation/content protocol
/// for strict identical-stamp replacement consistency; this is not that claim.
fn invalidate_changed_files(session: &RootSession, changes: &ChangeSet) {
    for (_, path) in changes.iter() {
        session.file_cache.invalidate(path.as_str());
    }
}

fn acknowledge_published(
    session: &RootSession,
    attempted: &ChangeSet,
    left: &ChangeSet,
    read_started: Instant,
) {
    let failed: HashSet<_> = left.iter().map(|(_, path)| path).collect();
    let covered: Vec<_> = attempted
        .iter()
        .map(|(_, path)| path)
        .filter(|path| !failed.contains(path))
        .cloned()
        .collect();
    if let Some(log) = session.freshness.lock_or_recover().clone() {
        log.acknowledge_applied(&covered, read_started);
    }
}

fn publish_ready(session: &RootSession, mut outcome: IndexState) -> bool {
    let mut pending = session.pending.lock_or_recover();
    let Some(opts) = session.writer_options() else {
        return false;
    };
    let read_started = Instant::now();
    let mut attempted = ChangeSet::default();
    let mut left = ChangeSet::default();
    if let IndexState::Ready(index) = &outcome {
        let drained = std::mem::take(&mut *pending);
        let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
        attempted = older.clone();
        attempted.merge(drained.clone());
        invalidate_changed_files(session, &attempted);
        let (inc, failed) =
            apply_with_fallback(session, index.incremental(), older, drained, &opts);
        left = failed;
        if let Some(inc) = inc {
            outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                session.root.clone(),
                inc,
            )));
        }
        note_unapplied(session, left.clone());
    }
    let prewarm = match &outcome {
        IndexState::Ready(index) => Some(Arc::clone(index)),
        _ => None,
    };
    let ready = prewarm.is_some();
    let published = session.publish_if_open(outcome, true);
    invalidate_changed_files(session, &attempted);
    if published {
        acknowledge_published(session, &attempted, &left, read_started);
        if let Some(index) = prewarm {
            index.prewarm(true);
        }
    }
    published && ready
}

/// Apply buffered changes when the index is Ready. The caller holds the
/// `pending` lock, which serialises every writer of `state` (apply, publish),
/// so the heavy `apply_reindex` runs on a snapshot *outside* the state lock and
/// the write lock is held only for the `Arc` swap. Queries keep reading the old
/// index meanwhile instead of queueing behind the rebuild.
///
/// Earlier failed changes ([`ApplyHealth::unapplied`]) are retried together
/// with `pending`. Whatever still fails is put back into `unapplied` instead
/// of being dropped, so the barrier can retry, rebuild, and say the answer
/// may be stale. Returns `true` when nothing is left unapplied.
fn apply_pending(session: &RootSession, pending: &mut ChangeSet) -> bool {
    let Some(_active) = BuildActive::enter_if_open(session) else {
        return false;
    };
    let previous = match &*session.state.read().unwrap_or_else(|e| e.into_inner()) {
        IndexState::Ready(index) => Arc::clone(index),
        _ => {
            return pending.is_empty()
                && session.apply_health.lock_or_recover().unapplied.is_empty()
        }
    };
    let changes = std::mem::take(pending);
    let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
    if changes.is_empty() && older.is_empty() {
        return true;
    }
    let Some(opts) = session.writer_options() else {
        // Closure raced admission. Keep the paths available for diagnostics;
        // the closed session will not perform any more index writes.
        pending.merge(changes);
        note_unapplied(session, older);
        return false;
    };
    let mut applied = older.clone();
    applied.merge(changes.clone());
    let read_started = Instant::now();
    invalidate_changed_files(session, &applied);
    let (inc, left) = apply_with_fallback(session, previous.incremental(), older, changes, &opts);
    if let Some(inc) = inc {
        let next = Arc::new(index::RepoIndex::from_incremental(
            session.root.clone(),
            inc,
        ));
        if !session.publish_if_open(IndexState::Ready(Arc::clone(&next)), false) {
            invalidate_changed_files(session, &applied);
            return false;
        }
        invalidate_changed_files(session, &applied);
        acknowledge_published(session, &applied, &left, read_started);
        next.prewarm(false);
        // Free the superseded graph (unless a reader still holds it) off this
        // thread: this may be a query waiting on its inline repair, and
        // tearing down a large repository's graph and parse products cost it
        // tens of ms. If the spawn fails the drop runs here.
        let _ = std::thread::Builder::new()
            .name("astrolabe-drop".into())
            .spawn(move || drop(previous));
        // Return the freed pages to the OS once edits go quiet (glibc only;
        // off-thread, long after the drop above has finished).
        crate::alloc::release_free_memory_when_quiet();
        if session.rebuild_inflight.load(Ordering::SeqCst) {
            session
                .rebuild_journal
                .lock_or_recover()
                .merge(applied.clone());
        }
    }
    invalidate_changed_files(session, &applied);
    let clean = left.is_empty();
    note_unapplied(session, left);
    clean
}

/// Apply `older` (earlier failures) plus `newer` onto `base`. If the
/// combined set fails and both parts are non-empty, apply `newer` alone so
/// one persistently failing file cannot block every later edit. Returns the
/// new incremental index (if any apply succeeded) and the changes that are
/// still not in it.
fn apply_with_fallback(
    session: &RootSession,
    base: Option<&astrolabe_core::index::IncrementalIndex>,
    older: ChangeSet,
    newer: ChangeSet,
    opts: &astrolabe_core::index::IndexOptions,
) -> (Option<astrolabe_core::index::IncrementalIndex>, ChangeSet) {
    let mut all = older.clone();
    all.merge(newer.clone());
    if all.is_empty() {
        return (None, all);
    }
    match try_apply(session, base, &all, opts) {
        Ok(inc) => return (Some(inc), ChangeSet::default()),
        Err(error) => {
            tracing::warn!(%error, files = all.len(), "incremental reindex failed; will retry");
        }
    }
    if !older.is_empty() && !newer.is_empty() {
        if let Ok(inc) = try_apply(session, base, &newer, opts) {
            return (Some(inc), older);
        }
    }
    (None, all)
}

fn try_apply(
    session: &RootSession,
    base: Option<&astrolabe_core::index::IncrementalIndex>,
    changes: &ChangeSet,
    opts: &astrolabe_core::index::IndexOptions,
) -> Result<astrolabe_core::index::IncrementalIndex, String> {
    #[cfg(test)]
    if session
        .fail_applies
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
    {
        return Err("injected apply failure".to_string());
    }
    crate::reindex::apply_reindex(&session.root, base, changes, opts)
}

/// Remember changes that are not in the index yet (merged with any already
/// recorded). An empty set clears the retry timer.
fn note_unapplied(session: &RootSession, left: ChangeSet) {
    let mut health = session.apply_health.lock_or_recover();
    if left.is_empty() && health.unapplied.is_empty() {
        health.next_inline_retry = None;
        return;
    }
    health.unapplied.merge(left);
    health.next_inline_retry = Some(Instant::now() + INLINE_RETRY_GAP);
}

/// Query-time handling of failed incremental updates: retry them inline
/// (rate-limited), then fall back to a background rebuild, and return a
/// short note while the index is still missing them.
fn settle_unapplied(session: &Arc<RootSession>) -> Option<String> {
    let retry_due = {
        let health = session.apply_health.lock_or_recover();
        if health.unapplied.is_empty() && health.in_rebuild.is_empty() {
            return None;
        }
        !health.unapplied.is_empty()
            && health
                .next_inline_retry
                .is_none_or(|at| Instant::now() >= at)
    };
    if retry_due {
        let mut pending = session.pending.lock_or_recover();
        apply_pending(session, &mut pending);
    }
    let (count, sample, needs_rebuild) = {
        let health = session.apply_health.lock_or_recover();
        let mut missing = health.in_rebuild.clone();
        missing.merge(health.unapplied.clone());
        if missing.is_empty() {
            return None;
        }
        let sample: Vec<String> = missing
            .iter()
            .take(3)
            .map(|(_, p)| p.as_str().to_owned())
            .collect();
        (
            missing.len(),
            sample,
            !health.unapplied.is_empty()
                && health
                    .rebuild_backoff_until
                    .is_none_or(|until| Instant::now() >= until),
        )
    };
    if needs_rebuild {
        trigger_background_rebuild_arc(session);
    }
    Some(incremental_failure_note(count, &sample))
}

pub(crate) fn incremental_failure_note(count: usize, sample: &[String]) -> String {
    let more = if count > sample.len() { " etc." } else { "" };
    format!(
        "Index may be stale: incremental update failed for {count} files ({}{more}); retried and triggered a background rebuild; results involving these files may be outdated. confidence: unknown",
        sample.join(", ")
    )
}

pub(crate) enum BarrierEffect {
    None,
    StaleNote(String),
}

/// What the query-time discovery pass did.
enum Discovery {
    Nothing,
    /// New files were found and applied to the live index before answering.
    Applied,
    /// More new files than the inline cap: the caller must degrade.
    Overflow(usize),
}

/// Backstop for create events the watcher never delivered: list files that
/// exist on disk but not in the live index, and apply them inline.
///
/// The watcher is the fast path; this covers the polling window, dropped
/// notifications and network mounts, where a new file would otherwise answer
/// a false zero. Only additions are found here (a directory mtime changed);
/// content edits and deletions stay with the event stream.
fn discover_new_paths(session: &RootSession) -> Discovery {
    let index = match &*session.state.read().unwrap_or_else(|e| e.into_inner()) {
        IndexState::Ready(index) => Arc::clone(index),
        _ => return Discovery::Nothing,
    };
    // Applying additions also retries `unapplied`; the retry gap exists to
    // throttle that, and the barrier reports those paths as stale instead.
    // Do not let discovery bypass it.
    if !session.apply_health.lock_or_recover().unapplied.is_empty() {
        return Discovery::Nothing;
    }
    let mut guard = match session.discovery.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        // Another query is already walking; it applies before it answers.
        Err(std::sync::TryLockError::WouldBlock) => return Discovery::Nothing,
    };
    let opts = astrolabe_core::scan::ScanOptions::default();
    let found = match astrolabe_core::scan::discover_new_files(
        &session.root,
        &opts,
        guard.walk_since(),
        session.knobs.barrier_max,
        |rel| index.admitted_paths().binary_search(rel).is_ok(),
    ) {
        Ok(found) => found,
        Err(overflow) => return Discovery::Overflow(overflow.found),
    };
    // Captured before the walk (plus slack) so a file created while we were
    // walking is caught next time instead of being sealed over.
    guard.since = SystemTime::now();
    if found.is_empty() {
        return Discovery::Nothing;
    }
    let changes = crate::reindex::filter_changeset(
        &session.root,
        ChangeSet {
            added: found,
            ..ChangeSet::default()
        },
    );
    if changes.is_empty() {
        return Discovery::Nothing;
    }
    if let Some(log) = session.freshness.lock_or_recover().clone() {
        let touched: Vec<RelPath> = changes.iter().map(|(_, p)| p.clone()).collect();
        log.note_repaired(&touched);
    }
    let mut pending = session.pending.lock_or_recover();
    pending.merge(changes);
    apply_pending(session, &mut pending);
    Discovery::Applied
}

fn discovery_overflow_note(found: usize) -> String {
    format!(
        "Index may be stale: {found} file(s) appeared on disk since the index was built and were not indexed inline; a background rebuild was triggered. Treat this result as stale. confidence: unknown"
    )
}

fn merge_effect(effect: BarrierEffect, note: String) -> BarrierEffect {
    match effect {
        BarrierEffect::None => BarrierEffect::StaleNote(note),
        BarrierEffect::StaleNote(first) => BarrierEffect::StaleNote(format!("{first}\n{note}")),
    }
}

fn discovery_effect(
    session: &Arc<RootSession>,
    discovery: Discovery,
    effect: BarrierEffect,
) -> BarrierEffect {
    match discovery {
        Discovery::Nothing | Discovery::Applied => effect,
        Discovery::Overflow(found) => {
            trigger_background_rebuild_arc(session);
            merge_effect(effect, discovery_overflow_note(found))
        }
    }
}

/// Query-time freshness barrier.
pub(crate) fn run_barrier(session: &Arc<RootSession>) -> BarrierEffect {
    // Runs without a watcher too: with `watch = false` this is the only way a
    // file created after the cold build becomes searchable.
    let discovery = discover_new_paths(session);
    let log = {
        let g = session.freshness.lock_or_recover();
        match g.as_ref() {
            Some(log) => Arc::clone(log),
            None => return discovery_effect(session, discovery, BarrierEffect::None),
        }
    };

    let started = Instant::now();
    let snapshot = log.snapshot();
    let watermark_age = snapshot.watermark.elapsed();
    let suspects = snapshot.suspects;
    let distrusted = snapshot.distrusted;
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

    let effect = match decision {
        BarrierDecision::Fresh => BarrierEffect::None,
        BarrierDecision::Repair(paths) if paths.is_empty() => {
            let before = log.watermark();
            log.request_verification();
            if wait_watermark_advance(&log, before, BARRIER_VERIFY_WAIT) {
                // The verification may have found changes that are not in
                // the index yet; they are suspects after the new watermark.
                let verified = log.snapshot();
                let found = verified.suspects;
                if verified.distrusted || found.len() > max_inline {
                    trigger_background_rebuild_arc(session);
                    BarrierEffect::StaleNote(staleness_note(found.len()))
                } else {
                    let count = found.len();
                    if !found.is_empty() && !inline_repair(session, found) {
                        BarrierEffect::StaleNote(staleness_note(count))
                    } else {
                        BarrierEffect::None
                    }
                }
            } else {
                // The verification scan did not finish in time: we answer
                // from an unverified index, so say so instead of staying silent.
                BarrierEffect::StaleNote(verification_timeout_note())
            }
        }
        BarrierDecision::Repair(paths) => {
            let count = paths.len();
            if inline_repair(session, paths) {
                BarrierEffect::None
            } else {
                BarrierEffect::StaleNote(staleness_note(count))
            }
        }
        BarrierDecision::Degrade { suspects } => {
            trigger_background_rebuild_arc(session);
            BarrierEffect::StaleNote(staleness_note(suspects))
        }
    };
    // Do not label the current result fresh when a later observed delta
    // arrived during repair. It remains pending for the next call; avoid an
    // unbounded repair loop while files are actively changing.
    let effect = match effect {
        BarrierEffect::None => {
            let remaining = log.snapshot();
            if remaining.distrusted || !remaining.suspects.is_empty() {
                BarrierEffect::StaleNote(staleness_note(remaining.suspects.len()))
            } else {
                BarrierEffect::None
            }
        }
        stale => stale,
    };
    // A repair (or an earlier watcher batch) whose apply failed is tracked
    // in `apply_health`, not lost: retry it, and say so while it is missing.
    match (
        discovery_effect(session, discovery, effect),
        settle_unapplied(session),
    ) {
        (effect, None) => effect,
        (BarrierEffect::None, Some(note)) => BarrierEffect::StaleNote(note),
        (BarrierEffect::StaleNote(first), Some(note)) => {
            BarrierEffect::StaleNote(format!("{first}\n{note}"))
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
    "Index may be stale: change verification did not finish within 2s; this result is based on an unverified index. confidence: unknown".to_string()
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

/// Direct file repair must never pretend a directory/rule-file edit only
/// affects that path. The watcher normally marks these distrusted at intake;
/// this is also a conservative boundary for older/queued raw suspects.
fn needs_tree_repair(session: &RootSession, paths: &[RelPath]) -> bool {
    let snapshot = session.snapshot_ready();
    paths.iter().any(|path| {
        let name = Path::new(path.as_str())
            .file_name()
            .and_then(|s| s.to_str());
        if matches!(name, Some(".gitignore" | ".ignore"))
            || path.as_str() == ".git/info/exclude"
            || session.root.join(path.as_str()).is_dir()
        {
            return true;
        }
        let prefix = format!("{}/", path.as_str().trim_end_matches('/'));
        snapshot
            .as_ref()
            .and_then(|index| index.incremental())
            .is_some_and(|index| {
                index
                    .admitted_paths()
                    .iter()
                    .any(|old| old.as_str().starts_with(&prefix))
            })
    })
}

fn inline_repair(session: &Arc<RootSession>, paths: Vec<RelPath>) -> bool {
    if needs_tree_repair(session, &paths) {
        if let Some(log) = session.freshness.lock_or_recover().clone() {
            log.request_verification();
        }
        trigger_background_rebuild_arc(session);
        return false;
    }
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
        return true;
    }
    // Tell the watcher before reading: the index is about to hold content
    // its snapshot may never see (edit, query, revert within the debounce).
    let log = session.freshness.lock_or_recover().clone();
    if let Some(log) = log {
        let touched: Vec<RelPath> = repair.iter().map(|(_, p)| p.clone()).collect();
        log.note_repaired(&touched);
    }
    let mut pending = session.pending.lock_or_recover();
    pending.merge(repair);
    apply_pending(session, &mut pending)
}

/// Kick a full `IncrementalIndex::build` without dropping to `Building`.
pub(crate) fn trigger_background_rebuild_arc(session: &Arc<RootSession>) {
    let Some(active) = BuildActive::enter_if_open(session) else {
        return;
    };
    {
        // Serialize the claim/journal reset with incremental publication. A
        // running apply must not append a delta just before we clear it.
        let _pending = session.pending.lock_or_recover();
        if session
            .apply_health
            .lock_or_recover()
            .rebuild_backoff_until
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        if session
            .rebuild_inflight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        session
            .rebuild_journal
            .lock_or_recover()
            .clone_from(&ChangeSet::default());
        let mut health = session.apply_health.lock_or_recover();
        let taken = std::mem::take(&mut health.unapplied);
        health.in_rebuild.merge(taken);
    }
    let slot = RebuildSlot::new(session);
    let worker_session = Arc::clone(session);
    let work = move || {
        let _active = active;
        let mut slot = slot;
        let session = worker_session;
        let started = Instant::now();
        let Some(opts) = session.writer_options() else {
            return;
        };
        let outcome = build_session_index(&session, opts.persist);
        match outcome {
            Some(outcome @ IndexState::Ready(_)) => {
                slot.published = publish_rebuilt(&session, outcome);
                if slot.published {
                    if let Some(log) = session.freshness.lock_or_recover().clone() {
                        log.clear_suspects_through(started);
                    }
                }
            }
            Some(IndexState::Failed(error)) => {
                tracing::warn!(%error, "background rebuild failed; backing off");
            }
            _ => {}
        }
    };
    if let Err(error) = spawn_rebuild_thread(session, work) {
        tracing::warn!(%error, "failed to spawn background rebuild thread; slot restored");
    }
}

fn spawn_rebuild_thread(
    _session: &RootSession,
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    #[cfg(test)]
    if _session.fail_rebuild_spawn.swap(false, Ordering::SeqCst) {
        return Err(std::io::Error::other(
            "injected rebuild thread creation failure",
        ));
    }
    std::thread::Builder::new()
        .name("astrolabe-rebuild".into())
        .spawn(work)
}

/// Publish a background rebuild without losing changes that were applied to
/// the old index (journal) or buffered (pending) while it was building.
fn publish_rebuilt(session: &RootSession, mut outcome: IndexState) -> bool {
    let mut pending = session.pending.lock_or_recover();
    let Some(opts) = session.writer_options() else {
        return false;
    };
    let read_started = Instant::now();
    let mut attempted = ChangeSet::default();
    let mut left = ChangeSet::default();
    let mut replay = std::mem::take(&mut *session.rebuild_journal.lock_or_recover());
    replay.merge(std::mem::take(&mut *pending));
    if let IndexState::Ready(index) = &outcome {
        let older = std::mem::take(&mut session.apply_health.lock_or_recover().unapplied);
        attempted = older.clone();
        attempted.merge(replay.clone());
        invalidate_changed_files(session, &attempted);
        let (inc, failed) = apply_with_fallback(session, index.incremental(), older, replay, &opts);
        left = failed;
        if let Some(inc) = inc {
            outcome = IndexState::Ready(Arc::new(index::RepoIndex::from_incremental(
                session.root.clone(),
                inc,
            )));
        }
        note_unapplied(session, left.clone());
    }
    let prewarm = match &outcome {
        IndexState::Ready(index) => Some(Arc::clone(index)),
        _ => None,
    };
    let published = session.publish_if_open(outcome, true);
    invalidate_changed_files(session, &attempted);
    if published {
        acknowledge_published(session, &attempted, &left, read_started);
        if let Some(index) = prewarm {
            index.prewarm(true);
        }
    }
    published
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
        "Multi-project workspace: cannot route to a unique sub-repo. Pass root (sub-repo name or absolute path), or make path start with the sub-repo name/absolute path. Candidates:\n",
    );
    for child in children {
        out.push_str(&format!("  {}\n", child.display()));
    }
    if let Some(name) = children
        .first()
        .and_then(|c| c.file_name())
        .and_then(|n| n.to_str())
    {
        out.push_str(&format!("e.g. root=\"{name}\"\n"));
    }
    out.push_str("confidence: unknown");
    out
}

pub(crate) fn route_session(
    dispatch: &DispatchState,
    hint: &str,
) -> Result<Arc<RootSession>, String> {
    let (path, evicted) = {
        let mut disp = dispatch.dispatcher.lock_or_recover();
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

/// Activate (or register) an absolute index root and return its session.
///
/// Used by per-call `index_root`: the path may be outside the startup child
/// set; [`RootDispatcher::activate`] registers it, residency / idle eviction
/// apply, and a cold build starts on first use.
pub(crate) fn activate_absolute_root(dispatch: &DispatchState, root: &Path) -> Arc<RootSession> {
    let (path, evicted) = {
        let mut disp = dispatch.dispatcher.lock_or_recover();
        let path = root.to_path_buf();
        let evicted = disp.activate(&path, Instant::now());
        (path, evicted)
    };
    for evicted in evicted {
        teardown_child(dispatch, &evicted);
    }
    ensure_session(dispatch, &path)
}

fn ensure_session(dispatch: &DispatchState, root: &Path) -> Arc<RootSession> {
    let mut map = dispatch.sessions.lock_or_recover();
    // Match Windows verbatim / plain forms of the same path so one project
    // does not get two sessions (and two cold builds).
    if let Some((_, existing)) = map
        .iter()
        .find(|(k, _)| astrolabe_core::paths::paths_eq_normalized(k, root))
    {
        return Arc::clone(existing);
    }
    let session = RootSession::new(
        root.to_path_buf(),
        dispatch.knobs.clone(),
        dispatch.cache_budget_bytes,
    );
    // Key by the session's canonical root (RootSession::new canonicalize's).
    map.insert(session.root.clone(), Arc::clone(&session));
    drop(map);
    start_root_indexing(Arc::clone(&session));
    session
}

pub(crate) fn teardown_child(dispatch: &DispatchState, root: &Path) {
    let mut map = dispatch.sessions.lock_or_recover();
    let key = map
        .keys()
        .find(|k| astrolabe_core::paths::paths_eq_normalized(k, root))
        .cloned();
    let session = key.and_then(|k| map.remove(&k));
    drop(map);
    if let Some(session) = session {
        session.teardown();
    }
}

/// How often the idle patrol runs for a shortest idle threshold. A quarter of
/// the threshold, capped at 30 s, floored at 50 ms for tests.
fn idle_patrol_interval(shortest: Duration) -> Duration {
    let quarter = shortest / 4;
    let capped = quarter.min(Duration::from_secs(30));
    if capped.is_zero() {
        Duration::from_millis(50)
    } else {
        capped
    }
}

/// One idle check of the primary workspace: soft-evict derived tables at
/// [`IdlePolicy::soft_evict`], hard-hibernate at [`IdlePolicy::hibernate`].
///
/// The two stages are exclusive: once the hard threshold is reached there is
/// nothing left to shed, so the soft stage is skipped. Both are no-ops unless
/// the index is `Ready` (a build or a failed build is left alone).
fn patrol_primary(primary: &Arc<RootSession>, now_secs: u64) {
    if primary.closed.load(Ordering::SeqCst) {
        return;
    }
    let observed_activity = primary.last_active.load(Ordering::Relaxed);
    let idle = if observed_activity == 0 {
        0
    } else {
        now_secs.saturating_sub(observed_activity)
    };
    let policy = primary.idle_policy;
    if idle >= policy.hibernate.as_secs() {
        // A just-woken index stays up for the cooldown even if it was never
        // queried: otherwise a wake triggered by a single call would be undone
        // by the next tick and the call would pay the warm start every time.
        let last_wake = primary.last_wake.load(Ordering::Relaxed);
        if last_wake != 0 && now_secs.saturating_sub(last_wake) < policy.wakeup_cooldown.as_secs() {
            return;
        }
        primary.hibernate(now_secs, observed_activity);
    } else if idle >= policy.soft_evict.as_secs() {
        // Only consume the per-spell trip when there is a snapshot to shed;
        // a session still building would otherwise burn the trip and never
        // shed the tables its publish prewarms.
        let ready = matches!(
            &*primary.state.read().unwrap_or_else(|e| e.into_inner()),
            IndexState::Ready(_)
        );
        if ready && primary.take_soft_eviction() {
            primary.soft_evict();
        }
    }
}

/// Idle-eviction ticker for the multi-root dispatcher plus the two-stage
/// idle patrol of the primary workspace.
///
/// `primary` is `None` for the secondary ticker that `ensure_index_root_session`
/// starts for the dynamic-roots dispatcher: the primary is patrolled by the
/// ticker `start_indexing` starts, and one patrol thread per primary is enough.
pub(crate) fn start_idle_ticker(
    primary: Option<Arc<RootSession>>,
    dispatch: Option<Arc<DispatchState>>,
    stop: Arc<AtomicBool>,
) {
    let shortest = match (&primary, &dispatch) {
        (Some(primary), Some(dispatch)) => primary.idle_policy.soft_evict.min(dispatch.knobs.idle),
        (Some(primary), None) => primary.idle_policy.soft_evict,
        (None, Some(dispatch)) => dispatch.knobs.idle,
        (None, None) => return,
    };
    let interval = idle_patrol_interval(shortest);
    std::thread::Builder::new()
        .name("astrolabe-idle-evict".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(interval);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(primary) = &primary {
                    patrol_primary(primary, current_time_secs());
                }
                let Some(dispatch) = &dispatch else {
                    continue;
                };
                let evicted = dispatch
                    .dispatcher
                    .lock_or_recover()
                    .evict_idle(Instant::now());
                for path in evicted {
                    teardown_child(dispatch, &path);
                }
            }
        })
        .expect("failed to spawn idle-evict thread");
}

pub(crate) fn teardown_all(dispatch: &DispatchState) {
    let sessions: Vec<Arc<RootSession>> = dispatch
        .sessions
        .lock_or_recover()
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
        assert!(verification_timeout_note().contains("Index may be stale"));
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

    /// perf/stale.py: append a function, let a query see it, then restore the
    /// original bytes within the watcher debounce. The deleted function must
    /// disappear on the next queries instead of lingering until a rebuild.
    #[test]
    fn edit_then_quick_revert_does_not_leave_stale_symbol() {
        let dir = unique_temp_dir();
        let orig = "def alpha():\n    return 1\n";
        std::fs::write(dir.join("a.py"), orig).unwrap();
        let knobs = McpKnobs {
            watch_mode: Some(WatchMode::Events),
            reindex_throttle: Duration::ZERO,
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
        for (round, gap_ms) in [0u64, 50, 300].into_iter().enumerate() {
            let name = format!("astro_stale_{round}");
            std::fs::write(
                dir.join("a.py"),
                format!("{orig}\n\ndef {name}():\n    return 2\n"),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let _ = run_barrier(&session);
                if has_symbol(&session, &name) {
                    break;
                }
                assert!(Instant::now() < deadline, "{name} never appeared");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::thread::sleep(Duration::from_millis(gap_ms));
            std::fs::write(dir.join("a.py"), orig).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let _ = run_barrier(&session);
                if !has_symbol(&session, &name) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "{name} still served 5 s after the revert (gap {gap_ms} ms)"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Poison `m` by panicking while holding its guard.
    fn poison<T: Send + 'static>(m: Arc<Mutex<T>>) {
        let _ = std::thread::spawn(move || {
            let _guard = m.lock().unwrap();
            panic!("poison for test");
        })
        .join();
    }

    #[test]
    fn poisoned_session_locks_are_recovered_not_fatal() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session(&dir);
        let (_rx, handle, log) = Watcher::new(dir.clone()).events(false).spawn_with_log();
        *session.freshness.lock().unwrap() = Some(log);
        poison(Arc::clone(&session.pending));
        poison(Arc::clone(&session.freshness));
        poison(Arc::clone(&session.rebuild_journal));
        poison(Arc::clone(&session.indexer_lock));
        poison(Arc::clone(&session.watch));
        assert!(session.pending.is_poisoned());

        // The reindex path and the query barrier keep working.
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(has_symbol(&session, "beta"));
        let _ = run_barrier(&session);
        assert!(!session.pending.is_poisoned(), "recovery clears the flag");
        session.teardown();
        handle.stop();

        let dispatch = DispatchState::new(vec![dir.clone()], McpKnobs::default(), 1 << 20);
        let _ = std::thread::scope(|s| {
            s.spawn(|| {
                let _g = dispatch.sessions.lock().unwrap();
                panic!("poison sessions");
            })
            .join()
        });
        assert!(dispatch.sessions.is_poisoned());
        teardown_all(&dispatch);
        teardown_child(&dispatch, &dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn barrier_note(session: &Arc<RootSession>) -> Option<String> {
        match run_barrier(session) {
            BarrierEffect::None => None,
            BarrierEffect::StaleNote(note) => Some(note),
        }
    }

    fn session_with_log(dir: &Path) -> (Arc<RootSession>, WatchHandle) {
        let session = ready_session(dir);
        let (_rx, handle, log) = Watcher::new(dir.to_path_buf())
            .events(false)
            .poll_interval(Duration::from_secs(3600))
            .spawn_with_log();
        let start = Instant::now();
        while !log.is_seeded() {
            assert!(start.elapsed() < Duration::from_secs(5), "never seeded");
            std::thread::sleep(Duration::from_millis(5));
        }
        *session.freshness.lock().unwrap() = Some(log);
        (session, handle)
    }

    #[test]
    fn barrier_discovers_a_file_the_watcher_never_saw() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        // An hour-long poll cycle: only query-time discovery can see b.py.
        let (session, handle) = session_with_log(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        assert!(!has_symbol(&session, "beta"), "the watcher cannot have run");
        assert_eq!(barrier_note(&session), None);
        assert!(
            has_symbol(&session, "beta"),
            "discovery must index a file no event announced"
        );
        handle.stop();
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_overflow_degrades_instead_of_repairing_inline() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            barrier_max: 1,
            parse_cache_bytes: DEFAULT_PARSE_CACHE_BYTES,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let persist = crate::reindex::refresh_leader(&session.indexer_lock, &session.root);
        let cold = build_index(&session.root, persist, session.knobs.parse_cache_bytes);
        assert!(matches!(cold, IndexState::Ready(_)));
        *session.state.write().unwrap() = cold;

        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        std::fs::write(dir.join("c.py"), "def gamma():\n    return 3\n").unwrap();
        let note = barrier_note(&session).expect("overflow must be reported, not repaired");
        assert!(note.contains("appeared on disk"), "{note}");
        assert!(note.contains("confidence: unknown"), "{note}");
        session.teardown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_incremental_update_is_retried_not_dropped() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, handle) = session_with_log(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        assert!(!has_symbol(&session, "beta"));
        assert_eq!(session.apply_health.lock().unwrap().unapplied.len(), 1);
        // Inside the retry gap the query says the answer may be stale
        // (and kicks a rebuild); it does not pretend to be fresh.
        session.apply_health.lock().unwrap().rebuild_backoff_until =
            Some(Instant::now() + Duration::from_secs(600));
        let note = barrier_note(&session).expect("stale note while unapplied");
        assert!(
            note.contains("incremental update failed") && note.contains("b.py"),
            "{note}"
        );
        // Once the retry is due, the query retries inline and succeeds.
        session.apply_health.lock().unwrap().next_inline_retry = None;
        assert_eq!(barrier_note(&session), None);
        assert!(has_symbol(&session, "beta"));
        assert!(session.apply_health.lock().unwrap().unapplied.is_empty());
        handle.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_apply_failure_falls_back_to_rebuild_and_keeps_new_edits() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, handle) = session_with_log(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("b.py")],
                ..ChangeSet::default()
            },
        );
        // Older failure + new edit: combined apply fails, the new edit alone
        // still lands, the old one stays recorded.
        std::fs::write(dir.join("c.py"), "def gamma():\n    return 3\n").unwrap();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(
            &session,
            &ChangeSet {
                added: vec![RelPath::new("c.py")],
                ..ChangeSet::default()
            },
        );
        assert!(
            has_symbol(&session, "gamma"),
            "new edit must not be blocked"
        );
        assert!(!has_symbol(&session, "beta"));
        // Retry fails again: the barrier notes it and rebuilds in background.
        session.fail_applies.store(1, Ordering::SeqCst);
        session.apply_health.lock().unwrap().next_inline_retry = None;
        let note = barrier_note(&session).expect("note while still failing");
        assert!(note.contains("incremental update failed"), "{note}");
        let deadline = Instant::now() + Duration::from_secs(30);
        while !has_symbol(&session, "beta") || session.rebuild_inflight.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "rebuild never absorbed b.py");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(has_symbol(&session, "gamma"));
        assert_eq!(barrier_note(&session), None, "note clears once rebuilt");
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

    fn idle_policy(soft: u64, hibernate: u64, cooldown: u64) -> IdlePolicy {
        IdlePolicy {
            soft_evict: Duration::from_secs(soft),
            hibernate: Duration::from_secs(hibernate),
            wakeup_cooldown: Duration::from_secs(cooldown),
        }
    }

    fn ready_session_with_policy(
        dir: &Path,
        knobs: McpKnobs,
        policy: IdlePolicy,
    ) -> Arc<RootSession> {
        let session =
            RootSession::new_with_idle_policy(dir.to_path_buf(), policy, knobs, 8 * 1024 * 1024);
        let cold = build_index(&session.root, false, session.knobs.parse_cache_bytes);
        *session.state.write().unwrap() = cold;
        session
    }

    #[test]
    fn touch_and_idle_secs_track_request_activity() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let now = current_time_secs();
        assert!(
            session.idle_secs(now) <= 1,
            "fresh session counts as active"
        );
        // A synthetic `now` 40 s ahead reads back as 40 s idle without
        // sleeping (and `idle_secs` takes the observation time, so the
        // process epoch value itself does not matter).
        assert_eq!(session.idle_secs(now + 40), 40);
        session.touch();
        assert_eq!(session.idle_secs(current_time_secs()), 0);
        // A never-touched session reports 0, not a huge idle time.
        session.last_active.store(0, Ordering::Relaxed);
        assert_eq!(session.idle_secs(now + 10_000), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn soft_patrol_sheds_derived_tables_and_clears_file_cache() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 3600, 0));
        let repo = session.snapshot_ready().expect("ready");
        // Build one derived table and put an entry in the file cache.
        let _ = repo.names();
        session.file_cache.insert("a.py", vec!["a".to_string()], 7);
        assert!(session.file_cache.get("a.py", 7).is_some());

        patrol_primary(&session, current_time_secs());

        assert_eq!(
            repo.shed_derived(),
            0,
            "soft eviction must already have dropped the derived tables"
        );
        assert!(
            session.file_cache.get("a.py", 7).is_none(),
            "soft eviction must clear the file cache"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn soft_patrol_skips_derived_shedding_inside_the_idle_window() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(300, 3600, 0));
        let repo = session.snapshot_ready().expect("ready");
        let _ = repo.names();
        // One second idle is well inside the 300 s soft window.
        patrol_primary(&session, current_time_secs() + 1);
        assert!(
            repo.shed_derived() > 0,
            "an in-use index keeps its derived tables"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn soft_patrol_runs_once_per_idle_spell() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 3600, 0));
        let repo = session.snapshot_ready().expect("ready");
        let _ = repo.names();
        patrol_primary(&session, current_time_secs());
        assert_eq!(repo.shed_derived(), 0, "first tick sheds the tables");
        // A query touches the session; the next tick may shed again.
        session.touch();
        let _ = repo.names();
        patrol_primary(&session, current_time_secs());
        assert_eq!(
            repo.shed_derived(),
            0,
            "a new idle spell sheds the freshly rebuilt tables"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hard_patrol_hibernates_and_releases_the_index() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 1, 0));
        let repo = session.snapshot_ready().expect("ready");
        // Patrol with an observation time well past the 1 s hard threshold.
        patrol_primary(&session, current_time_secs() + 120);
        assert!(
            matches!(*session.state.read().unwrap(), IndexState::Hibernated),
            "idle past the hard threshold must hibernate"
        );
        // The state's Arc is dropped on "astrolabe-drop"; only this test's
        // clone may remain afterwards.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&repo) > 1 {
            assert!(
                Instant::now() < deadline,
                "hibernated index was never dropped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hibernation_respects_the_wakeup_cooldown() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        // Woken "just now": even a long idle must not re-hibernate.
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 1, 600));
        let now = current_time_secs();
        session.last_wake.store(now, Ordering::Relaxed);
        patrol_primary(&session, now + 120);
        assert!(
            matches!(*session.state.read().unwrap(), IndexState::Ready(_)),
            "a just-woken index stays up for the cooldown"
        );
        // Past the cooldown the same idle state hibernates again.
        patrol_primary(&session, now + 700);
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Hibernated
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hibernation_is_refused_while_a_rebuild_is_in_flight() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 1, 0));
        session.rebuild_inflight.store(true, Ordering::SeqCst);
        patrol_primary(&session, current_time_secs() + 120);
        assert!(
            matches!(*session.state.read().unwrap(), IndexState::Ready(_)),
            "an index being rebuilt must not be hibernated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wake_returns_a_ready_index_and_respects_the_cooldown() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 1, 600));
        *session.state.write().unwrap() = IndexState::Hibernated;

        assert!(session.ensure_awake(), "wake must finish inside WAKE_WAIT");
        let repo = session.snapshot_ready().expect("ready after wake");
        assert!(repo.graph().symbols.iter().any(|s| s.name == "alpha"));

        // A Ready session is never woken (and never waits).
        assert!(session.ensure_awake());

        // Hibernated again inside the cooldown: the wake is refused, and the
        // session stays hibernated instead of starting a second build.
        *session.state.write().unwrap() = IndexState::Hibernated;
        session
            .last_wake
            .store(current_time_secs(), Ordering::Relaxed);
        assert!(!session.ensure_awake(), "cooldown must refuse the wake");
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Hibernated
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wake_cancelled_by_teardown_stays_retryable() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };
        let session = ready_session_with_policy(&dir, knobs, idle_policy(0, 1, 0));
        *session.state.write().unwrap() = IndexState::Hibernated;
        session.teardown();
        assert!(!session.ensure_awake(), "a closed session cannot wake");
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Hibernated
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn hard_patrol_rechecks_activity_before_swapping_ready() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 60, 0),
        );
        // Model the patrol's observation, followed by a routed request before
        // hibernate acquires the write lock. No wall-clock delay is required.
        let observed = current_time_secs() + 10;
        session.last_active.store(observed, Ordering::Relaxed);
        let patrol_now = observed + 120;
        assert!(session.idle_secs(patrol_now) >= 60);
        session.touch();
        assert!(session.ensure_awake());
        assert!(!session.hibernate(patrol_now, observed));
        assert!(session.snapshot_ready().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_wake_callers_join_the_building_winner() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        *session.state.write().unwrap() = IndexState::Hibernated;
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.wake_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        assert!(session.wake_if_hibernated());
        entered.wait();
        assert!(matches!(
            *session.state.read().unwrap(),
            IndexState::Building
        ));
        // This is the loser path: it must report an existing wake even though
        // Hibernated was replaced before this request reached the claim lock.
        assert!(session.wake_if_hibernated());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        *session.wake_wait_started.lock().unwrap() = Some(started_tx);
        let waiter_session = Arc::clone(&session);
        let waiter = std::thread::spawn(move || waiter_session.ensure_awake());
        let started = started_rx.recv_timeout(Duration::from_secs(2));
        resume.wait();
        assert!(
            started.is_ok(),
            "concurrent request must join the wake wait"
        );
        assert!(waiter.join().unwrap());
        assert!(session.snapshot_ready().is_some());
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_waits_for_wake_after_parsing_before_publish() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        *session.state.write().unwrap() = IndexState::Hibernated;
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.wake_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        assert!(session.wake_if_hibernated());
        entered.wait();
        session.teardown();
        assert!(
            !session.wait_builds_idle(Duration::ZERO),
            "publication phase must still count as active"
        );
        assert!(
            !session.ensure_awake(),
            "closed waiters must not wait for the deadline"
        );
        resume.wait();
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        assert!(session.snapshot_ready().is_none());
        assert!(session.indexer_lock.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn closed_rebuild_cannot_reacquire_ownership_or_publish() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        let outcome = build_index(&dir, false, session.knobs.parse_cache_bytes);
        *session.state.write().unwrap() = IndexState::Hibernated;
        session.teardown();
        publish_rebuilt(&session, outcome);
        assert!(session.snapshot_ready().is_none());
        assert!(session.indexer_lock.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_while_building_and_wait_wake_respond_to_notify_cancel_and_close() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let knobs = McpKnobs {
            watch_mode: None,
            ..McpKnobs::default()
        };

        // 1. wait_while_building wakes immediately on notify_state_change.
        let session = RootSession::new(dir.clone(), knobs.clone(), 8 * 1024 * 1024);
        let built = build_index(&dir, false, session.knobs.parse_cache_bytes);
        let producer_session = Arc::clone(&session);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            *producer_session.state.write().unwrap() = built;
            producer_session.notify_state_change();
        });
        let started = Instant::now();
        assert!(session.wait_while_building(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_while_building should wake immediately on notify_state_change"
        );
        producer.join().unwrap();

        // 2. wait_while_building wakes immediately on teardown (close).
        let session_close = RootSession::new(dir.clone(), knobs.clone(), 8 * 1024 * 1024);
        let closer_session = Arc::clone(&session_close);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            closer_session.teardown();
        });
        let started = Instant::now();
        assert!(!session_close.wait_while_building(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_while_building should stop promptly on close"
        );
        closer.join().unwrap();

        // 3. wait_while_building stops promptly on cancel::scope flag.
        let session_cancel = RootSession::new(dir.clone(), knobs.clone(), 8 * 1024 * 1024);
        let flag = Arc::new(AtomicBool::new(false));
        let flag_bg = Arc::clone(&flag);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            flag_bg.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let ok = crate::server::cancel::scope(flag, || {
            session_cancel.wait_while_building(Duration::from_secs(5))
        });
        assert!(!ok);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_while_building should stop promptly on cancel"
        );
        canceller.join().unwrap();

        // 4. wait_wake wakes immediately on notify_state_change (including from Hibernated).
        let session_wake = RootSession::new(dir.clone(), knobs.clone(), 8 * 1024 * 1024);
        *session_wake.state.write().unwrap() = IndexState::Hibernated;
        let built_wake = build_index(&dir, false, session_wake.knobs.parse_cache_bytes);
        let waker_session = Arc::clone(&session_wake);
        let waker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            *waker_session.state.write().unwrap() = built_wake;
            waker_session.notify_state_change();
        });
        let started = Instant::now();
        assert!(session_wake.wait_wake(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_wake should wake immediately on notify_state_change"
        );
        waker.join().unwrap();

        // 5. wait_wake stops promptly on cancel and on close.
        let session_wake_cancel = RootSession::new(dir.clone(), knobs.clone(), 8 * 1024 * 1024);
        let flag = Arc::new(AtomicBool::new(false));
        let flag_bg = Arc::clone(&flag);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            flag_bg.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        let ok = crate::server::cancel::scope(flag, || {
            session_wake_cancel.wait_wake(Duration::from_secs(5))
        });
        assert!(!ok);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_wake should stop promptly on cancel"
        );
        canceller.join().unwrap();

        let session_wake_close = RootSession::new(dir.clone(), knobs, 8 * 1024 * 1024);
        let closer_session = Arc::clone(&session_wake_close);
        let closer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            closer_session.teardown();
        });
        let started = Instant::now();
        assert!(!session_wake_close.wait_wake(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "wait_wake should stop promptly on close"
        );
        closer.join().unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cold_shutdown_counts_publication_and_retains_ownership() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = RootSession::new(
            dir.clone(),
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            1024 * 1024,
        );
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.writer_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        start_root_indexing(Arc::clone(&session));
        entered.wait();
        session.teardown();
        let still_active = !session.wait_builds_idle(Duration::ZERO);
        let ownership_retained = session.indexer_lock.lock().unwrap().is_some();
        let blocked_takeover = astrolabe_core::elect::IndexerLock::try_acquire(&dir)
            .unwrap()
            .is_none();
        resume.wait();
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        assert!(
            still_active,
            "cold publication must remain registered after parsing"
        );
        assert!(
            ownership_retained && blocked_takeover,
            "cancelled writer must retain leadership until its final safe point"
        );
        assert!(
            session.snapshot_ready().is_none(),
            "closed cold build published Ready"
        );
        assert!(session.indexer_lock.lock().unwrap().is_none());
        assert!(astrolabe_core::elect::IndexerLock::try_acquire(&dir)
            .unwrap()
            .is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn incremental_shutdown_rejects_late_publication() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        let before = session.snapshot_ready().unwrap();
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.writer_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        let worker_session = Arc::clone(&session);
        let worker = std::thread::spawn(move || {
            apply_or_buffer(
                &worker_session,
                &ChangeSet {
                    added: vec![RelPath::new("b.py")],
                    ..ChangeSet::default()
                },
            )
        });
        entered.wait();
        session.teardown();
        let still_active = !session.wait_builds_idle(Duration::ZERO);
        let ownership_retained = session.indexer_lock.lock().unwrap().is_some();
        resume.wait();
        worker.join().unwrap();
        assert!(still_active && ownership_retained);
        assert!(session.wait_builds_idle(Duration::ZERO));
        assert!(
            Arc::ptr_eq(&before, &session.snapshot_ready().unwrap()),
            "closed incremental writer replaced the serving snapshot"
        );
        assert!(session.indexer_lock.lock().unwrap().is_none());
        // A queued batch after closure must not reacquire ownership either.
        apply_or_buffer(
            &session,
            &ChangeSet {
                modified: vec![RelPath::new("a.py")],
                ..ChangeSet::default()
            },
        );
        assert!(session.indexer_lock.lock().unwrap().is_none());
        assert!(session.wait_builds_idle(Duration::ZERO));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn incremental_writer_excludes_hard_hibernation() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.writer_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        let worker_session = Arc::clone(&session);
        let worker = std::thread::spawn(move || {
            apply_or_buffer(
                &worker_session,
                &ChangeSet {
                    added: vec![RelPath::new("b.py")],
                    ..ChangeSet::default()
                },
            )
        });
        entered.wait();
        let observed = session.last_active.load(Ordering::Relaxed);
        let now = observed + 120;
        let slept_during_apply = session.hibernate(now, observed);
        resume.wait();
        worker.join().unwrap();
        assert!(
            !slept_during_apply,
            "an admitted incremental writer must exclude hibernation"
        );
        assert!(session
            .snapshot_ready()
            .unwrap()
            .graph()
            .symbols
            .iter()
            .any(|s| s.name == "beta"));
        assert!(
            session.hibernate(now, observed),
            "idle may hibernate after the writer finishes"
        );
        assert!(session.snapshot_ready().is_none());
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rebuild_spawn_failure_restores_retry_queue_and_slot() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let session = ready_session_with_policy(
            &dir,
            McpKnobs {
                watch_mode: None,
                ..McpKnobs::default()
            },
            idle_policy(0, 1, 0),
        );
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        session
            .apply_health
            .lock()
            .unwrap()
            .unapplied
            .added
            .push(RelPath::new("b.py"));
        session.fail_rebuild_spawn.store(true, Ordering::SeqCst);
        trigger_background_rebuild_arc(&session);
        assert!(!session.rebuild_inflight.load(Ordering::SeqCst));
        assert!(session.wait_builds_idle(Duration::ZERO));
        {
            let health = session.apply_health.lock().unwrap();
            assert!(health.in_rebuild.is_empty());
            assert!(health.unapplied.iter().any(|(_, p)| p.as_str() == "b.py"));
            assert!(health.rebuild_backoff_until.is_some());
        }
        // Advance the logical retry state without a wall-clock sleep.
        session.apply_health.lock().unwrap().rebuild_backoff_until = None;
        trigger_background_rebuild_arc(&session);
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        assert!(!session.rebuild_inflight.load(Ordering::SeqCst));
        assert!(session.apply_health.lock().unwrap().in_rebuild.is_empty());
        assert!(session.apply_health.lock().unwrap().unapplied.is_empty());
        assert!(session
            .snapshot_ready()
            .unwrap()
            .graph()
            .symbols
            .iter()
            .any(|s| s.name == "beta"));
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The receiver is deliberately owned by the test rather than a reindex
    /// worker. Receiving a batch proves observation completed, not publication.
    fn session_with_manual_watch_consumer(
        dir: &Path,
    ) -> (
        Arc<RootSession>,
        std::sync::mpsc::Receiver<ChangeSet>,
        WatchHandle,
        Arc<FreshnessLog>,
    ) {
        let session = ready_session(dir);
        let mut watcher = Watcher::new(dir.to_path_buf())
            .events(false)
            // This fixture observes batches directly, not debounce behavior.
            // An initial background scan can win the explicit verification;
            // emit that delta immediately instead of parking it for an hour.
            .debounce(Duration::ZERO)
            .poll_interval(Duration::from_secs(3600));
        assert!(watcher.check().is_empty());
        let (rx, handle, log) = watcher.spawn_with_log();
        *session.freshness.lock().unwrap() = Some(Arc::clone(&log));
        (session, rx, handle, log)
    }

    #[test]
    fn queued_delta_survives_later_observation_until_repair() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, rx, handle, log) = session_with_manual_watch_consumer(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        log.request_verification();
        let batch = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(batch.iter().any(|(_, p)| p.as_str() == "b.py"));
        // An unchanged second scan must not hide the queued/coalescing batch.
        let before = log.watermark();
        log.request_verification();
        assert!(wait_watermark_advance(&log, before, Duration::from_secs(5)));
        assert!(log.snapshot().suspects.contains(&RelPath::new("b.py")));
        assert!(!has_symbol(&session, "beta"));
        assert!(matches!(run_barrier(&session), BarrierEffect::None));
        assert!(has_symbol(&session, "beta"));
        assert!(
            log.snapshot().suspects.is_empty(),
            "successful repair was never acknowledged"
        );
        handle.stop();
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn publication_ack_preserves_change_observed_during_apply() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, rx, handle, log) = session_with_manual_watch_consumer(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        log.request_verification();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.writer_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        assert!(first.iter().any(|(_, p)| p.as_str() == "b.py"));
        let worker_session = Arc::clone(&session);
        let worker = std::thread::spawn(move || run_barrier(&worker_session));
        entered.wait();
        std::fs::write(dir.join("b.py"), "def gamma():\n    return 3\n").unwrap();
        log.request_verification();
        let second = rx.recv_timeout(Duration::from_secs(5));
        handle.stop();
        resume.wait();
        let effect = worker.join().unwrap();
        assert!(
            matches!(effect, BarrierEffect::StaleNote(_)),
            "a delta observed during repair must prevent a fresh result"
        );
        assert!(second.unwrap().iter().any(|(_, p)| p.as_str() == "b.py"));
        assert!(has_symbol(&session, "beta"));
        assert!(!has_symbol(&session, "gamma"));
        assert!(
            log.snapshot().suspects.contains(&RelPath::new("b.py")),
            "publication acknowledged a change newer than its read cutoff"
        );
        assert!(matches!(run_barrier(&session), BarrierEffect::None));
        assert!(has_symbol(&session, "gamma"));
        assert!(!has_symbol(&session, "beta"));
        assert!(log.snapshot().suspects.is_empty());
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_apply_does_not_ack_observed_delta() {
        let dir = unique_temp_dir();
        std::fs::write(dir.join("a.py"), "def alpha():\n    return 1\n").unwrap();
        let (session, rx, handle, log) = session_with_manual_watch_consumer(&dir);
        std::fs::write(dir.join("b.py"), "def beta():\n    return 2\n").unwrap();
        log.request_verification();
        let batch = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.stop();
        session.fail_applies.store(1, Ordering::SeqCst);
        apply_or_buffer(&session, &batch);
        assert!(!has_symbol(&session, "beta"));
        assert!(log.snapshot().suspects.contains(&RelPath::new("b.py")));
        assert!(inline_repair(&session, vec![RelPath::new("b.py")]));
        assert!(has_symbol(&session, "beta"));
        assert!(log.snapshot().suspects.is_empty());
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn known_change_invalidates_restored_stamp_cache_and_racing_fill() {
        use astrolabe_core::cache::CachedLines;
        let dir = unique_temp_dir();
        let path = dir.join("a.py");
        let old_text = "def alpha():\n    return 1\n";
        let new_text = "def bravo():\n    return 2\n";
        std::fs::write(&path, old_text).unwrap();
        let session = ready_session(&dir);
        let metadata = std::fs::metadata(&path).unwrap();
        let modified = metadata.modified().unwrap();
        let held = Arc::new(CachedLines::from_text(old_text));
        session
            .file_cache
            .insert_precise("a.py", Arc::clone(&held), modified, metadata.len());
        std::fs::write(&path, new_text).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let entered = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *session.writer_before_publish.lock().unwrap() =
            Some((Arc::clone(&entered), Arc::clone(&resume)));
        let worker_session = Arc::clone(&session);
        let worker = std::thread::spawn(move || {
            apply_or_buffer(
                &worker_session,
                &ChangeSet {
                    modified: vec![RelPath::new("a.py")],
                    ..ChangeSet::default()
                },
            )
        });
        entered.wait();
        let invalidated_before = session
            .file_cache
            .get_precise("a.py", modified, metadata.len())
            .is_none();
        // Model an old read filling the cache while this index write is in flight.
        session
            .file_cache
            .insert_precise("a.py", Arc::clone(&held), modified, metadata.len());
        resume.wait();
        worker.join().unwrap();
        assert!(invalidated_before);
        assert!(session
            .file_cache
            .get_precise("a.py", modified, metadata.len())
            .is_none());
        assert_eq!(
            held.as_text(),
            old_text,
            "existing readers keep valid Arc ownership"
        );
        assert!(has_symbol(&session, "bravo"));
        assert!(!has_symbol(&session, "alpha"));
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn directory_and_ignore_repairs_require_full_rebuild() {
        let dir = unique_temp_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/a.py"), "def nested():\n    return 1\n").unwrap();
        std::fs::write(dir.join("other.py"), "def other():\n    return 2\n").unwrap();
        let session = ready_session(&dir);
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        session.fail_rebuild_spawn.store(true, Ordering::SeqCst);
        assert!(!inline_repair(&session, vec![RelPath::new("sub")]));
        assert!(
            has_symbol(&session, "nested"),
            "direct repair must not pretend to remove a subtree"
        );
        session.apply_health.lock().unwrap().rebuild_backoff_until = None;
        trigger_background_rebuild_arc(&session);
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        assert!(!has_symbol(&session, "nested"));
        std::fs::write(dir.join(".gitignore"), "other.py\n").unwrap();
        session.fail_rebuild_spawn.store(true, Ordering::SeqCst);
        assert!(!inline_repair(&session, vec![RelPath::new(".gitignore")]));
        assert!(has_symbol(&session, "other"));
        session.apply_health.lock().unwrap().rebuild_backoff_until = None;
        trigger_background_rebuild_arc(&session);
        assert!(session.wait_builds_idle(Duration::from_secs(5)));
        assert!(!has_symbol(&session, "other"));
        session.teardown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
