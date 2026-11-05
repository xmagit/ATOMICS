//! The `atomics.v1` protocol server: a thread inside the running app that serves the board, diffs
//! and terminal screens to clients on this machine. Off unless `[remote] enabled`; see
//! docs/daemon.md.
//!
//! It reads only what the runtime thread publishes (`atomics_core::projection`, `Terminals`), so it
//! never takes the `State` lock, and every socket write happens with no lock of any kind held.
//! Screens are copied under each emulator's short lock on a thread of their own (`screens.rs`), so
//! an emulator that is busy parsing or being painted never delays board events. Each client has
//! its own thread and a bounded queue; a client that stops reading is resynchronised or dropped
//! and cannot delay the host or other clients. A client holding a terminal's lease (`lease.rs`)
//! types into it from its own thread, straight to the PTY. Mutations go to whatever carries them
//! out (`ops.rs`; the runtime thread in the app), which journals them (`journal.rs`). Clients that
//! arrive through a rendezvous service (`rendezvous/`) are served by the same session code
//! ([`Sessions`]).

pub mod clients;
#[cfg(test)]
mod echo_bench;
mod hub;
#[cfg(test)]
mod input_tests;
pub mod journal;
pub mod lease;
mod link;
#[cfg(test)]
mod mutation_tests;
pub mod ops;
pub mod rendezvous;
#[cfg(test)]
mod screen_tests;
mod screens;
mod session;
#[cfg(test)]
pub(crate) mod tests;

use std::cell::Cell;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atomics_core::advert::{self, Advertisement};
use atomics_core::projection::Projections;
use atomics_core::wire::{self, MAX_QUEUED_FRAMES, Role};
use ring::rand::SecureRandom;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{HandshakeError, http};

pub use hub::RingLimits;
pub use lease::{Lease, TerminalControl};
#[cfg(test)]
pub use ops::NoCommands;
pub use ops::{Commands, OpReply, OpRequest};
pub use screens::{ScreenConfig, ScreenSource, TerminalEntry, Terminals};
pub use session::{Departed, DiffSource, Peer, PeerDevice, SessionConfig};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 0 picks a free port.
    pub port: u16,
    pub allowed_origins: Vec<String>,
    /// Where to write the advertisement (`atomics_core::advert`) for local clients; removed on
    /// stop while it is still this server's.
    pub advertise: Option<PathBuf>,
    /// Authenticated clients at once.
    pub max_clients: usize,
    /// Connections still inside the upgrade, counted apart from `max_clients` so that ones that
    /// never authenticate cannot take the real clients' places.
    pub max_pre_auth: usize,
    /// How long a connection has, from accept, to finish the whole upgrade.
    pub upgrade_deadline: Duration,
    /// What this listener's clients may do. They hold the bearer token from `protocol.json`, so
    /// on loopback they are controllers; view-only devices arrive with the rendezvous.
    pub role: Role,
    /// Clients paired by a token of their own (`clients.rs`), which they present instead of the
    /// per-start one: a desktop reaching this host through a forward.
    pub paired: Option<Arc<clients::PairedClients>>,
    pub write_timeout: Duration,
    pub poll_interval: Duration,
    pub queue_frames: usize,
    pub queue_bytes: usize,
    /// The most bytes a board frame may hold; a snapshot or diff over it is sent in parts or pages.
    pub frame_budget: usize,
    pub ring: RingLimits,
    pub session: SessionConfig,
    pub screens: ScreenConfig,
    pub lease: lease::Policy,
    /// This host has no desktop of its own (`atomics host`): it offers `fit`, so a client's panes
    /// give its terminals their own size.
    pub headless: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 0,
            allowed_origins: Vec::new(),
            advertise: None,
            max_clients: 4,
            max_pre_auth: 4,
            upgrade_deadline: Duration::from_secs(5),
            role: Role::Controller,
            paired: None,
            write_timeout: Duration::from_secs(5),
            poll_interval: Duration::from_millis(20),
            queue_frames: MAX_QUEUED_FRAMES,
            queue_bytes: 4 << 20,
            frame_budget: wire::MAX_FRAME_BYTES,
            ring: RingLimits::default(),
            session: SessionConfig::default(),
            screens: ScreenConfig::default(),
            lease: lease::Policy::default(),
            headless: false,
        }
    }
}

pub struct Server {
    addr: SocketAddr,
    #[allow(dead_code)] // read by tests; clients get the token from protocol.json
    token: String,
    shared: Arc<session::Shared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    screens: Option<JoinHandle<()>>,
    advertise: Option<PathBuf>,
    #[allow(dead_code)] // read by tests
    active: Arc<AtomicUsize>,
    #[allow(dead_code)] // read by tests
    pending: Arc<AtomicUsize>,
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buf = vec![0u8; bytes];
    ring::rand::SystemRandom::new().fill(&mut buf).map_err(|_| io::Error::other("no system randomness"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

impl Server {
    /// Bind 127.0.0.1 and start serving. The caller keeps the returned handle for the server's life,
    /// publishes the terminals it may stream into `terminals`, and carries out the mutations that
    /// arrive on `commands`.
    pub fn start(
        config: ServerConfig,
        projections: Arc<Projections>,
        diffs: Arc<dyn DiffSource>,
        terminals: Arc<Terminals>,
        commands: Arc<dyn Commands>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.port))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let token = random_hex(32)?;
        let epoch = random_hex(8)?;
        if let Some(path) = &config.advertise {
            write_advertisement(path, &Advertisement::current(&format!("ws://{addr}"), &token))?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let screens = Arc::new(screens::Screens::new(terminals, epoch.clone(), config.screens.clone()));
        let hub = Arc::new(Mutex::new(
            hub::Hub::new(epoch, config.ring.clone(), config.queue_frames, config.queue_bytes)
                .with_frame_budget(config.frame_budget),
        ));
        let advertise = config.advertise.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let pending = Arc::new(AtomicUsize::new(0));
        let shared = Arc::new(session::Shared {
            hub: hub.clone(),
            projections: projections.clone(),
            diffs,
            screens: screens.clone(),
            commands,
            stop: stop.clone(),
            config: config.session.clone(),
            lease: config.lease.clone(),
            headless: config.headless,
            queue_frames: config.queue_frames,
            frame_budget: config.frame_budget,
        });
        let accept = Accept {
            shared: shared.clone(),
            token: token.clone(),
            config: config.clone(),
            active: active.clone(),
            pending: pending.clone(),
        };
        let thread = std::thread::Builder::new().name("atomics-protocol".into()).spawn({
            let stop = stop.clone();
            move || {
                while !stop.load(Ordering::Acquire) {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => accept.client(stream),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    }
                    hub.lock().unwrap_or_else(|e| e.into_inner()).poll(&projections);
                    std::thread::sleep(accept.config.poll_interval);
                }
            }
        })?;
        let polling = screens.clone();
        let screens_thread = std::thread::Builder::new().name("atomics-protocol-screens".into()).spawn({
            let (stop, interval, echo) = (stop.clone(), config.poll_interval, config.screens.echo_poll);
            move || {
                while !stop.load(Ordering::Acquire) {
                    // Input unparks this thread so its echo is copied without waiting a whole interval.
                    let echoing = polling.poll();
                    std::thread::park_timeout(if echoing { echo } else { interval });
                }
            }
        });
        let screens = match screens_thread {
            Ok(handle) => {
                screens.set_thread(handle.thread().clone());
                handle
            }
            Err(e) => {
                stop.store(true, Ordering::Release);
                let _ = thread.join();
                if let Some(path) = &config.advertise {
                    advert::remove_if_ours(path, &token);
                }
                return Err(e);
            }
        };
        Ok(Self { addr, token, shared, stop, thread: Some(thread), screens: Some(screens), advertise, active, pending })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serves clients that arrive some other way than this server's listener.
    pub fn sessions(&self) -> Sessions {
        Sessions { shared: self.shared.clone() }
    }

    #[allow(dead_code)]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Clients with a live connection, for tests and diagnostics.
    #[allow(dead_code)]
    pub fn active_clients(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    /// Connections still inside the upgrade, for tests and diagnostics.
    #[allow(dead_code)]
    pub fn pending_upgrades(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in [self.thread.take(), self.screens.take()].into_iter().flatten() {
            thread.thread().unpark();
            let _ = thread.join();
        }
        if let Some(path) = self.advertise.take() {
            // Another instance may have replaced the file since; that one's stays.
            advert::remove_if_ours(&path, &self.token);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Serves clients whose link was made elsewhere, such as a rendezvous channel, with the same
/// session code, queues and limits as a loopback client after its upgrade. The server's stop
/// ends them too.
#[derive(Clone)]
pub struct Sessions {
    shared: Arc<session::Shared>,
}

impl Sessions {
    /// Serve one client on the calling thread, until its session ends.
    pub(crate) fn run(&self, link: &mut dyn link::Link, peer: &Peer) {
        session::serve(link, peer, &self.shared);
    }
}

/// The advertisement for clients on this machine, readable by this user only.
fn write_advertisement(path: &Path, advert: &Advertisement) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    file.write_all(serde_json::to_string(advert).map_err(io::Error::other)?.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(tmp, path)
}

struct Accept {
    shared: Arc<session::Shared>,
    token: String,
    config: ServerConfig,
    active: Arc<AtomicUsize>,
    pending: Arc<AtomicUsize>,
}

/// A place in one of the connection counters, given back on drop.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(counter: &Arc<AtomicUsize>, limit: usize) -> Option<Slot> {
        if counter.fetch_add(1, Ordering::AcqRel) >= limit {
            counter.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Slot(counter.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// What the upgrade of one connection needs.
struct Upgrade {
    token: String,
    paired: Option<Arc<clients::PairedClients>>,
    role: Role,
    origins: Vec<String>,
    write_timeout: Duration,
    deadline: Instant,
    active: Arc<AtomicUsize>,
    max_clients: usize,
}

impl Accept {
    fn client(&self, stream: TcpStream) {
        // Until its token is accepted a connection holds only a pre-auth place, so a local process
        // that never finishes the upgrade cannot use up the places real clients need.
        let Some(pre_auth) = Slot::take(&self.pending, self.config.max_pre_auth) else {
            return; // dropping the stream refuses the connection before any parsing
        };
        let upgrade = Upgrade {
            token: self.token.clone(),
            paired: self.config.paired.clone(),
            role: self.config.role,
            origins: self.config.allowed_origins.clone(),
            write_timeout: self.config.write_timeout,
            deadline: Instant::now() + self.config.upgrade_deadline,
            active: self.active.clone(),
            max_clients: self.config.max_clients,
        };
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new().name("atomics-protocol-client".into()).spawn(move || {
            let _ = serve_stream(stream, &shared, &upgrade, pre_auth);
        });
        if let Err(e) = spawned {
            // The connection and its place went with the closure.
            eprintln!("atomics: the protocol server could not start a client thread: {e}");
        }
    }
}

fn serve_stream(stream: TcpStream, shared: &session::Shared, up: &Upgrade, pre_auth: Slot) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(up.write_timeout))?;
    let claimed: Cell<Option<Slot>> = Cell::new(None);
    let peer: Cell<Option<Peer>> = Cell::new(None);
    #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
    let authenticate = |request: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let (response, who) = authorise(request, response, &up.token, up.paired.as_deref(), &up.origins)?;
        // Authenticated: only now does the connection take one of the client places.
        claimed.set(Some(Slot::take(&up.active, up.max_clients).ok_or_else(|| refuse(503, "too many clients"))?));
        peer.set(Some(match who {
            Some(device) => Peer { role: Role::Controller, device: Some(device) },
            None => Peer::local(up.role),
        }));
        Ok(response)
    };
    let stream = link::Stream::new(stream)?.with_deadline(up.deadline);
    let mut attempt = tungstenite::accept_hdr_with_config(stream, authenticate, Some(link::socket_config()));
    let socket = loop {
        match attempt {
            Ok(socket) => break socket,
            // A read timed out: at the deadline, or a moment before it.
            Err(HandshakeError::Interrupted(mid)) => {
                if Instant::now() >= up.deadline {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "the upgrade did not finish in time"));
                }
                attempt = mid.handshake();
            }
            Err(HandshakeError::Failure(e)) => return Err(io::Error::other(e.to_string())),
        }
    };
    drop(pre_auth);
    let _client = claimed.take();
    let mut link = link::WsLink::new(socket, up.write_timeout)?;
    let peer = peer.take().unwrap_or_else(|| Peer::local(up.role));
    session::serve(&mut link, &peer, shared);
    Ok(())
}

/// Equal contents in time that depends only on the length.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn refuse(status: u16, why: &str) -> ErrorResponse {
    http::Response::builder().status(status).body(Some(why.to_string())).expect("static response")
}

/// Whether a `Host` header names this machine's loopback: a browser a DNS-rebinding page drives
/// sends the page's own host name. Any port: a client may reach the server through a forward that
/// listens on another one (an ssh forward from a Mac to its VM).
fn loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => return false,
    };
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost")
}

#[allow(clippy::result_large_err)] // `ErrorResponse` is the type tungstenite's callback requires
/// The upgrade itself is authenticated: a loopback Host header (against DNS rebinding), an exact
/// Origin when one is sent, the bearer token (this start's, or a paired client's), and the
/// `atomics.v1` subprotocol. A paired client's token says which one it is.
fn authorise(
    request: &Request,
    mut response: Response,
    token: &str,
    paired: Option<&clients::PairedClients>,
    origins: &[String],
) -> Result<(Response, Option<PeerDevice>), ErrorResponse> {
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    if !header("host").is_some_and(loopback_host) {
        return Err(refuse(403, "unexpected Host"));
    }
    if let Some(origin) = header("origin")
        && !origins.iter().any(|o| o == origin)
    {
        return Err(refuse(403, "Origin not allowed"));
    }
    let presented = header("authorization").and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let device = if same_secret(presented.as_bytes(), token.as_bytes()) {
        None
    } else {
        match paired.and_then(|paired| paired.find(presented)) {
            Some(device) => Some(device),
            None => return Err(refuse(401, "a bearer token is required")),
        }
    };
    let offers = header("sec-websocket-protocol").is_some_and(|v| v.split(',').any(|p| p.trim() == wire::SUBPROTOCOL));
    if !offers {
        return Err(refuse(400, "subprotocol atomics.v1 is required"));
    }
    response.headers_mut().insert("Sec-WebSocket-Protocol", http::HeaderValue::from_static(wire::SUBPROTOCOL));
    Ok((response, device))
}
