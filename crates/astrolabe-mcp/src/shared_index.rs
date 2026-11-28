//! One index per repository across sessions: "leader hosts, followers forward".
//!
//! Several MCP sessions on the same repository (Claude Code + Cursor, or a
//! handful of sub-agents) used to each build the full graph, NameIndex,
//! FileCache, watcher and language servers: N sessions, N copies. Here the
//! process that wins `indexer.lock` (see `astrolabe_core::elect`) also serves
//! a user-private IPC endpoint (Unix domain socket on Unix, loopback TCP +
//! endpoint JSON with a 256-bit `auth_token` on Windows). Later sessions
//! connect, hand-shake on version, and forward `tools/call` / `resources/read`
//! instead of building anything; they are a thin stdio shim of a few MB.
//!
//! * **Unix socket:** `$XDG_RUNTIME_DIR/astrolabe/<hash(root)>.sock` on Linux
//!   (`~/.cache/astrolabe/run/` when unset), `~/Library/Caches/astrolabe/` on
//!   macOS. Directory 0700, socket 0600. Never under the repo: macOS
//!   `sun_path` is 104 bytes. `ASTROLABE_SHARED_INDEX_DIR` overrides the
//!   directory (tests).
//! * **Windows loopback TCP:** `127.0.0.1:<ephemeral>` advertised in
//!   `<runtime_dir>/<hash(root)>.endpoint.json` (`ASTROLABE_SHARED_INDEX_DIR`,
//!   then `%LOCALAPPDATA%\astrolabe\run`, then `%USERPROFILE%\.astrolabe\run`;
//!   `None` if none is set), authenticated with a 256-bit CSPRNG
//!   `auth_token` verified in constant time before any handshake reply.
//! * **Handshake:** protocol, package version, `EXTRACTOR_VERSION`,
//!   `SCHEMA_VERSION`, `CACHE_FORMAT_VERSION`, a hash of the full tool
//!   catalog, a hash of the `ASTROLABE_*` settings, toolchain env hash, and
//!   the normalized canonical root (`path_norm_key`). Any difference -> this
//!   session indexes on its own (the old behaviour).
//! * **Per-session state** (once-only legends, coverage note, loop guard,
//!   dedup tables, resources, the client context and its output shape) lives
//!   in a per-connection view on the leader, keyed by the connection.
//! * **Leader exit:** close the endpoint (followers see EOF), then release the
//!   lock. Followers race for the lock: the winner warm-loads from redb and
//!   binds the endpoint, the others reconnect. Calls arriving meanwhile wait up
//!   to [`QUEUE_BUDGET`], then the session falls back to its own index.
//! * **Fault isolation:** a leader that stops answering (tool timeout + 30 s)
//!   is abandoned and the follower indexes on its own.
//! * `ASTROLABE_SHARED_INDEX=off` disables all of it. Multi-project mode always
//!   runs independently.
// Platforms other than Unix and Windows use the stub below; protocol types are unused there.
#![cfg_attr(not(any(unix, windows)), allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use astrolabe_core::elect::IndexerLock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::ClientContext;

/// Wire protocol version; bump on any frame change.
pub(crate) const PROTO: u32 = 3;
/// How long a call waits for a leader hand-over before going local.
pub(crate) const QUEUE_BUDGET: Duration = Duration::from_secs(3);
/// At startup, a held lock with no reachable socket (an older or opted-out
/// process holds it) is waited on this long before indexing independently.
pub(crate) const STARTUP_GRACE: Duration = Duration::from_millis(500);

/// Env vars that do not change what a tool answers, or are carried per
/// session, and so may differ between leader and follower.
const SETTINGS_IGNORED: &[&str] = &[
    "ASTROLABE_SHARED_INDEX",
    "ASTROLABE_SHARED_INDEX_DIR",
    "ASTROLABE_ROOT",
    "ASTROLABE_CONTEXT",
    "ASTROLABE_ALWAYS_LOAD",
    "ASTROLABE_BUILD_WAIT_MS",
];

/// Environment a language server or toolchain launched for a forwarded
/// call would inherit (`get_diagnostics` toolchain path, `lsp=true`, rename,
/// LSP fallbacks). The leader spawns them with *its* environment, so a
/// follower whose toolchain env differs must not share.
const TOOLCHAIN_ENV: &[&str] = &[
    "PATH",
    "GOPATH",
    "GOROOT",
    "GOFLAGS",
    "GOWORK",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "VIRTUAL_ENV",
    "NODE_PATH",
];

/// Hash of [`TOOLCHAIN_ENV`] (unset and empty are distinct).
pub(crate) fn toolchain_env_hash() -> String {
    let mut buf = Vec::new();
    for key in TOOLCHAIN_ENV {
        buf.extend_from_slice(key.as_bytes());
        match std::env::var_os(key) {
            Some(value) => {
                buf.push(b'=');
                buf.extend_from_slice(value.to_string_lossy().as_bytes());
            }
            None => buf.push(b'!'),
        }
        buf.push(0);
    }
    format!("{:016x}", fnv64(&buf))
}

/// `ASTROLABE_SHARED_INDEX` (default on; `off`/`0`/`false`/`no` disables).
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("ASTROLABE_SHARED_INDEX")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "off" | "0" | "false" | "no"
    )
}

/// FNV-1a 64: stable across builds and toolchains (std's hasher is not),
/// which matters because two releases must agree on the socket name to see
/// each other's version and refuse politely.
pub(crate) fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Platform-normalized canonical root key used for handshake comparison and
/// endpoint hashing.
pub(crate) fn root_key(root: &Path) -> String {
    astrolabe_core::paths::path_norm_key(root)
}

/// Stable 64-bit hash of the normalized root key.
pub(crate) fn root_hash(root: &Path) -> u64 {
    fnv64(root_key(root).as_bytes())
}

/// Hash of every `ASTROLABE_*` setting that can change an answer.
pub(crate) fn settings_hash() -> String {
    let mut vars: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.to_string_lossy().into_owned())))
        .filter(|(k, _)| k.starts_with("ASTROLABE_") && !SETTINGS_IGNORED.contains(&k.as_str()))
        .collect();
    vars.sort();
    let mut buf = String::new();
    for (k, v) in vars {
        buf.push_str(&k);
        buf.push('=');
        buf.push_str(&v);
        buf.push('\0');
    }
    format!("{:016x}", fnv64(buf.as_bytes()))
}

/// [`ClientContext`] on the wire: the leader rebuilds the follower's own
/// context (a custom YAML path may not resolve from the leader's cwd).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WireContext {
    pub name: String,
    pub load_key: String,
    pub structured_tool_output: Option<bool>,
    pub excluded_tools: Vec<String>,
    pub notes: String,
}

impl From<&ClientContext> for WireContext {
    fn from(c: &ClientContext) -> Self {
        Self {
            name: c.name.clone(),
            load_key: c.load_key.clone(),
            structured_tool_output: c.structured_tool_output,
            excluded_tools: c.excluded_tools.clone(),
            notes: c.notes.clone(),
        }
    }
}

impl From<WireContext> for ClientContext {
    fn from(w: WireContext) -> Self {
        ClientContext {
            name: w.name,
            load_key: w.load_key,
            structured_tool_output: w.structured_tool_output,
            excluded_tools: w.excluded_tools,
            notes: w.notes,
        }
    }
}

/// First frame a follower sends.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Hello {
    pub proto: u32,
    pub pkg: String,
    pub extractor: u32,
    pub schema: u32,
    pub cache_format: u32,
    pub tools_hash: String,
    pub settings_hash: String,
    /// [`toolchain_env_hash`].
    #[serde(default)]
    pub toolchain_hash: String,
    pub root: String,
    pub session_id: String,
    pub pid: u32,
    pub cwd: String,
    pub context: WireContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_token: Option<String>,
}

impl Hello {
    pub(crate) fn new(root: &Path, tools_hash: String, context: &ClientContext) -> Self {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        Self {
            proto: PROTO,
            pkg: env!("CARGO_PKG_VERSION").to_string(),
            extractor: astrolabe_core::store::EXTRACTOR_VERSION,
            schema: astrolabe_core::store::SCHEMA_VERSION,
            cache_format: astrolabe_core::store::CACHE_FORMAT_VERSION,
            tools_hash,
            settings_hash: settings_hash(),
            toolchain_hash: toolchain_env_hash(),
            root: root_key(root),
            session_id: format!("{pid}-{nanos:x}"),
            pid,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            context: context.into(),
            auth_token: None,
        }
    }

    /// Why `other` (a follower) cannot share this leader, if it cannot.
    pub(crate) fn mismatch(&self, other: &Hello) -> Option<String> {
        let checks: [(&str, String, String); 8] = [
            ("proto", self.proto.to_string(), other.proto.to_string()),
            ("version", self.pkg.clone(), other.pkg.clone()),
            (
                "extractor",
                self.extractor.to_string(),
                other.extractor.to_string(),
            ),
            (
                "schema",
                format!("{}/{}", self.schema, self.cache_format),
                format!("{}/{}", other.schema, other.cache_format),
            ),
            ("tools", self.tools_hash.clone(), other.tools_hash.clone()),
            (
                "settings",
                self.settings_hash.clone(),
                other.settings_hash.clone(),
            ),
            (
                "toolchain env",
                self.toolchain_hash.clone(),
                other.toolchain_hash.clone(),
            ),
            ("root", self.root.clone(), other.root.clone()),
        ];
        checks
            .into_iter()
            .find(|(_, mine, theirs)| mine != theirs)
            .map(|(what, mine, theirs)| {
                format!("{what} differs (leader {mine}, follower {theirs})")
            })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct HelloReply {
    pub ok: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub leader_pid: u32,
}

/// Follower → leader frames (one JSON object per line).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Req {
    Call {
        id: u64,
        name: String,
        #[serde(default)]
        args: Option<serde_json::Map<String, Value>>,
    },
    Read {
        id: u64,
        uri: String,
    },
    Cancel {
        id: u64,
    },
}

/// Leader → follower frame: `ok` is the result JSON, `err` an `ErrorData`.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Resp {
    pub id: u64,
    #[serde(default)]
    pub ok: Option<Value>,
    #[serde(default)]
    pub err: Option<Value>,
}

/// What a forwarded call needs, without the id.
#[derive(Clone, Debug)]
pub(crate) enum Op {
    Call {
        name: String,
        args: Option<serde_json::Map<String, Value>>,
    },
    Read {
        uri: String,
    },
}

/// Outcome of [`Shared::forward`].
#[derive(Debug)]
pub(crate) enum Forwarded {
    /// The leader answered: `Ok(result json)` or `Err(ErrorData json)`.
    Done(Result<Value, Value>),
    /// Serve locally (this process is the leader, or fell back).
    Local,
    /// The client cancelled while waiting.
    Cancelled,
}

/// One follower's view on the leader. Implemented by the server.
pub(crate) trait SessionView: Send + Sync {
    fn call(
        &self,
        name: &str,
        args: Option<serde_json::Map<String, Value>>,
        cancel: Arc<AtomicBool>,
    ) -> Result<Value, Value>;
    fn read(&self, uri: &str) -> Result<Value, Value>;
}

/// The server side of sharing, implemented by the server.
pub(crate) trait Host: Send + Sync {
    /// Fresh per-session state over the shared index.
    fn open_session(&self, context: ClientContext) -> Arc<dyn SessionView>;
    /// Hold `lock` as this root's indexer leader.
    fn adopt_lock(&self, lock: IndexerLock);
    /// Start (once) the local watcher + index build.
    fn start_local(&self);
    /// Hold this root's indexer lock if free (`refresh_leader`); `true` when
    /// this process holds it afterwards.
    fn try_take_lock(&self) -> bool;
    /// Drop the indexer lock (a promotion that lost a race with shutdown).
    fn release_lock(&self);
}

/// Socket directory: see the module docs.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn socket_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ASTROLABE_SHARED_INDEX_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    if cfg!(target_os = "macos") {
        return home.map(|h| h.join("Library").join("Caches").join("astrolabe"));
    }
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        let run = PathBuf::from(run);
        if run.is_dir() {
            return Some(run.join("astrolabe"));
        }
    }
    home.map(|h| h.join(".cache").join("astrolabe").join("run"))
}

/// Longest socket path accepted (macOS `sun_path` is 104 bytes incl. NUL).
#[cfg_attr(not(unix), allow(dead_code))]
const MAX_SOCKET_PATH: usize = 100;

/// Socket for `root` inside `dir`.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn socket_path_in(dir: &Path, root: &Path) -> Option<PathBuf> {
    let name = format!("{:016x}.sock", root_hash(root));
    let path = dir.join(name);
    (path.as_os_str().len() <= MAX_SOCKET_PATH).then_some(path)
}

/// Endpoint metadata file for `root` inside `dir` (Windows loopback TCP).
pub(crate) fn endpoint_path_in(dir: &Path, root: &Path) -> PathBuf {
    dir.join(format!("{:016x}.endpoint.json", root_hash(root)))
}

#[cfg(any(unix, windows))]
mod core;
#[cfg(any(unix, windows))]
mod tcp_transport;
#[cfg(unix)]
mod transport_unix;

#[cfg(unix)]
pub(crate) type Shared = core::Shared<transport_unix::UnixTransport>;
#[cfg(unix)]
pub(crate) use transport_unix::private_socket_path;

#[cfg(windows)]
pub(crate) type Shared = core::Shared<tcp_transport::TcpTransport>;
#[cfg(windows)]
pub(crate) use tcp_transport::private_endpoint_path as private_socket_path;

/// Platforms without Unix or Windows transport index independently.
#[cfg(not(any(unix, windows)))]
mod stub {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct Shared;

    impl Shared {
        pub(crate) fn forward(
            &self,
            _op: Op,
            _reply_timeout: Option<Duration>,
            _cancelled: &dyn Fn() -> bool,
        ) -> Forwarded {
            Forwarded::Local
        }
        pub(crate) fn local_since(&self) -> Option<std::time::Instant> {
            None
        }
        pub(crate) fn shutdown(&self) {}
        pub(crate) fn is_follower(&self) -> bool {
            false
        }
    }

    pub(crate) fn private_socket_path(_root: &Path) -> Option<PathBuf> {
        None
    }
}
#[cfg(not(any(unix, windows)))]
pub(crate) use stub::{private_socket_path, Shared};

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Hello {
        Hello::new(
            Path::new("/repo"),
            "t".into(),
            &ClientContext::default_builtin(),
        )
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn handshake_mismatch_names_the_field() {
        let a = hello();
        let mut b = hello();
        assert_eq!(a.mismatch(&b), None, "session id / pid / cwd may differ");
        b.pkg = "0.0.1".into();
        assert!(a.mismatch(&b).unwrap().starts_with("version"));
        let mut c = hello();
        c.extractor += 1;
        assert!(a.mismatch(&c).unwrap().starts_with("extractor"));
        let mut d = hello();
        d.tools_hash = "x".into();
        assert!(a.mismatch(&d).unwrap().starts_with("tools"));
        let mut e = hello();
        e.root = "/other".into();
        assert!(a.mismatch(&e).unwrap().starts_with("root"));
        let mut f = hello();
        f.toolchain_hash = "other-path".into();
        assert!(a.mismatch(&f).unwrap().starts_with("toolchain env"));
    }

    #[test]
    fn wire_context_round_trips() {
        let ctx = ClientContext::builtin("cursor").unwrap();
        let back: ClientContext = WireContext::from(&ctx).into();
        assert_eq!(back, ctx);
    }

    #[test]
    fn socket_path_is_short_and_per_root() {
        let dir = Path::new("/run/user/1000/astrolabe");
        let a = socket_path_in(dir, Path::new("/a")).unwrap();
        let b = socket_path_in(dir, Path::new("/b")).unwrap();
        assert_ne!(a, b);
        assert!(a.as_os_str().len() <= MAX_SOCKET_PATH);
        let long = PathBuf::from(format!("/{}", "x".repeat(120)));
        assert!(socket_path_in(&long, Path::new("/a")).is_none());
    }

    #[test]
    fn root_key_and_hash_normalize_windows_paths() {
        let verbatim = Path::new(r"\\?\C:\repo");
        let trailing = Path::new(r"C:\repo\");
        let lower_slash = Path::new("c:/repo");

        let k1 = root_key(verbatim);
        let k2 = root_key(trailing);
        let k3 = root_key(lower_slash);
        assert_eq!(k1, "c:/repo");
        assert_eq!(k1, k2);
        assert_eq!(k2, k3);

        let h1 = fnv64(k1.as_bytes());
        let h2 = fnv64(k2.as_bytes());
        let h3 = fnv64(k3.as_bytes());
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
        assert_eq!(root_hash(verbatim), h1);
        assert_eq!(root_hash(trailing), h1);
        assert_eq!(root_hash(lower_slash), h1);

        let unc_verbatim = Path::new(r"\\?\UNC\Server\Share\Repo\");
        let unc_plain = Path::new(r"\\server\share\repo");
        assert_eq!(root_key(unc_verbatim), "//server/share/repo");
        assert_eq!(root_key(unc_verbatim), root_key(unc_plain));
        assert_eq!(root_hash(unc_verbatim), root_hash(unc_plain));
    }

    #[test]
    fn hello_auth_token_serde_omits_none_and_preserves_some() {
        let h_none = hello();
        assert_eq!(h_none.auth_token, None);
        let json_none = serde_json::to_string(&h_none).unwrap();
        assert!(
            !json_none.contains("auth_token"),
            "auth_token=None must be omitted: {json_none}"
        );
        let decoded_none: Hello = serde_json::from_str(&json_none).unwrap();
        assert_eq!(decoded_none.auth_token, None);

        let mut h_some = hello();
        h_some.auth_token = Some("deadbeef".into());
        assert_eq!(h_none.mismatch(&h_some), None);
        let json_some = serde_json::to_string(&h_some).unwrap();
        assert!(json_some.contains("\"auth_token\":\"deadbeef\""));
        let decoded_some: Hello = serde_json::from_str(&json_some).unwrap();
        assert_eq!(decoded_some.auth_token.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn win_runtime_dir_fallback_resolution() {
        use std::ffi::OsStr;

        // 1. ASTROLABE_SHARED_INDEX_DIR wins over everything else.
        assert_eq!(
            tcp_transport::win_runtime_dir_from(
                Some(OsStr::new("/custom/shared")),
                Some(OsStr::new("/local/app")),
                Some(OsStr::new("/user/prof")),
            ),
            Some(PathBuf::from("/custom/shared"))
        );

        // Empty ASTROLABE_SHARED_INDEX_DIR falls through to LOCALAPPDATA.
        assert_eq!(
            tcp_transport::win_runtime_dir_from(
                Some(OsStr::new("")),
                Some(OsStr::new("/local/app")),
                Some(OsStr::new("/user/prof")),
            ),
            Some(PathBuf::from("/local/app").join("astrolabe").join("run"))
        );

        // 2. LOCALAPPDATA when ASTROLABE_SHARED_INDEX_DIR is None.
        assert_eq!(
            tcp_transport::win_runtime_dir_from(
                None,
                Some(OsStr::new("/local/app")),
                Some(OsStr::new("/user/prof")),
            ),
            Some(PathBuf::from("/local/app").join("astrolabe").join("run"))
        );

        // 3. USERPROFILE when LOCALAPPDATA is unset/empty.
        assert_eq!(
            tcp_transport::win_runtime_dir_from(
                None,
                Some(OsStr::new("")),
                Some(OsStr::new("/user/prof")),
            ),
            Some(PathBuf::from("/user/prof").join(".astrolabe").join("run"))
        );

        // 4. None when all env vars are unset/empty (no shared %TEMP% fallback).
        assert_eq!(
            tcp_transport::win_runtime_dir_from(None, None, Some(OsStr::new(""))),
            None
        );
        assert_eq!(tcp_transport::win_runtime_dir_from(None, None, None), None);
    }

    #[test]
    fn settings_ignored_includes_always_load_and_build_wait() {
        assert!(SETTINGS_IGNORED.contains(&"ASTROLABE_ALWAYS_LOAD"));
        assert!(SETTINGS_IGNORED.contains(&"ASTROLABE_BUILD_WAIT_MS"));
    }
}
