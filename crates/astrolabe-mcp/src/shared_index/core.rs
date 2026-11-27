//! Platform-independent framing, connection multiplexing, election, and
//! hand-over state machine for [`super`].

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use astrolabe_core::elect::IndexerLock;
use serde::Serialize;

use super::{Forwarded, Hello, HelloReply, Host, Op, Req, Resp, QUEUE_BUDGET, STARTUP_GRACE};
use crate::session::LockOrRecover;

/// Handshake read/write cap on both ends.
pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// A peer that stops reading for this long is treated as gone.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Poll step while waiting for the lock / a reply.
pub(crate) const STEP: Duration = Duration::from_millis(25);
/// How often an independent session checks whether it can take the lock.
const PROMOTE_POLL: Duration = Duration::from_secs(1);
/// Reconnect attempts per call after the leader went away mid-call.
const MAX_HANDOVERS_PER_CALL: usize = 3;
/// Upper bound on the initial `Hello` frame before authentication.
const MAX_HELLO_BYTES: u64 = 64 * 1024;

pub(crate) trait Stream: io::Read + io::Write + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()>;
    #[allow(dead_code)]
    fn read_timeout(&self) -> io::Result<Option<Duration>>;
    fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()>;
    fn shutdown(&self, how: std::net::Shutdown) -> io::Result<()>;
}

pub(crate) trait StopListener: Send + 'static {
    fn stop(self);
}

pub(crate) trait Transport: Send + Sync + 'static {
    type Stream: Stream;
    type Listener: StopListener;

    fn dial(endpoint: &Path, hello: &Hello) -> Result<(Self::Stream, Hello), ConnectError>;
    fn bind(endpoint: &Path, hello: Hello, host: Arc<dyn Host>) -> io::Result<Self::Listener>;
}

pub(crate) fn write_frame<W: io::Write>(mut stream: W, frame: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()
}

// ---------------------------------------------------------------- follower

/// A follower's connection to the leader. Calls are multiplexed by id; a
/// reader thread routes replies and notices EOF.
pub(crate) struct Conn<S: Stream> {
    writer: Mutex<S>,
    pending: Mutex<HashMap<u64, mpsc::Sender<Resp>>>,
    next: AtomicU64,
    alive: AtomicBool,
    leader_pid: u32,
}

#[derive(Debug)]
pub(crate) enum ConnectError {
    /// Nobody (sharing-capable) is listening.
    NoLeader,
    /// A leader answered and refused this follower (version / settings).
    Refused(String),
}

#[derive(Debug)]
pub(crate) enum CallError {
    Disconnected,
    Timeout,
    Cancelled,
}

pub(crate) type Connected<S> = (Arc<Conn<S>>, BufReader<S>);

pub(crate) fn connect<T: Transport>(
    endpoint: &Path,
    hello: &Hello,
) -> Result<Connected<T::Stream>, ConnectError> {
    let (mut stream, wire_hello) = T::dial(endpoint, hello)?;
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HELLO_TIMEOUT));
    write_frame(&mut stream, &wire_hello).map_err(|_| ConnectError::NoLeader)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|_| ConnectError::NoLeader)?);
    let _ = reader.get_ref().set_read_timeout(Some(HELLO_TIMEOUT));
    let mut line = String::new();
    match io::Read::take(&mut reader, MAX_HELLO_BYTES).read_line(&mut line) {
        Ok(n) if n > 0 && line.ends_with('\n') => {}
        _ => return Err(ConnectError::NoLeader),
    }
    let reply: HelloReply = serde_json::from_str(&line).map_err(|_| ConnectError::NoLeader)?;
    if !reply.ok {
        return Err(ConnectError::Refused(
            reply.reason.unwrap_or_else(|| "refused".into()),
        ));
    }
    let _ = stream.set_read_timeout(None);
    let _ = reader.get_ref().set_read_timeout(None);
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    Ok((
        Arc::new(Conn {
            writer: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            next: AtomicU64::new(0),
            alive: AtomicBool::new(true),
            leader_pid: reply.leader_pid,
        }),
        reader,
    ))
}

pub(crate) fn spawn_reader<S: Stream>(
    conn: Arc<Conn<S>>,
    reader: BufReader<S>,
    on_eof: impl FnOnce() + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name("astrolabe-shared-rx".into())
        .spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<Resp>(&line) {
                    Ok(resp) => {
                        if let Some(tx) = conn.pending.lock_or_recover().remove(&resp.id) {
                            let _ = tx.send(resp);
                        }
                    }
                    Err(error) => tracing::warn!(%error, "shared index: bad reply frame"),
                }
            }
            conn.alive.store(false, Ordering::SeqCst);
            // Dropping the senders wakes every waiting call with Disconnected.
            conn.pending.lock_or_recover().clear();
            on_eof();
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "shared index: cannot spawn reader thread");
    }
}

impl<S: Stream> Conn<S> {
    pub(crate) fn request(
        &self,
        op: &Op,
        timeout: Option<Duration>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Resp, CallError> {
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::channel();
        self.pending.lock_or_recover().insert(id, tx);
        if !self.alive.load(Ordering::SeqCst) {
            self.pending.lock_or_recover().remove(&id);
            return Err(CallError::Disconnected);
        }
        let frame = match op {
            Op::Call { name, args } => Req::Call {
                id,
                name: name.clone(),
                args: args.clone(),
            },
            Op::Read { uri } => Req::Read {
                id,
                uri: uri.clone(),
            },
        };
        if write_frame(&mut *self.writer.lock_or_recover(), &frame).is_err() {
            self.pending.lock_or_recover().remove(&id);
            return Err(CallError::Disconnected);
        }
        let started = Instant::now();
        loop {
            match rx.recv_timeout(STEP * 2) {
                Ok(resp) => return Ok(resp),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(CallError::Disconnected),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancelled() {
                        self.cancel(id);
                        return Err(CallError::Cancelled);
                    }
                    if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                        self.cancel(id);
                        return Err(CallError::Timeout);
                    }
                }
            }
        }
    }

    fn cancel(&self, id: u64) {
        self.pending.lock_or_recover().remove(&id);
        let _ = write_frame(&mut *self.writer.lock_or_recover(), &Req::Cancel { id });
    }

    pub(crate) fn close(&self) {
        let _ = self
            .writer
            .lock_or_recover()
            .shutdown(std::net::Shutdown::Both);
    }
}

// ---------------------------------------------------------------- election

enum Elected<S: Stream> {
    Follower(Arc<Conn<S>>, BufReader<S>),
    Leader(IndexerLock),
    Independent(String),
}

enum Role<S: Stream> {
    Follower(Arc<Conn<S>>),
    Electing,
    Local,
    Closed,
}

/// Sharing state of one server process (single-root mode only).
pub(crate) struct Shared<T: Transport> {
    root: PathBuf,
    sock: PathBuf,
    hello: Hello,
    role: Mutex<Role<T::Stream>>,
    settled: Condvar,
    host: Mutex<Option<Arc<dyn Host>>>,
    leader: Mutex<Option<T::Listener>>,
    local_started: AtomicBool,
    local_since: Mutex<Option<Instant>>,
    /// The promotion watcher is running (or done).
    promote_watch: AtomicBool,
    /// Binding the socket failed once; do not keep retrying.
    bind_failed: AtomicBool,
    me: Weak<Shared<T>>,
}

impl<T: Transport> std::fmt::Debug for Shared<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let role = match &*self.role.lock_or_recover() {
            Role::Follower(_) => "follower",
            Role::Electing => "electing",
            Role::Local => "local",
            Role::Closed => "closed",
        };
        f.debug_struct("Shared")
            .field("sock", &self.sock)
            .field("role", &role)
            .finish_non_exhaustive()
    }
}

impl<T: Transport> Shared<T> {
    pub(crate) fn new(root: PathBuf, sock: PathBuf, hello: Hello) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            root,
            sock,
            hello,
            role: Mutex::new(Role::Electing),
            settled: Condvar::new(),
            host: Mutex::new(None),
            leader: Mutex::new(None),
            local_started: AtomicBool::new(false),
            local_since: Mutex::new(None),
            promote_watch: AtomicBool::new(false),
            bind_failed: AtomicBool::new(false),
            me: me.clone(),
        })
    }

    pub(crate) fn set_host(&self, host: Arc<dyn Host>) {
        *self.host.lock_or_recover() = Some(host);
    }

    fn host(&self) -> Option<Arc<dyn Host>> {
        self.host.lock_or_recover().clone()
    }

    /// Startup election: follow a live leader, become the leader, or index
    /// independently. Returns quickly (<= [`STARTUP_GRACE`] when a
    /// non-sharing process holds the lock).
    pub(crate) fn boot(&self) {
        let outcome = self.elect(STARTUP_GRACE);
        self.settle(outcome, false);
    }

    pub(crate) fn is_follower(&self) -> bool {
        matches!(*self.role.lock_or_recover(), Role::Follower(_))
    }

    /// When this process started serving locally after a hand-over (not at
    /// boot), so the first calls can wait for the warm load.
    pub(crate) fn local_since(&self) -> Option<Instant> {
        *self.local_since.lock_or_recover()
    }

    fn elect(&self, grace: Duration) -> Elected<T::Stream> {
        let started = Instant::now();
        loop {
            match IndexerLock::try_acquire(&self.root) {
                Ok(Some(lock)) => return Elected::Leader(lock),
                Ok(None) => {}
                Err(error) => return Elected::Independent(format!("indexer lock: {error}")),
            }
            match connect::<T>(&self.sock, &self.hello) {
                Ok((conn, reader)) => return Elected::Follower(conn, reader),
                Err(ConnectError::Refused(reason)) => return Elected::Independent(reason),
                Err(ConnectError::NoLeader) => {}
            }
            if started.elapsed() >= grace {
                return Elected::Independent(
                    "the indexer lock is held by a session that does not share \
                     (older version, ASTROLABE_SHARED_INDEX=off, or different settings)"
                        .into(),
                );
            }
            std::thread::sleep(STEP);
        }
    }

    fn settle(&self, outcome: Elected<T::Stream>, handover: bool) {
        match outcome {
            Elected::Follower(conn, reader) => {
                let mut role = self.role.lock_or_recover();
                if !matches!(*role, Role::Electing) {
                    // Went local (queue timeout) or shut down meanwhile.
                    drop(role);
                    conn.close();
                    return;
                }
                let weak = self.me.clone();
                let watched = Arc::clone(&conn);
                spawn_reader(Arc::clone(&conn), reader, move || {
                    if let Some(shared) = weak.upgrade() {
                        shared.on_disconnect(&watched);
                    }
                });
                tracing::info!(
                    leader_pid = conn.leader_pid,
                    sock = %self.sock.display(),
                    "shared index: following the leader (no local index)"
                );
                *role = Role::Follower(conn);
                self.settled.notify_all();
            }
            Elected::Leader(lock) => self.become_leader(lock, handover),
            Elected::Independent(reason) => {
                tracing::info!(%reason, "shared index: indexing independently");
                self.go_local(handover);
            }
        }
    }

    fn become_leader(&self, lock: IndexerLock, handover: bool) {
        if matches!(*self.role.lock_or_recover(), Role::Closed) {
            return;
        }
        let Some(host) = self.host() else { return };
        host.adopt_lock(lock);
        self.serve_socket(&host, "");
        self.go_local(handover);
    }

    /// Bind and serve the endpoint (we hold the lock). `true` when serving.
    fn serve_socket(&self, host: &Arc<dyn Host>, how: &str) -> bool {
        let mut leader = self.leader.lock_or_recover();
        if leader.is_some() {
            return true;
        }
        if matches!(*self.role.lock_or_recover(), Role::Closed) {
            return false;
        }
        match T::bind(&self.sock, self.hello.clone(), Arc::clone(host)) {
            Ok(listener) => {
                tracing::info!(sock = %self.sock.display(), "shared index: leader, serving other sessions{how}");
                *leader = Some(listener);
                true
            }
            Err(error) => {
                self.bind_failed.store(true, Ordering::SeqCst);
                tracing::warn!(%error, sock = %self.sock.display(), "shared index: cannot bind socket; leader for this session only");
                false
            }
        }
    }

    /// An independent process may get the lock later (the holder that did
    /// not share exits, or `refresh_leader` on an apply wins it). Poll for
    /// it and start serving, so later sessions share instead of waiting out
    /// [`STARTUP_GRACE`] and indexing alone.
    fn watch_for_promotion(&self) {
        if self.promote_watch.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = self.me.clone();
        let spawned = std::thread::Builder::new()
            .name("astrolabe-shared-promote".into())
            .spawn(move || loop {
                std::thread::sleep(PROMOTE_POLL);
                let Some(me) = weak.upgrade() else { return };
                if matches!(*me.role.lock_or_recover(), Role::Closed)
                    || me.leader.lock_or_recover().is_some()
                    || me.bind_failed.load(Ordering::SeqCst)
                {
                    return;
                }
                let Some(host) = me.host() else { return };
                if host.try_take_lock() {
                    if !me.serve_socket(&host, " (took over the indexer lock)")
                        && matches!(*me.role.lock_or_recover(), Role::Closed)
                    {
                        host.release_lock();
                    }
                    return;
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "shared index: cannot spawn promotion watcher");
        }
    }

    fn go_local(&self, handover: bool) {
        {
            let mut role = self.role.lock_or_recover();
            if matches!(*role, Role::Closed) {
                return;
            }
            *role = Role::Local;
            self.settled.notify_all();
        }
        if handover {
            let mut since = self.local_since.lock_or_recover();
            since.get_or_insert_with(Instant::now);
        }
        if !self.local_started.swap(true, Ordering::SeqCst) {
            if let Some(host) = self.host() {
                host.start_local();
            }
        }
        if self.leader.lock_or_recover().is_none() {
            self.watch_for_promotion();
        }
    }

    /// The leader connection `conn` is gone: re-elect in the background.
    fn on_disconnect(&self, conn: &Arc<Conn<T::Stream>>) {
        {
            let mut role = self.role.lock_or_recover();
            match &*role {
                Role::Follower(current) if Arc::ptr_eq(current, conn) => *role = Role::Electing,
                _ => return,
            }
        }
        tracing::info!(
            leader_pid = conn.leader_pid,
            "shared index: leader went away; taking over or reconnecting"
        );
        let Some(me) = self.me.upgrade() else { return };
        let spawned = std::thread::Builder::new()
            .name("astrolabe-shared-elect".into())
            .spawn(move || {
                let outcome = me.elect(QUEUE_BUDGET);
                me.settle(outcome, true);
            });
        if spawned.is_err() {
            self.go_local(true);
        }
    }

    /// The leader stopped answering: stop using it for good.
    fn abandon(&self, conn: &Arc<Conn<T::Stream>>) {
        tracing::warn!(
            leader_pid = conn.leader_pid,
            "shared index: leader did not answer in time; indexing independently"
        );
        self.go_local(true);
        conn.close();
    }

    /// Forward one request to the leader, waiting up to [`QUEUE_BUDGET`]
    /// through a hand-over. `reply_timeout` bounds one leader round trip.
    pub(crate) fn forward(
        &self,
        op: Op,
        reply_timeout: Option<Duration>,
        cancelled: &dyn Fn() -> bool,
    ) -> Forwarded {
        let mut deadline = Instant::now() + QUEUE_BUDGET;
        let mut handovers = 0;
        loop {
            let conn = {
                let mut role = self.role.lock_or_recover();
                loop {
                    match &*role {
                        Role::Follower(conn) => break Arc::clone(conn),
                        Role::Local | Role::Closed => return Forwarded::Local,
                        Role::Electing => {
                            let now = Instant::now();
                            if now >= deadline {
                                drop(role);
                                tracing::warn!("shared index: no leader within the queue budget; indexing independently");
                                self.go_local(true);
                                return Forwarded::Local;
                            }
                            if cancelled() {
                                return Forwarded::Cancelled;
                            }
                            let wait = (deadline - now).min(STEP * 2);
                            role = match self.settled.wait_timeout(role, wait) {
                                Ok((guard, _)) => guard,
                                Err(poisoned) => poisoned.into_inner().0,
                            };
                        }
                    }
                }
            };
            match conn.request(&op, reply_timeout, cancelled) {
                Ok(resp) => {
                    return Forwarded::Done(match resp.err {
                        Some(err) => Err(err),
                        None => Ok(resp.ok.unwrap_or(serde_json::Value::Null)),
                    })
                }
                Err(CallError::Cancelled) => return Forwarded::Cancelled,
                Err(CallError::Timeout) => {
                    self.abandon(&conn);
                    return Forwarded::Local;
                }
                Err(CallError::Disconnected) => {
                    self.on_disconnect(&conn);
                    handovers += 1;
                    if handovers > MAX_HANDOVERS_PER_CALL {
                        self.go_local(true);
                        return Forwarded::Local;
                    }
                    deadline = Instant::now() + QUEUE_BUDGET;
                }
            }
        }
    }

    /// Session over: close the socket (followers see EOF) or our leader
    /// connection. Call before the indexer lock is released.
    pub(crate) fn shutdown(&self) {
        let previous = std::mem::replace(&mut *self.role.lock_or_recover(), Role::Closed);
        self.settled.notify_all();
        if let Role::Follower(conn) = previous {
            conn.close();
        }
        let listener = self.leader.lock_or_recover().take();
        if let Some(listener) = listener {
            listener.stop();
        }
        // Break the server <-> shared reference cycle.
        self.host.lock_or_recover().take();
    }
}

// ---------------------------------------------------------------- leader

/// One follower session on the leader.
pub(crate) fn serve_conn<S: Stream>(
    mut stream: S,
    mine: &Hello,
    host: &dyn Host,
    expected_token: Option<&str>,
) {
    let _ = stream.set_read_timeout(Some(HELLO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let _ = reader.get_ref().set_read_timeout(Some(HELLO_TIMEOUT));
    let mut line = String::new();
    match (&mut reader).take(MAX_HELLO_BYTES).read_line(&mut line) {
        Ok(n) if n > 0 && line.ends_with('\n') => {}
        Ok(n) => {
            if expected_token.is_none() && n > 0 {
                let _ = write_frame(
                    &mut stream,
                    &HelloReply {
                        ok: false,
                        reason: Some("hello frame too large or incomplete".into()),
                        leader_pid: std::process::id(),
                    },
                );
            }
            return;
        }
        Err(_) => return,
    }
    let theirs: Hello = match serde_json::from_str(&line) {
        Ok(hello) => hello,
        Err(error) => {
            if expected_token.is_none() {
                let _ = write_frame(
                    &mut stream,
                    &HelloReply {
                        ok: false,
                        reason: Some(format!("bad hello: {error}")),
                        leader_pid: std::process::id(),
                    },
                );
            }
            return;
        }
    };
    if let Some(expected) = expected_token {
        let provided = theirs.auth_token.as_deref().unwrap_or("");
        if !super::tcp_transport::constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
            return;
        }
    }
    let reason = mine.mismatch(&theirs);
    let reply = HelloReply {
        ok: reason.is_none(),
        reason: reason.clone(),
        leader_pid: std::process::id(),
    };
    if write_frame(&mut stream, &reply).is_err() {
        return;
    }
    if let Some(reason) = reason {
        tracing::info!(follower_pid = theirs.pid, %reason, "shared index: refused a follower");
        return;
    }
    let _ = stream.set_read_timeout(None);
    let _ = reader.get_ref().set_read_timeout(None);
    tracing::info!(
        follower_pid = theirs.pid,
        session = %theirs.session_id,
        context = %theirs.context.name,
        cwd = %theirs.cwd,
        "shared index: follower attached"
    );
    let view = host.open_session(theirs.context.clone().into());
    let writer = Arc::new(Mutex::new(stream));
    let flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>> = Arc::new(Mutex::new(HashMap::new()));
    let reply =
        |writer: &Mutex<S>, id: u64, result: Result<serde_json::Value, serde_json::Value>| {
            let (ok, err) = match result {
                Ok(value) => (Some(value), None),
                Err(error) => (None, Some(error)),
            };
            let _ = write_frame(&mut *writer.lock_or_recover(), &Resp { id, ok, err });
        };
    for line in reader.lines() {
        let Ok(line) = line else { break };
        match serde_json::from_str::<Req>(&line) {
            Ok(Req::Call { id, name, args }) => {
                let flag = Arc::new(AtomicBool::new(false));
                flags.lock_or_recover().insert(id, Arc::clone(&flag));
                let view = Arc::clone(&view);
                let worker_writer = Arc::clone(&writer);
                let worker_flags = Arc::clone(&flags);
                let spawned = std::thread::Builder::new()
                    .name("astrolabe-shared-call".into())
                    .spawn(move || {
                        let result = view.call(&name, args, flag);
                        worker_flags.lock_or_recover().remove(&id);
                        reply(&worker_writer, id, result);
                    });
                if spawned.is_err() {
                    flags.lock_or_recover().remove(&id);
                    reply(
                        &writer,
                        id,
                        Err(
                            serde_json::json!({"code": -32603, "message": "leader cannot spawn a worker"}),
                        ),
                    );
                }
            }
            Ok(Req::Read { id, uri }) => reply(&writer, id, view.read(&uri)),
            Ok(Req::Cancel { id }) => {
                if let Some(flag) = flags.lock_or_recover().get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            Err(error) => tracing::warn!(%error, "shared index: bad request frame"),
        }
    }
    for flag in flags.lock_or_recover().values() {
        flag.store(true, Ordering::Relaxed);
    }
    tracing::info!(follower_pid = theirs.pid, session = %theirs.session_id, "shared index: follower detached");
}
