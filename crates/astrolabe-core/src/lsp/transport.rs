//! JSON-RPC 2.0 client over an LSP stdio subprocess.
//!
//! [`LspTransport`] is the wire client. [`StdioSpawn`] implements
//! [`super::pool::SpawnServer`] so the pool can store `Box<dyn LanguageServer>`
//! without naming this module. Document lifetime and LSP method mapping live
//! on [`LspSession`].
//!
//! # Framing
//!
//! LSP is **not** newline-delimited JSON. Each message is
//! `Content-Length: N\r\n\r\n` plus `N` bytes of UTF-8 JSON. A single `read`
//! can yield a partial header, a header with a truncated body, or several
//! complete messages. A byte-buffer state machine ingests whatever arrived,
//! pulls zero or more complete bodies, and leaves the rest.
//!
//! # Threads
//!
//! `astrolabe-core` is synchronous; there is no tokio here. Three OS threads
//! plus the caller's:
//!
//! * **stdout reader** — frames messages and dispatches by kind: responses
//!   complete a pending `id`, notifications are stored (diagnostics) or logged,
//!   server-initiated requests are answered immediately.
//! * **stderr drainer** — reads the pipe to completion. A language server
//!   that fills the stderr buffer deadlocks if nobody consumes it.
//! * **caller threads** — `request` is `&self` and blocks on a oneshot until
//!   the matching `id` arrives, the timeout fires, or the process dies.
//!
//! Writes to the child's stdin are serialized with a mutex so frames cannot
//! interleave. Concurrent in-flight requests are matched by `id`, not by
//! arrival order.
//!
//! # Lifecycle
//!
//! [`LspTransport::connect`] spawns, then runs `initialize` / `initialized`.
//! [`LspTransport::shutdown`] sends `shutdown` / `exit` and reaps the child.
//! Drop always kills whatever is left so a forgotten server cannot leak.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use memchr::memmem;
use serde::Serialize;
use serde_json::{json, Value};

use super::pool::SpawnServer;
use super::{
    Diagnostic, LanguageServer, Location, LspError, Position, Range, ServerSpec, Severity,
    TextEdit, WorkspaceEdit,
};
use crate::types::{Language, RelPath};

/// Default per-request timeout. Pool and tests override this.
///
/// Aligned with Serena's DEFAULT_LS_REQUEST_TIMEOUT (300s). Pyright and other LSPs on a cold
/// workspace before `textDocument/definition` / `references` answer. Override
/// with `ASTROLABE_LSP_TIMEOUT` (seconds) on [`StdioSpawn::new`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const LIFE_RUNNING: u8 = 0;
const LIFE_SHUTDOWN: u8 = 1;
const LIFE_DEAD: u8 = 2;

/// JSON-RPC / LSP stdio client. `Send + Sync`; the pool holds `Arc<Self>`.
pub struct LspTransport {
    shared: Arc<Shared>,
    reader: Mutex<Option<JoinHandle<()>>>,
    stderr: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    spec: ServerSpec,
    pid: u32,
    timeout: Duration,
    child: Mutex<Child>,
    stdin: Mutex<Option<std::process::ChildStdin>>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Result<Value, LspError>>>>,
    next_id: AtomicU64,
    life: AtomicU8,
    diagnostics: Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>,
    notifications: Mutex<Vec<(String, Value)>>,
    initialize_result: Mutex<Option<Value>>,
    workspace_folders: Mutex<Option<Value>>,
    stderr_bytes: AtomicU64,
    cwd: Option<PathBuf>,
    /// Whether the server has ever announced background work.
    ///
    /// Distinguishes "finished indexing" from "has not started yet": both
    /// show zero active tokens, but only the first means the answers are
    /// trustworthy.
    saw_progress: AtomicBool,
    /// Work-done progress tokens the server has begun but not ended.
    ///
    /// A server that is still indexing answers queries with an empty result
    /// rather than an error, which is indistinguishable from a genuine "no
    /// results" unless we track this.
    active_progress: Mutex<HashSet<String>>,
    ready_confirmed: AtomicBool,
}

impl std::fmt::Debug for LspTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspTransport")
            .field("language", &self.shared.spec.language)
            .field("pid", &self.shared.pid)
            .finish()
    }
}

impl LspTransport {
    /// Spawn `spec.command` with piped stdio. Does not handshake.
    pub fn spawn(spec: &ServerSpec, timeout: Duration) -> Result<Self, LspError> {
        Self::start(spec, None, timeout)
    }

    /// Spawn with `cwd` as the child's working directory.
    pub fn spawn_in(
        spec: &ServerSpec,
        cwd: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        Self::start(spec, Some(cwd.as_ref()), timeout)
    }

    /// Spawn in `root` and complete `initialize` / `initialized`.
    pub fn connect(
        spec: &ServerSpec,
        root: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self, LspError> {
        Self::connect_with(spec, root, timeout, None)
    }

    /// [`connect`] with optional LSP `initializationOptions`.
    pub fn connect_with(
        spec: &ServerSpec,
        root: impl AsRef<Path>,
        timeout: Duration,
        initialization_options: Option<Value>,
    ) -> Result<Self, LspError> {
        let root = root.as_ref();
        let transport = Self::spawn_in(spec, root, timeout)?;
        match transport.initialize_with(root, initialization_options) {
            Ok(_) => Ok(transport),
            Err(err) => {
                transport.force_stop();
                Err(err)
            }
        }
    }

    fn start(spec: &ServerSpec, cwd: Option<&Path>, timeout: Duration) -> Result<Self, LspError> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(LspError::Unavailable(
                    spec.language,
                    spec.install_hint.clone(),
                ));
            }
            Err(err) => return Err(LspError::Startup(spec.language, err.to_string())),
        };

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| LspError::Startup(spec.language, "child stderr was not piped".into()))?;
        let pid = child.id();

        let shared = Arc::new(Shared {
            spec: spec.clone(),
            pid,
            timeout,
            child: Mutex::new(child),
            stdin: Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            life: AtomicU8::new(LIFE_RUNNING),
            diagnostics: Mutex::new(HashMap::new()),
            notifications: Mutex::new(Vec::new()),
            saw_progress: AtomicBool::new(false),
            active_progress: Mutex::new(HashSet::new()),
            ready_confirmed: AtomicBool::new(false),
            initialize_result: Mutex::new(None),
            workspace_folders: Mutex::new(None),
            stderr_bytes: AtomicU64::new(0),
            cwd: cwd.map(Path::to_path_buf),
        });

        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("astrolabe-lsp-stdout".into())
            .spawn(move || read_stdout(stdout, reader_shared))
            .map_err(|err| LspError::Startup(spec.language, err.to_string()))?;

        let stderr_shared = Arc::clone(&shared);
        let stderr_thread = match thread::Builder::new()
            .name("astrolabe-lsp-stderr".into())
            .spawn(move || drain_stderr(stderr, stderr_shared))
        {
            Ok(handle) => handle,
            Err(err) => {
                let transport = Self {
                    shared,
                    reader: Mutex::new(Some(reader)),
                    stderr: Mutex::new(None),
                };
                transport.force_stop();
                return Err(LspError::Startup(spec.language, err.to_string()));
            }
        };

        Ok(Self {
            shared,
            reader: Mutex::new(Some(reader)),
            stderr: Mutex::new(Some(stderr_thread)),
        })
    }

    pub fn language(&self) -> Language {
        self.shared.spec.language
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.shared.spec
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid
    }

    /// Working directory the child was spawned in, if one was set.
    pub fn root(&self) -> Option<&Path> {
        self.shared.cwd.as_deref()
    }

    pub fn is_alive(&self) -> bool {
        self.shared.life.load(Ordering::SeqCst) == LIFE_RUNNING
    }

    /// Bytes the stderr thread has consumed. Used by tests; also a cheap
    /// signal that the drain loop is actually running.
    pub fn stderr_bytes(&self) -> u64 {
        self.shared.stderr_bytes.load(Ordering::Relaxed)
    }

    /// Raw `initialize` result, when a handshake has completed.
    pub fn initialize_result(&self) -> Option<Value> {
        lock(&self.shared.initialize_result).clone()
    }

    /// Latest `textDocument/publishDiagnostics` per document URI.
    pub fn published_diagnostics(&self) -> HashMap<String, Vec<lsp_types::Diagnostic>> {
        lock(&self.shared.diagnostics).clone()
    }

    /// Whether the server has background work outstanding.
    ///
    /// Callers use this to avoid reporting an empty result as authoritative
    /// while the server is still building its index.
    pub fn is_busy(&self) -> bool {
        !lock(&self.shared.active_progress).is_empty()
    }

    /// Poll until no background work is outstanding, or `deadline` elapses.
    ///
    /// Polling rather than waiting on a condvar keeps this off the reader
    /// thread's hot path; indexing takes seconds, so 50 ms granularity costs
    /// nothing and cannot deadlock if a server never ends a token.
    pub fn wait_until_ready(&self, deadline: Duration) -> bool {
        if self.shared.ready_confirmed.load(Ordering::Acquire) {
            return true;
        }

        let until = Instant::now() + deadline;
        let tick = Duration::from_millis(50).min(deadline);

        // A server that has just started has no active tokens *yet*, which
        // is indistinguishable from having finished. Give it a window to
        // announce its work before believing it is idle.
        let settle_until = Instant::now() + SETTLE_WINDOW.min(deadline);
        while !self.shared.saw_progress.load(Ordering::SeqCst) && Instant::now() < settle_until {
            std::thread::sleep(tick);
        }
        // Servers that never report progress have no startup phase to wait
        // out; the window above was the whole cost.
        if !self.shared.saw_progress.load(Ordering::SeqCst) {
            self.shared.ready_confirmed.store(true, Ordering::Release);
            return true;
        }

        // Require the server to be *continuously* quiet, not merely idle at
        // the instant we look. Measured against rust-analyzer on a 10-line
        // crate, the active-token set empties 11 separate times between
        // 0.18 s and 1.57 s before indexing is genuinely done, so a single
        // sample lands in one of those gaps and reports a cold server ready.
        let mut quiet_since: Option<Instant> = None;
        loop {
            if self.is_busy() {
                quiet_since = None;
            } else {
                let since = *quiet_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= QUIET_PERIOD {
                    self.shared.ready_confirmed.store(true, Ordering::Release);
                    return true;
                }
            }
            if Instant::now() >= until {
                let ready = !self.is_busy();
                if ready {
                    self.shared.ready_confirmed.store(true, Ordering::Release);
                }
                return ready;
            }
            std::thread::sleep(tick);
        }
    }

    /// Server-to-client notifications observed so far (capped).
    pub fn notifications(&self) -> Vec<(String, Value)> {
        lock(&self.shared.notifications).clone()
    }

    /// Resident-set size of the child, when the OS makes it observable.
    pub fn memory_bytes(&self) -> Option<u64> {
        if self.shared.life.load(Ordering::SeqCst) == LIFE_DEAD {
            return None;
        }
        rss_bytes(self.shared.pid)
    }

    /// JSON-RPC request. Blocks until the matching response, a timeout, or a crash.
    pub fn request<P: Serialize>(&self, method: &str, params: P) -> Result<Value, LspError> {
        self.request_timeout(method, params, self.shared.timeout)
    }

    pub fn request_timeout<P: Serialize>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let params =
            serde_json::to_value(params).map_err(|err| LspError::Protocol(err.to_string()))?;
        self.request_value(method, params, timeout)
    }

    /// Typed helper so the pool can write `call::<lsp_types::request::References>(params)`.
    pub fn call<R>(&self, params: R::Params) -> Result<R::Result, LspError>
    where
        R: lsp_types::request::Request,
        R::Params: Serialize,
    {
        let result = self.request(R::METHOD, params)?;
        serde_json::from_value(result).map_err(|err| LspError::Protocol(err.to_string()))
    }

    pub fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let params =
            serde_json::to_value(params).map_err(|err| LspError::Protocol(err.to_string()))?;
        self.send_message(rpc_notification(method, params))
    }

    pub fn notify_typed<N>(&self, params: N::Params) -> Result<(), LspError>
    where
        N: lsp_types::notification::Notification,
        N::Params: Serialize,
    {
        self.notify(N::METHOD, params)
    }

    /// `initialize` request plus `initialized` notification.
    pub fn initialize(&self, root: &Path) -> Result<Value, LspError> {
        self.initialize_with(root, None)
    }

    pub fn initialize_with(
        &self,
        root: &Path,
        initialization_options: Option<Value>,
    ) -> Result<Value, LspError> {
        let root_uri = path_to_file_uri(root);
        let folder_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("workspace");
        let folders = json!([{ "uri": root_uri, "name": folder_name }]);
        *lock(&self.shared.workspace_folders) = Some(folders.clone());

        let mut params = json!({
            "processId": std::process::id(),
            "rootPath": root.to_string_lossy(),
            "rootUri": root_uri,
            "capabilities": client_capabilities(),
            "workspaceFolders": folders,
            "clientInfo": {
                "name": "astrolabe",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "locale": "en",
            "trace": "off",
        });
        if let Some(options) = initialization_options {
            params["initializationOptions"] = options;
        }

        let result = self.request("initialize", params)?;
        *lock(&self.shared.initialize_result) = Some(result.clone());
        self.notify("initialized", json!({}))?;
        Ok(result)
    }

    /// `shutdown` request, `exit` notification, then reap the child.
    pub fn shutdown(&self) -> Result<(), LspError> {
        if self.shared.life.load(Ordering::SeqCst) != LIFE_RUNNING {
            self.force_stop();
            return Ok(());
        }
        let result = self.request("shutdown", Value::Null);
        self.shared.life.store(LIFE_SHUTDOWN, Ordering::SeqCst);
        let _ = self.notify("exit", Value::Null);
        self.force_stop();
        match result {
            Ok(_) | Err(LspError::Crashed) | Err(LspError::Timeout(_)) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Send `method`, retrying while the server answers `ContentModified`.
    ///
    /// The spec designates that code retryable and expects the client to
    /// re-issue. `rust-analyzer` returns it for the first query after a file
    /// is opened, so without this the opening query of every session fails
    /// — which is exactly the case an agent hits first.
    fn request_value(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let mut attempt = 0;
        loop {
            match self.request_once(method, params.clone(), timeout) {
                Err(LspError::ContentModified) if attempt + 1 < CONTENT_MODIFIED_ATTEMPTS => {
                    attempt += 1;
                    tracing::debug!(
                        method,
                        attempt,
                        "language server reported content modified; retrying"
                    );
                    std::thread::sleep(CONTENT_MODIFIED_BACKOFF);
                }
                other => return other,
            }
        }
    }

    fn request_once(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        match self.shared.life.load(Ordering::SeqCst) {
            LIFE_DEAD => return Err(LspError::Crashed),
            LIFE_SHUTDOWN if method != "shutdown" => {
                return Err(LspError::Protocol("language server has shut down".into()));
            }
            _ => {}
        }
        if child_exited(&self.shared) {
            if self.shared.life.load(Ordering::SeqCst) == LIFE_RUNNING {
                self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
                fail_pending(&self.shared);
            }
            if method != "shutdown" {
                return Err(LspError::Crashed);
            }
        }

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.pending).insert(id, tx);

        if let Err(err) = self.send_message(rpc_request(id, method, params)) {
            lock(&self.shared.pending).remove(&id);
            return Err(err);
        }

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Disconnected) => Err(LspError::Crashed),
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.pending).remove(&id);
                if let Ok(late) = rx.try_recv() {
                    return late;
                }
                let _ = self.notify("$/cancelRequest", json!({ "id": id }));
                Err(LspError::Timeout(timeout))
            }
        }
    }

    fn send_message(&self, payload: Value) -> Result<(), LspError> {
        write_rpc(&self.shared, &payload)
    }

    fn force_stop(&self) {
        self.shared.ready_confirmed.store(false, Ordering::Release);
        let previous = self.shared.life.swap(LIFE_SHUTDOWN, Ordering::SeqCst);
        if previous == LIFE_DEAD {
            self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
        }
        {
            *lock(&self.shared.stdin) = None;
        }
        {
            let mut child = lock(&self.shared.child);
            reap_child(&mut child, Duration::from_millis(200));
        }
        self.shared.life.store(LIFE_DEAD, Ordering::SeqCst);
        fail_pending(&self.shared);
        if let Some(handle) = lock(&self.reader).take() {
            let _ = handle.join();
        }
        if let Some(handle) = lock(&self.stderr).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LspTransport {
    fn drop(&mut self) {
        self.force_stop();
    }
}

/// Production [`SpawnServer`]: handshake a stdio language server for one workspace.
///
/// [`SpawnServer::spawn`] does not receive a workspace root (that is a gap in
/// the pool trait). The root is stored here instead.
/// LSP error code for `ContentModified`.
const LSP_CONTENT_MODIFIED: i64 = -32801;

/// Attempts for a request the server answered with `ContentModified`.
const CONTENT_MODIFIED_ATTEMPTS: u32 = 4;

/// Backoff between those attempts. The server is re-analysing the file, which
/// takes milliseconds once the index is warm.
const CONTENT_MODIFIED_BACKOFF: Duration = Duration::from_millis(250);

/// How long a freshly started server is given to announce background work
/// before we accept its silence as "already indexed".
const SETTLE_WINDOW: Duration = Duration::from_secs(3);

/// How long the server must stay quiet before its index is trusted.
///
/// Covers the transient gaps between a server's startup tasks; measured at
/// under 900 ms of gap for rust-analyzer, so 1 s clears them with margin.
const QUIET_PERIOD: Duration = Duration::from_secs(1);

fn timeout_from_env() -> Option<Duration> {
    let raw = std::env::var("ASTROLABE_LSP_TIMEOUT").ok()?;
    match raw.parse::<u64>() {
        Ok(0) => None,
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                "ASTROLABE_LSP_TIMEOUT is not a valid integer (seconds); using default"
            );
            None
        }
    }
}

#[derive(Clone, Debug)]
pub struct StdioSpawn {
    pub root: PathBuf,
    pub timeout: Duration,
}

impl StdioSpawn {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            timeout: timeout_from_env().unwrap_or(DEFAULT_TIMEOUT),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl SpawnServer for StdioSpawn {
    // `root` comes from the caller rather than `self.root`: the pool knows
    // which workspace it is indexing, and a spawner should not have to be
    // rebuilt to point at a different one.
    fn spawn(
        &self,
        spec: &ServerSpec,
        root: &std::path::Path,
    ) -> Result<Box<dyn LanguageServer>, LspError> {
        let transport = LspTransport::connect(spec, root, self.timeout)?;
        Ok(Box::new(LspSession {
            transport,
            root: root.to_path_buf(),
            opened: Mutex::new(HashSet::new()),
        }))
    }
}

/// [`LanguageServer`] over [`LspTransport`]: `didOpen`, method calls, URI conversion.
pub struct LspSession {
    transport: LspTransport,
    root: PathBuf,
    opened: Mutex<HashSet<String>>,
}

impl LspSession {
    fn file_uri(&self, path: &RelPath) -> String {
        path_to_file_uri(&self.root.join(path.as_str()))
    }

    fn ensure_open(&self, path: &RelPath) -> Result<(), LspError> {
        let key = path.as_str().to_string();
        if lock(&self.opened).contains(&key) {
            return Ok(());
        }
        let abs = self.root.join(path.as_str());
        let text = std::fs::read_to_string(&abs).map_err(|err| {
            LspError::Protocol(format!("could not read {} ({err})", path.as_str()))
        })?;
        self.transport.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": self.file_uri(path),
                    "languageId": language_id(self.transport.language()),
                    "version": 1,
                    "text": text,
                }
            }),
        )?;
        lock(&self.opened).insert(key);
        Ok(())
    }
}

impl LanguageServer for LspSession {
    fn language(&self) -> Language {
        self.transport.language()
    }

    fn references(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        self.ensure_open(path)?;
        let result = self.transport.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
                "context": { "includeDeclaration": true },
            }),
        )?;
        Ok(locations_from_value(&result, &self.root))
    }

    fn definition(&self, path: &RelPath, position: Position) -> Result<Vec<Location>, LspError> {
        self.ensure_open(path)?;
        let result = self.transport.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
            }),
        )?;
        Ok(locations_from_value(&result, &self.root))
    }

    fn hover(&self, path: &RelPath, position: Position) -> Result<Value, LspError> {
        self.ensure_open(path)?;
        self.transport.request(
            crate::lsp::queries::HOVER_METHOD,
            crate::lsp::queries::hover_params(&self.file_uri(path), position),
        )
    }

    fn diagnostics(&self, path: &RelPath) -> Result<Vec<Diagnostic>, LspError> {
        self.ensure_open(path)?;
        let uri = self.file_uri(path);
        match self.transport.request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        ) {
            Ok(value) => Ok(diagnostics_from_pull(&value, path)),
            Err(LspError::Protocol(_)) => {
                let diags = self.push_diagnostics(path, &uri);
                if diags.is_empty() {
                    Err(LspError::Protocol(
                        "pull diagnostic unsupported and push diagnostics empty".into(),
                    ))
                } else {
                    Ok(diags)
                }
            }
            Err(LspError::Timeout(timeout)) => {
                // Prefer any push diagnostics that arrived while the pull hung,
                // but keep Timeout visible when the fallback is empty.
                let diags = self.push_diagnostics(path, &uri);
                if diags.is_empty() {
                    Err(LspError::Timeout(timeout))
                } else {
                    Ok(diags)
                }
            }
            Err(err) => Err(err),
        }
    }

    fn prepare_rename(
        &self,
        path: &RelPath,
        position: Position,
        new_name: &str,
    ) -> Result<WorkspaceEdit, LspError> {
        self.ensure_open(path)?;
        let result = self.transport.request(
            "textDocument/rename",
            json!({
                "textDocument": { "uri": self.file_uri(path) },
                "position": { "line": position.line, "character": position.character },
                "newName": new_name,
            }),
        )?;
        Ok(workspace_edit_from_value(&result, &self.root))
    }

    fn is_busy(&self) -> bool {
        self.transport.is_busy()
    }

    fn wait_until_ready(&self, deadline: Duration) -> bool {
        self.transport.wait_until_ready(deadline)
    }

    fn memory_bytes(&self) -> Option<u64> {
        self.transport.memory_bytes()
    }

    fn shutdown(&self) -> Result<(), LspError> {
        self.transport.shutdown()
    }
}

impl LspSession {
    fn push_diagnostics(&self, path: &RelPath, uri: &str) -> Vec<Diagnostic> {
        let snap = self.transport.published_diagnostics();
        let Some(items) = snap.get(uri).or_else(|| {
            snap.iter()
                .find(|(k, _)| k.ends_with(path.as_str()))
                .map(|(_, v)| v)
        }) else {
            return Vec::new();
        };
        items
            .iter()
            .map(|d| diagnostic_from_lsp(path.clone(), d))
            .collect()
    }
}

fn language_id(language: Language) -> &'static str {
    match language {
        Language::Python => "python",
        Language::Go => "go",
        Language::Java => "java",
        Language::Rust => "rust",
        Language::TypeScript => "typescript",
        Language::Tsx => "typescriptreact",
        Language::JavaScript => "javascript",
    }
}

fn locations_from_value(value: &Value, root: &Path) -> Vec<Location> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| location_from_value(item, root))
            .collect(),
        Value::Object(_) => location_from_value(value, root).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn location_from_value(value: &Value, root: &Path) -> Option<Location> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))?;
    Some(Location {
        path: uri_to_rel(uri, root)?,
        range: range_from_value(range)?,
    })
}

fn range_from_value(value: &Value) -> Option<Range> {
    Some(Range {
        start: position_from_value(value.get("start")?)?,
        end: position_from_value(value.get("end")?)?,
    })
}

fn position_from_value(value: &Value) -> Option<Position> {
    Some(Position {
        line: value.get("line")?.as_u64()? as u32,
        character: value.get("character")?.as_u64()? as u32,
    })
}

fn diagnostics_from_pull(value: &Value, path: &RelPath) -> Vec<Diagnostic> {
    let items = value
        .get("items")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let range = range_from_value(item.get("range")?)?;
            let message = item.get("message")?.as_str()?.to_string();
            let severity = severity_from_u64(item.get("severity").and_then(Value::as_u64));
            let code = item.get("code").and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            Some(Diagnostic {
                path: path.clone(),
                range,
                severity,
                message,
                code,
            })
        })
        .collect()
}

fn diagnostic_from_lsp(path: RelPath, d: &lsp_types::Diagnostic) -> Diagnostic {
    Diagnostic {
        path,
        range: Range {
            start: Position {
                line: d.range.start.line,
                character: d.range.start.character,
            },
            end: Position {
                line: d.range.end.line,
                character: d.range.end.character,
            },
        },
        severity: match d.severity {
            Some(s) if s == lsp_types::DiagnosticSeverity::ERROR => Severity::Error,
            Some(s) if s == lsp_types::DiagnosticSeverity::WARNING => Severity::Warning,
            Some(s) if s == lsp_types::DiagnosticSeverity::INFORMATION => Severity::Information,
            Some(s) if s == lsp_types::DiagnosticSeverity::HINT => Severity::Hint,
            _ => Severity::Information,
        },
        message: d.message.clone(),
        code: match &d.code {
            Some(lsp_types::NumberOrString::Number(n)) => Some(n.to_string()),
            Some(lsp_types::NumberOrString::String(s)) => Some(s.clone()),
            None => None,
        },
    }
}

fn severity_from_u64(n: Option<u64>) -> Severity {
    match n {
        Some(1) => Severity::Error,
        Some(2) => Severity::Warning,
        Some(3) => Severity::Information,
        Some(4) => Severity::Hint,
        _ => Severity::Information,
    }
}

fn workspace_edit_from_value(value: &Value, root: &Path) -> WorkspaceEdit {
    let mut edits: Vec<(RelPath, Vec<TextEdit>)> = Vec::new();
    if let Some(doc_changes) = value
        .get("documentChanges")
        .and_then(Value::as_array)
        .filter(|arr| !arr.is_empty())
    {
        for change in doc_changes {
            let uri = change
                .get("textDocument")
                .and_then(|td| td.get("uri"))
                .and_then(Value::as_str);
            let Some(uri) = uri else {
                continue;
            };
            let Some(path) = uri_to_rel(uri, root) else {
                continue;
            };
            let file_edits: Vec<TextEdit> = change
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(text_edit_from_value)
                .collect();
            if !file_edits.is_empty() {
                edits.push((path, file_edits));
            }
        }
    } else if let Some(changes) = value.get("changes").and_then(Value::as_object) {
        for (uri, list) in changes {
            let Some(path) = uri_to_rel(uri, root) else {
                continue;
            };
            let Some(items) = list.as_array() else {
                continue;
            };
            let file_edits: Vec<TextEdit> = items.iter().filter_map(text_edit_from_value).collect();
            if !file_edits.is_empty() {
                edits.push((path, file_edits));
            }
        }
    }
    WorkspaceEdit { edits }
}

fn text_edit_from_value(value: &Value) -> Option<TextEdit> {
    Some(TextEdit {
        range: range_from_value(value.get("range")?)?,
        new_text: value.get("newText")?.as_str()?.to_string(),
    })
}

fn uri_to_rel(uri: &str, root: &Path) -> Option<RelPath> {
    let decoded = if let Some(rest) = uri.strip_prefix("file://") {
        percent_decode(rest)
    } else {
        percent_decode(uri)
    };
    let path = PathBuf::from(decoded);
    let root_abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let abs = if path.is_absolute() {
        path.canonicalize().unwrap_or(path)
    } else {
        return Some(RelPath::new(path.to_string_lossy()));
    };
    let rel = abs.strip_prefix(&root_abs).ok()?;
    Some(RelPath::new(rel.to_string_lossy()))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn client_capabilities() -> Value {
    json!({
        "workspace": {
            "applyEdit": false,
            "workspaceFolders": true,
            "configuration": true,
            "symbol": {},
        },
        "textDocument": {
            "publishDiagnostics": { "relatedInformation": true },
            "definition": { "linkSupport": true },
            "references": {},
            "rename": { "prepareSupport": true },
            "diagnostic": {},
            "synchronization": { "didSave": true },
        },
        "window": {
            "workDoneProgress": true,
            "showMessage": { "messageActionItem": { "additionalPropertiesSupport": false } },
        },
        "general": {
            "positionEncodings": ["utf-16"],
        },
    })
}

fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("id".into(), json!(id));
    obj.insert("method".into(), json!(method));
    obj.insert("params".into(), params);
    Value::Object(obj)
}

fn rpc_notification(method: &str, params: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("jsonrpc".into(), json!("2.0"));
    obj.insert("method".into(), json!(method));
    if !params.is_null() {
        obj.insert("params".into(), params);
    }
    Value::Object(obj)
}

fn write_rpc(shared: &Shared, payload: &Value) -> Result<(), LspError> {
    let body = serde_json::to_vec(payload).map_err(|err| LspError::Protocol(err.to_string()))?;
    let mut stdin = lock(&shared.stdin);
    let stdin = stdin.as_mut().ok_or(LspError::Crashed)?;
    write_frame(stdin, &body).map_err(|_| {
        shared.life.store(LIFE_DEAD, Ordering::SeqCst);
        fail_pending(shared);
        LspError::Crashed
    })
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(32 + body.len());
    write!(buf, "Content-Length: {}\r\n\r\n", body.len())?;
    buf.extend_from_slice(body);
    writer.write_all(&buf)?;
    writer.flush()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail_pending(shared: &Shared) {
    let waiters: Vec<_> = lock(&shared.pending).drain().map(|(_, tx)| tx).collect();
    for tx in waiters {
        let _ = tx.send(Err(LspError::Crashed));
    }
}

fn child_exited(shared: &Shared) -> bool {
    match lock(&shared.child).try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => true,
    }
}

fn reap_child(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
            Err(_) => return,
        }
    }
}

fn read_stdout(stdout: std::process::ChildStdout, shared: Arc<Shared>) {
    let mut reader = stdout;
    let mut parser = FrameParser::new();
    loop {
        loop {
            match parser.next() {
                Ok(Some(body)) => dispatch(&shared, &body),
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!(error = %err, "lsp framing error");
                    on_reader_exit(&shared);
                    return;
                }
            }
        }
        let mut tmp = [0u8; 8192];
        match reader.read(&mut tmp) {
            Ok(0) => {
                on_reader_exit(&shared);
                return;
            }
            Ok(n) => parser.ingest(&tmp[..n]),
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                on_reader_exit(&shared);
                return;
            }
        }
    }
}

fn on_reader_exit(shared: &Shared) {
    let previous =
        shared
            .life
            .compare_exchange(LIFE_RUNNING, LIFE_DEAD, Ordering::SeqCst, Ordering::SeqCst);
    if previous == Ok(LIFE_RUNNING) {
        fail_pending(shared);
    }
}

fn drain_stderr(mut stderr: std::process::ChildStderr, shared: Arc<Shared>) {
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                shared.stderr_bytes.fetch_add(n as u64, Ordering::Relaxed);
                tracing::debug!(
                    target: "astrolabe_lsp_stderr",
                    "{}",
                    String::from_utf8_lossy(&buf[..n])
                );
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn dispatch(shared: &Shared, body: &[u8]) {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(error = %err, "lsp: invalid JSON body");
            return;
        }
    };
    if let Some(batch) = msg.as_array() {
        for item in batch {
            dispatch_one(shared, item);
        }
        return;
    }
    dispatch_one(shared, &msg);
}

fn dispatch_one(shared: &Shared, msg: &Value) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id");
    match (method, id) {
        (Some(method), Some(id)) => handle_server_request(shared, method, msg.get("params"), id),
        (Some(method), None) => handle_notification(shared, method, msg.get("params")),
        (None, Some(id)) => handle_response(shared, id, msg),
        (None, None) => {}
    }
}

fn handle_response(shared: &Shared, id: &Value, msg: &Value) {
    let Some(n) = rpc_id_u64(id) else {
        tracing::debug!(?id, "lsp response with unrecognised id");
        return;
    };
    let tx = lock(&shared.pending).remove(&n);
    let Some(tx) = tx else {
        return;
    };
    let reply = if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(Value::as_i64);
        if code == Some(LSP_CONTENT_MODIFIED) {
            Err(LspError::ContentModified)
        } else {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("language server returned an error");
            Err(LspError::Protocol(message.to_string()))
        }
    } else {
        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
    };
    let _ = tx.send(reply);
}

fn handle_notification(shared: &Shared, method: &str, params: Option<&Value>) {
    let params = params.cloned().unwrap_or(Value::Null);
    if method == "$/progress" {
        track_progress(shared, &params);
    }
    if method == "textDocument/publishDiagnostics" {
        if let Ok(parsed) = serde_json::from_value::<PublishDiagnosticsLoose>(params.clone()) {
            lock(&shared.diagnostics).insert(parsed.uri, parsed.diagnostics);
        }
    }
    let mut log = lock(&shared.notifications);
    if log.len() < 64 {
        log.push((method.to_string(), params));
    }
}

/// Follow `begin` / `end` of a work-done progress token.
///
/// `rust-analyzer` and `gopls` both report initial indexing this way, so an
/// outstanding token is the one portable signal that the server's answers are
/// not yet complete.
fn track_progress(shared: &Shared, params: &Value) {
    let Some(token) = params.get("token") else {
        return;
    };
    // Tokens may be strings or integers; normalise so both compare equal.
    let token = match token {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match params.pointer("/value/kind").and_then(Value::as_str) {
        Some("begin") => {
            shared.saw_progress.store(true, Ordering::SeqCst);
            // Indexing (or other work) resumed — do not keep a stale ready latch.
            shared.ready_confirmed.store(false, Ordering::Release);
            lock(&shared.active_progress).insert(token);
        }
        Some("end") => {
            lock(&shared.active_progress).remove(&token);
        }
        _ => {}
    }
}

fn handle_server_request(shared: &Shared, method: &str, params: Option<&Value>, id: &Value) {
    let params = params.cloned().unwrap_or(Value::Null);
    tracing::debug!(method, "lsp server request");
    let result = reverse_request_result(shared, method, &params);
    let response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    if let Err(err) = write_rpc(shared, &response) {
        tracing::debug!(error = %err, method, "failed to answer server request");
    }
}

fn reverse_request_result(shared: &Shared, method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => {
            let n = params
                .get("items")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            Value::Array(vec![Value::Null; n])
        }
        "workspace/workspaceFolders" => lock(&shared.workspace_folders)
            .clone()
            .unwrap_or(Value::Null),
        "workspace/applyEdit" => json!({ "applied": false }),
        "window/showDocument" => json!({ "success": false }),
        _ => Value::Null,
    }
}

fn rpc_id_u64(id: &Value) -> Option<u64> {
    match id {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| u64::try_from(i).ok())),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

#[derive(serde::Deserialize)]
struct PublishDiagnosticsLoose {
    uri: String,
    #[serde(default)]
    diagnostics: Vec<lsp_types::Diagnostic>,
}

/// Incremental LSP header/body splitter. Incomplete input is `Ok(None)`.
struct FrameParser {
    buf: Vec<u8>,
}

impl FrameParser {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn ingest(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn next(&mut self) -> Result<Option<Vec<u8>>, LspError> {
        let Some(header_end) = find_header_end(&self.buf) else {
            if self.buf.len() > MAX_HEADER_BYTES {
                return Err(LspError::Protocol(format!(
                    "LSP header exceeded {MAX_HEADER_BYTES} bytes without a terminator"
                )));
            }
            return Ok(None);
        };
        let header = &self.buf[..header_end];
        let Some(content_len) = parse_content_length(header) else {
            return Err(LspError::Protocol(
                "LSP message missing Content-Length header".into(),
            ));
        };
        if content_len > MAX_BODY_BYTES {
            return Err(LspError::Protocol(format!(
                "LSP payload {content_len} exceeds {MAX_BODY_BYTES} bytes"
            )));
        }
        let total = header_end.saturating_add(content_len);
        if self.buf.len() < total {
            return Ok(None);
        }
        let body = self.buf[header_end..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(body))
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    let crlf = memmem::find(buf, b"\r\n\r\n").map(|i| i + 4);
    let lf = memmem::find(buf, b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn parse_content_length(header: &[u8]) -> Option<usize> {
    for line in header.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = memchr::memchr(b':', line) else {
            continue;
        };
        let name = line[..colon].trim_ascii();
        if !name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let value = line[colon + 1..].trim_ascii();
        let s = std::str::from_utf8(value).ok()?;
        return s.parse().ok();
    }
    None
}

fn path_to_file_uri(path: &Path) -> String {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let rendered = path.to_string_lossy().replace('\\', "/");
    let mut uri = String::from("file://");
    if cfg!(windows) && !rendered.starts_with('/') {
        uri.push('/');
    }
    for &b in rendered.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b':' => {
                uri.push(b as char)
            }
            _ => uri.push_str(&format!("%{b:02X}")),
        }
    }
    uri
}

fn rss_bytes(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        for line in status.lines() {
            let Some(rest) = line.strip_prefix("VmRSS:") else {
                continue;
            };
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let kb: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kb.saturating_mul(1024))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicU64;

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    const FAKE_SERVER: &str = r#"
import json, os, sys, time

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if b":" in line:
            k, v = line.decode("utf-8", "replace").split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(n)
    if len(body) != n:
        return None
    return json.loads(body)

def frame(obj):
    data = json.dumps(obj, separators=(",", ":")).encode()
    return ("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data

def send(obj):
    sys.stdout.buffer.write(frame(obj))
    sys.stdout.buffer.flush()

def send_split(obj):
    blob = frame(obj)
    idx = blob.find(b"\r\n\r\n") + 4
    sys.stdout.buffer.write(blob[:idx])
    sys.stdout.buffer.flush()
    time.sleep(0.05)
    sys.stdout.buffer.write(blob[idx:])
    sys.stdout.buffer.flush()

def reply(msg, result):
    send({"jsonrpc": "2.0", "id": msg["id"], "result": result})

if "--flood" in sys.argv:
    chunk = b"e" * 65536
    for _ in range(64):
        sys.stderr.buffer.write(chunk)
        sys.stderr.buffer.flush()

BATCH = []

while True:
    msg = read_msg()
    if msg is None:
        break
    method = msg.get("method")
    if method is None:
        continue
    if method == "exit":
        break
    if method == "initialized" or "id" not in msg:
        continue
    if method == "initialize":
        reply(msg, {"capabilities": {"textDocumentSync": 1}, "serverInfo": {"name": "astrolabe-fake"}})
    elif method == "shutdown":
        reply(msg, None)
    elif method == "ping":
        params = msg.get("params") or {}
        if isinstance(params, dict) and params.get("batch"):
            BATCH.append(msg)
            if len(BATCH) >= int(params["batch"]):
                for m in reversed(list(BATCH)):
                    reply(m, m.get("params"))
                BATCH.clear()
        else:
            delay = 0.0
            if isinstance(params, dict):
                delay = float(params.get("delay") or 0)
            if delay:
                time.sleep(delay)
            reply(msg, params)
    elif method == "hang":
        time.sleep(30)
        reply(msg, {})
    elif method == "crash":
        os._exit(1)
    elif method == "fail":
        send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32000, "message": "intentional failure"}})
    elif method == "probe-reverse":
        send({"jsonrpc": "2.0", "id": "rev-1", "method": "window/workDoneProgress/create", "params": {"token": "t1"}})
        resp = read_msg()
        reply(msg, {"reverse_id": None if resp is None else resp.get("id"), "reverse_result": None if resp is None else resp.get("result")})
    elif method == "probe-config":
        send({"jsonrpc": "2.0", "id": 77, "method": "workspace/configuration", "params": {"items": [{"section": "a"}, {"section": "b"}]}})
        resp = read_msg()
        reply(msg, resp)
    elif method == "listen-diag":
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {
            "uri": "file:///tmp/fake.py",
            "diagnostics": [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}, "message": "boom", "severity": 1}]
        }})
        reply(msg, {"ok": True})
    elif method == "listen-split":
        send_split({"jsonrpc": "2.0", "id": msg["id"], "result": {"split": True}})
    elif method == "listen-sticky":
        note = {"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": {"uri": "file:///sticky.py", "diagnostics": []}}
        resp = {"jsonrpc": "2.0", "id": msg["id"], "result": {"sticky": True}}
        sys.stdout.buffer.write(frame(note) + frame(resp))
        sys.stdout.buffer.flush()
    elif method == "blob":
        n = int((msg.get("params") or {}).get("n") or 0)
        reply(msg, {"blob": "x" * n})
    elif method in ("textDocument/references", "textDocument/definition"):
        params = msg.get("params") or {}
        doc = (params.get("textDocument") or {}).get("uri")
        pos = params.get("position") or {"line": 0, "character": 0}
        reply(msg, [{"uri": doc, "range": {"start": pos, "end": pos}}])
    elif method == "textDocument/diagnostic":
        reply(msg, {"kind": "full", "items": []})
    elif method == "textDocument/rename":
        params = msg.get("params") or {}
        doc = (params.get("textDocument") or {}).get("uri")
        pos = params.get("position") or {"line": 0, "character": 0}
        new = params.get("newName") or "x"
        reply(msg, {"changes": {doc: [{"range": {"start": pos, "end": pos}, "newText": new}]}})
    else:
        reply(msg, None)
"#;

    struct TmpDir(PathBuf);

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique_temp() -> TmpDir {
        let dir = std::env::temp_dir().join(format!(
            "astrolabe-lsp-{}-{}",
            std::process::id(),
            TEST_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        TmpDir(dir)
    }

    fn python3() -> PathBuf {
        for name in ["python3", "python"] {
            if let Ok(status) = Command::new(name)
                .args([
                    "-c",
                    "import sys; raise SystemExit(0 if sys.version_info[0] >= 3 else 1)",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
            {
                if status.success() {
                    return PathBuf::from(name);
                }
            }
        }
        panic!("transport tests require python3 on PATH");
    }

    fn spawn_fake(flood: bool, timeout: Duration) -> (LspTransport, TmpDir) {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).expect("write fake server");
        let mut args = vec!["-u".into(), script.to_string_lossy().into_owned()];
        if flood {
            args.push("--flood".into());
        }
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args,
            install_hint: "install python3".into(),
        };
        let transport =
            LspTransport::spawn_in(&spec, &tmp.0, timeout).expect("spawn fake language server");
        (transport, tmp)
    }

    fn encode_frame(body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write!(buf, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        buf.extend_from_slice(body);
        buf
    }

    struct Chunked {
        chunks: Vec<Vec<u8>>,
        index: usize,
        offset: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.index >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.index][self.offset..];
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            self.offset += n;
            if self.offset >= self.chunks[self.index].len() {
                self.index += 1;
                self.offset = 0;
            }
            Ok(n)
        }
    }

    fn read_all_messages(reader: &mut impl Read) -> Vec<Vec<u8>> {
        let mut parser = FrameParser::new();
        let mut out = Vec::new();
        loop {
            while let Some(body) = parser.next().expect("frame") {
                out.push(body);
            }
            let mut tmp = [0u8; 64];
            match reader.read(&mut tmp).expect("read") {
                0 => break,
                n => parser.ingest(&tmp[..n]),
            }
        }
        while let Some(body) = parser.next().expect("frame") {
            out.push(body);
        }
        out
    }

    #[test]
    fn parser_half_packet_header_then_body() {
        let body = br#"{"ok":true}"#;
        let frame = encode_frame(body);
        let split = frame.iter().position(|&b| b == b'{').unwrap();
        let mut parser = FrameParser::new();
        parser.ingest(&frame[..split]);
        assert!(parser.next().unwrap().is_none());
        parser.ingest(&frame[split..]);
        assert_eq!(parser.next().unwrap().unwrap(), body);
    }

    #[test]
    fn parser_sticky_packets() {
        let a = encode_frame(br#"{"a":1}"#);
        let b = encode_frame(br#"{"b":2}"#);
        let mut combined = a;
        combined.extend_from_slice(&b);
        let mut parser = FrameParser::new();
        parser.ingest(&combined);
        assert_eq!(parser.next().unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(parser.next().unwrap().unwrap(), br#"{"b":2}"#);
        assert!(parser.next().unwrap().is_none());
    }

    #[test]
    fn parser_large_message() {
        let blob = "x".repeat(1_000_000);
        let json = serde_json::to_vec(&json!({ "blob": blob })).unwrap();
        let frame = encode_frame(&json);
        let mut parser = FrameParser::new();
        parser.ingest(&frame);
        assert_eq!(parser.next().unwrap().unwrap(), json);
    }

    #[test]
    fn parser_extra_headers_and_lf_only() {
        let body = br#"{}"#;
        let mut frame =
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\nContent-Length: 2\n\n"
                .to_vec();
        frame.extend_from_slice(body);
        let mut parser = FrameParser::new();
        parser.ingest(&frame);
        assert_eq!(parser.next().unwrap().unwrap(), body);
    }

    #[test]
    fn parser_rejects_missing_content_length() {
        let mut parser = FrameParser::new();
        parser.ingest(b"X-Ignore: 1\r\n\r\n{}");
        let err = parser.next().unwrap_err();
        assert!(matches!(err, LspError::Protocol(_)));
    }

    #[test]
    fn parser_rejects_oversize_payload() {
        let header = format!("Content-Length: {}\r\n\r\n", MAX_BODY_BYTES + 1);
        let mut parser = FrameParser::new();
        parser.ingest(header.as_bytes());
        let err = parser.next().unwrap_err();
        assert!(matches!(err, LspError::Protocol(_)));
    }

    #[test]
    fn parser_one_byte_reads_reassemble_two_frames() {
        let frames = [encode_frame(br#"{"a":1}"#), encode_frame(br#"{"b":2}"#)].concat();
        let mut reader = Chunked {
            chunks: frame_as_bytes(frames),
            index: 0,
            offset: 0,
        };
        let messages = read_all_messages(&mut reader);
        assert_eq!(
            messages,
            vec![br#"{"a":1}"#.to_vec(), br#"{"b":2}"#.to_vec()]
        );
    }

    fn frame_as_bytes(bytes: Vec<u8>) -> Vec<Vec<u8>> {
        bytes.into_iter().map(|b| vec![b]).collect()
    }

    #[test]
    fn missing_binary_is_unavailable() {
        let spec = ServerSpec {
            language: Language::Rust,
            command: PathBuf::from("/this/does/not/exist/astrolabe-lsp-missing"),
            args: vec![],
            install_hint: "install rust-analyzer".into(),
        };
        let err = LspTransport::spawn(&spec, Duration::from_secs(1)).unwrap_err();
        match err {
            LspError::Unavailable(Language::Rust, hint) => {
                assert!(hint.contains("rust-analyzer"));
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn ping_pairs_request_and_response() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("ping", json!({"token": "abc"})).unwrap();
        assert_eq!(result["token"], "abc");
        t.shutdown().unwrap();
    }

    #[test]
    fn handshake_initialize_and_shutdown() {
        let tmp = unique_temp();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let t = LspTransport::connect(&spec, &tmp.0, Duration::from_secs(5)).unwrap();
        let info = t.initialize_result().unwrap();
        assert_eq!(info["serverInfo"]["name"], "astrolabe-fake");
        assert!(t.is_alive());
        t.shutdown().unwrap();
    }

    #[test]
    fn concurrent_requests_match_by_id() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let t = Arc::new(t);
        let start = Instant::now();
        let mut joins = Vec::new();
        for i in 0..8 {
            let t = Arc::clone(&t);
            joins.push(thread::spawn(move || {
                t.request("ping", json!({"i": i, "batch": 8})).unwrap()
            }));
        }
        let mut seen = Vec::new();
        for join in joins {
            let value = join.join().expect("thread");
            seen.push(value["i"].as_u64().unwrap());
        }
        seen.sort();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "batched in-flight pings took {:?}",
            start.elapsed()
        );
        t.shutdown().unwrap();
    }

    #[test]
    fn publish_diagnostics_are_captured() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        t.request("listen-diag", json!({})).unwrap();
        let diags = t.published_diagnostics();
        let items = diags.get("file:///tmp/fake.py").expect("uri");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "boom");
        t.shutdown().unwrap();
    }

    #[test]
    fn reverse_progress_request_is_answered() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("probe-reverse", json!({})).unwrap();
        assert_eq!(result["reverse_id"], "rev-1");
        t.shutdown().unwrap();
    }

    #[test]
    fn reverse_configuration_returns_matching_nulls() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("probe-config", json!({})).unwrap();
        assert_eq!(result["result"], json!([null, null]));
        t.shutdown().unwrap();
    }

    #[test]
    fn process_split_frame_still_completes() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("listen-split", json!({})).unwrap();
        assert_eq!(result["split"], true);
        t.shutdown().unwrap();
    }

    #[test]
    fn process_sticky_frames_dispatch_notification_and_response() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let result = t.request("listen-sticky", json!({})).unwrap();
        assert_eq!(result["sticky"], true);
        assert!(t.published_diagnostics().contains_key("file:///sticky.py"));
        t.shutdown().unwrap();
    }

    #[test]
    fn large_payload_round_trip() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(10));
        let n = 200_000;
        let result = t.request("blob", json!({ "n": n })).unwrap();
        assert_eq!(result["blob"].as_str().unwrap().len(), n);
        t.shutdown().unwrap();
    }

    #[test]
    fn timeout_returns_timeout_error() {
        let (t, _tmp) = spawn_fake(false, Duration::from_millis(150));
        let start = Instant::now();
        let err = t.request("hang", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Timeout(_)));
        assert!(start.elapsed() < Duration::from_secs(2));
        t.shutdown().unwrap();
    }

    #[test]
    fn crash_surfaces_as_crashed_not_hang() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let err = t.request("crash", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Crashed));
        let err = t.request("ping", json!({})).unwrap_err();
        assert!(matches!(err, LspError::Crashed));
    }

    #[test]
    fn protocol_error_is_surfaced() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let err = t.request("fail", json!({})).unwrap_err();
        match err {
            LspError::Protocol(message) => assert!(message.contains("intentional")),
            other => panic!("expected Protocol, got {other:?}"),
        }
        t.shutdown().unwrap();
    }

    #[test]
    fn stderr_flood_does_not_deadlock() {
        let (t, _tmp) = spawn_fake(true, Duration::from_secs(10));
        let result = t.request("ping", json!({"ok": true})).unwrap();
        assert_eq!(result["ok"], true);
        assert!(
            t.stderr_bytes() >= 64 * 65536,
            "stderr was not drained: {}",
            t.stderr_bytes()
        );
        t.shutdown().unwrap();
    }

    #[test]
    fn memory_bytes_is_observable_on_this_os() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let bytes = t.memory_bytes();
        assert!(bytes.is_some() && bytes.unwrap() > 0, "rss={bytes:?}");
        t.shutdown().unwrap();
    }

    #[test]
    fn transport_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LspTransport>();
        assert_send_sync::<LspSession>();
        assert_send_sync::<StdioSpawn>();
    }

    #[test]
    fn stdio_spawn_language_server_queries() {
        let tmp = unique_temp();
        fs::write(tmp.0.join("hello.py"), "def greet():\n    return 1\n").unwrap();
        let script = tmp.0.join("fake_lsp.py");
        fs::write(&script, FAKE_SERVER).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let spawn = StdioSpawn::new(&tmp.0).with_timeout(Duration::from_secs(5));
        let server = spawn.spawn(&spec, tmp.0.as_path()).unwrap();
        let path = RelPath::new("hello.py");
        let pos = Position {
            line: 0,
            character: 4,
        };
        let refs = server.references(&path, pos).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path.as_str(), "hello.py");
        let defs = server.definition(&path, pos).unwrap();
        assert_eq!(defs.len(), 1);
        let edit = server.prepare_rename(&path, pos, "hi").unwrap();
        assert_eq!(edit.edits.len(), 1);
        assert_eq!(edit.edits[0].1[0].new_text, "hi");
        assert!(server.diagnostics(&path).unwrap().is_empty());
        assert!(server.memory_bytes().unwrap() > 0);
        server.shutdown().unwrap();
    }

    #[test]
    fn workspace_edit_prefers_document_changes() {
        let tmp = unique_temp();
        let file_a = tmp.0.join("a.rs");
        let file_b = tmp.0.join("b.rs");
        fs::write(&file_a, "").unwrap();
        fs::write(&file_b, "").unwrap();
        let uri_a = path_to_file_uri(&file_a);
        let uri_b = path_to_file_uri(&file_b);

        let val = json!({
            "changes": {
                uri_a.clone(): [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "from_changes"
                    }
                ]
            },
            "documentChanges": [
                {
                    "textDocument": { "uri": uri_b },
                    "edits": [
                        {
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 5 }
                            },
                            "newText": "from_doc_changes"
                        }
                    ]
                }
            ]
        });
        let edit = workspace_edit_from_value(&val, &tmp.0);
        assert_eq!(edit.edits.len(), 1);
        assert_eq!(edit.edits[0].0.as_str(), "b.rs");
        assert_eq!(edit.edits[0].1[0].new_text, "from_doc_changes");

        // When documentChanges is empty, fall back to changes
        let val_empty_doc = json!({
            "changes": {
                uri_a: [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "from_changes"
                    }
                ]
            },
            "documentChanges": []
        });
        let edit_empty_doc = workspace_edit_from_value(&val_empty_doc, &tmp.0);
        assert_eq!(edit_empty_doc.edits.len(), 1);
        assert_eq!(edit_empty_doc.edits[0].0.as_str(), "a.rs");
        assert_eq!(edit_empty_doc.edits[0].1[0].new_text, "from_changes");
    }

    #[test]
    fn warm_lsp_wait_until_ready_fast_path() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        assert!(t.wait_until_ready(Duration::from_millis(500)));
        assert!(t.shared.ready_confirmed.load(Ordering::Acquire));

        // Subsequent call returns immediately via fast path
        let start = Instant::now();
        assert!(t.wait_until_ready(Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_millis(10));
        t.shutdown().unwrap();
    }

    #[test]
    fn write_rpc_failure_fails_pending_requests() {
        let (t, _tmp) = spawn_fake(false, Duration::from_secs(5));
        let (tx, rx) = mpsc::channel();
        lock(&t.shared.pending).insert(999, tx);

        // Kill child so write_frame fails on broken pipe
        let _ = lock(&t.shared.child).kill();
        let _ = lock(&t.shared.child).wait();

        // Small writes might succeed initially into kernel pipe buffers; write until EPIPE trips
        let mut res = Ok(());
        for _ in 0..100 {
            res = write_rpc(&t.shared, &json!({"test": 1, "payload": "x".repeat(4096)}));
            if res.is_err() {
                break;
            }
        }
        assert!(matches!(res, Err(LspError::Crashed)));
        assert_eq!(t.shared.life.load(Ordering::SeqCst), LIFE_DEAD);

        // Verify the pending request was woken up and failed with Crashed immediately
        let pending_res = rx
            .recv_timeout(Duration::from_millis(500))
            .expect("pending request woke up");
        assert!(matches!(pending_res, Err(LspError::Crashed)));
    }

    #[test]
    fn diagnostics_fallback_empty_returns_protocol_error() {
        let tmp = unique_temp();
        let file = tmp.0.join("test.py");
        fs::write(&file, "x = 1\n").unwrap();

        let script = tmp.0.join("fake_lsp.py");
        // Fake server where textDocument/diagnostic fails with protocol error, and no publishDiagnostics are pushed
        let fake_code = r#"
import json, sys

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            return None
        if line in (b"\r\n", b"\n"):
            break
        if b":" in line:
            k, v = line.decode("utf-8", "replace").split(":", 1)
            headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    body = sys.stdin.buffer.read(n)
    if len(body) != n:
        return None
    return json.loads(body)

def reply_err(msg, code, message):
    data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": code, "message": message}}, separators=(",", ":")).encode()
    sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
    sys.stdout.buffer.flush()

while True:
    msg = read_msg()
    if msg is None or msg.get("method") == "exit":
        break
    if "id" not in msg:
        continue
    if msg.get("method") == "initialize":
        data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}}, separators=(",", ":")).encode()
        sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
        sys.stdout.buffer.flush()
    elif msg.get("method") == "textDocument/diagnostic":
        reply_err(msg, -32601, "Method not found")
    else:
        data = json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": None}, separators=(",", ":")).encode()
        sys.stdout.buffer.write(("Content-Length: %d\r\n\r\n" % len(data)).encode("ascii") + data)
        sys.stdout.buffer.flush()
"#;
        fs::write(&script, fake_code).unwrap();
        let spec = ServerSpec {
            language: Language::Python,
            command: python3(),
            args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            install_hint: "install python3".into(),
        };
        let spawn = StdioSpawn::new(&tmp.0).with_timeout(Duration::from_secs(5));
        let server = spawn.spawn(&spec, tmp.0.as_path()).unwrap();
        let path = RelPath::new("test.py");
        let res = server.diagnostics(&path);
        match res {
            Err(LspError::Protocol(msg)) => {
                assert!(msg.contains("pull diagnostic unsupported and push diagnostics empty"));
            }
            other => panic!("expected Protocol error, got {other:?}"),
        }
        server.shutdown().unwrap();
    }
}
