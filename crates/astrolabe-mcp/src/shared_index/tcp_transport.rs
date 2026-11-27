//! Loopback TCP transport for Windows (with 256-bit `auth_token` endpoint file).
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::core::{ConnectError, StopListener, Stream, Transport, STEP};
use super::{endpoint_path_in, Hello, Host};
use crate::session::LockOrRecover;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_ACTIVE_CONNS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EndpointFile {
    pub port: u16,
    pub pid: u32,
    pub token: String,
    pub root_key: String,
}

pub(crate) fn win_runtime_dir_from(
    shared_index_dir: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    user_profile: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(dir) = shared_index_dir.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(d) = local_app_data.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("astrolabe").join("run"));
    }
    if let Some(d) = user_profile.filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join(".astrolabe").join("run"));
    }
    None
}

pub(crate) fn win_runtime_dir() -> Option<PathBuf> {
    win_runtime_dir_from(
        std::env::var_os("ASTROLABE_SHARED_INDEX_DIR").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn is_symlink_or_reparse_point(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return true;
        }
    }
    false
}

fn ensure_safe_dir(dir: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if is_symlink_or_reparse_point(&meta) || !meta.is_dir() {
                return Err(io::Error::other(
                    "runtime directory is a symlink, reparse point, or non-directory",
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if is_symlink_or_reparse_point(&meta) || !meta.is_dir() {
        return Err(io::Error::other(
            "runtime directory is a symlink, reparse point, or non-directory",
        ));
    }
    Ok(())
}

fn check_safe_existing_file(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if is_symlink_or_reparse_point(&meta) || !meta.is_file() {
                return Err(io::Error::other(
                    "endpoint file is a symlink, reparse point, or non-file",
                ));
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn private_endpoint_path_in(dir: &Path, root: &Path) -> Option<PathBuf> {
    if let Err(error) = ensure_safe_dir(dir) {
        tracing::warn!(dir = %dir.display(), %error, "shared index: cannot create runtime directory; indexing independently");
        return None;
    }
    let endpoint = endpoint_path_in(dir, root);
    if let Err(error) = check_safe_existing_file(&endpoint) {
        tracing::warn!(endpoint = %endpoint.display(), %error, "shared index: unsafe endpoint file path; indexing independently");
        return None;
    }
    Some(endpoint)
}

pub(crate) fn private_endpoint_path(root: &Path) -> Option<PathBuf> {
    let dir = win_runtime_dir()?;
    private_endpoint_path_in(&dir, root)
}

fn write_endpoint_atomic(endpoint_path: &Path, ep: &EndpointFile) -> io::Result<()> {
    if let Some(parent) = endpoint_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ensure_safe_dir(parent)?;
    }
    check_safe_existing_file(endpoint_path)?;
    let file_name = endpoint_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("endpoint.json");
    let pid = std::process::id();
    let (tmp_path, mut file) = loop {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = endpoint_path.with_file_name(format!("{file_name}.tmp.{pid}.{seq}"));
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    };

    let bytes = match serde_json::to_vec(ep) {
        Ok(b) => b,
        Err(err) => {
            drop(file);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(io::Error::other(err));
        }
    };

    let write_res = (|| -> io::Result<()> {
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    })();
    drop(file);
    if let Err(err) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    let mut last_err = None;
    for attempt in 0..10u64 {
        if let Err(err) = check_safe_existing_file(endpoint_path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        match std::fs::rename(&tmp_path, endpoint_path) {
            Ok(()) => return Ok(()),
            Err(err)
                if err.kind() == io::ErrorKind::PermissionDenied
                    || matches!(err.raw_os_error(), Some(32) | Some(5)) =>
            {
                last_err = Some(err);
                let delay_ms = 5 + ((attempt * 3) % 16);
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(err);
            }
        }
    }
    let _ = std::fs::remove_file(&tmp_path);
    Err(last_err.unwrap_or_else(|| io::Error::other("rename failed")))
}

fn read_endpoint(path: &Path) -> io::Result<EndpointFile> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let pmeta = std::fs::symlink_metadata(parent)?;
        if is_symlink_or_reparse_point(&pmeta) || !pmeta.is_dir() {
            return Err(io::Error::other(
                "endpoint parent directory is a symlink, reparse point, or non-directory",
            ));
        }
    }
    let meta = std::fs::symlink_metadata(path)?;
    if is_symlink_or_reparse_point(&meta) || !meta.is_file() {
        return Err(io::Error::other(
            "endpoint file is a symlink, reparse point, or non-file",
        ));
    }
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(unix)]
fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")?;
    file.read_exact(buf)
}

#[cfg(windows)]
fn random_bytes(buf: &mut [u8]) -> io::Result<()> {
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            h_algorithm: *mut core::ffi::c_void,
            pb_buffer: *mut u8,
            cb_buffer: u32,
            dw_flags: u32,
        ) -> i32;
    }
    let len = u32::try_from(buf.len()).map_err(|_| io::Error::other("buffer too large"))?;
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{status:08x}"
        )))
    }
}

fn generate_auth_token() -> io::Result<String> {
    let mut buf = [0u8; 32];
    random_bytes(&mut buf)?;
    Ok(encode_hex_lower(&buf))
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let max_len = a.len().max(b.len());
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..max_len {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

fn remove_endpoint_if_token_matches(path: &Path, expected_token: &str) {
    if let Ok(ep) = read_endpoint(path) {
        if ep.token == expected_token {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct ActiveConnGuard(Arc<AtomicUsize>);

impl ActiveConnGuard {
    fn try_acquire(counter: &Arc<AtomicUsize>) -> Option<Self> {
        let mut cur = counter.load(Ordering::Acquire);
        loop {
            if cur >= MAX_ACTIVE_CONNS {
                return None;
            }
            match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Self(Arc::clone(counter))),
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Stream for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }

    fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, dur)
    }

    fn read_timeout(&self) -> io::Result<Option<Duration>> {
        TcpStream::read_timeout(self)
    }

    fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, dur)
    }

    fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

pub(crate) struct TcpTransport;

pub(crate) struct TcpListenerGuard {
    endpoint_path: PathBuf,
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<HashMap<u64, TcpStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Transport for TcpTransport {
    type Stream = TcpStream;
    type Listener = TcpListenerGuard;

    fn dial(endpoint: &Path, hello: &Hello) -> Result<(Self::Stream, Hello), ConnectError> {
        let ep = read_endpoint(endpoint).map_err(|_| ConnectError::NoLeader)?;
        if ep.root_key != hello.root {
            return Err(ConnectError::NoLeader);
        }
        let stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], ep.port)),
            CONNECT_TIMEOUT,
        )
        .map_err(|_| ConnectError::NoLeader)?;
        let _ = stream.set_nodelay(true);
        let mut wire_hello = hello.clone();
        wire_hello.auth_token = Some(ep.token);
        Ok((stream, wire_hello))
    }

    fn bind(endpoint: &Path, hello: Hello, host: Arc<dyn Host>) -> io::Result<Self::Listener> {
        let token = generate_auth_token()?;
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let ep = EndpointFile {
            port,
            pid: std::process::id(),
            token: token.clone(),
            root_key: hello.root.clone(),
        };
        write_endpoint_atomic(endpoint, &ep)?;

        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<HashMap<u64, TcpStream>>> = Arc::new(Mutex::new(HashMap::new()));
        let active_conns = Arc::new(AtomicUsize::new(0));
        let spawned = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
            let active_conns = Arc::clone(&active_conns);
            let token = token.clone();
            std::thread::Builder::new()
                .name("astrolabe-shared-accept".into())
                .spawn(move || {
                    let mut next = 0u64;
                    for stream in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let stream = match stream {
                            Ok(stream) => stream,
                            Err(error) => {
                                tracing::warn!(%error, "shared index: accept failed");
                                std::thread::sleep(STEP * 4);
                                continue;
                            }
                        };
                        let Some(guard) = ActiveConnGuard::try_acquire(&active_conns) else {
                            tracing::warn!(
                                max = MAX_ACTIVE_CONNS,
                                "shared index: active connection limit reached; dropping connection"
                            );
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            continue;
                        };
                        let _ = stream.set_nodelay(true);
                        let clone = match stream.try_clone() {
                            Ok(clone) => clone,
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    "shared index: failed to clone accepted tcp stream; dropping connection"
                                );
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                                continue;
                            }
                        };
                        next += 1;
                        let key = next;
                        conns.lock_or_recover().insert(key, clone);
                        let hello = hello.clone();
                        let host = Arc::clone(&host);
                        let session_conns = Arc::clone(&conns);
                        let token = token.clone();
                        let session_spawned = std::thread::Builder::new()
                            .name("astrolabe-shared-session".into())
                            .spawn(move || {
                                let _guard = guard;
                                super::core::serve_conn(stream, &hello, host.as_ref(), Some(&token));
                                session_conns.lock_or_recover().remove(&key);
                            });
                        if let Err(error) = session_spawned {
                            tracing::warn!(%error, "shared index: failed to spawn session thread");
                            if let Some(conn) = conns.lock_or_recover().remove(&key) {
                                let _ = conn.shutdown(std::net::Shutdown::Both);
                            }
                        }
                    }
                })
        };
        let thread = match spawned {
            Ok(thread) => thread,
            Err(err) => {
                remove_endpoint_if_token_matches(endpoint, &token);
                return Err(err);
            }
        };
        Ok(TcpListenerGuard {
            endpoint_path: endpoint.to_path_buf(),
            port,
            token,
            stop,
            conns,
            thread: Some(thread),
        })
    }
}

impl TcpListenerGuard {
    fn stop_inner(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
            CONNECT_TIMEOUT,
        );
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        for (_, stream) in self.conns.lock_or_recover().drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        remove_endpoint_if_token_matches(&self.endpoint_path, &self.token);
    }
}

impl StopListener for TcpListenerGuard {
    fn stop(mut self) {
        self.stop_inner();
    }
}

impl Drop for TcpListenerGuard {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    use astrolabe_core::elect::IndexerLock;
    use serde_json::{Map, Value};

    use super::super::core::{connect, spawn_reader, write_frame};
    use super::super::{endpoint_path_in, root_key, Op, SessionView};
    use crate::context::ClientContext;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let id = SEQ.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("as-tcp-{tag}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct MockView;

    impl SessionView for MockView {
        fn call(
            &self,
            name: &str,
            args: Option<Map<String, Value>>,
            _cancel: Arc<AtomicBool>,
        ) -> Result<Value, Value> {
            Ok(serde_json::json!({
                "called": name,
                "args": args,
            }))
        }

        fn read(&self, uri: &str) -> Result<Value, Value> {
            Ok(serde_json::json!({
                "uri": uri,
            }))
        }
    }

    struct MockHost;

    impl Host for MockHost {
        fn open_session(&self, _context: ClientContext) -> Arc<dyn SessionView> {
            Arc::new(MockView)
        }
        fn adopt_lock(&self, _lock: IndexerLock) {}
        fn start_local(&self) {}
        fn try_take_lock(&self) -> bool {
            false
        }
        fn release_lock(&self) {}
    }

    #[test]
    fn constant_time_eq_behaves_correctly() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abcdef0123", b"abcdef0123"));
        assert!(!constant_time_eq(b"abcdef0123", b"abcdef0124"));
        assert!(!constant_time_eq(b"abcdef0123", b"abcdef012"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(!constant_time_eq(b"a", b""));
    }

    #[test]
    fn tcp_leader_and_follower_call_and_read() {
        let dir = TempDir::new("call-read");
        let root = dir.0.join("repo");
        let endpoint = endpoint_path_in(&dir.0, &root);
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());

        let listener =
            TcpTransport::bind(&endpoint, hello.clone(), Arc::new(MockHost)).expect("bind");
        let ep: EndpointFile =
            serde_json::from_slice(&std::fs::read(&endpoint).expect("read endpoint"))
                .expect("parse endpoint");
        assert_ne!(ep.port, 0);
        assert_eq!(ep.pid, std::process::id());
        assert_eq!(ep.token.len(), 64);
        assert!(ep
            .token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(ep.root_key, root_key(&root));

        let (conn, reader) = connect::<TcpTransport>(&endpoint, &hello).expect("connect");
        assert_eq!(reader.get_ref().read_timeout().unwrap(), None);
        spawn_reader(Arc::clone(&conn), reader, || {});

        let mut args = Map::new();
        args.insert("query".into(), Value::String("greet".into()));
        let call_resp = conn
            .request(
                &Op::Call {
                    name: "find_symbol".into(),
                    args: Some(args.clone()),
                },
                Some(Duration::from_secs(5)),
                &|| false,
            )
            .expect("call request");
        assert_eq!(call_resp.err, None);
        assert_eq!(
            call_resp.ok,
            Some(serde_json::json!({
                "called": "find_symbol",
                "args": args,
            }))
        );

        let read_resp = conn
            .request(
                &Op::Read {
                    uri: "astrolabe://res/42".into(),
                },
                Some(Duration::from_secs(5)),
                &|| false,
            )
            .expect("read request");
        assert_eq!(read_resp.err, None);
        assert_eq!(
            read_resp.ok,
            Some(serde_json::json!({
                "uri": "astrolabe://res/42",
            }))
        );

        conn.close();
        listener.stop();
        assert!(!endpoint.exists(), "stop should remove own endpoint file");
    }

    #[test]
    fn unauthorized_clients_are_silently_closed() {
        let dir = TempDir::new("unauth");
        let root = dir.0.join("repo");
        let endpoint = endpoint_path_in(&dir.0, &root);
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());

        let listener =
            TcpTransport::bind(&endpoint, hello.clone(), Arc::new(MockHost)).expect("bind");
        let ep: EndpointFile =
            serde_json::from_slice(&std::fs::read(&endpoint).expect("read endpoint"))
                .expect("parse endpoint");

        let wrong_same_len = if ep.token.starts_with('0') {
            format!("1{}", &ep.token[1..])
        } else {
            format!("0{}", &ep.token[1..])
        };

        let cases = [None, Some(wrong_same_len), Some("short-token".to_string())];

        for bad_token in cases {
            let mut bad_hello = hello.clone();
            bad_hello.auth_token = bad_token;
            let mut stream = TcpStream::connect(("127.0.0.1", ep.port)).expect("connect raw");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            write_frame(&mut stream, &bad_hello).expect("write hello");
            let mut buf = Vec::new();
            let n = stream.read_to_end(&mut buf).expect("read to end");
            assert_eq!(n, 0, "expected silent EOF, got {buf:?}");
        }

        let mut malformed = TcpStream::connect(("127.0.0.1", ep.port)).expect("connect raw");
        malformed
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        malformed
            .write_all(b"not valid json\n")
            .expect("write malformed hello");
        malformed.flush().expect("flush malformed hello");
        let mut buf = Vec::new();
        let n = malformed.read_to_end(&mut buf).expect("read to end");
        assert_eq!(n, 0, "expected silent EOF on malformed hello, got {buf:?}");

        listener.stop();
    }

    #[test]
    fn dial_rejects_mismatched_root_key() {
        let dir = TempDir::new("root-mismatch");
        let root_a = dir.0.join("repo-a");
        let root_b = dir.0.join("repo-b");
        let endpoint = endpoint_path_in(&dir.0, &root_a);
        let hello_a = Hello::new(&root_a, "tools-1".into(), &ClientContext::default_builtin());
        let hello_b = Hello::new(&root_b, "tools-1".into(), &ClientContext::default_builtin());

        let listener = TcpTransport::bind(&endpoint, hello_a, Arc::new(MockHost)).expect("bind");
        let res = TcpTransport::dial(&endpoint, &hello_b);
        assert!(
            matches!(res, Err(ConnectError::NoLeader)),
            "expected ConnectError::NoLeader on root_key mismatch"
        );

        listener.stop();
    }

    #[test]
    fn old_leader_stop_does_not_delete_new_leaders_endpoint_file() {
        let dir = TempDir::new("token-guard");
        let root = dir.0.join("repo");
        let endpoint = endpoint_path_in(&dir.0, &root);
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());

        let listener1 =
            TcpTransport::bind(&endpoint, hello.clone(), Arc::new(MockHost)).expect("bind 1");
        let ep1: EndpointFile =
            serde_json::from_slice(&std::fs::read(&endpoint).expect("read ep1"))
                .expect("parse ep1");

        let listener2 =
            TcpTransport::bind(&endpoint, hello.clone(), Arc::new(MockHost)).expect("bind 2");
        let ep2: EndpointFile =
            serde_json::from_slice(&std::fs::read(&endpoint).expect("read ep2"))
                .expect("parse ep2");
        assert_ne!(ep1.token, ep2.token);

        listener1.stop();
        assert!(
            endpoint.exists(),
            "old leader must not remove new leader's endpoint file"
        );
        let current: EndpointFile =
            serde_json::from_slice(&std::fs::read(&endpoint).expect("read current"))
                .expect("parse current");
        assert_eq!(current, ep2);

        listener2.stop();
        assert!(
            !endpoint.exists(),
            "current leader stop must remove its own endpoint file"
        );
    }

    #[test]
    fn ensure_safe_dir_and_check_safe_existing_file_reject_wrong_file_types() {
        let dir = TempDir::new("wrong-types");
        let file_path = dir.0.join("not-a-dir");
        std::fs::write(&file_path, b"x").expect("write file");
        assert!(ensure_safe_dir(&file_path).is_err());

        let sub_dir = dir.0.join("is-a-dir");
        std::fs::create_dir_all(&sub_dir).expect("create dir");
        assert!(check_safe_existing_file(&sub_dir).is_err());
        assert!(read_endpoint(&sub_dir).is_err());

        let dir_meta = std::fs::symlink_metadata(&sub_dir).expect("dir metadata");
        let file_meta = std::fs::symlink_metadata(&file_path).expect("file metadata");
        assert!(!is_symlink_or_reparse_point(&dir_meta));
        assert!(!is_symlink_or_reparse_point(&file_meta));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_runtime_dir() {
        let dir = TempDir::new("symlink-dir");
        let real_dir = dir.0.join("real");
        std::fs::create_dir_all(&real_dir).expect("create real dir");
        let sym_dir = dir.0.join("sym");
        std::os::unix::fs::symlink(&real_dir, &sym_dir).expect("create dir symlink");

        let root = dir.0.join("repo");
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());
        let ep = EndpointFile {
            port: 12345,
            pid: std::process::id(),
            token: "0123456789abcdef".repeat(4),
            root_key: root_key(&root),
        };
        let real_ep = endpoint_path_in(&real_dir, &root);
        write_endpoint_atomic(&real_ep, &ep).expect("write real endpoint");

        let sym_meta = std::fs::symlink_metadata(&sym_dir).expect("sym dir meta");
        assert!(is_symlink_or_reparse_point(&sym_meta));
        assert!(ensure_safe_dir(&sym_dir).is_err());
        assert_eq!(private_endpoint_path_in(&sym_dir, &root), None);

        let sym_ep = endpoint_path_in(&sym_dir, &root);
        assert!(write_endpoint_atomic(&sym_ep, &ep).is_err());
        assert!(read_endpoint(&sym_ep).is_err());
        assert!(matches!(
            TcpTransport::dial(&sym_ep, &hello),
            Err(ConnectError::NoLeader)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_endpoint_file() {
        let dir = TempDir::new("symlink-file");
        let root = dir.0.join("repo");
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());
        let ep = EndpointFile {
            port: 12345,
            pid: std::process::id(),
            token: "0123456789abcdef".repeat(4),
            root_key: root_key(&root),
        };
        let real_file = dir.0.join("real.json");
        write_endpoint_atomic(&real_file, &ep).expect("write real file");

        let sym_ep = endpoint_path_in(&dir.0, &root);
        std::os::unix::fs::symlink(&real_file, &sym_ep).expect("create file symlink");

        let sym_meta = std::fs::symlink_metadata(&sym_ep).expect("sym file meta");
        assert!(is_symlink_or_reparse_point(&sym_meta));
        assert_eq!(private_endpoint_path_in(&dir.0, &root), None);
        assert!(write_endpoint_atomic(&sym_ep, &ep).is_err());
        assert!(read_endpoint(&sym_ep).is_err());
        assert!(matches!(
            TcpTransport::dial(&sym_ep, &hello),
            Err(ConnectError::NoLeader)
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_symlink_and_reparse_point_rejection() {
        let dir = TempDir::new("win-symlink");
        let root = dir.0.join("repo");
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());
        let ep = EndpointFile {
            port: 12345,
            pid: std::process::id(),
            token: "0123456789abcdef".repeat(4),
            root_key: root_key(&root),
        };

        let real_dir = dir.0.join("real");
        std::fs::create_dir_all(&real_dir).expect("create real dir");
        let sym_dir = dir.0.join("sym-dir");
        if std::os::windows::fs::symlink_dir(&real_dir, &sym_dir).is_ok() {
            let meta = std::fs::symlink_metadata(&sym_dir).expect("sym dir meta");
            assert!(is_symlink_or_reparse_point(&meta));
            assert!(ensure_safe_dir(&sym_dir).is_err());
            assert_eq!(private_endpoint_path_in(&sym_dir, &root), None);
        }

        let real_file = dir.0.join("real.json");
        write_endpoint_atomic(&real_file, &ep).expect("write real file");
        let sym_ep = endpoint_path_in(&dir.0, &root);
        if std::os::windows::fs::symlink_file(&real_file, &sym_ep).is_ok() {
            let meta = std::fs::symlink_metadata(&sym_ep).expect("sym file meta");
            assert!(is_symlink_or_reparse_point(&meta));
            assert_eq!(private_endpoint_path_in(&dir.0, &root), None);
            assert!(write_endpoint_atomic(&sym_ep, &ep).is_err());
            assert!(read_endpoint(&sym_ep).is_err());
            assert!(matches!(
                TcpTransport::dial(&sym_ep, &hello),
                Err(ConnectError::NoLeader)
            ));
        }
    }

    #[test]
    fn enforces_max_active_conns_and_recovers_after_close() {
        use std::io::{BufRead, BufReader};

        let dir = TempDir::new("max-conns");
        let root = dir.0.join("repo");
        let endpoint = endpoint_path_in(&dir.0, &root);
        let hello = Hello::new(&root, "tools-1".into(), &ClientContext::default_builtin());

        let listener =
            TcpTransport::bind(&endpoint, hello.clone(), Arc::new(MockHost)).expect("bind");
        let ep = read_endpoint(&endpoint).expect("read endpoint");

        let mut wire_hello = hello.clone();
        wire_hello.auth_token = Some(ep.token.clone());

        let mut active = Vec::with_capacity(MAX_ACTIVE_CONNS);
        for i in 0..MAX_ACTIVE_CONNS {
            let mut stream =
                TcpStream::connect(("127.0.0.1", ep.port)).expect("connect active slot");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            write_frame(&mut stream, &wire_hello).expect("write hello");
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            reader
                .read_line(&mut line)
                .unwrap_or_else(|e| panic!("read HelloReply for slot {i}: {e}"));
            let reply: super::super::HelloReply =
                serde_json::from_str(&line).expect("parse HelloReply");
            assert!(reply.ok, "slot {i} handshake should succeed");
            active.push(stream);
        }

        let mut overflow =
            TcpStream::connect(("127.0.0.1", ep.port)).expect("connect 65th overflow");
        overflow
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set overflow timeout");
        let mut buf = Vec::new();
        let n = overflow.read_to_end(&mut buf).expect("read overflow EOF");
        assert_eq!(n, 0, "65th connection should be closed silently with EOF");

        drop(active.pop());

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let recovered = loop {
            if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", ep.port)) {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                if write_frame(&mut stream, &wire_hello).is_ok() {
                    let mut line = String::new();
                    let mut reader = BufReader::new(&stream);
                    if let Ok(n) = reader.read_line(&mut line) {
                        if n > 0 {
                            let reply: super::super::HelloReply =
                                serde_json::from_str(&line).expect("parse recovered HelloReply");
                            assert!(reply.ok);
                            break stream;
                        }
                    }
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for connection slot to recover after closing an active connection"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        active.push(recovered);

        drop(active);
        listener.stop();
    }
}
