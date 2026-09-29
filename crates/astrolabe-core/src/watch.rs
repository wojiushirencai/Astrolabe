//! Hybrid file watcher for incremental re-index: OS events by default, polling fallback.
//!
//! The index is a snapshot. After an editor save the graph is stale until
//! something rebuilds it. This module is that something: it detects filesystem
//! changes and diffs `mtime` + size against the previous snapshot so the
//! watched set matches the indexer (gitignore, default excludes, size/binary
//! gates).
//!
//! ## Architecture: Event fast path + conditional safety + freshness log
//!
//! 1. **Native OS events as a path hint**: We register a
//!    [`notify::RecommendedWatcher`] recursively on the workspace root
//!    (FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on
//!    Windows). Events carry paths; they are not themselves the diff.
//! 2. **Prefix filter before any walk**: Paths under excluded directories
//!    (`.git`, `node_modules`, `target`, `.astrolabe`, … plus
//!    [`ScanOptions::extra_excludes`] prefixes) are dropped on arrival. If
//!    every event in a wake is dropped, the loop does not arm debounce and
//!    does not call [`Watcher::check`] / [`Watcher::check_paths`] — this
//!    breaks the self-trigger loop of writing into `.astrolabe`.
//! 3. **Debounce & pending-path merge**: Surviving paths are merged into a
//!    deduplicated pending set (paths, not event objects). Rapid editor
//!    saves collapse into one directed diff after
//!    [`WatchConfig::debounce`] (default 500 ms).
//! 4. **Directed diff, storm degrade**: After the quiet period,
//!    [`Watcher::check_paths`] stats only the suspects (and admits new
//!    paths via [`crate::scan::path_admission`]). Directory events expand
//!    that subtree with a bounded walk. If the pending set exceeds
//!    [`WatchConfig::storm_paths`] (default 5000; e.g. `git checkout` of a
//!    large branch), the loop degrades to a full [`Watcher::check`].
//! 5. **Safety net**: A slow periodic full-tree scan (every
//!    [`WatchConfig::safety_interval`], default 30 minutes) still runs
//!    during OS-event silence. Overflow or a storm sets
//!    [`FreshnessLog::distrusted`]; while distrusted the safety scan is
//!    scheduled within [`DISTRUST_SAFETY_INTERVAL`] (60 s) and a clean
//!    full check clears the flag. The first deadline is phase-shifted by a
//!    random offset in `[0, interval)` so concurrent watchers do not
//!    stampede.
//! 6. **Automatic polling fallback**: If the OS notification system fails
//!    to initialize or errors at runtime, the watcher logs a warning and
//!    polls at [`WatchConfig::poll_interval`] (default 10 s).
//!
//! ## Cost model
//!
//! In event mode, idle CPU is 0.0% because the background thread sleeps on
//! channel `recv_timeout`. A typical editor save is a handful of `stat`
//! calls via `check_paths`, not a tree walk. A file is hashed only when it
//! looks different (`mtime`/size) or is still inside the mtime-precision
//! window. Full `check` (scan + diff) runs on storm/overflow, on the
//! safety interval, and in polling mode. The safety walk is phase-jittered
//! so N processes on one tree do not align their full scans.
//!
//! ## mtime precision
//!
//! Some filesystems report `mtime` at 1 s (APFS/HFS in compatibility
//! mode, some NFS) or 2 s (FAT). Two same-size writes in that window can
//! share one `mtime`. Files whose `mtime` is within
//! [`MTIME_UNTRUSTED_WINDOW`] of "now" (or in the future) are treated as
//! unsettled: we store a content hash on observe and re-hash on the next
//! check even if metadata looks unchanged.
//!
//! ## Usage
//!
//! Embed in an existing loop (you own scheduling and debounce):
//!
//! ```ignore
//! let mut watcher = astrolabe_core::watch::Watcher::new(root);
//! let _ = watcher.check(); // baseline; returns empty
//! loop {
//!     std::thread::sleep(std::time::Duration::from_secs(10));
//!     let changes = watcher.check();
//!     if !changes.is_empty() {
//!         // reindex `changes.added` + `changes.modified`;
//!         // drop `changes.removed` from the graph
//!     }
//! }
//! ```
//!
//! Or spawn a daemon thread that debounces and pushes on a channel:
//!
//! ```ignore
//! let (rx, handle, log) = astrolabe_core::watch::Watcher::new(root)
//!     .config(astrolabe_core::watch::WatchConfig::default())
//!     .spawn_with_log();
//! while let Ok(changes) = rx.recv() {
//!     // one changeset per quiet period
//! }
//! handle.stop();
//! ```

use crate::scan::{self, ScanOptions};
use crate::types::RelPath;
use notify::{RecursiveMode, Watcher as _};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// Files whose mtime is newer than this are not trusted without a hash.
///
/// FAT timestamps are 2 s; most other coarse clocks are 1 s. A 2 s window
/// covers both, plus a little clock skew.
pub const MTIME_UNTRUSTED_WINDOW: Duration = Duration::from_secs(2);

const STOP_SLICE: Duration = Duration::from_millis(50);
const EVENT_SAFETY_INTERVAL: Duration = Duration::from_secs(1800);
/// While [`FreshnessLog`] is distrusted, a full safety scan is scheduled
/// within this cap rather than waiting out [`WatchConfig::safety_interval`].
const DISTRUST_SAFETY_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_STORM_PATHS: usize = 5000;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// Directory names matching [`crate::scan`]'s `EXCLUDED_DIRS` (kept local so
/// this module does not depend on scan internals). A path is excluded when
/// any component equals one of these names.
const EXCLUDED_DIR_NAMES: &[&str] = &[
    ".astrolabe",
    ".git",
    "node_modules",
    "target",
    "build",
    "dist",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".openvisio",
    ".serena",
];

/// Polling cadence, emit delay, event-mode safety scan, and storm threshold
/// for [`Watcher::spawn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Cadence for the polling fallback when native OS events are disabled or unavailable.
    pub poll_interval: Duration,
    /// Quiet period before a pending [`ChangeSet`] is sent. Rapid editor
    /// saves collapse into one event.
    pub debounce: Duration,
    /// Periodic full-tree scan in event mode even during complete OS-event
    /// silence. Guards against dropped or lost notifications. Default 30
    /// minutes. `Duration::ZERO` disables the safety scan (unless
    /// [`FreshnessLog`] is distrusted, in which case a 60 s scan is still
    /// scheduled).
    pub safety_interval: Duration,
    /// Phase-shift the first safety-scan deadline by a random offset in
    /// `[0, safety_interval)` so concurrent watchers on the same tree do not
    /// stampede. Default true; tests that depend on loop timing should set
    /// this false.
    pub safety_jitter: bool,
    /// Pending suspect-path count that forces a full-tree [`Watcher::check`].
    /// Default 5000. Covers `git checkout` of a large branch.
    pub storm_paths: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            poll_interval: Duration::from_secs(10),
            debounce: Duration::from_millis(500),
            safety_interval: EVENT_SAFETY_INTERVAL,
            safety_jitter: true,
            storm_paths: DEFAULT_STORM_PATHS,
        }
    }
}

impl WatchConfig {
    /// Periodic full-tree scan interval in event mode. See [`Self::safety_interval`].
    pub fn safety_interval(mut self, d: Duration) -> Self {
        self.safety_interval = d;
        self
    }

    /// Enable or disable the first-scan phase offset. See [`Self::safety_jitter`].
    pub fn safety_jitter(mut self, enabled: bool) -> Self {
        self.safety_jitter = enabled;
        self
    }

    /// Storm threshold. See [`Self::storm_paths`].
    pub fn storm_paths(mut self, n: usize) -> Self {
        self.storm_paths = n;
        self
    }
}

/// Query-side freshness barrier: watcher thread writes, readers observe.
///
/// Shared via [`Arc`]. A watermark is the last completed verification (a
/// finished full [`Watcher::check`], or a finished directed
/// [`Watcher::check_paths`]). Overflow / storm sets `distrusted` until the
/// next clean full check.
pub struct FreshnessLog {
    inner: Mutex<FreshnessInner>,
    /// Set by the query-time barrier when the watermark is stale but there
    /// are no suspects (`Repair([])`). The watch loop polls this every
    /// [`STOP_SLICE`] and runs a full [`Watcher::check`] (which calls
    /// [`FreshnessLog::note_verified`]).
    verify_requested: AtomicBool,
}

struct FreshnessInner {
    watermark: Instant,
    distrusted: bool,
    /// Suspect paths accumulated after the current watermark, in note order.
    /// When `distrusted`, [`FreshnessLog::suspect_paths`] hides these and
    /// returns empty (callers look at [`FreshnessLog::distrusted`]).
    suspects: Vec<(Instant, RelPath)>,
}

impl FreshnessLog {
    fn new() -> Self {
        FreshnessLog {
            inner: Mutex::new(FreshnessInner {
                watermark: Instant::now(),
                distrusted: false,
                suspects: Vec::new(),
            }),
            verify_requested: AtomicBool::new(false),
        }
    }

    /// Ask the watch loop to run a full verification on the next `STOP_SLICE`.
    ///
    /// Used by the MCP freshness barrier for `Repair([])` (stale watermark,
    /// no suspects). A clean [`Watcher::check`] pushes the watermark.
    pub fn request_verification(&self) {
        self.verify_requested.store(true, Ordering::SeqCst);
    }

    fn take_verification_request(&self) -> bool {
        self.verify_requested.swap(false, Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FreshnessInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Instant of the last completed verification (full check or directed).
    pub fn watermark(&self) -> Instant {
        self.lock().watermark
    }

    /// Suspect paths noted after `since`.
    ///
    /// Includes event paths and the pending set as it stood before a storm
    /// expanded into a full check. When the log is distrusted this returns
    /// empty; callers should consult [`Self::distrusted`].
    pub fn suspect_paths(&self, since: Instant) -> Vec<RelPath> {
        let g = self.lock();
        if g.distrusted {
            return Vec::new();
        }
        let mut out: Vec<RelPath> = g
            .suspects
            .iter()
            .filter(|(t, _)| *t > since)
            .map(|(_, p)| p.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Overflow or storm happened, and no clean full check has run since.
    pub fn distrusted(&self) -> bool {
        self.lock().distrusted
    }

    pub(crate) fn note_paths(&self, paths: &[RelPath]) {
        if paths.is_empty() {
            return;
        }
        let mut g = self.lock();
        let now = Instant::now();
        for p in paths {
            g.suspects.push((now, p.clone()));
        }
    }

    pub(crate) fn note_distrust(&self) {
        self.lock().distrusted = true;
    }

    /// A completed full check: push watermark, clear distrust and suspects.
    pub(crate) fn note_verified(&self) {
        let mut g = self.lock();
        g.watermark = Instant::now();
        g.distrusted = false;
        g.suspects.clear();
    }

    fn bump_watermark(&self) {
        self.lock().watermark = Instant::now();
    }
}

impl std::fmt::Debug for FreshnessLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("FreshnessLog")
            .field("distrusted", &g.distrusted)
            .field("suspects", &g.suspects.len())
            .finish()
    }
}

/// What happened to one file since the previous snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

/// A deterministic set of path changes.
///
/// Each vec is sorted by [`RelPath`]. Field order is added → modified →
/// removed so a debug print matches the names the orchestrator cares about.
/// When applying to a graph, drop `removed` first, then parse `added` and
/// `modified`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSet {
    pub added: Vec<RelPath>,
    pub modified: Vec<RelPath>,
    pub removed: Vec<RelPath>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len()
    }

    /// Paths in stable order: added, then modified, then removed.
    /// Within each kind, paths are sorted.
    pub fn iter(&self) -> impl Iterator<Item = (ChangeKind, &RelPath)> {
        self.added
            .iter()
            .map(|p| (ChangeKind::Added, p))
            .chain(self.modified.iter().map(|p| (ChangeKind::Modified, p)))
            .chain(self.removed.iter().map(|p| (ChangeKind::Removed, p)))
    }

    /// Fold `other` into this set with editor-style coalescing.
    ///
    /// Used by the background thread to collapse bursts inside the debounce
    /// window. Also usable by an orchestrator that batches its own `check`
    /// results.
    pub fn merge(&mut self, other: ChangeSet) {
        if other.is_empty() {
            return;
        }
        let mut map = self.to_map();
        for (kind, path) in other.iter() {
            apply_coalesce(&mut map, path.clone(), kind);
        }
        *self = ChangeSet::from_map(map);
    }

    fn to_map(&self) -> BTreeMap<RelPath, ChangeKind> {
        let mut map = BTreeMap::new();
        for (kind, path) in self.iter() {
            map.insert(path.clone(), kind);
        }
        map
    }

    fn from_map(map: BTreeMap<RelPath, ChangeKind>) -> Self {
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut removed = Vec::new();
        for (path, kind) in map {
            match kind {
                ChangeKind::Added => added.push(path),
                ChangeKind::Modified => modified.push(path),
                ChangeKind::Removed => removed.push(path),
            }
        }
        ChangeSet {
            added,
            modified,
            removed,
        }
    }
}

fn apply_coalesce(map: &mut BTreeMap<RelPath, ChangeKind>, path: RelPath, next: ChangeKind) {
    match (map.get(&path).copied(), next) {
        (None, kind) => {
            map.insert(path, kind);
        }
        (Some(ChangeKind::Added), ChangeKind::Added) => {}
        (Some(ChangeKind::Added), ChangeKind::Modified) => {}
        (Some(ChangeKind::Added), ChangeKind::Removed) => {
            map.remove(&path);
        }
        (Some(ChangeKind::Modified), ChangeKind::Added) => {}
        (Some(ChangeKind::Modified), ChangeKind::Modified) => {}
        (Some(ChangeKind::Modified), ChangeKind::Removed) => {
            map.insert(path, ChangeKind::Removed);
        }
        (Some(ChangeKind::Removed), ChangeKind::Added) => {
            map.insert(path, ChangeKind::Modified);
        }
        (Some(ChangeKind::Removed), ChangeKind::Modified) => {
            map.insert(path, ChangeKind::Modified);
        }
        (Some(ChangeKind::Removed), ChangeKind::Removed) => {}
    }
}

#[derive(Clone, Debug)]
struct FileStamp {
    mtime: SystemTime,
    len: u64,
    hash: Option<u64>,
    unsettled: bool,
    mtime_ok: bool,
}

/// Hybrid filesystem watcher (native OS events with polling fallback).
///
/// Holds the last snapshot; [`check`] returns the full-tree delta and
/// [`check_paths`] diffs a suspect set. Prefer [`spawn`] / [`spawn_with_log`]
/// for a background thread that debounces and pushes [`ChangeSet`]s.
pub struct Watcher {
    root: PathBuf,
    scan: ScanOptions,
    config: WatchConfig,
    snapshot: BTreeMap<RelPath, FileStamp>,
    seeded: bool,
    events: bool,
    freshness: Arc<FreshnessLog>,
    check_calls: Arc<AtomicU64>,
    check_paths_calls: Arc<AtomicU64>,
}

impl Watcher {
    /// Construct without touching disk. The first [`check`] (or [`spawn`])
    /// takes the baseline and returns an empty changeset.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Watcher {
            root: root.into(),
            scan: ScanOptions::default(),
            config: WatchConfig::default(),
            snapshot: BTreeMap::new(),
            seeded: false,
            events: true,
            freshness: Arc::new(FreshnessLog::new()),
            check_calls: Arc::new(AtomicU64::new(0)),
            check_paths_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Enable or disable native OS filesystem events (default: true).
    /// When disabled, [`Watcher::spawn`] uses pure polling at [`WatchConfig::poll_interval`].
    pub fn events(mut self, enabled: bool) -> Self {
        self.events = enabled;
        self
    }

    pub fn scan_options(mut self, scan: ScanOptions) -> Self {
        self.scan = scan;
        self
    }

    pub fn config(mut self, config: WatchConfig) -> Self {
        self.config = config;
        self
    }

    pub fn poll_interval(mut self, poll_interval: Duration) -> Self {
        self.config.poll_interval = poll_interval;
        self
    }

    pub fn debounce(mut self, debounce: Duration) -> Self {
        self.config.debounce = debounce;
        self
    }

    pub fn safety_interval(mut self, safety_interval: Duration) -> Self {
        self.config.safety_interval = safety_interval;
        self
    }

    pub fn safety_jitter(mut self, enabled: bool) -> Self {
        self.config.safety_jitter = enabled;
        self
    }

    pub fn storm_paths(mut self, n: usize) -> Self {
        self.config.storm_paths = n;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Files in the current snapshot (0 until the first `check` / spawn).
    pub fn tracked_files(&self) -> usize {
        self.snapshot.len()
    }

    pub fn is_seeded(&self) -> bool {
        self.seeded
    }

    /// Diff the tree against the last snapshot.
    ///
    /// The first call only records a baseline and returns empty. Later
    /// calls return added / modified / removed paths, each vec sorted.
    /// Completing this full scan pushes the freshness watermark and
    /// clears distrust.
    pub fn check(&mut self) -> ChangeSet {
        self.check_calls.fetch_add(1, Ordering::Relaxed);
        let now = SystemTime::now();
        let observed = observe_tree(&self.root, &self.scan);

        if !self.seeded {
            self.snapshot = stamps_from_observed(&self.root, &observed, now);
            self.seeded = true;
            self.freshness.note_verified();
            return ChangeSet::default();
        }

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut next_snapshot = BTreeMap::new();

        for (path, meta) in &observed {
            match self.snapshot.get(path) {
                None => {
                    added.push(path.clone());
                    next_snapshot
                        .insert(path.clone(), make_stamp(&self.root, path, meta, now, None));
                }
                Some(old) => {
                    let (stamp, changed) = confirm(&self.root, path, old, meta, now);
                    if changed {
                        modified.push(path.clone());
                    }
                    next_snapshot.insert(path.clone(), stamp);
                }
            }
        }

        let mut removed = Vec::new();
        for path in self.snapshot.keys() {
            if !observed.contains_key(path) {
                removed.push(path.clone());
            }
        }

        self.snapshot = next_snapshot;

        let changes = ChangeSet {
            added,
            modified,
            removed,
        };
        if !changes.is_empty() {
            tracing::debug!(
                added = changes.added.len(),
                modified = changes.modified.len(),
                removed = changes.removed.len(),
                "watch changeset"
            );
        }
        self.freshness.note_verified();
        changes
    }

    /// Directed diff of `paths` against the snapshot.
    ///
    /// Deduplicates `paths`. Directories are expanded with a bounded walk
    /// (excluded names skipped; snapshot keys under the prefix included so
    /// deletions are visible). Paths already in the snapshot are `stat`ed
    /// and run through [`confirm`]; new paths go through
    /// [`scan::path_admission`]. If the unique / expanded set exceeds
    /// [`WatchConfig::storm_paths`], this degrades to [`Self::check`].
    pub fn check_paths(&mut self, paths: &[PathBuf]) -> ChangeSet {
        self.check_paths_calls.fetch_add(1, Ordering::Relaxed);
        if !self.seeded {
            return self.check();
        }
        let cap = self.config.storm_paths;
        let mut unique = Vec::new();
        let mut seen = HashSet::new();
        for p in paths {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                self.root.join(p)
            };
            if seen.insert(abs.clone()) {
                unique.push(abs);
            }
        }
        if unique.len() > cap {
            self.freshness.note_distrust();
            return self.check();
        }

        let extra = &self.scan.extra_excludes;
        let mut work: BTreeSet<RelPath> = BTreeSet::new();
        for abs in &unique {
            if abs == &self.root
                || (relativize(&self.root, abs).is_none() && abs.starts_with(&self.root))
            {
                // Root-directory event: treat as a bounded walk of the whole
                // tree. Hitting `cap` degrades to a full check.
                if expand_dir_bounded(&self.root, abs, extra, cap, &mut work).is_err() {
                    self.freshness.note_distrust();
                    return self.check();
                }
                if work.len() > cap {
                    self.freshness.note_distrust();
                    return self.check();
                }
                continue;
            }
            let Some(rel) = relativize(&self.root, abs) else {
                continue;
            };
            if is_excluded_rel(&rel, extra) {
                continue;
            }
            if abs.is_dir() {
                if expand_dir_bounded(&self.root, abs, extra, cap, &mut work).is_err() {
                    self.freshness.note_distrust();
                    return self.check();
                }
                let prefix = rel.as_str();
                for k in self.snapshot.keys() {
                    if rel_is_under(k, prefix) {
                        work.insert(k.clone());
                        if work.len() > cap {
                            self.freshness.note_distrust();
                            return self.check();
                        }
                    }
                }
            } else {
                work.insert(rel.clone());
                if !abs.exists() {
                    let prefix = rel.as_str();
                    for k in self.snapshot.keys() {
                        if rel_is_under(k, prefix) {
                            work.insert(k.clone());
                            if work.len() > cap {
                                self.freshness.note_distrust();
                                return self.check();
                            }
                        }
                    }
                }
                if work.len() > cap {
                    self.freshness.note_distrust();
                    return self.check();
                }
            }
        }

        if work.len() > cap {
            self.freshness.note_distrust();
            return self.check();
        }

        let now = SystemTime::now();
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut removed = Vec::new();

        for rel in work {
            match self.snapshot.get(&rel).cloned() {
                Some(old) => match observe_one(&self.root, &rel) {
                    Some(meta) => {
                        let (stamp, changed) = confirm(&self.root, &rel, &old, &meta, now);
                        if changed {
                            modified.push(rel.clone());
                        }
                        self.snapshot.insert(rel, stamp);
                    }
                    None => {
                        self.snapshot.remove(&rel);
                        removed.push(rel);
                    }
                },
                None => match admit_new(&self.root, &rel, &self.scan) {
                    Admit::Indexed => {
                        if let Some(meta) = observe_one(&self.root, &rel) {
                            let stamp = make_stamp(&self.root, &rel, &meta, now, None);
                            self.snapshot.insert(rel.clone(), stamp);
                            added.push(rel);
                        }
                    }
                    Admit::Missing | Admit::Skip => {}
                },
            }
        }

        added.sort();
        modified.sort();
        removed.sort();
        let changes = ChangeSet {
            added,
            modified,
            removed,
        };
        if !changes.is_empty() {
            tracing::debug!(
                added = changes.added.len(),
                modified = changes.modified.len(),
                removed = changes.removed.len(),
                "watch directed changeset"
            );
        }
        self.freshness.bump_watermark();
        changes
    }

    /// Run `check` on a background thread and send debounced changesets.
    ///
    /// Delegates to [`Self::spawn_with_log`] and drops the log handle.
    pub fn spawn(self) -> (Receiver<ChangeSet>, WatchHandle) {
        let (rx, handle, _log) = self.spawn_with_log();
        (rx, handle)
    }

    /// Like [`Self::spawn`], also returning the shared [`FreshnessLog`].
    ///
    /// The thread is daemon-like: dropping the [`WatchHandle`] signals stop
    /// **without joining**, so it cannot hold the process open. Call
    /// [`WatchHandle::stop`] for a graceful join.
    pub fn spawn_with_log(self) -> (Receiver<ChangeSet>, WatchHandle, Arc<FreshnessLog>) {
        let log = Arc::clone(&self.freshness);
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        // First full-tree scan runs on the background thread (see event_loop /
        // run_loop), not on the spawn caller. If already seeded, that initial
        // check preserves any non-empty delta since the prior check.
        let notify_ctx = if self.events {
            match init_notify(&self.root) {
                Some((feed, guard)) => Some((feed, guard)),
                None => {
                    tracing::warn!(
                        "filesystem event watcher could not be initialized; falling back to polling"
                    );
                    None
                }
            }
        } else {
            None
        };

        let thread = thread::Builder::new()
            .name("astrolabe-watch".into())
            .spawn(move || {
                if let Some((feed, guard)) = notify_ctx {
                    let (watcher, ok) =
                        event_loop(self, feed, guard, tx.clone(), Arc::clone(&stop_thread));
                    if !ok && !stop_thread.load(Ordering::Relaxed) {
                        tracing::warn!(
                            "filesystem event watcher encountered an error; falling back to polling"
                        );
                        run_loop(watcher, tx, stop_thread);
                    }
                    return;
                }
                run_loop(self, tx, stop_thread);
            })
            .expect("failed to spawn astrolabe-watch thread");
        (
            rx,
            WatchHandle {
                stop,
                thread: Some(thread),
            },
            log,
        )
    }
}

/// Handle for the thread started by [`Watcher::spawn`].
pub struct WatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

// Derived `Debug` is unavailable because `JoinHandle` is only `Debug` on some
// versions; report the observable state instead so callers holding this in a
// `#[derive(Debug)]` struct still compile.
impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle")
            .field("stopping", &self.stop.load(Ordering::Relaxed))
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl WatchHandle {
    /// True once the thread has exited (or was never started).
    pub fn is_finished(&self) -> bool {
        self.thread
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(true)
    }

    /// Ask the thread to exit. Does not wait; see [`stop`](Self::stop).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Signal stop and join the thread.
    pub fn stop(mut self) {
        self.request_stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Detach: dropping JoinHandle without join must not block process
        // exit. The thread notices the flag within STOP_SLICE.
    }
}

/// One wake from the OS event backend (or a test double).
#[derive(Debug)]
enum FeedWake {
    /// Native event(s). `overflow` is latched when the bounded channel
    /// dropped at least one event since the last recv.
    Events { paths: Vec<PathBuf>, overflow: bool },
    /// Backend error → fall back to polling.
    Error,
}

trait EventFeed {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError>;
}

struct NotifyFeed {
    rx: Receiver<Result<notify::Event, notify::Error>>,
    overflow: Arc<AtomicBool>,
}

impl EventFeed for NotifyFeed {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                let overflow = self.overflow.swap(false, Ordering::SeqCst) || ev.need_rescan();
                Ok(FeedWake::Events {
                    paths: ev.paths,
                    overflow,
                })
            }
            Ok(Err(_)) => Ok(FeedWake::Error),
            Err(e) => {
                if self.overflow.swap(false, Ordering::SeqCst) {
                    Ok(FeedWake::Events {
                        paths: Vec::new(),
                        overflow: true,
                    })
                } else {
                    Err(e)
                }
            }
        }
    }
}

/// Cap queued OS events so a burst cannot grow the channel without bound.
/// Overflow latches a flag; the receiver marks the tree dirty and the
/// safety / debounce path recovers with a full check.
const NOTIFY_CHANNEL_CAP: usize = 65536;

fn init_notify(root: &Path) -> Option<(NotifyFeed, notify::RecommendedWatcher)> {
    let overflow = Arc::new(AtomicBool::new(false));
    let overflow_cb = Arc::clone(&overflow);
    let (tx, rx) = mpsc::sync_channel(NOTIFY_CHANNEL_CAP);
    let mut watcher = match notify::recommended_watcher(move |res| match tx.try_send(res) {
        Ok(()) => {}
        Err(mpsc::TrySendError::Full(_)) => {
            overflow_cb.store(true, Ordering::SeqCst);
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {}
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(%e, "failed to create notify recommended_watcher");
            return None;
        }
    };
    if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
        tracing::warn!(%e, root = %root.display(), "failed to register notify watch on root");
        return None;
    }
    Some((NotifyFeed { rx, overflow }, watcher))
}

/// Mix pid, a high-res clock, and a per-call counter into a u64.
/// Not cryptographic — only used to desynchronize safety-scan deadlines.
fn entropy64() -> u64 {
    use std::sync::atomic::AtomicU64 as Seq;
    static SEQ: Seq = Seq::new(1);
    let mut h = FNV_OFFSET;
    h ^= u64::from(std::process::id());
    h = h.wrapping_mul(FNV_PRIME);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    h ^= nanos;
    h = h.wrapping_mul(FNV_PRIME);
    h ^= SEQ.fetch_add(1, Ordering::Relaxed);
    h.wrapping_mul(FNV_PRIME)
}

/// Offset in `[0, interval)`. `interval == 0` yields zero.
fn safety_phase_offset(interval: Duration, entropy: u64) -> Duration {
    let total = interval.as_nanos();
    if total == 0 {
        return Duration::ZERO;
    }
    duration_from_nanos_u128((entropy as u128) % total)
}

fn duration_from_nanos_u128(n: u128) -> Duration {
    const NANOS_PER_SEC: u128 = 1_000_000_000;
    Duration::new((n / NANOS_PER_SEC) as u64, (n % NANOS_PER_SEC) as u32)
}

fn initial_last_safety(config: &WatchConfig) -> Instant {
    let now = Instant::now();
    if !config.safety_jitter {
        return now;
    }
    now.checked_sub(safety_phase_offset(config.safety_interval, entropy64()))
        .unwrap_or(now)
}

/// Effective wait until the next full safety scan.
///
/// Distrust caps the wait at [`DISTRUST_SAFETY_INTERVAL`]. A configured
/// `Duration::ZERO` still disables safety when *not* distrusted.
fn safety_wait(configured: Duration, distrusted: bool) -> Duration {
    if distrusted {
        if configured.is_zero() {
            DISTRUST_SAFETY_INTERVAL
        } else {
            configured.min(DISTRUST_SAFETY_INTERVAL)
        }
    } else {
        configured
    }
}

fn event_loop<F: EventFeed, G>(
    mut watcher: Watcher,
    mut feed: F,
    guard: G,
    tx: mpsc::Sender<ChangeSet>,
    stop: Arc<AtomicBool>,
) -> (Watcher, bool) {
    let debounce = watcher.config.debounce;
    let storm_paths = watcher.config.storm_paths;
    let safety_interval = watcher.config.safety_interval;
    let mut last_event: Option<Instant> = None;
    let mut last_safety = initial_last_safety(&watcher.config);
    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut all_dirty = false;

    // Seed baseline (!seeded → empty) or emit catch-up delta if already seeded.
    {
        let batch = watcher.check();
        if !batch.is_empty() && tx.send(batch).is_err() {
            drop(guard);
            return (watcher, true);
        }
    }

    while !stop.load(Ordering::Relaxed) {
        match feed.recv_timeout(STOP_SLICE) {
            Ok(FeedWake::Events { paths, overflow }) => {
                let mut kept = 0usize;
                let mut noted: Vec<RelPath> = Vec::new();
                if overflow {
                    all_dirty = true;
                    watcher.freshness.note_distrust();
                }
                for p in paths {
                    if let Some((abs, rel)) = keep_event_path(&watcher.root, &watcher.scan, &p) {
                        if pending.insert(abs) {
                            noted.push(rel);
                        }
                        kept += 1;
                    }
                }
                // Drain coalesced events so a bounded channel does not stay full.
                loop {
                    match feed.recv_timeout(Duration::ZERO) {
                        Ok(FeedWake::Events { paths, overflow }) => {
                            if overflow {
                                all_dirty = true;
                                watcher.freshness.note_distrust();
                            }
                            for p in paths {
                                if let Some((abs, rel)) =
                                    keep_event_path(&watcher.root, &watcher.scan, &p)
                                {
                                    if pending.insert(abs) {
                                        noted.push(rel);
                                    }
                                    kept += 1;
                                }
                            }
                        }
                        Ok(FeedWake::Error) => {
                            drop(guard);
                            return (watcher, false);
                        }
                        Err(_) => break,
                    }
                }
                if pending.len() > storm_paths {
                    all_dirty = true;
                    watcher.freshness.note_distrust();
                }
                if !noted.is_empty() {
                    watcher.freshness.note_paths(&noted);
                }
                if kept > 0 || overflow || all_dirty {
                    last_event = Some(Instant::now());
                }
            }
            Ok(FeedWake::Error) => {
                drop(guard);
                return (watcher, false);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                drop(guard);
                return (watcher, false);
            }
        }

        // Query-time Repair([]) asks for a full check so a stale watermark
        // can advance without waiting for the safety interval.
        if watcher.freshness.take_verification_request() {
            all_dirty = false;
            pending.clear();
            last_event = None;
            last_safety = Instant::now();
            let batch = watcher.check();
            if !batch.is_empty() && tx.send(batch).is_err() {
                break;
            }
        }

        let quiet = last_event.map(|t| t.elapsed() >= debounce).unwrap_or(false);
        let wait = safety_wait(safety_interval, watcher.freshness.distrusted());
        let safety_due = !wait.is_zero() && last_safety.elapsed() >= wait;

        if quiet || safety_due {
            last_event = None;
            // Safety scans and storm/overflow always take the full-tree path.
            // Directed `check_paths` does not reset the safety clock, so a
            // distrusted log still gets a full scan within the 60 s cap.
            let storm = all_dirty || pending.len() > storm_paths;
            let batch = if safety_due || storm {
                all_dirty = false;
                pending.clear();
                last_safety = Instant::now();
                watcher.check()
            } else {
                let paths: Vec<PathBuf> = pending.drain().collect();
                watcher.check_paths(&paths)
            };
            if !batch.is_empty() && tx.send(batch).is_err() {
                break;
            }
        }
    }

    // Flush pending debounce on stop, matching run_loop's pending flush.
    if last_event.is_some() || !pending.is_empty() || all_dirty {
        let batch = if all_dirty || pending.len() > storm_paths {
            watcher.check()
        } else if pending.is_empty() {
            ChangeSet::default()
        } else {
            let paths: Vec<PathBuf> = pending.drain().collect();
            watcher.check_paths(&paths)
        };
        if !batch.is_empty() {
            let _ = tx.send(batch);
        }
    }

    drop(guard);
    (watcher, true)
}

fn run_loop(mut watcher: Watcher, tx: mpsc::Sender<ChangeSet>, stop: Arc<AtomicBool>) {
    let poll_interval = clamp_interval(watcher.config.poll_interval);
    let debounce = watcher.config.debounce;
    let mut pending = ChangeSet::default();
    let mut last_activity: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) {
        let batch = watcher.check();
        if !batch.is_empty() {
            pending.merge(batch);
            last_activity = Some(Instant::now());
        }

        let quiet = last_activity
            .map(|t| t.elapsed() >= debounce)
            .unwrap_or(false);
        if !pending.is_empty() && (debounce.is_zero() || quiet) {
            let emit = std::mem::take(&mut pending);
            last_activity = None;
            if tx.send(emit).is_err() {
                break;
            }
        }

        if wait_or_stop_checking(&stop, poll_interval, &mut watcher, &tx) {
            if !pending.is_empty() {
                let _ = tx.send(std::mem::take(&mut pending));
            }
            break;
        }
    }
    if !pending.is_empty() {
        let _ = tx.send(pending);
    }
}

fn clamp_interval(d: Duration) -> Duration {
    if d.is_zero() {
        Duration::from_millis(1)
    } else {
        d
    }
}

/// Sleep up to `total`, waking every [`STOP_SLICE`] to honor stop and
/// on-demand [`FreshnessLog::request_verification`].
///
/// Returns true if stop was requested (or the channel closed during a
/// verification check).
fn wait_or_stop_checking(
    stop: &AtomicBool,
    total: Duration,
    watcher: &mut Watcher,
    tx: &mpsc::Sender<ChangeSet>,
) -> bool {
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        if watcher.freshness.take_verification_request() {
            let batch = watcher.check();
            if !batch.is_empty() && tx.send(batch).is_err() {
                return true;
            }
        }
        let elapsed = start.elapsed();
        if elapsed >= total {
            return stop.load(Ordering::Relaxed);
        }
        thread::sleep((total - elapsed).min(STOP_SLICE));
    }
}

struct Observed {
    mtime: SystemTime,
    len: u64,
    mtime_ok: bool,
}

fn observe_tree(root: &Path, opts: &ScanOptions) -> BTreeMap<RelPath, Observed> {
    let scanned = scan::scan(root, opts);
    let mut out = BTreeMap::new();
    for rel in scanned.files {
        let abs = root.join(rel.as_str());
        let Ok(meta) = abs.metadata() else {
            continue;
        };
        let (mtime, mtime_ok) = match meta.modified() {
            Ok(t) => (t, true),
            Err(_) => (SystemTime::UNIX_EPOCH, false),
        };
        out.insert(
            rel,
            Observed {
                mtime,
                len: meta.len(),
                mtime_ok,
            },
        );
    }
    out
}

fn observe_one(root: &Path, rel: &RelPath) -> Option<Observed> {
    let abs = root.join(rel.as_str());
    let meta = abs.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let (mtime, mtime_ok) = match meta.modified() {
        Ok(t) => (t, true),
        Err(_) => (SystemTime::UNIX_EPOCH, false),
    };
    Some(Observed {
        mtime,
        len: meta.len(),
        mtime_ok,
    })
}

fn stamps_from_observed(
    root: &Path,
    observed: &BTreeMap<RelPath, Observed>,
    now: SystemTime,
) -> BTreeMap<RelPath, FileStamp> {
    observed
        .iter()
        .map(|(path, meta)| (path.clone(), make_stamp(root, path, meta, now, None)))
        .collect()
}

fn make_stamp(
    root: &Path,
    path: &RelPath,
    meta: &Observed,
    now: SystemTime,
    prev_hash: Option<u64>,
) -> FileStamp {
    let unsettled = is_unsettled(meta.mtime, meta.mtime_ok, now);
    let hash = if unsettled {
        hash_file(&root.join(path.as_str())).or(prev_hash)
    } else {
        prev_hash
    };
    FileStamp {
        mtime: meta.mtime,
        len: meta.len,
        hash,
        unsettled,
        mtime_ok: meta.mtime_ok,
    }
}

fn confirm(
    root: &Path,
    path: &RelPath,
    old: &FileStamp,
    meta: &Observed,
    now: SystemTime,
) -> (FileStamp, bool) {
    let abs = root.join(path.as_str());
    let mut unsettled = is_unsettled(meta.mtime, meta.mtime_ok, now);
    let meta_changed =
        meta.mtime != old.mtime || meta.len != old.len || meta.mtime_ok != old.mtime_ok;

    if !meta_changed && !old.unsettled && !unsettled {
        return (
            FileStamp {
                mtime: meta.mtime,
                len: meta.len,
                hash: old.hash,
                unsettled: false,
                mtime_ok: meta.mtime_ok,
            },
            false,
        );
    }

    // Suspected: hash only this file. Skip a full-tree content walk.
    let new_hash = hash_file(&abs);
    let content_changed = match (old.hash, new_hash) {
        (Some(prev), Some(next)) => prev != next,
        // First successful hash with no prior: only treat as modified when
        // metadata also changed. Settling unreadable→readable (or other
        // unsettled→hashed) with unchanged mtime/size must not spuriously
        // report modified — the hash becomes the new baseline.
        (None, Some(_)) => meta_changed,
        (None, None) => {
            if old.unsettled {
                unsettled = true;
            }
            meta_changed
        }
        (Some(_), None) => {
            unsettled = true;
            false
        }
    };

    let stamp = FileStamp {
        mtime: meta.mtime,
        len: meta.len,
        hash: new_hash.or(old.hash),
        unsettled,
        mtime_ok: meta.mtime_ok,
    };
    (stamp, content_changed)
}

fn is_unsettled(mtime: SystemTime, mtime_ok: bool, now: SystemTime) -> bool {
    if !mtime_ok {
        return true;
    }
    match now.duration_since(mtime) {
        Ok(age) => age < MTIME_UNTRUSTED_WINDOW,
        Err(_) => true,
    }
}

fn hash_file(path: &Path) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut buf = [0u8; 8192];
    let mut h = FNV_OFFSET;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        for b in &buf[..n] {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
    }
    Some(h)
}

fn relativize(root: &Path, path: &Path) -> Option<RelPath> {
    let stripped = path.strip_prefix(root).ok()?;
    if stripped.as_os_str().is_empty() {
        return None;
    }
    Some(RelPath::new(stripped.to_string_lossy().as_ref()))
}

fn is_excluded_rel(rel: &RelPath, extra: &[String]) -> bool {
    let s = rel.as_str();
    for comp in s.split('/') {
        if !comp.is_empty() && EXCLUDED_DIR_NAMES.contains(&comp) {
            return true;
        }
    }
    for raw in extra {
        let ex = RelPath::new(raw.as_str());
        let e = ex.as_str().trim_end_matches('/');
        if e.is_empty() {
            continue;
        }
        if s == e || (s.len() > e.len() && s.starts_with(e) && s.as_bytes()[e.len()] == b'/') {
            return true;
        }
    }
    false
}

fn rel_is_under(path: &RelPath, dir: &str) -> bool {
    let p = path.as_str();
    if dir.is_empty() {
        return true;
    }
    p == dir || (p.len() > dir.len() && p.starts_with(dir) && p.as_bytes()[dir.len()] == b'/')
}

fn keep_event_path(root: &Path, scan: &ScanOptions, path: &Path) -> Option<(PathBuf, RelPath)> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let rel = relativize(root, &abs)?;
    if is_excluded_rel(&rel, &scan.extra_excludes) {
        return None;
    }
    Some((abs, rel))
}

struct Storm;

fn expand_dir_bounded(
    root: &Path,
    dir: &Path,
    extra: &[String],
    cap: usize,
    out: &mut BTreeSet<RelPath>,
) -> Result<(), Storm> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let rd = match fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            let p = ent.path();
            let Some(rel) = relativize(root, &p) else {
                continue;
            };
            if is_excluded_rel(&rel, extra) {
                continue;
            }
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                out.insert(rel);
                if out.len() > cap {
                    return Err(Storm);
                }
            }
        }
    }
    Ok(())
}

enum Admit {
    Indexed,
    Missing,
    Skip,
}

/// Admission of a newly observed path into the watched set.
///
/// Delegates to [`scan::path_admission`] (WP-A2) so gitignore, size, and
/// binary gates match the indexer.
fn admit_new(root: &Path, rel: &RelPath, opts: &ScanOptions) -> Admit {
    match scan::path_admission(root, rel, opts) {
        scan::PathAdmission::Indexed => Admit::Indexed,
        scan::PathAdmission::Missing => Admit::Missing,
        scan::PathAdmission::Ignored
        | scan::PathAdmission::TooLarge
        | scan::PathAdmission::Binary => Admit::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::UNIX_EPOCH;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos();
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "astrolabe-watch-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            let path = fs::canonicalize(&path).expect("canonicalize test directory");
            TestDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create parent directory");
            }
            fs::write(&path, contents).expect("write test file");
        }

        fn abs(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_scan() -> ScanOptions {
        ScanOptions {
            respect_gitignore: false,
            audit_ignored: false,
            ..ScanOptions::default()
        }
    }

    fn watcher(dir: &TestDir) -> Watcher {
        // Jitter would make the first safety scan fire immediately with
        // probability STOP_SLICE/interval; keep tests deterministic.
        Watcher::new(dir.path())
            .scan_options(test_scan())
            .safety_jitter(false)
    }

    /// Bump mtime so tests do not depend on filesystem timestamp resolution.
    fn bump_mtime(path: &Path) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime bump");
        let old = file
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or_else(SystemTime::now);
        file.set_modified(old + Duration::from_secs(2))
            .expect("set_modified");
    }

    fn rewrite_and_bump(dir: &TestDir, rel: &str, contents: impl AsRef<[u8]>) {
        dir.write(rel, contents);
        bump_mtime(&dir.abs(rel));
    }

    #[test]
    fn detects_added_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        assert_eq!(w.tracked_files(), 1);

        dir.write("nested/b.rs", "fn b() {}");
        let cs = w.check();
        assert_eq!(cs.added, vec![RelPath::new("nested/b.rs")]);
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn detects_modified_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        rewrite_and_bump(&dir, "a.rs", "fn a() { 1 }");
        let cs = w.check();
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn detects_removed_file() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        dir.write("b.rs", "fn b() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        fs::remove_file(dir.abs("a.rs")).expect("remove");
        let cs = w.check();
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.modified.is_empty());
    }

    #[test]
    fn no_changes_returns_empty() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        assert!(w.check().is_empty());
        assert!(w.check().is_empty());
        assert_eq!(w.tracked_files(), 1);
    }

    #[test]
    fn changeset_order_is_deterministic() {
        let dir = TestDir::new();
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        dir.write("z.rs", "z");
        dir.write("a.rs", "a");
        dir.write("m.rs", "m");
        dir.write("nested/b.rs", "b");
        let cs = w.check();
        assert_eq!(
            cs.added,
            vec![
                RelPath::new("a.rs"),
                RelPath::new("m.rs"),
                RelPath::new("nested/b.rs"),
                RelPath::new("z.rs"),
            ]
        );
        let kinds: Vec<_> = cs
            .iter()
            .map(|(k, p)| (k, p.as_str().to_string()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (ChangeKind::Added, "a.rs".into()),
                (ChangeKind::Added, "m.rs".into()),
                (ChangeKind::Added, "nested/b.rs".into()),
                (ChangeKind::Added, "z.rs".into()),
            ]
        );
    }

    #[test]
    fn merge_coalesces_editor_bursts() {
        let mut pending = ChangeSet {
            added: vec![RelPath::new("new.rs")],
            modified: vec![RelPath::new("old.rs")],
            removed: Vec::new(),
        };
        pending.merge(ChangeSet {
            added: Vec::new(),
            modified: vec![RelPath::new("new.rs")],
            removed: vec![RelPath::new("old.rs"), RelPath::new("gone.rs")],
        });
        pending.merge(ChangeSet {
            added: vec![RelPath::new("gone.rs")],
            modified: Vec::new(),
            removed: vec![RelPath::new("new.rs")],
        });
        assert!(pending.added.is_empty(), "{pending:?}");
        assert_eq!(pending.modified, vec![RelPath::new("gone.rs")]);
        assert_eq!(pending.removed, vec![RelPath::new("old.rs")]);
    }

    #[test]
    fn debounce_collapses_rapid_events() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(40))
            .debounce(Duration::from_millis(180))
            .spawn();

        thread::sleep(Duration::from_millis(80));
        dir.write("a.rs", "a");
        thread::sleep(Duration::from_millis(50));
        dir.write("b.rs", "b");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("debounced changeset");
        assert_eq!(
            cs.added,
            vec![RelPath::new("a.rs"), RelPath::new("b.rs")],
            "{cs:?}"
        );
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());

        match rx.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("debounce leaked a second event: {other:?}"),
        }

        handle.stop();
    }

    #[test]
    fn background_thread_stops() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(10))
            .spawn();

        thread::sleep(Duration::from_millis(40));
        assert!(!handle.is_finished());
        handle.stop();

        let start = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    if start.elapsed() > Duration::from_secs(2) {
                        panic!("watch thread did not drop the sender after stop");
                    }
                }
                Ok(_) => {}
            }
        }
    }

    #[test]
    fn pending_changes_flushed_on_stop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(20))
            .debounce(Duration::from_secs(10)) // Long debounce so changes stay pending
            .spawn();

        thread::sleep(Duration::from_millis(50));
        dir.write("added.rs", "content");
        // Give time for poll to detect the file and put it in pending
        thread::sleep(Duration::from_millis(60));

        handle.stop();

        // The pending change should be flushed when stopping despite long debounce
        let cs = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("flushed changeset on stop");
        assert_eq!(cs.added, vec![RelPath::new("added.rs")]);
    }

    #[test]
    fn temporary_unreadable_file_handling() {
        let dir = TestDir::new();
        dir.write("test.rs", "initial content");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        let path = dir.abs("test.rs");

        // Simulate file becoming temporarily unreadable (000 permissions on unix).
        // `scan` drops ReadError paths from the watched set, so the file is
        // removed while unreadable and re-added when readable again — not a
        // confirm()-level "modified" cycle.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

            let cs1 = w.check();
            assert!(cs1.modified.is_empty());
            assert_eq!(cs1.removed, vec![RelPath::new("test.rs")]);

            let cs2 = w.check();
            assert!(
                cs2.is_empty(),
                "must not keep emitting while unreadable: {cs2:?}"
            );

            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

            let cs3 = w.check();
            assert!(cs3.modified.is_empty());
            assert_eq!(cs3.added, vec![RelPath::new("test.rs")]);
            assert!(cs3.removed.is_empty());
        }
    }

    #[test]
    fn confirm_settle_unreadable_to_readable_no_spurious_modified() {
        let dir = TestDir::new();
        dir.write("test.rs", "unchanged bytes");
        let rel = RelPath::new("test.rs");
        let abs = dir.abs("test.rs");
        let meta_fs = abs.metadata().expect("metadata");
        let mtime = meta_fs.modified().expect("mtime");
        let observed = Observed {
            mtime,
            len: meta_fs.len(),
            mtime_ok: true,
        };
        // Prior observe could not hash (unreadable); metadata unchanged.
        let old = FileStamp {
            mtime,
            len: meta_fs.len(),
            hash: None,
            unsettled: true,
            mtime_ok: true,
        };
        // Far enough in the future that mtime is outside the untrusted window.
        let now = mtime + MTIME_UNTRUSTED_WINDOW + Duration::from_secs(1);
        let (stamp, changed) = confirm(dir.path(), &rel, &old, &observed, now);
        assert!(
            !changed,
            "settling to a first hash with unchanged mtime/size must not report modified"
        );
        assert!(stamp.hash.is_some());
        assert!(!stamp.unsettled);
    }

    #[test]
    fn first_check_is_baseline_not_adds() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        dir.write("b.rs", "b");
        let mut w = watcher(&dir);
        let cs = w.check();
        assert!(cs.is_empty());
        assert_eq!(w.tracked_files(), 2);
        assert!(w.is_seeded());
    }

    #[test]
    fn same_size_rewrite_inside_untrusted_window_is_detected() {
        let dir = TestDir::new();
        dir.write("a.rs", "AAAA");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        // Same length, rewrite through the existing inode so a coarse
        // clock might keep the same mtime. The unsettled-window hash
        // must still see the new bytes.
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(dir.abs("a.rs"))
                .expect("reopen");
            f.write_all(b"BBBB").expect("rewrite");
            f.flush().expect("flush");
        }

        let cs = w.check();
        assert_eq!(
            cs.modified,
            vec![RelPath::new("a.rs")],
            "same-size rewrite must be visible even when mtime is coarse"
        );
    }

    #[test]
    fn event_mode_detects_added() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        dir.write("added.rs", "added content");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected added file");
        assert_eq!(cs.added, vec![RelPath::new("added.rs")]);
        handle.stop();
    }

    #[test]
    fn event_mode_detects_modified() {
        let dir = TestDir::new();
        dir.write("a.rs", "initial");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        rewrite_and_bump(&dir, "a.rs", "modified content");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected modified file");
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        handle.stop();
    }

    #[test]
    fn event_mode_detects_removed() {
        let dir = TestDir::new();
        dir.write("a.rs", "initial");
        let (rx, handle) = watcher(&dir).debounce(Duration::from_millis(50)).spawn();

        thread::sleep(Duration::from_millis(100));
        fs::remove_file(dir.abs("a.rs")).expect("remove");

        let cs = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("event mode detected removed file");
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        handle.stop();
    }

    #[test]
    fn forced_poll_mode_detects_change() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20))
            .spawn();

        thread::sleep(Duration::from_millis(50));
        dir.write("poll_new.rs", "content");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("poll mode detected added file");
        assert_eq!(cs.added, vec![RelPath::new("poll_new.rs")]);
        handle.stop();
    }

    struct ErrorFeed {
        yielded: bool,
    }

    impl EventFeed for ErrorFeed {
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.yielded {
                self.yielded = true;
                Ok(FeedWake::Error)
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    #[test]
    fn spawn_preserves_seeded_pre_spawn_changeset() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20));
        assert!(w.check().is_empty());

        // Change after seed, before spawn — must not be discarded.
        dir.write("between.rs", "between");
        let (rx, handle) = w.spawn();

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("seeded pre-spawn changeset");
        assert_eq!(cs.added, vec![RelPath::new("between.rs")], "{cs:?}");
        handle.stop();
    }

    struct OneShotFeed {
        fired: bool,
        path: PathBuf,
    }

    impl EventFeed for OneShotFeed {
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.fired {
                self.fired = true;
                Ok(FeedWake::Events {
                    paths: vec![self.path.clone()],
                    overflow: false,
                })
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    #[test]
    fn event_loop_flushes_pending_debounce_on_stop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let flushed = dir.abs("flushed.rs");
        let mut w = watcher(&dir).debounce(Duration::from_secs(60));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = OneShotFeed {
            fired: false,
            path: flushed,
        };

        let thread = thread::spawn(move || {
            let _ = event_loop(w, feed, (), tx, stop_thread);
        });

        // Wait until the one-shot event arms last_event inside the loop.
        thread::sleep(Duration::from_millis(80));
        dir.write("flushed.rs", "flush me");
        thread::sleep(Duration::from_millis(40));

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");

        let cs = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("pending debounce flushed on stop");
        assert_eq!(cs.added, vec![RelPath::new("flushed.rs")], "{cs:?}");
    }

    struct TimeoutFeed;

    impl EventFeed for TimeoutFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            // Honor the timeout so the loop does not busy-spin; event_loop
            // passes STOP_SLICE (50 ms) here.
            if !timeout.is_zero() {
                thread::sleep(timeout);
            }
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn watch_config_default_safety_interval_is_thirty_minutes() {
        let cfg = WatchConfig::default();
        assert_eq!(cfg.safety_interval, Duration::from_secs(1800));
        assert_eq!(cfg.storm_paths, 5000);
        assert!(cfg.safety_jitter);
        assert_eq!(
            WatchConfig::default()
                .safety_interval(Duration::from_secs(30))
                .safety_jitter(false)
                .safety_interval,
            Duration::from_secs(30)
        );
        assert!(!WatchConfig::default().safety_jitter(false).safety_jitter);
        assert_eq!(WatchConfig::default().storm_paths(7).storm_paths, 7);
    }

    #[test]
    fn safety_phase_offset_stays_inside_interval() {
        let interval = Duration::from_secs(1800);
        for seed in [0u64, 1, 42, u64::MAX] {
            let offset = safety_phase_offset(interval, seed);
            assert!(offset < interval, "seed={seed} offset={offset:?}");
        }
        assert_eq!(safety_phase_offset(Duration::ZERO, 1), Duration::ZERO);
        assert_eq!(
            safety_phase_offset(Duration::from_nanos(1), 0),
            Duration::ZERO
        );
    }

    #[test]
    fn distrust_caps_safety_wait_at_sixty_seconds() {
        assert_eq!(
            safety_wait(Duration::from_secs(1800), true),
            Duration::from_secs(60)
        );
        assert_eq!(
            safety_wait(Duration::from_millis(80), true),
            Duration::from_millis(80)
        );
        assert_eq!(safety_wait(Duration::ZERO, false), Duration::ZERO);
        assert_eq!(safety_wait(Duration::ZERO, true), Duration::from_secs(60));
        assert_eq!(
            safety_wait(Duration::from_secs(1800), false),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn event_loop_safety_interval_scans_without_os_events() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_millis(80))
            .debounce(Duration::from_secs(60));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let _ = event_loop(w, TimeoutFeed, (), tx, stop_thread);
        });

        // Let the loop seed, then write so only the safety scan (not the
        // startup check) can observe the new file.
        thread::sleep(Duration::from_millis(30));
        dir.write("late.rs", "late");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("configured safety interval should pick up a silent write");
        assert_eq!(cs.added, vec![RelPath::new("late.rs")], "{cs:?}");

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    fn event_feed_error_degrades_to_polling() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);

        let feed = ErrorFeed { yielded: false };
        let dummy_guard = ();

        let thread = thread::spawn(move || {
            let (watcher, ok) =
                event_loop(w, feed, dummy_guard, tx.clone(), Arc::clone(&stop_thread));
            assert!(!ok, "event_loop should report failure on backend error");
            run_loop(watcher, tx, stop_thread);
        });

        thread::sleep(Duration::from_millis(50));
        dir.write("fallback.rs", "fallback content");

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("polling fallback detected change");
        assert_eq!(cs.added, vec![RelPath::new("fallback.rs")]);

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join");
    }

    struct BurstFeed {
        paths: Vec<PathBuf>,
        overflow: bool,
        fired: bool,
    }

    impl EventFeed for BurstFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            if !self.fired {
                self.fired = true;
                return Ok(FeedWake::Events {
                    paths: std::mem::take(&mut self.paths),
                    overflow: self.overflow,
                });
            }
            if timeout.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            thread::sleep(timeout);
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn excluded_events_do_not_trigger_check() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        dir.write(".git/HEAD", "ref: refs/heads/main");
        dir.write("target/out.rs", "x");
        dir.write("node_modules/pkg/index.js", "x");
        dir.write(".astrolabe/store", "x");
        let mut scan = test_scan();
        scan.extra_excludes = vec!["scratch".into()];
        dir.write("scratch/tmp.rs", "x");

        let mut w = watcher(&dir)
            .scan_options(scan)
            .debounce(Duration::from_millis(30));
        assert!(w.check().is_empty());
        let seed_mtime = w
            .snapshot
            .get(&RelPath::new("seed.rs"))
            .expect("seed stamp")
            .mtime;
        let checks = Arc::clone(&w.check_calls);
        let path_checks = Arc::clone(&w.check_paths_calls);
        let before_full = checks.load(Ordering::Relaxed);
        let before_paths = path_checks.load(Ordering::Relaxed);

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = BurstFeed {
            paths: vec![
                dir.abs(".git/HEAD"),
                dir.abs("target/out.rs"),
                dir.abs("node_modules/pkg/index.js"),
                dir.abs(".astrolabe/store"),
                dir.abs("scratch/tmp.rs"),
            ],
            overflow: false,
            fired: false,
        };

        let thread = thread::spawn(move || event_loop(w, feed, (), tx, stop_thread));

        // Seed check inside event_loop, then excluded burst, then debounce.
        thread::sleep(Duration::from_millis(200));
        match rx.recv_timeout(Duration::from_millis(50)) {
            Err(RecvTimeoutError::Timeout) => {}
            other => panic!("excluded events leaked a changeset: {other:?}"),
        }

        stop.store(true, Ordering::SeqCst);
        let (w, _) = thread.join().expect("join event_loop");

        // event_loop always does one full check at start; excluded events
        // must not add another full check or a directed check.
        assert_eq!(
            w.check_calls.load(Ordering::Relaxed),
            before_full + 1,
            "excluded events must not trigger another full check"
        );
        assert_eq!(
            w.check_paths_calls.load(Ordering::Relaxed),
            before_paths,
            "excluded events must not trigger check_paths"
        );
        assert_eq!(
            w.snapshot
                .get(&RelPath::new("seed.rs"))
                .expect("seed stamp")
                .mtime,
            seed_mtime
        );
    }

    #[test]
    fn check_paths_detects_added() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        dir.write("nested/b.rs", "b");
        let cs = w.check_paths(&[dir.abs("nested/b.rs")]);
        assert_eq!(cs.added, vec![RelPath::new("nested/b.rs")]);
        assert!(cs.modified.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn check_paths_detects_modified() {
        let dir = TestDir::new();
        dir.write("a.rs", "fn a() {}");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        rewrite_and_bump(&dir, "a.rs", "fn a() { 1 }");
        let cs = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(cs.modified, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.removed.is_empty());
    }

    #[test]
    fn check_paths_detects_removed() {
        let dir = TestDir::new();
        dir.write("a.rs", "a");
        dir.write("b.rs", "b");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());
        fs::remove_file(dir.abs("a.rs")).expect("remove");
        let cs = w.check_paths(&[dir.abs("a.rs")]);
        assert_eq!(cs.removed, vec![RelPath::new("a.rs")]);
        assert!(cs.added.is_empty());
        assert!(cs.modified.is_empty());
    }

    #[test]
    fn check_paths_expands_directory_event() {
        let dir = TestDir::new();
        dir.write("sub/a.rs", "a");
        dir.write("sub/b.rs", "b");
        let mut w = watcher(&dir);
        assert!(w.check().is_empty());

        rewrite_and_bump(&dir, "sub/a.rs", "a2");
        fs::remove_file(dir.abs("sub/b.rs")).expect("remove");
        dir.write("sub/deep/c.rs", "c");

        let cs = w.check_paths(&[dir.abs("sub")]);
        assert_eq!(cs.added, vec![RelPath::new("sub/deep/c.rs")], "{cs:?}");
        assert_eq!(cs.modified, vec![RelPath::new("sub/a.rs")], "{cs:?}");
        assert_eq!(cs.removed, vec![RelPath::new("sub/b.rs")], "{cs:?}");
    }

    #[test]
    fn check_paths_storm_degrades_to_full_check() {
        let dir = TestDir::new();
        dir.write("keep.rs", "k");
        dir.write("seen.rs", "s");
        let mut w = watcher(&dir).storm_paths(3);
        assert!(w.check().is_empty());
        rewrite_and_bump(&dir, "keep.rs", "k2");
        dir.write("outside.rs", "o");

        let dummies: Vec<PathBuf> = (0..4).map(|i| dir.abs(&format!("nope{i}.rs"))).collect();
        let before_full = w.check_calls.load(Ordering::Relaxed);
        let cs = w.check_paths(&dummies);
        assert!(
            w.check_calls.load(Ordering::Relaxed) > before_full,
            "storm must run a full check"
        );
        assert!(
            cs.modified.contains(&RelPath::new("keep.rs")),
            "full check must see keep.rs, not in the dummy set: {cs:?}"
        );
        assert!(
            cs.added.contains(&RelPath::new("outside.rs")),
            "full check must see outside.rs: {cs:?}"
        );
    }

    #[test]
    fn overflow_distrust_fast_safety_then_clears() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_millis(80))
            .debounce(Duration::from_secs(60));
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        assert!(!log.distrusted());

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = BurstFeed {
            paths: Vec::new(),
            overflow: true,
            fired: false,
        };

        let thread = thread::spawn(move || {
            let _ = event_loop(w, feed, (), tx, stop_thread);
        });

        let start = Instant::now();
        while !log.distrusted() {
            if start.elapsed() > Duration::from_secs(2) {
                panic!("overflow did not set distrust");
            }
            thread::sleep(Duration::from_millis(5));
        }

        dir.write("late.rs", "late");
        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("fast safety after distrust should pick up a silent write");
        assert_eq!(cs.added, vec![RelPath::new("late.rs")], "{cs:?}");
        assert!(!log.distrusted(), "clean full check must clear distrust");

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    fn freshness_log_watermark_and_suspect_paths() {
        let log = FreshnessLog::new();
        let t0 = log.watermark();
        assert!(!log.distrusted());
        assert!(log.suspect_paths(t0).is_empty());

        thread::sleep(Duration::from_millis(2));
        log.note_paths(&[RelPath::new("b.rs"), RelPath::new("a.rs")]);
        assert_eq!(
            log.suspect_paths(t0),
            vec![RelPath::new("a.rs"), RelPath::new("b.rs")]
        );
        thread::sleep(Duration::from_millis(2));
        assert!(log.suspect_paths(Instant::now()).is_empty());

        log.note_distrust();
        assert!(log.distrusted());
        assert!(
            log.suspect_paths(t0).is_empty(),
            "distrusted log hides suspects"
        );

        thread::sleep(Duration::from_millis(2));
        log.note_verified();
        assert!(!log.distrusted());
        assert!(log.watermark() > t0);
        assert!(log.suspect_paths(t0).is_empty());
    }

    #[test]
    fn spawn_with_log_shares_freshness() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle, log) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_millis(30))
            .debounce(Duration::from_millis(20))
            .spawn_with_log();
        assert!(!log.distrusted());
        // Drop any baseline/empty noise, then stop.
        let _ = rx.recv_timeout(Duration::from_millis(50));
        handle.stop();
    }

    #[test]
    fn request_verification_advances_watermark_poll_loop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let (rx, handle, log) = watcher(&dir)
            .events(false)
            .poll_interval(Duration::from_secs(30))
            .debounce(Duration::ZERO)
            .safety_interval(Duration::ZERO)
            .safety_jitter(false)
            .spawn_with_log();

        // First check seeds baseline immediately; give it a moment.
        thread::sleep(Duration::from_millis(40));
        let wm = log.watermark();

        dir.write("late.rs", "fn late() {}");
        bump_mtime(&dir.abs("late.rs"));
        log.request_verification();

        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("on-demand check should emit late.rs without waiting poll_interval");
        assert!(
            cs.added.iter().any(|p| p.as_str() == "late.rs"),
            "expected late.rs added, got {cs:?}"
        );
        assert!(
            log.watermark() > wm,
            "request_verification must push the watermark"
        );
        handle.stop();
    }

    struct HangFeed;

    impl EventFeed for HangFeed {
        fn recv_timeout(&mut self, timeout: Duration) -> Result<FeedWake, RecvTimeoutError> {
            thread::sleep(timeout);
            Err(RecvTimeoutError::Timeout)
        }
    }

    #[test]
    fn request_verification_event_loop_runs_check() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir)
            .safety_interval(Duration::from_secs(3600))
            .debounce(Duration::from_secs(60))
            .safety_jitter(false);
        let _ = w.check();
        let log = Arc::clone(&w.freshness);
        let wm = log.watermark();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let _ = event_loop(w, HangFeed, (), tx, stop_thread);
        });

        thread::sleep(Duration::from_millis(20));
        dir.write("late.rs", "late");
        bump_mtime(&dir.abs("late.rs"));
        log.request_verification();
        let cs = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("event_loop should honor request_verification within STOP_SLICE");
        assert!(
            cs.added.iter().any(|p| p.as_str() == "late.rs"),
            "expected late.rs added, got {cs:?}"
        );
        assert!(log.watermark() > wm);

        stop.store(true, Ordering::SeqCst);
        thread.join().expect("join event_loop");
    }

    #[test]
    #[ignore]
    fn bench_check_on_serena() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../serena");
        assert!(
            root.is_dir(),
            "missing corpus {}: {}",
            root.display(),
            "expected ../serena next to the Astrolabe workspace"
        );

        let mut w = Watcher::new(&root);
        let t0 = Instant::now();
        let baseline = w.check();
        let baseline_dt = t0.elapsed();
        assert!(baseline.is_empty());
        let files = w.tracked_files();

        let mut times = Vec::with_capacity(8);
        for _ in 0..8 {
            let t = Instant::now();
            let cs = w.check();
            times.push(t.elapsed());
            assert!(cs.is_empty(), "serena mutated during the bench: {cs:?}");
        }
        times.sort();
        println!(
            "serena files={files} baseline={baseline_dt:?} \
             check min/median/max {:?}/{:?}/{:?}",
            times[0],
            times[times.len() / 2],
            times[times.len() - 1],
        );
    }
}
