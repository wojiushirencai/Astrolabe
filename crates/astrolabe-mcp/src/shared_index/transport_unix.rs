//! Unix-socket transport for [`super`].

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::core::{self, ConnectError, StopListener, Stream, Transport, STEP};
use super::{socket_dir, socket_path_in, Hello, Host};
use crate::session::LockOrRecover;

/// Socket for `root` in the private directory, creating the directory
/// (0700). `None` when there is no usable private directory or the path
/// would be too long.
pub(crate) fn private_socket_path(root: &Path) -> Option<PathBuf> {
    let dir = socket_dir()?;
    let path = socket_path_in(&dir, root)?;
    match ensure_private_dir(&dir) {
        Ok(()) => Some(path),
        Err(error) => {
            tracing::warn!(dir = %dir.display(), %error, "shared index: no private socket directory; indexing independently");
            None
        }
    }
}

/// Create `dir` as 0700 and refuse a symlink or a directory others can
/// write to (chmod only succeeds for the owner).
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::other("socket directory is not a directory"));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.mode() & 0o077 != 0 {
            return Err(io::Error::other("socket directory is not private"));
        }
    }
    Ok(())
}

impl Stream for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }

    fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        UnixStream::set_read_timeout(self, dur)
    }

    fn read_timeout(&self) -> io::Result<Option<Duration>> {
        UnixStream::read_timeout(self)
    }

    fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        UnixStream::set_write_timeout(self, dur)
    }

    fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

pub(crate) struct UnixTransport;

pub(crate) struct UnixListenerGuard {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    conns: Arc<Mutex<HashMap<u64, UnixStream>>>,
    thread: Option<std::thread::JoinHandle<()>>,
    inode: (u64, u64),
}

impl Transport for UnixTransport {
    type Stream = UnixStream;
    type Listener = UnixListenerGuard;

    fn dial(endpoint: &Path, hello: &Hello) -> Result<(Self::Stream, Hello), ConnectError> {
        let stream = UnixStream::connect(endpoint).map_err(|_| ConnectError::NoLeader)?;
        Ok((stream, hello.clone()))
    }

    fn bind(endpoint: &Path, hello: Hello, host: Arc<dyn Host>) -> io::Result<Self::Listener> {
        // We hold the indexer lock, so a socket file here is stale (its
        // leader died without unlinking).
        match std::fs::remove_file(endpoint) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(endpoint)?;
        std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))?;
        let meta = std::fs::symlink_metadata(endpoint)?;
        let inode = (meta.dev(), meta.ino());
        let stop = Arc::new(AtomicBool::new(false));
        let conns: Arc<Mutex<HashMap<u64, UnixStream>>> = Arc::new(Mutex::new(HashMap::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let conns = Arc::clone(&conns);
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
                        let clone = match stream.try_clone() {
                            Ok(clone) => clone,
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    "shared index: failed to clone accepted unix stream; dropping connection"
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
                        let spawned = std::thread::Builder::new()
                            .name("astrolabe-shared-session".into())
                            .spawn(move || {
                                core::serve_conn(stream, &hello, host.as_ref(), None);
                                session_conns.lock_or_recover().remove(&key);
                            });
                        if let Err(error) = spawned {
                            tracing::warn!(%error, "shared index: failed to spawn session thread");
                            if let Some(conn) = conns.lock_or_recover().remove(&key) {
                                let _ = conn.shutdown(std::net::Shutdown::Both);
                            }
                        }
                    }
                })?
        };
        Ok(UnixListenerGuard {
            path: endpoint.to_path_buf(),
            stop,
            conns,
            thread: Some(thread),
            inode,
        })
    }
}

impl UnixListenerGuard {
    fn stop_inner(&mut self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the blocking accept so the listener is dropped.
        let _ = UnixStream::connect(&self.path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        for (_, stream) in self.conns.lock_or_recover().drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if (meta.dev(), meta.ino()) == self.inode {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

impl StopListener for UnixListenerGuard {
    /// Stop accepting, drop every follower (they see EOF), unlink the socket.
    fn stop(mut self) {
        self.stop_inner();
    }
}

impl Drop for UnixListenerGuard {
    fn drop(&mut self) {
        self.stop_inner();
    }
}
