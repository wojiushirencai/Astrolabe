//! Language-server lifecycle pool.
//!
//! Constraint 1 of the parent module is the reason this file exists. Measured
//! on Serena: four whole-repo reference searches grew the language-server
//! subprocess from 186 MB to 719 MB, and 94% of that growth lived outside the
//! host, where `GOMEMLIMIT` / a moka weigher cannot see it. Nothing ever
//! reclaimed the process. This pool is the missing trigger: servers start on
//! the first request, and a daemon thread shuts them down when they sit idle
//! or grow past a resident-set budget.
//!
//! # State machine (per language)
//!
//! ```text
//!          start ok                    checkout
//!   Empty ──────────► Live ◄──────────────────────────► Busy
//!     ▲                │  ▲         last guard dropped     │
//!     │     idle TTL   │  │                                │
//!     │     or memory  │  └──────── crash / shutdown ──────┘
//!     └────────────────┘
//!          start fail
//!   Empty ──────────► Failed ── cooldown elapsed ──► retry start
//! ```
//!
//! `Failed` is not permanent: a missing binary is cached for
//! [`LspPoolConfig::start_cooldown`] so we do not pay a slow spawn on every
//! query, then retried so installing the server later starts working.
//!
//! # Why reclaim cannot interrupt an in-flight request
//!
//! The monitor thread never calls [`LanguageServer::shutdown`] while a
//! checkout is held. Per-language state lives under one mutex; `in_flight`
//! (the number of live [`ServerGuard`]s for the current generation) is only
//! mutated while that mutex is held.
//!
//! * **Checkout** (`acquire`): lock → ensure a live server → `in_flight += 1`
//!   → clone the `Arc` → unlock. The request then runs against the `Arc`
//!   with no pool lock held.
//! * **Release** (guard `Drop`): lock → if `generation` still matches,
//!   `in_flight -= 1` and refresh `last_used` → unlock.
//! * **Reclaim**: lock → if `in_flight > 0`, skip → otherwise take the
//!   `Arc` out of the slot and bump `generation` → unlock → *then* call
//!   `shutdown`. A concurrent `acquire` either runs before the take (and
//!   bumps `in_flight`, so reclaim skips) or after it (and starts a new
//!   server). There is no window where `in_flight` is zero on paper but a
//!   guard still exists: creating a guard and incrementing the counter are
//!   the same critical section.
//!
//! `memory_bytes()` and `shutdown()` run *outside* the slot lock so a slow
//! `/proc` read or shutdown handshake cannot stall a checkout. After the
//! unlocked sample, reclaim re-locks and re-checks `generation`,
//! `in_flight`, idle time, and the memory predicate before taking the
//! server. A request that arrived in between either makes `in_flight > 0`
//! (skip) or refreshes `last_used` (idle no longer holds).
//!
//! Crash handling is the one path that retires a server with `in_flight > 0`:
//! the process is already dead. Reclaim still will not shut down a healthy
//! busy server.
//!
//! # Integration
//!
//! `transport` and `discovery` are separate workstreams. This module talks
//! to them only through [`DiscoverServers`], [`SpawnServer`], and
//! [`ServerFactory`]. Production wiring is [`CompositeFactory`]; tests inject
//! a fake via [`LspPool::with_factory`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{LanguageServer, LspError, ServerSpec};
use crate::types::Language;

/// Idle time-to-idle before an unused server is shut down.
pub const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(5 * 60);

/// Steady-state resident-set ceiling for one language-server process tree.
///
/// Measured against `rust-analyzer`, the hungriest server we target: it peaks
/// above 1 GiB while building its initial index on a mid-size repo, so a
/// lower ceiling reclaims it before it can answer anything.
pub const DEFAULT_MEMORY_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Ceiling that applies even while the server is still indexing.
///
/// Indexing is a transient peak, so [`DEFAULT_MEMORY_LIMIT_BYTES`] is waived
/// until the server reports it has finished. This bound is not waived: it is
/// what stops a runaway server from taking the machine with it.
pub const DEFAULT_MEMORY_HARD_LIMIT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// How long to wait for a newly started server to finish indexing.
///
/// `rust-analyzer` needs roughly 25 s on a small crate and longer on a large
/// one, most of it spent fetching and building the crate graph.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(120);

/// Wait after a failed start before trying again.
pub const DEFAULT_START_COOLDOWN: Duration = Duration::from_secs(60);

/// How often the daemon thread evaluates reclaim predicates.
pub const DEFAULT_MONITOR_INTERVAL: Duration = Duration::from_secs(10);

/// How to find a [`ServerSpec`] for a language.
///
/// Implemented by the `discovery` workstream. A missing binary must surface
/// as [`LspError::Unavailable`], not as a spawn attempt.
pub trait DiscoverServers: Send + Sync {
    fn discover(&self, language: Language) -> Result<ServerSpec, LspError>;
}

/// How to launch a running server from a [`ServerSpec`].
///
/// Implemented by the `transport` workstream. The pool never names a
/// concrete transport type; it only stores `Box<dyn LanguageServer>`.
pub trait SpawnServer: Send + Sync {
    /// `root` is the workspace the server should index; LSP `initialize`
    /// requires a `rootUri`, so it has to travel with the spawn request
    /// rather than being captured by the implementor.
    fn spawn(&self, spec: &ServerSpec, root: &Path) -> Result<Box<dyn LanguageServer>, LspError>;
}

/// Combined factory used by the pool.
///
/// Tests inject a fake. Production wraps discovery + transport with
/// [`CompositeFactory`].
pub trait ServerFactory: Send + Sync {
    fn start(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError>;
}

/// Production wiring: discover a server for the language, then spawn it
/// against the workspace root.
pub struct CompositeFactory<D, T> {
    pub discovery: D,
    pub transport: T,
    /// Workspace the spawned servers should index.
    pub root: PathBuf,
}

impl<D, T> CompositeFactory<D, T> {
    pub fn new(discovery: D, transport: T, root: impl Into<PathBuf>) -> Self {
        CompositeFactory {
            discovery,
            transport,
            root: root.into(),
        }
    }
}

impl<D, T> ServerFactory for CompositeFactory<D, T>
where
    D: DiscoverServers,
    T: SpawnServer,
{
    fn start(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError> {
        let spec = self.discovery.discover(language)?;
        self.transport.spawn(&spec, &self.root)
    }
}

/// Lifecycle thresholds. `Duration::ZERO` / `0` disables the matching
/// reclaim or retry delay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspPoolConfig {
    /// Time-to-idle. `ZERO` disables idle reclaim.
    pub idle_ttl: Duration,
    /// Per-server RSS ceiling. `0` disables memory reclaim.
    /// How long [`LspPool::acquire`] waits for a freshly started server to
    /// finish indexing. `ZERO` disables the wait.
    pub ready_timeout: Duration,
    /// Steady-state ceiling, waived while the server is still indexing.
    pub memory_limit_bytes: u64,
    /// Ceiling enforced at all times, including during indexing. `0`
    /// disables it.
    pub memory_hard_limit_bytes: u64,
    /// After a failed start, do not retry until this elapses. `ZERO` retries
    /// on every request.
    pub start_cooldown: Duration,
    /// Daemon tick. `ZERO` does not spawn a monitor; the host drives
    /// [`LspPool::reclaim`] itself.
    pub monitor_interval: Duration,
}

impl Default for LspPoolConfig {
    fn default() -> Self {
        LspPoolConfig {
            idle_ttl: DEFAULT_IDLE_TTL,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT_BYTES,
            memory_hard_limit_bytes: DEFAULT_MEMORY_HARD_LIMIT_BYTES,
            start_cooldown: DEFAULT_START_COOLDOWN,
            monitor_interval: DEFAULT_MONITOR_INTERVAL,
        }
    }
}

/// One pool of language servers, typically one per workspace.
///
/// The struct is `Send + Sync`. The monitor thread is a daemon: dropping the
/// pool signals it and detaches, so process exit is not blocked on the next
/// tick. [`LspPool::stop_monitor`] joins it for tests and clean shutdown.
pub struct LspPool {
    inner: Arc<Inner>,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

struct Inner {
    factory: Arc<dyn ServerFactory>,
    config: LspPoolConfig,
    slots: Mutex<HashMap<Language, Arc<Slot>>>,
    stop: AtomicBool,
    park: Park,
}

struct Park {
    mutex: Mutex<()>,
    cv: Condvar,
}

struct Slot {
    state: Mutex<SlotState>,
}

struct SlotState {
    server: Option<Arc<dyn LanguageServer>>,
    in_flight: usize,
    generation: u64,
    last_used: Instant,
    start_failure: Option<StartFailure>,
}

struct StartFailure {
    at: Instant,
    kind: StartFailureKind,
}

enum StartFailureKind {
    Unavailable(String),
    Startup(String),
    Timeout(Duration),
    Protocol(String),
    Crashed,
}

/// Checkout of a running server. Dropping it releases the in-flight count
/// that reclaim consults. Implements [`LanguageServer`] so a crash can
/// retire the slot without the caller doing extra bookkeeping.
#[must_use = "the guard keeps the server checked out; dropping it releases the checkout"]
pub struct ServerGuard {
    pool: Weak<Inner>,
    slot: Arc<Slot>,
    server: Arc<dyn LanguageServer>,
    language: Language,
    generation: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl StartFailure {
    fn from_error(err: &LspError) -> Self {
        let kind = match err {
            LspError::Unavailable(_, message) => StartFailureKind::Unavailable(message.clone()),
            LspError::Startup(_, message) => StartFailureKind::Startup(message.clone()),
            LspError::Timeout(duration) => StartFailureKind::Timeout(*duration),
            LspError::Protocol(message) => StartFailureKind::Protocol(message.clone()),
            // Not a startup condition, but it has to map somewhere; treating
            // it as a protocol failure keeps the cooldown honest if a server
            // somehow answers `initialize` this way.
            LspError::ContentModified => StartFailureKind::Protocol(err.to_string()),
            LspError::Crashed => StartFailureKind::Crashed,
        };
        StartFailure {
            at: Instant::now(),
            kind,
        }
    }

    fn to_error(&self, language: Language) -> LspError {
        match &self.kind {
            StartFailureKind::Unavailable(message) => {
                LspError::Unavailable(language, message.clone())
            }
            StartFailureKind::Startup(message) => LspError::Startup(language, message.clone()),
            StartFailureKind::Timeout(duration) => LspError::Timeout(*duration),
            StartFailureKind::Protocol(message) => LspError::Protocol(message.clone()),
            StartFailureKind::Crashed => LspError::Crashed,
        }
    }
}

impl LspPool {
    /// Build a pool around an injected factory and the default thresholds.
    pub fn with_factory(factory: impl ServerFactory + 'static) -> Self {
        Self::with_factory_and_config(factory, LspPoolConfig::default())
    }

    /// Build a pool around an injected factory and explicit thresholds.
    pub fn with_factory_and_config(
        factory: impl ServerFactory + 'static,
        config: LspPoolConfig,
    ) -> Self {
        let inner = Arc::new(Inner {
            factory: Arc::new(factory),
            config,
            slots: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            park: Park {
                mutex: Mutex::new(()),
                cv: Condvar::new(),
            },
        });
        let monitor = spawn_monitor(Arc::clone(&inner));
        LspPool {
            inner,
            monitor: Mutex::new(monitor),
        }
    }

    /// Production constructor: `discovery` finds a [`ServerSpec`],
    /// `transport` turns it into a [`LanguageServer`].
    pub fn from_discovery_and_transport(
        discovery: impl DiscoverServers + 'static,
        transport: impl SpawnServer + 'static,
        root: impl Into<PathBuf>,
        config: LspPoolConfig,
    ) -> Self {
        Self::with_factory_and_config(CompositeFactory::new(discovery, transport, root), config)
    }

    /// Check out the server for `language`, starting it if needed.
    ///
    /// The server is not reclaimed while the returned guard is alive.
    pub fn acquire(&self, language: Language) -> Result<ServerGuard, LspError> {
        let guard = self.inner.acquire(language)?;
        // Outside the slot lock, and with the guard already holding the
        // server checked out: waiting here blocks only this caller, not the
        // other languages or the reclaim monitor.
        if !self.inner.config.ready_timeout.is_zero()
            && !guard.wait_until_ready(self.inner.config.ready_timeout)
        {
            tracing::warn!(
                language = language.name(),
                timeout_ms = self.inner.config.ready_timeout.as_millis() as u64,
                "language server still indexing after timeout; answers may be incomplete"
            );
        }
        Ok(guard)
    }

    /// Run `f` against a checked-out server. The checkout is released even
    /// if `f` panics. [`LspError::Crashed`] retires the slot so the next
    /// call starts a new process.
    pub fn with_server<T>(
        &self,
        language: Language,
        f: impl FnOnce(&dyn LanguageServer) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        let guard = self.acquire(language)?;
        match f(&guard) {
            Err(LspError::Crashed) => {
                guard.retire_crashed();
                Err(LspError::Crashed)
            }
            other => other,
        }
    }

    /// Evaluate idle and memory predicates on every resident server.
    ///
    /// Safe to call from any thread. In-flight servers are left untouched.
    pub fn reclaim(&self) {
        self.inner.reclaim_all();
    }

    /// Ask the monitor to exit. Does not wait; see [`stop_monitor`](Self::stop_monitor).
    pub fn request_stop(&self) {
        self.inner.request_stop();
    }

    /// Signal the monitor and join it. After this, reclaim only happens when
    /// the host calls [`reclaim`](Self::reclaim) (or a last checkout drops
    /// over the memory budget).
    pub fn stop_monitor(&self) {
        self.request_stop();
        if let Some(thread) = lock(&self.monitor).take() {
            let _ = thread.join();
        }
    }

    /// True once the monitor has exited or was never started.
    pub fn monitor_is_finished(&self) -> bool {
        lock(&self.monitor)
            .as_ref()
            .map(JoinHandle::is_finished)
            .unwrap_or(true)
    }

    /// Stop the monitor and shut down every resident server, including those
    /// with in-flight checkouts. Use this on workspace teardown; ordinary
    /// reclaim never takes this path.
    pub fn shutdown(&self) {
        self.stop_monitor();
        self.inner.shutdown_all(true);
    }

    /// Whether a live process currently occupies the slot for `language`.
    pub fn is_resident(&self, language: Language) -> bool {
        self.inner.is_resident(language)
    }
}

impl Drop for LspPool {
    fn drop(&mut self) {
        self.request_stop();
        // Detach: dropping JoinHandle without join must not block process
        // exit. The thread notices the flag on the next wait (or immediately
        // via the condvar) and then drops its Arc<Inner>, which shuts down
        // leftover servers.
        let _ = lock(&self.monitor).take();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown_all(true);
    }
}

impl Inner {
    fn request_stop(&self) {
        // Store-then-notify under the park mutex so a waiter cannot miss the
        // signal between its stop check and `wait_timeout`.
        let _guard = lock(&self.park.mutex);
        self.stop.store(true, Ordering::SeqCst);
        self.park.cv.notify_all();
    }

    fn slot_for(&self, language: Language) -> Arc<Slot> {
        let mut slots = lock(&self.slots);
        slots
            .entry(language)
            .or_insert_with(|| {
                Arc::new(Slot {
                    state: Mutex::new(SlotState {
                        server: None,
                        in_flight: 0,
                        generation: 0,
                        last_used: Instant::now(),
                        start_failure: None,
                    }),
                })
            })
            .clone()
    }

    fn acquire(self: &Arc<Self>, language: Language) -> Result<ServerGuard, LspError> {
        let slot = self.slot_for(language);
        let mut state = lock(&slot.state);

        if let Some(server) = state.server.clone() {
            state.in_flight += 1;
            state.last_used = Instant::now();
            let generation = state.generation;
            drop(state);
            return Ok(ServerGuard {
                pool: Arc::downgrade(self),
                slot,
                server,
                language,
                generation,
            });
        }

        if let Some(failure) = &state.start_failure {
            let cooled = !self.config.start_cooldown.is_zero()
                && failure.at.elapsed() < self.config.start_cooldown;
            if cooled {
                tracing::debug!(
                    language = language.name(),
                    elapsed_ms = failure.at.elapsed().as_millis() as u64,
                    cooldown_ms = self.config.start_cooldown.as_millis() as u64,
                    "skipping language-server start; cooldown active"
                );
                return Err(failure.to_error(language));
            }
        }

        // Start while holding the slot lock so two first-time callers cannot
        // launch two processes. Other languages use other slots and proceed.
        match self.factory.start(language) {
            Ok(boxed) => {
                let server: Arc<dyn LanguageServer> = Arc::from(boxed);
                state.generation += 1;
                state.in_flight = 1;
                state.last_used = Instant::now();
                state.start_failure = None;
                state.server = Some(Arc::clone(&server));
                let generation = state.generation;
                tracing::info!(
                    language = language.name(),
                    generation,
                    "started language server on demand"
                );
                drop(state);
                Ok(ServerGuard {
                    pool: Arc::downgrade(self),
                    slot,
                    server,
                    language,
                    generation,
                })
            }
            Err(err) => {
                state.start_failure = Some(StartFailure::from_error(&err));
                state.server = None;
                tracing::warn!(
                    language = language.name(),
                    error = %err,
                    cooldown_ms = self.config.start_cooldown.as_millis() as u64,
                    "language server failed to start; further attempts cooled down"
                );
                Err(err)
            }
        }
    }

    fn reclaim_all(&self) {
        let snapshot: Vec<(Language, Arc<Slot>)> = {
            let slots = lock(&self.slots);
            slots
                .iter()
                .map(|(language, slot)| (*language, Arc::clone(slot)))
                .collect()
        };
        for (language, slot) in snapshot {
            self.try_reclaim(language, &slot);
        }
    }

    fn try_reclaim(&self, language: Language, slot: &Slot) {
        let peeked = {
            let state = lock(&slot.state);
            let Some(server) = state.server.clone() else {
                return;
            };
            if state.in_flight > 0 {
                tracing::trace!(
                    language = language.name(),
                    in_flight = state.in_flight,
                    "skipping reclaim; server in use"
                );
                return;
            }
            Peeked {
                server,
                generation: state.generation,
                idle_for: state.last_used.elapsed(),
            }
        };

        let memory = peeked.server.memory_bytes();
        let busy = peeked.server.is_busy();
        if !self.should_reclaim(peeked.idle_for, memory, busy) {
            return;
        }

        let taken = {
            let mut state = lock(&slot.state);
            if state.generation != peeked.generation || state.in_flight > 0 {
                return;
            }
            if state.server.is_none() {
                return;
            }
            if !self.should_reclaim(state.last_used.elapsed(), memory, busy) {
                return;
            }
            let idle_for = state.last_used.elapsed();
            let server = state.server.take();
            state.generation += 1;
            state.in_flight = 0;
            (server, idle_for)
        };

        let Some(server) = taken.0 else {
            return;
        };
        let idle_for = taken.1;
        let reason = reclaim_reason(&self.config, idle_for, memory, busy);
        let memory_before = memory.unwrap_or(0);
        tracing::info!(
            language = language.name(),
            reason,
            idle_ms = idle_for.as_millis() as u64,
            memory_before_bytes = memory_before,
            memory_after_bytes = 0u64,
            memory_limit_bytes = self.config.memory_limit_bytes,
            idle_ttl_ms = self.config.idle_ttl.as_millis() as u64,
            "reclaiming language server"
        );
        match server.shutdown() {
            Ok(()) => tracing::info!(
                language = language.name(),
                reason,
                memory_before_bytes = memory_before,
                memory_after_bytes = 0u64,
                "language server shut down"
            ),
            Err(err) => tracing::warn!(
                language = language.name(),
                reason,
                error = %err,
                "language server shutdown failed during reclaim"
            ),
        }
    }

    fn should_reclaim(&self, idle_for: Duration, memory: Option<u64>, busy: bool) -> bool {
        let idle_hit = !self.config.idle_ttl.is_zero() && idle_for >= self.config.idle_ttl;
        idle_hit || memory_over_budget(&self.config, memory, busy)
    }

    fn shutdown_all(&self, force: bool) {
        let snapshot: Vec<(Language, Arc<Slot>)> = {
            let slots = lock(&self.slots);
            slots
                .iter()
                .map(|(language, slot)| (*language, Arc::clone(slot)))
                .collect()
        };
        for (language, slot) in snapshot {
            let server = {
                let mut state = lock(&slot.state);
                if !force && state.in_flight > 0 {
                    continue;
                }
                let server = state.server.take();
                if server.is_some() {
                    state.generation += 1;
                    state.in_flight = 0;
                }
                server
            };
            if let Some(server) = server {
                tracing::info!(
                    language = language.name(),
                    force,
                    "shutting down language server (pool teardown)"
                );
                if let Err(err) = server.shutdown() {
                    tracing::warn!(
                        language = language.name(),
                        error = %err,
                        "language server shutdown failed during pool teardown"
                    );
                }
            }
        }
    }

    fn is_resident(&self, language: Language) -> bool {
        let slot = {
            let slots = lock(&self.slots);
            slots.get(&language).cloned()
        };
        match slot {
            Some(slot) => lock(&slot.state).server.is_some(),
            None => false,
        }
    }
}

struct Peeked {
    server: Arc<dyn LanguageServer>,
    generation: u64,
    idle_for: Duration,
}

/// Whether `memory` is over the ceiling that currently applies.
///
/// While the server is indexing only the hard limit applies. Enforcing the
/// steady-state limit there would kill every server that is merely at its
/// transient peak, and it would never get far enough to answer a query.
fn memory_over_budget(config: &LspPoolConfig, memory: Option<u64>, busy: bool) -> bool {
    let Some(bytes) = memory else {
        return false;
    };
    let limit = if busy {
        config.memory_hard_limit_bytes
    } else {
        config.memory_limit_bytes
    };
    limit > 0 && bytes >= limit
}

fn reclaim_reason(
    config: &LspPoolConfig,
    idle_for: Duration,
    memory: Option<u64>,
    busy: bool,
) -> &'static str {
    let memory_hit = memory_over_budget(config, memory, busy);
    let idle_hit = !config.idle_ttl.is_zero() && idle_for >= config.idle_ttl;
    match (memory_hit, idle_hit) {
        (true, true) => "idle+memory",
        (true, false) => "memory",
        (false, true) => "idle",
        (false, false) => "none",
    }
}

fn spawn_monitor(inner: Arc<Inner>) -> Option<JoinHandle<()>> {
    if inner.config.monitor_interval.is_zero() {
        return None;
    }
    let interval = inner.config.monitor_interval;
    match thread::Builder::new()
        .name("astrolabe-lsp-pool".into())
        .spawn(move || monitor_loop(inner, interval))
    {
        Ok(handle) => Some(handle),
        Err(err) => {
            tracing::error!(error = %err, "failed to spawn language-server pool monitor");
            None
        }
    }
}

fn monitor_loop(inner: Arc<Inner>, interval: Duration) {
    while !inner.stop.load(Ordering::SeqCst) {
        if park_or_stop(&inner, interval) {
            break;
        }
        inner.reclaim_all();
    }
}

fn park_or_stop(inner: &Inner, interval: Duration) -> bool {
    let mutex_guard = lock(&inner.park.mutex);
    if inner.stop.load(Ordering::SeqCst) {
        return true;
    }
    match inner.park.cv.wait_timeout(mutex_guard, interval) {
        Ok(_) => {}
        Err(poisoned) => {
            drop(poisoned.into_inner());
        }
    }
    inner.stop.load(Ordering::SeqCst)
}

impl ServerGuard {
    fn retire_crashed(&self) {
        let server = {
            let mut state = lock(&self.slot.state);
            if state.generation != self.generation {
                return;
            }
            let Some(server) = state.server.take() else {
                return;
            };
            state.generation += 1;
            state.in_flight = 0;
            server
        };
        tracing::warn!(
            language = self.language.name(),
            "language server crashed; will restart on next request"
        );
        if let Err(err) = server.shutdown() {
            tracing::debug!(
                language = self.language.name(),
                error = %err,
                "shutdown after crash returned an error"
            );
        }
    }

    fn call<T>(
        &self,
        op: impl FnOnce(&dyn LanguageServer) -> Result<T, LspError>,
    ) -> Result<T, LspError> {
        match op(&*self.server) {
            Err(LspError::Crashed) => {
                self.retire_crashed();
                Err(LspError::Crashed)
            }
            other => other,
        }
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let last_of_generation = {
            let mut state = lock(&self.slot.state);
            if state.generation != self.generation {
                return;
            }
            state.in_flight = state.in_flight.saturating_sub(1);
            state.last_used = Instant::now();
            state.in_flight == 0 && state.server.is_some()
        };
        // Opportunistic memory reclaim so a just-finished heavy query does
        // not wait for the next monitor tick. Idle reclaim still belongs to
        // the monitor: last_used was just refreshed, so idle cannot fire here.
        if last_of_generation {
            if let Some(pool) = self.pool.upgrade() {
                pool.try_reclaim(self.language, &self.slot);
            }
        }
    }
}

impl LanguageServer for ServerGuard {
    fn language(&self) -> Language {
        self.server.language()
    }

    // Must forward: a guard that always claimed "not busy" would hide a
    // still-indexing server behind the pool.
    fn is_busy(&self) -> bool {
        self.server.is_busy()
    }

    fn wait_until_ready(&self, deadline: Duration) -> bool {
        self.server.wait_until_ready(deadline)
    }

    fn references(
        &self,
        path: &crate::types::RelPath,
        position: super::Position,
    ) -> Result<Vec<super::Location>, LspError> {
        self.call(|server| server.references(path, position))
    }

    fn definition(
        &self,
        path: &crate::types::RelPath,
        position: super::Position,
    ) -> Result<Vec<super::Location>, LspError> {
        self.call(|server| server.definition(path, position))
    }

    fn hover(
        &self,
        path: &crate::types::RelPath,
        position: super::Position,
    ) -> Result<serde_json::Value, LspError> {
        self.call(|server| server.hover(path, position))
    }

    fn diagnostics(
        &self,
        path: &crate::types::RelPath,
    ) -> Result<Vec<super::Diagnostic>, LspError> {
        self.call(|server| server.diagnostics(path))
    }

    fn prepare_rename(
        &self,
        path: &crate::types::RelPath,
        position: super::Position,
        new_name: &str,
    ) -> Result<super::WorkspaceEdit, LspError> {
        self.call(|server| server.prepare_rename(path, position, new_name))
    }

    fn memory_bytes(&self) -> Option<u64> {
        self.server.memory_bytes()
    }

    fn shutdown(&self) -> Result<(), LspError> {
        self.retire_crashed();
        Ok(())
    }
}

/// Lets the router drive a real pool.
///
/// The guard returned by [`LspPool::acquire`] is what keeps reclamation and
/// in-flight requests from racing: it holds the slot's in-flight count up for
/// as long as the caller holds it. Handing out a bare reference instead would
/// let the monitor thread stop a server while a request was still running.
impl super::router::LspProvider for LspPool {
    fn acquire(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError> {
        Ok(Box::new(LspPool::acquire(self, language)?))
    }

    // `install_hint` keeps the trait default. When a start actually fails the
    // pool surfaces `LspError::Unavailable(lang, hint)` carrying the hint
    // discovery produced, and the router prefers that over this fallback;
    // this text is only reached before any attempt has been made.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::{Diagnostic, Location, Position, WorkspaceEdit};
    use crate::types::RelPath;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::AtomicUsize;

    fn assert_send_sync<T: Send + Sync>() {}

    #[derive(Clone)]
    struct FakeFactory {
        starts: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        memory: Arc<AtomicU64>,
        fail_starts: Arc<AtomicBool>,
        crash_on_use: Arc<AtomicBool>,
    }

    struct FakeServer {
        language: Language,
        memory: Arc<AtomicU64>,
        shutdowns: Arc<AtomicUsize>,
        crash_on_use: bool,
    }

    impl FakeFactory {
        fn new() -> Self {
            FakeFactory {
                starts: Arc::new(AtomicUsize::new(0)),
                shutdowns: Arc::new(AtomicUsize::new(0)),
                memory: Arc::new(AtomicU64::new(1_048_576)),
                fail_starts: Arc::new(AtomicBool::new(false)),
                crash_on_use: Arc::new(AtomicBool::new(false)),
            }
        }

        fn starts(&self) -> usize {
            self.starts.load(Ordering::SeqCst)
        }

        fn shutdowns(&self) -> usize {
            self.shutdowns.load(Ordering::SeqCst)
        }
    }

    impl ServerFactory for FakeFactory {
        fn start(&self, language: Language) -> Result<Box<dyn LanguageServer>, LspError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail_starts.load(Ordering::SeqCst) {
                return Err(LspError::Unavailable(
                    language,
                    "rust-analyzer is not installed; try rustup component add rust-analyzer".into(),
                ));
            }
            Ok(Box::new(FakeServer {
                language,
                memory: Arc::clone(&self.memory),
                shutdowns: Arc::clone(&self.shutdowns),
                crash_on_use: self.crash_on_use.load(Ordering::SeqCst),
            }))
        }
    }

    impl FakeServer {
        fn fail_if_crashed(&self) -> Result<(), LspError> {
            if self.crash_on_use {
                Err(LspError::Crashed)
            } else {
                Ok(())
            }
        }
    }

    impl LanguageServer for FakeServer {
        fn language(&self) -> Language {
            self.language
        }

        fn references(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<Vec<Location>, LspError> {
            self.fail_if_crashed()?;
            Ok(Vec::new())
        }

        fn definition(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<Vec<Location>, LspError> {
            self.fail_if_crashed()?;
            Ok(Vec::new())
        }

        fn hover(
            &self,
            _path: &RelPath,
            _position: Position,
        ) -> Result<serde_json::Value, LspError> {
            self.fail_if_crashed()?;
            Ok(serde_json::Value::Null)
        }

        fn diagnostics(&self, _path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
            self.fail_if_crashed()?;
            Ok(Vec::new())
        }

        fn prepare_rename(
            &self,
            _path: &RelPath,
            _position: Position,
            _new_name: &str,
        ) -> Result<WorkspaceEdit, LspError> {
            self.fail_if_crashed()?;
            Ok(WorkspaceEdit::default())
        }

        fn memory_bytes(&self) -> Option<u64> {
            Some(self.memory.load(Ordering::SeqCst))
        }

        fn shutdown(&self) -> Result<(), LspError> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn quiet_config() -> LspPoolConfig {
        LspPoolConfig {
            idle_ttl: Duration::ZERO,
            ready_timeout: Duration::ZERO,
            memory_limit_bytes: 0,
            memory_hard_limit_bytes: 0,
            start_cooldown: Duration::from_secs(60),
            monitor_interval: Duration::ZERO,
        }
    }

    fn dummy_pos() -> Position {
        Position {
            line: 0,
            character: 0,
        }
    }

    fn dummy_path() -> RelPath {
        RelPath::new("src/lib.rs")
    }

    #[test]
    fn pool_and_guard_are_send_sync() {
        assert_send_sync::<LspPool>();
        assert_send_sync::<ServerGuard>();
    }

    #[test]
    fn starts_on_first_request_only() {
        let factory = FakeFactory::new();
        let pool = LspPool::with_factory_and_config(factory.clone(), quiet_config());

        assert_eq!(factory.starts(), 0);
        assert!(!pool.is_resident(Language::Python));

        let first = pool.acquire(Language::Python).expect("start");
        assert_eq!(factory.starts(), 1);
        assert!(pool.is_resident(Language::Python));

        let second = pool.acquire(Language::Python).expect("reuse");
        assert_eq!(factory.starts(), 1);

        drop(first);
        drop(second);
        pool.shutdown();
        assert_eq!(factory.starts(), 1);
    }

    #[test]
    fn idle_timeout_reclaims_unused_server() {
        let factory = FakeFactory::new();
        let mut config = quiet_config();
        config.idle_ttl = Duration::from_millis(40);
        let pool = LspPool::with_factory_and_config(factory.clone(), config);

        drop(pool.acquire(Language::Go).expect("start"));
        assert_eq!(factory.shutdowns(), 0);
        assert!(pool.is_resident(Language::Go));

        thread::sleep(Duration::from_millis(80));
        pool.reclaim();

        assert_eq!(factory.shutdowns(), 1);
        assert!(!pool.is_resident(Language::Go));
        pool.shutdown();
    }

    #[test]
    fn memory_over_budget_reclaims_unused_server() {
        let factory = FakeFactory::new();
        let mut config = quiet_config();
        config.memory_limit_bytes = 8 * 1024 * 1024;
        let pool = LspPool::with_factory_and_config(factory.clone(), config);

        drop(pool.acquire(Language::Rust).expect("start"));
        assert_eq!(factory.shutdowns(), 0);

        factory.memory.store(32 * 1024 * 1024, Ordering::SeqCst);
        pool.reclaim();

        assert_eq!(factory.shutdowns(), 1);
        assert!(!pool.is_resident(Language::Rust));
        pool.shutdown();
    }

    #[test]
    fn in_use_server_is_not_reclaimed() {
        let factory = FakeFactory::new();
        let mut config = quiet_config();
        config.idle_ttl = Duration::from_millis(1);
        config.memory_limit_bytes = 1;
        let pool = LspPool::with_factory_and_config(factory.clone(), config);

        let guard = pool.acquire(Language::Java).expect("start");
        factory.memory.store(64 * 1024 * 1024, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        pool.reclaim();

        assert_eq!(factory.shutdowns(), 0);
        assert!(pool.is_resident(Language::Java));
        drop(guard);
        pool.shutdown();
    }

    #[test]
    fn reclaim_then_next_request_restarts_transparently() {
        let factory = FakeFactory::new();
        let mut config = quiet_config();
        config.memory_limit_bytes = 8 * 1024 * 1024;
        let pool = LspPool::with_factory_and_config(factory.clone(), config);

        let first = pool.acquire(Language::TypeScript).expect("start");
        first
            .references(&dummy_path(), dummy_pos())
            .expect("query on first generation");
        drop(first);

        factory.memory.store(32 * 1024 * 1024, Ordering::SeqCst);
        pool.reclaim();
        assert_eq!(factory.starts(), 1);
        assert_eq!(factory.shutdowns(), 1);

        factory.memory.store(1_048_576, Ordering::SeqCst);
        let second = pool.acquire(Language::TypeScript).expect("restart");
        assert_eq!(factory.starts(), 2);
        second
            .definition(&dummy_path(), dummy_pos())
            .expect("query on second generation");
        drop(second);
        pool.shutdown();
    }

    #[test]
    fn failed_start_is_cooled_down_and_not_retried_every_request() {
        let factory = FakeFactory::new();
        factory.fail_starts.store(true, Ordering::SeqCst);
        let mut config = quiet_config();
        config.start_cooldown = Duration::from_millis(80);
        let pool = LspPool::with_factory_and_config(factory.clone(), config);

        let first = pool.acquire(Language::Rust);
        assert!(matches!(
            first,
            Err(LspError::Unavailable(Language::Rust, _))
        ));
        assert_eq!(factory.starts(), 1);

        let second = pool.acquire(Language::Rust);
        assert!(matches!(
            second,
            Err(LspError::Unavailable(Language::Rust, _))
        ));
        assert_eq!(
            factory.starts(),
            1,
            "cooldown must suppress the second spawn"
        );

        thread::sleep(Duration::from_millis(120));
        let third = pool.acquire(Language::Rust);
        assert!(matches!(
            third,
            Err(LspError::Unavailable(Language::Rust, _))
        ));
        assert_eq!(factory.starts(), 2, "cooldown elapsed; start is retried");
        pool.shutdown();
    }

    #[test]
    fn crash_is_healed_on_the_next_request() {
        let factory = FakeFactory::new();
        factory.crash_on_use.store(true, Ordering::SeqCst);
        let pool = LspPool::with_factory_and_config(factory.clone(), quiet_config());

        let crashed = pool
            .with_server(Language::Python, |server| {
                server.references(&dummy_path(), dummy_pos())
            })
            .expect_err("first generation crashes");
        assert!(matches!(crashed, LspError::Crashed));
        assert_eq!(factory.starts(), 1);
        assert!(!pool.is_resident(Language::Python));

        factory.crash_on_use.store(false, Ordering::SeqCst);
        let recovered = pool
            .with_server(Language::Python, |server| {
                server.references(&dummy_path(), dummy_pos())
            })
            .expect("second generation is healthy");
        assert!(recovered.is_empty());
        assert_eq!(factory.starts(), 2);
        pool.shutdown();
    }

    #[test]
    fn monitor_can_stop_and_drop_does_not_join() {
        let factory = FakeFactory::new();
        let mut config = quiet_config();
        config.monitor_interval = Duration::from_millis(20);
        let pool = LspPool::with_factory_and_config(factory.clone(), config.clone());
        assert!(!pool.monitor_is_finished());

        pool.stop_monitor();
        assert!(pool.monitor_is_finished());
        pool.shutdown();

        let hanging = LspPool::with_factory_and_config(
            factory,
            LspPoolConfig {
                monitor_interval: Duration::from_secs(30),
                ..config
            },
        );
        let started = Instant::now();
        drop(hanging);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "dropping the pool must detach the monitor, not join a 30s tick"
        );
    }

    #[test]
    fn zero_thresholds_disable_reclaim() {
        let factory = FakeFactory::new();
        let pool = LspPool::with_factory_and_config(factory.clone(), quiet_config());

        drop(pool.acquire(Language::JavaScript).expect("start"));
        factory.memory.store(u64::MAX, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(20));
        pool.reclaim();

        assert_eq!(factory.shutdowns(), 0);
        assert!(pool.is_resident(Language::JavaScript));
        pool.shutdown();
    }

    struct StaticDiscovery {
        spec: ServerSpec,
    }

    impl DiscoverServers for StaticDiscovery {
        fn discover(&self, language: Language) -> Result<ServerSpec, LspError> {
            if language == self.spec.language {
                Ok(self.spec.clone())
            } else {
                Err(LspError::Unavailable(
                    language,
                    "no server configured".into(),
                ))
            }
        }
    }

    struct CountingTransport {
        factory: FakeFactory,
        seen_roots: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl SpawnServer for CountingTransport {
        fn spawn(
            &self,
            spec: &ServerSpec,
            root: &Path,
        ) -> Result<Box<dyn LanguageServer>, LspError> {
            self.seen_roots
                .lock()
                .expect("roots poisoned")
                .push(root.to_path_buf());
            self.factory.start(spec.language)
        }
    }

    #[test]
    fn composite_factory_wires_discovery_then_transport() {
        let factory = FakeFactory::new();
        let discovery = StaticDiscovery {
            spec: ServerSpec {
                language: Language::Go,
                command: PathBuf::from("gopls"),
                args: vec!["serve".into()],
                install_hint: "go install golang.org/x/tools/gopls@latest".into(),
            },
        };
        let seen_roots = Arc::new(Mutex::new(Vec::new()));
        let transport = CountingTransport {
            factory: factory.clone(),
            seen_roots: Arc::clone(&seen_roots),
        };
        let root = PathBuf::from("/workspace/under/test");
        let pool = LspPool::from_discovery_and_transport(
            discovery,
            transport,
            root.clone(),
            quiet_config(),
        );

        drop(pool.acquire(Language::Go).expect("discovered + spawned"));
        assert_eq!(factory.starts(), 1);
        // The workspace root has to reach the spawner: LSP `initialize`
        // needs a `rootUri`, and a server pointed at the wrong tree indexes
        // the wrong code.
        assert_eq!(
            *seen_roots.lock().expect("roots poisoned"),
            vec![root.clone()]
        );

        let missing = pool.acquire(Language::Python);
        assert!(matches!(
            missing,
            Err(LspError::Unavailable(Language::Python, _))
        ));
        assert_eq!(factory.starts(), 1);
        pool.shutdown();
    }

    #[test]
    fn concurrent_checkouts_share_one_process() {
        let factory = FakeFactory::new();
        let pool = Arc::new(LspPool::with_factory_and_config(
            factory.clone(),
            quiet_config(),
        ));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let pool = Arc::clone(&pool);
                thread::spawn(move || {
                    let guard = pool.acquire(Language::Rust).expect("checkout");
                    guard
                        .diagnostics(&dummy_path())
                        .expect("query under checkout");
                    drop(guard);
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("worker panicked");
        }
        assert_eq!(factory.starts(), 1);
        pool.shutdown();
    }

    #[test]
    fn indexing_server_is_spared_the_steady_state_limit_but_not_the_hard_one() {
        let config = LspPoolConfig {
            memory_limit_bytes: 1024,
            memory_hard_limit_bytes: 4096,
            ..quiet_config()
        };

        // Measured behaviour this guards: rust-analyzer peaked at 1165 MB
        // while indexing and was reclaimed against a 1024 MB ceiling after
        // 4.6 s, so it never answered a single query. Indexing is a
        // transient peak; reclaiming there guarantees the server never gets
        // past it.
        assert!(
            !memory_over_budget(&config, Some(2048), true),
            "a server at its indexing peak must be allowed to finish"
        );
        assert!(
            memory_over_budget(&config, Some(2048), false),
            "once indexed, the steady-state ceiling applies"
        );
        assert!(
            memory_over_budget(&config, Some(8192), true),
            "the hard ceiling is never waived, or a runaway server takes the machine"
        );
    }
}
