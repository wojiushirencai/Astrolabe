//! Hybrid file watcher for incremental re-index: OS events by default, polling fallback.
//!
//! The index is a snapshot. After an editor save the graph is stale until
//! something rebuilds it. This module is that something: it detects filesystem
//! changes and diffs `mtime` + size against the previous snapshot via
//! [`crate::scan::scan`] so the watched set matches the indexer (gitignore,
//! default excludes, size/binary gates).
//!
//! ## Architecture: Native Events + Snapshot Diff + Polling Fallback
//!
//! 1. **Native OS events as wake-up signal**: We register a [`notify::RecommendedWatcher`]
//!    recursively on the workspace root (FSEvents on macOS, inotify on Linux,
//!    ReadDirectoryChangesW on Windows). When files are saved, the OS wakes our
//!    background thread immediately.
//! 2. **Debounce & coalesce**: Rapid file saves (e.g. `git checkout` or multi-file
//!    editor saves) are debounced (default 500 ms quiet period) so bursts of
//!    event notifications collapse into a single snapshot diff.
//! 3. **Invariant: watched set == indexed set**: Event notifications only act as
//!    a wake-up trigger. When debounced, [`Watcher::check`] runs [`crate::scan::scan`]
//!    to diff against the last snapshot. This guarantees that `.gitignore`,
//!    excluded directories, and binary/size limits are applied consistently.
//! 4. **Safety net**: A slow periodic check (every 10 minutes) runs even during
//!    complete event silence to guard against any dropped or lost OS events.
//! 5. **Automatic polling fallback**: If the OS notification system fails to
//!    initialize or encounters an error at runtime (e.g. exhausted inotify watches),
//!    the watcher automatically logs a warning and falls back to polling at
//!    [`WatchConfig::poll_interval`] (default 10 s).
//!
//! ## Cost model
//!
//! In event mode, idle CPU is 0.0% because the background thread sleeps on
//! channel `recv_timeout`. When changes settle, a single `check` walks the tree.
//! A file is hashed only when it looks different (`mtime`/size) or is
//! still inside the mtime-precision window.
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
//! let (rx, handle) = astrolabe_core::watch::Watcher::new(root)
//!     .config(astrolabe_core::watch::WatchConfig::default())
//!     .spawn();
//! while let Ok(changes) = rx.recv() {
//!     // one changeset per quiet period
//! }
//! handle.stop();
//! ```

use crate::scan::{self, ScanOptions};
use crate::types::RelPath;
use notify::{RecursiveMode, Watcher as _};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// Files whose mtime is newer than this are not trusted without a hash.
///
/// FAT timestamps are 2 s; most other coarse clocks are 1 s. A 2 s window
/// covers both, plus a little clock skew.
pub const MTIME_UNTRUSTED_WINDOW: Duration = Duration::from_secs(2);

const STOP_SLICE: Duration = Duration::from_millis(50);
const EVENT_SAFETY_INTERVAL: Duration = Duration::from_secs(600);
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// Polling cadence and emit delay for [`Watcher::spawn`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Cadence for the polling fallback when native OS events are disabled or unavailable.
    pub poll_interval: Duration,
    /// Quiet period before a pending [`ChangeSet`] is sent. Rapid editor
    /// saves collapse into one event.
    pub debounce: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            poll_interval: Duration::from_secs(10),
            debounce: Duration::from_millis(500),
        }
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
/// Holds the last snapshot; [`check`] returns the delta. Prefer [`spawn`] for
/// a background thread that debounces and pushes [`ChangeSet`]s on a channel.
pub struct Watcher {
    root: PathBuf,
    scan: ScanOptions,
    config: WatchConfig,
    snapshot: BTreeMap<RelPath, FileStamp>,
    seeded: bool,
    events: bool,
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
    pub fn check(&mut self) -> ChangeSet {
        let now = SystemTime::now();
        let observed = observe_tree(&self.root, &self.scan);

        if !self.seeded {
            self.snapshot = stamps_from_observed(&self.root, &observed, now);
            self.seeded = true;
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
        changes
    }

    /// Run `check` on a background thread and send debounced changesets.
    ///
    /// By default, registers native OS filesystem notifications (`notify`) and
    /// falls back to polling at [`WatchConfig::poll_interval`] if native watching
    /// fails.
    ///
    /// The thread is daemon-like: dropping the [`WatchHandle`] signals stop
    /// **without joining**, so it cannot hold the process open. Call
    /// [`WatchHandle::stop`] for a graceful join.
    pub fn spawn(self) -> (Receiver<ChangeSet>, WatchHandle) {
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

trait EventFeed {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Result<(), ()>, RecvTimeoutError>;
}

impl EventFeed for Receiver<Result<notify::Event, notify::Error>> {
    fn recv_timeout(&mut self, timeout: Duration) -> Result<Result<(), ()>, RecvTimeoutError> {
        match Receiver::recv_timeout(self, timeout) {
            Ok(Ok(_)) => Ok(Ok(())),
            Ok(Err(_)) => Ok(Err(())),
            Err(e) => Err(e),
        }
    }
}

/// Cap queued OS events so a burst cannot grow the channel without bound.
/// Overflow drops events; debounce coalescing and the safety-interval scan recover.
const NOTIFY_CHANNEL_CAP: usize = 1024;

fn init_notify(
    root: &Path,
) -> Option<(
    Receiver<Result<notify::Event, notify::Error>>,
    notify::RecommendedWatcher,
)> {
    let (tx, rx) = mpsc::sync_channel(NOTIFY_CHANNEL_CAP);
    let mut watcher = match notify::recommended_watcher(move |res| {
        match tx.try_send(res) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                // Drain-on-overflow at the sender: drop; receiver + safety poll catch up.
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
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
    Some((rx, watcher))
}

fn event_loop<F: EventFeed, G>(
    mut watcher: Watcher,
    mut feed: F,
    guard: G,
    tx: mpsc::Sender<ChangeSet>,
    stop: Arc<AtomicBool>,
) -> (Watcher, bool) {
    let debounce = watcher.config.debounce;
    let mut last_event: Option<Instant> = None;
    let mut last_safety = Instant::now();

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
            Ok(Ok(())) => {
                last_event = Some(Instant::now());
                // Drain coalesced events so a bounded channel does not stay full.
                loop {
                    match feed.recv_timeout(Duration::ZERO) {
                        Ok(Ok(())) => {}
                        Ok(Err(())) => {
                            drop(guard);
                            return (watcher, false);
                        }
                        Err(_) => break,
                    }
                }
            }
            Ok(Err(())) => {
                drop(guard);
                return (watcher, false);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                drop(guard);
                return (watcher, false);
            }
        }

        let quiet = last_event.map(|t| t.elapsed() >= debounce).unwrap_or(false);
        let safety_due = last_safety.elapsed() >= EVENT_SAFETY_INTERVAL;

        if quiet || safety_due {
            last_event = None;
            last_safety = Instant::now();
            let batch = watcher.check();
            if !batch.is_empty() && tx.send(batch).is_err() {
                break;
            }
        }
    }

    // Flush pending debounce on stop, matching run_loop's pending flush.
    if last_event.is_some() {
        let batch = watcher.check();
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

        if wait_or_stop(&stop, poll_interval) {
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

fn wait_or_stop(stop: &AtomicBool, total: Duration) -> bool {
    let start = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
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
        Watcher::new(dir.path()).scan_options(test_scan())
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
            assert!(cs2.is_empty(), "must not keep emitting while unreadable: {cs2:?}");

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
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<Result<(), ()>, RecvTimeoutError> {
            if !self.yielded {
                self.yielded = true;
                Ok(Err(()))
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
    }

    impl EventFeed for OneShotFeed {
        fn recv_timeout(&mut self, _timeout: Duration) -> Result<Result<(), ()>, RecvTimeoutError> {
            if !self.fired {
                self.fired = true;
                Ok(Ok(()))
            } else {
                Err(RecvTimeoutError::Timeout)
            }
        }
    }

    #[test]
    fn event_loop_flushes_pending_debounce_on_stop() {
        let dir = TestDir::new();
        dir.write("seed.rs", "seed");
        let mut w = watcher(&dir).debounce(Duration::from_secs(60));
        let _ = w.check();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let feed = OneShotFeed { fired: false };

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
