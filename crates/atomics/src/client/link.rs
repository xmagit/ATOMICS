//! One WebSocket to a host's protocol server, on a thread of its own: the native client's transport. It
//! connects with the host's bearer token, then carries `atomics.v1` text frames both ways until either
//! side closes. What happens is reported as [`Event`]s, each followed by a wake, so the window draws it;
//! tunnel frames go to the preview relay instead, which needs no frame of the window's.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use atomics_core::tunnel::close;
use atomics_core::wire;
use tungstenite::client::IntoClientRequest;
use tungstenite::{HandshakeError, Message, WebSocket};
use zeroize::Zeroizing;

/// Where a host's server is and how to get in.
pub struct Endpoint {
    /// The address to dial: the server's, or a forward's that leads to it.
    pub addr: Address,
    /// The `Host` the upgrade names: always a loopback name, whatever port the forward has.
    pub host: String,
    pub token: Zeroizing<String>,
}

/// What the link dials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// A port on this machine's loopback.
    Tcp(SocketAddr),
    /// A forward's Unix socket, in a folder only this user can enter (the VM's host, `vm.rs`).
    Unix(PathBuf),
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Tcp(addr) => write!(f, "{addr}"),
            Address::Unix(path) => write!(f, "{}", path.display()),
        }
    }
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("addr", &self.addr).field("host", &self.host).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Open {
        socket: u64,
        protocol: String,
    },
    Data {
        socket: u64,
        bytes: Vec<u8>,
    },
    /// The link ended; `unreachable` says why when it never got through (the host could not be found
    /// or connected to), for the window to show.
    Closed {
        socket: u64,
        code: u16,
        unreachable: Option<String>,
    },
}

/// Without a pipe to wake it: how long a read waits before the thread looks for frames to send.
const POLL: Duration = Duration::from_millis(4);
/// How long the thread waits for the host or a frame to send before it looks at `stop` again; a frame to
/// send, or a stop, ends the wait at once.
const IDLE: Duration = Duration::from_millis(250);
const CONNECT: Duration = Duration::from_secs(5);

pub struct Link {
    outgoing: Frames,
    stop: Arc<AtomicBool>,
}

/// Frames for the host, from any thread: each ends the link thread's wait.
#[derive(Clone)]
pub struct Frames {
    tx: Sender<String>,
    wake: Option<Arc<UnixStream>>,
}

impl Frames {
    pub fn send(&self, frame: String) {
        if self.tx.send(frame).is_ok() {
            self.wake();
        }
    }

    fn wake(&self) {
        // A full pipe already holds a wake.
        if let Some(wake) = &self.wake {
            let _ = (&**wake).write(&[1]);
        }
    }
}

impl Link {
    /// Find the host with `resolve` and connect, on a thread of its own; `wake` is called after every
    /// event. A host that cannot be found is a drop (1006), so the driver tries again later; why is in
    /// the closing event. `previews` takes the host's tunnel frames as they arrive.
    pub fn open(
        socket: u64,
        resolve: super::Resolve,
        events: SyncSender<Event>,
        wake: Arc<dyn Fn() + Send + Sync>,
        previews: crate::preview::client::Tap,
    ) -> Link {
        let (tx, frames) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let running = stop.clone();
        // Without the pipe the thread looks for frames to send every `POLL` instead.
        let (to_link, woken) = match UnixStream::pair().and_then(|(a, b)| {
            a.set_nonblocking(true)?;
            b.set_nonblocking(true)?;
            Ok((a, b))
        }) {
            Ok((to_link, woken)) => (Some(Arc::new(to_link)), Some(woken)),
            Err(_) => (None, None),
        };
        let outgoing = Frames { tx, wake: to_link };
        let spawned = std::thread::Builder::new().name("atomics-client-link".into()).spawn(move || {
            let send = |event| {
                let _ = events.send(event);
                wake();
            };
            let found = resolve().map_err(|error| Refused::because(format!("{error:#}")));
            let (code, unreachable) = match found.and_then(|endpoint| connect(&endpoint)) {
                Ok((mut ws, protocol)) => {
                    send(Event::Open { socket, protocol });
                    let mut received = |bytes: Vec<u8>| {
                        if !previews.take(&bytes) {
                            send(Event::Data { socket, bytes })
                        }
                    };
                    (pump(&mut ws, &frames, woken.as_ref(), &running, &mut received), None)
                }
                Err(Refused { code, why }) => (code, Some(why)),
            };
            send(Event::Closed { socket, code, unreachable });
        });
        if spawned.is_err() {
            eprintln!("atomics: could not start the connection's thread");
        }
        Link { outgoing, stop }
    }

    pub fn send(&self, frame: String) {
        self.outgoing.send(frame);
    }

    /// Where frames to send go, for a thread other than the window's (the preview relay).
    pub fn frames(&self) -> &Frames {
        &self.outgoing
    }

    /// Close it normally; no event follows that matters (the socket is no longer the driver's).
    pub fn close(&self) {
        self.stop.store(true, Ordering::Release);
        self.outgoing.wake();
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.close();
    }
}

/// Why a dial did not get through: the close code the driver gets, and what the window shows.
#[derive(Debug)]
pub(super) struct Refused {
    pub code: u16,
    pub why: String,
}

impl Refused {
    /// A drop (1006): the driver dials again later.
    fn because(why: impl std::fmt::Display) -> Self {
        Self { code: close::ABNORMAL, why: why.to_string() }
    }
}

/// The connection under the WebSocket: a loopback port, or a forward's Unix socket.
pub(super) enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Stream {
    fn open(addr: &Address) -> io::Result<Stream> {
        match addr {
            Address::Tcp(addr) => {
                let stream = TcpStream::connect_timeout(addr, CONNECT)?;
                let _ = stream.set_nodelay(true);
                Ok(Stream::Tcp(stream))
            }
            Address::Unix(path) => UnixStream::connect(path).map(Stream::Unix),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Stream::Unix(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    fn fd(&self) -> std::os::fd::RawFd {
        match self {
            Stream::Tcp(stream) => stream.as_raw_fd(),
            Stream::Unix(stream) => stream.as_raw_fd(),
        }
    }

    fn set_timeouts(&self, read: Duration, write: Duration) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.set_read_timeout(Some(read)).and(stream.set_write_timeout(Some(write))),
            Stream::Unix(stream) => stream.set_read_timeout(Some(read)).and(stream.set_write_timeout(Some(write))),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.read(buf),
            Stream::Unix(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(stream) => stream.write(buf),
            Stream::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(stream) => stream.flush(),
            Stream::Unix(stream) => stream.flush(),
        }
    }
}

/// The upgrade, with the token. A refused token is [`close::UNAUTHORISED`]; anything else that fails is
/// 1006, a drop.
pub(super) fn connect(endpoint: &Endpoint) -> Result<(WebSocket<Stream>, String), Refused> {
    let addr = &endpoint.addr;
    let stream = Stream::open(addr).map_err(|error| Refused::because(format!("cannot connect to {addr}: {error}")))?;
    stream.set_timeouts(CONNECT, CONNECT).map_err(Refused::because)?;
    let mut request = format!("ws://{}", endpoint.host).into_client_request().map_err(Refused::because)?;
    let headers = request.headers_mut();
    let bearer = Zeroizing::new(format!("Bearer {}", endpoint.token.as_str()));
    // Never the error itself: it could quote the token.
    let bearer = bearer.parse().map_err(|_| Refused::because("the token cannot be sent in a header"))?;
    headers.insert("Authorization", bearer);
    headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().map_err(Refused::because)?);
    match tungstenite::client::client_with_config(request, stream, Some(socket_config())) {
        Ok((ws, response)) => {
            let protocol = response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            ws.get_ref().set_timeouts(POLL, CONNECT).map_err(Refused::because)?;
            Ok((ws, protocol))
        }
        Err(HandshakeError::Failure(tungstenite::Error::Http(response))) if response.status() == 401 => {
            Err(Refused { code: close::UNAUTHORISED, why: format!("the host at {addr} refused the token") })
        }
        Err(HandshakeError::Failure(error)) => {
            Err(Refused::because(format!("the host at {addr} did not take the connection: {error}")))
        }
        Err(HandshakeError::Interrupted(_)) => {
            Err(Refused::because(format!("the host at {addr} did not answer in time")))
        }
    }
}

fn socket_config() -> tungstenite::protocol::WebSocketConfig {
    tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(wire::MAX_FRAME_BYTES))
        .max_frame_size(Some(wire::MAX_FRAME_BYTES))
}

/// Frames both ways until the host closes, the link drops, or `stop`. The close code it ended with.
/// Between frames the thread waits for the host or a byte on `woken`, which comes with each frame to
/// send; without `woken`, it looks for frames to send after every read times out.
fn pump(
    ws: &mut WebSocket<Stream>,
    frames: &Receiver<String>,
    woken: Option<&UnixStream>,
    stop: &AtomicBool,
    received: &mut dyn FnMut(Vec<u8>),
) -> u16 {
    // Nothing was waiting the last time the thread read: wait for the host or a wake before reading.
    let mut idle = false;
    loop {
        if stop.load(Ordering::Acquire) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return close::NORMAL;
        }
        loop {
            match frames.try_recv() {
                Ok(frame) => {
                    trace(">", &frame);
                    if ws.write(Message::text(frame)).is_err() {
                        return close::ABNORMAL;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    return close::NORMAL;
                }
            }
        }
        match ws.flush() {
            Ok(()) => {}
            Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
            Err(_) => return close::ABNORMAL,
        }
        let read = match woken {
            Some(woken) => {
                // Woken for a frame to send, or nothing came: send, then wait again.
                if idle && !wait(ws.get_ref(), woken) {
                    continue;
                }
                read_now(ws)
            }
            None => ws.read(),
        };
        idle = false;
        match read {
            Ok(Message::Text(text)) => {
                trace("<", text.as_str());
                received(text.as_bytes().to_vec())
            }
            Ok(Message::Close(frame)) => return frame.map_or(close::ABNORMAL, |f| u16::from(f.code)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if would_block(&e) => idle = true,
            Err(tungstenite::Error::ConnectionClosed) => return close::NORMAL,
            Err(_) => return close::ABNORMAL,
        }
    }
}

/// The next frame the host has sent, without waiting for one: a frame already in hand, or what the
/// socket holds. Writes, between reads, wait as before.
fn read_now(ws: &mut WebSocket<Stream>) -> tungstenite::Result<Message> {
    if ws.get_ref().set_nonblocking(true).is_err() {
        return ws.read();
    }
    let read = ws.read();
    let _ = ws.get_ref().set_nonblocking(false);
    read
}

/// Wait up to [`IDLE`] for the host to send or for a wake, then empty the wake pipe: a wake after
/// that is a byte that ends the next wait. Whether the host's side has something to read.
fn wait(stream: &Stream, woken: &UnixStream) -> bool {
    let mut fds = [
        libc::pollfd { fd: stream.fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: woken.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    // SAFETY: `fds` is a valid array of two pollfd for the duration of the call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, IDLE.as_millis() as libc::c_int) };
    let mut sink = [0u8; 64];
    while matches!((&*woken).read(&mut sink), Ok(n) if n > 0) {}
    // A failed wait reads, as the thread did before it waited.
    ready < 0 || fds[0].revents != 0
}

/// `ATOMICS_CLIENT_TRACE=1`: each frame's kind and size on stderr, for diagnosing a connection. Never
/// its content, which may hold terminal text.
fn trace(direction: &str, frame: &str) {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("ATOMICS_CLIENT_TRACE").is_some_and(|v| v == "1"));
    if *ON {
        let kind = serde_json::from_str::<serde_json::Value>(frame)
            .ok()
            .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .unwrap_or_default();
        eprintln!("atomics client {direction} {kind} ({} bytes)", frame.len());
    }
}

fn would_block(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}
