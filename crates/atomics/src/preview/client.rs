//! Native local listeners. One open action accepts a session or no session for future machine views.
//!
//! The listeners and the relay behind them run on a thread of their own, never on the window's frames:
//! the window may stop drawing (another app in front, the window hidden), and a dev server's page loads
//! hundreds of modules, each a round trip. The thread wakes when a socket is ready or a tunnel frame
//! arrives from the host, which the connection's thread hands it directly ([`Tap`]) and which it answers
//! straight onto that connection. The window's turns only open and close listeners, as the board says.
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

use super::relay::Relay;
use crate::client::link::Frames;
use atomics_core::preview::Preview;
use atomics_core::wire::{self, Envelope, MAX_QUEUED_FRAMES, MAX_TUNNELS, Message};
use atomics_ui::Command;
use atomics_web::remote::RemoteBackend;

struct Listening {
    socket: TcpListener,
    session: Option<String>,
    preview: Preview,
}

/// What the window and the relay thread share.
#[derive(Default)]
struct Tunnels {
    listeners: HashMap<u16, Listening>,
    relay: Relay,
    next: u64,
    /// Whether the host agreed to tunnels on the current connection: until then, and after it ends, the
    /// connection's thread leaves tunnel frames to the window, which drops them.
    enabled: bool,
    /// Tunnel frames from the host, for the relay thread.
    inbox: Vec<Message>,
    /// The host sent more than [`MAX_QUEUED_FRAMES`] before the relay thread took them.
    overflow: bool,
    /// The connection's outgoing frames, by socket: where the relay thread sends. Without one, frames
    /// wait in `outbox` for the window's next turn.
    sink: Option<(u64, Frames)>,
    outbox: Vec<Message>,
    /// Errors for the window to show.
    errors: Vec<String>,
}

impl Tunnels {
    fn send(&mut self, message: Message) {
        match &self.sink {
            Some((_, frames)) => {
                if let Ok(frame) = wire::encode(&Envelope::new(message)) {
                    frames.send(frame);
                }
            }
            None => self.outbox.push(message),
        }
    }

    /// One turn of the relay: the host's frames, new connections, and both ways across every socket.
    fn turn(&mut self) {
        if std::mem::take(&mut self.overflow) {
            self.relay = Relay::default();
            self.inbox.clear();
            self.errors.push("Preview peer exceeded the receive queue".into());
        }
        for message in std::mem::take(&mut self.inbox) {
            if let Message::TunnelRefused { reason, .. } = &message {
                self.errors.push(format!("Preview: {reason}"));
            }
            let id = match &message {
                Message::TunnelOpened { id }
                | Message::TunnelRefused { id, .. }
                | Message::TunnelData { id, .. }
                | Message::TunnelAck { id, .. }
                | Message::TunnelClose { id, .. } => id.clone(),
                _ => continue,
            };
            if self.relay.receive(message).is_err() {
                self.relay.remove(&id);
                self.send(Message::TunnelClose { id, abort: true });
            }
        }
        let mut opened = Vec::new();
        for l in self.listeners.values() {
            while self.relay.len() < MAX_TUNNELS {
                match l.socket.accept() {
                    Ok((stream, _)) => {
                        self.next += 1;
                        let id = format!("preview-{}", self.next);
                        if self.relay.accept(id.clone(), l.preview.port, stream).is_ok() {
                            opened.push(Message::TunnelOpen { id, port: l.preview.port });
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        let listeners = &self.listeners;
        opened.extend(self.relay.poll(|port| listeners.contains_key(&port)));
        for message in opened {
            self.send(message);
        }
    }

    /// What the relay thread waits on until its next turn: listeners while it can take another
    /// connection, and the relay's sockets that can move data.
    fn interest(&self) -> Vec<libc::pollfd> {
        let accepting = self.relay.len() < MAX_TUNNELS;
        let listeners = self.listeners.values().filter(|_| accepting).map(|l| (l.socket.as_raw_fd(), libc::POLLIN));
        listeners.chain(self.relay.interest()).map(|(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect()
    }
}

struct Shared {
    tunnels: Mutex<Tunnels>,
    /// Ends the relay thread's wait: a byte on this pipe, made when the thread starts.
    wake_tx: OnceLock<UnixStream>,
    stop: AtomicBool,
    /// Draws the window again, for an error to show or a frame to send without a connection's sink.
    window: Arc<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Tunnels> {
        self.tunnels.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake(&self) {
        // A full pipe already holds a wake.
        if let Some(tx) = self.wake_tx.get() {
            let _ = (&*tx).write(&[1]);
        }
    }
}

/// The connection thread's way to the relay: tunnel frames go to the relay thread, not the window.
#[derive(Clone)]
pub struct Tap(Arc<Shared>);

impl Tap {
    /// Take `frame` if it is a tunnel frame on a connection with tunnels; `false` leaves it to the window.
    pub fn take(&self, frame: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(frame) else { return false };
        // Most frames are not tunnel frames: only these are decoded here, and again by the window.
        if !text.contains("tunnel_") {
            return false;
        }
        let Ok(envelope) = wire::decode(text) else { return false };
        if !matches!(
            envelope.message,
            Message::TunnelOpened { .. }
                | Message::TunnelRefused { .. }
                | Message::TunnelData { .. }
                | Message::TunnelAck { .. }
                | Message::TunnelClose { .. }
        ) {
            return false;
        }
        let mut tunnels = self.0.lock();
        if !tunnels.enabled {
            return false;
        }
        if tunnels.inbox.len() < MAX_QUEUED_FRAMES {
            tunnels.inbox.push(envelope.message);
        } else {
            tunnels.overflow = true;
        }
        drop(tunnels);
        self.0.wake();
        true
    }
}

pub struct Client {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Default for Client {
    fn default() -> Self {
        Self::new(Arc::new(|| {}))
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Client {
    /// Previews for a window that `window` draws again.
    pub fn new(window: Arc<dyn Fn() + Send + Sync>) -> Self {
        let shared = Arc::new(Shared {
            tunnels: Mutex::default(),
            wake_tx: OnceLock::new(),
            stop: AtomicBool::new(false),
            window,
        });
        Self { shared, thread: None }
    }

    /// For the connection's thread: hands tunnel frames to the relay as they arrive.
    pub fn tap(&self) -> Tap {
        Tap(self.shared.clone())
    }

    /// The connection ended: every listener and tunnel with it.
    pub fn reset(&self) {
        *self.shared.lock() = Tunnels::default();
    }

    /// Return the plain-HTTP browser URL after the loopback listener is ready.
    #[cfg(test)]
    pub fn open(&mut self, session: Option<String>, preview: Preview) -> io::Result<String> {
        let local = self.listen(session, preview.clone())?;
        Ok(preview.url(local))
    }

    /// The local port forwarding `preview`, listening once this returns.
    fn listen(&mut self, session: Option<String>, preview: Preview) -> io::Result<u16> {
        preview.validate().map_err(io::Error::other)?;
        self.start()?;
        let mut tunnels = self.shared.lock();
        if !tunnels.listeners.contains_key(&preview.port) {
            if tunnels.listeners.len() >= MAX_TUNNELS {
                return Err(io::Error::other("too many previews"));
            }
            let socket = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, preview.port))
                .or_else(|_| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
            socket.set_nonblocking(true)?;
            tunnels.listeners.insert(preview.port, Listening { socket, session, preview: preview.clone() });
        }
        let local = tunnels.listeners[&preview.port].socket.local_addr()?.port();
        drop(tunnels);
        self.shared.wake();
        Ok(local)
    }

    /// The relay thread, started with the first listener.
    fn start(&mut self) -> io::Result<()> {
        if self.thread.is_some() {
            return Ok(());
        }
        let (wake_tx, wake_rx) = UnixStream::pair()?;
        wake_tx.set_nonblocking(true)?;
        wake_rx.set_nonblocking(true)?;
        let shared = self.shared.clone();
        let thread =
            std::thread::Builder::new().name("atomics-preview-relay".into()).spawn(move || run(&shared, &wake_rx))?;
        // The thread takes a turn first, so nothing waits on a wake sent before this.
        let _ = self.shared.wake_tx.set(wake_tx);
        self.thread = Some(thread);
        Ok(())
    }

    #[cfg(test)]
    pub fn active(&self) -> bool {
        !self.shared.lock().listeners.is_empty()
    }

    /// The window's turn: open and close listeners as the board says, hand the relay `link` (the
    /// connection's socket and its outgoing frames), and show what went wrong. The relay itself moves
    /// data between turns.
    pub fn pump(&mut self, remote: &RemoteBackend, link: Option<(u64, &Frames)>) {
        if !remote.tunnels() {
            self.reset();
            remote.set_active_previews(Vec::new());
            return;
        }
        for action in remote.take_preview_actions() {
            match action {
                Command::OpenPreview { session, preview } => match self.listen(session, preview.clone()) {
                    // The probe goes through this listener and the relay: off this thread.
                    Ok(local) => {
                        std::thread::spawn(move || {
                            let _ = open_browser(&preview.url_with(local, speaks_tls(local)));
                        });
                    }
                    Err(error) => remote.preview_error(format!("Could not open Preview: {error}")),
                },
                Command::StopPreview { port } => {
                    self.shared.lock().listeners.remove(&port);
                }
                _ => {}
            }
        }
        let mut tunnels = self.shared.lock();
        tunnels.enabled = true;
        if tunnels.sink.as_ref().map(|(socket, _)| *socket) != link.map(|(socket, _)| socket) {
            tunnels.sink = link.map(|(socket, frames)| (socket, frames.clone()));
        }
        tunnels.listeners.retain(|port, l| {
            (l.session.is_none() && remote.machine_previewable(*port))
                || remote.with_mirror(|m| {
                    m.sessions.values().any(|s| {
                        l.session.as_ref().is_none_or(|id| s.id == *id)
                            && s.status != atomics_core::session::AgentStatus::Exited
                            && s.previews.iter().any(|p| p.port == *port)
                    })
                })
        });
        // Frames that came the window's way: from a backend fed by hand rather than a tapped connection.
        let mut came = remote.take_tunnel_messages();
        let room = MAX_QUEUED_FRAMES.saturating_sub(tunnels.inbox.len());
        if came.len() > room {
            came.truncate(room);
            tunnels.overflow = true;
        }
        tunnels.inbox.extend(came);
        for error in std::mem::take(&mut tunnels.errors) {
            remote.preview_error(error);
        }
        for message in std::mem::take(&mut tunnels.outbox) {
            remote.send_tunnel(message);
        }
        remote.set_active_previews(tunnels.listeners.keys().copied().collect());
        drop(tunnels);
        self.shared.wake();
    }
}

/// The relay thread: a turn, then a wait until a socket is ready or something wakes it.
fn run(shared: &Shared, wake_rx: &UnixStream) {
    while !shared.stop.load(Ordering::Acquire) {
        let mut fds = {
            let mut tunnels = shared.lock();
            if tunnels.enabled {
                tunnels.turn();
            }
            if !tunnels.errors.is_empty() || (tunnels.sink.is_none() && !tunnels.outbox.is_empty()) {
                (shared.window)();
            }
            tunnels.interest()
        };
        fds.push(libc::pollfd { fd: wake_rx.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        // Every socket the next turn can use is in `fds`; the timeout is only a backstop.
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 250) };
        let mut sink = [0u8; 64];
        while matches!((&*wake_rx).read(&mut sink), Ok(n) if n > 0) {}
    }
}

/// Whether the server on this machine's loopback `port` answers a TLS ClientHello, so the browser should
/// open it over https. Asked once, when someone opens the preview; discovery never probes. A TLS server
/// answers with a handshake or an alert record; a plain HTTP server with text, or by closing.
pub fn speaks_tls(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::time::Duration;
    let address = (std::net::Ipv4Addr::LOCALHOST, port).into();
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&address, Duration::from_secs(2)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if stream.write_all(&client_hello("localhost")).is_err() {
        return false;
    }
    let mut first = [0u8; 1];
    matches!(stream.read(&mut first), Ok(1) if first[0] == 0x16 || first[0] == 0x15)
}

/// A minimal TLS 1.2 ClientHello naming `host`: enough for a server to answer with its own handshake
/// record, or an alert, either of which says it speaks TLS.
fn client_hello(host: &str) -> Vec<u8> {
    let name = host.as_bytes();
    let mut server_name = vec![0x00, 0x00]; // extension: server_name
    server_name.extend(((name.len() + 5) as u16).to_be_bytes());
    server_name.extend(((name.len() + 3) as u16).to_be_bytes());
    server_name.push(0x00); // host_name
    server_name.extend((name.len() as u16).to_be_bytes());
    server_name.extend(name);
    let mut body = vec![0x03, 0x03]; // TLS 1.2
    body.extend([0u8; 32]); // random
    body.push(0x00); // no session id
    body.extend([0x00, 0x06, 0xc0, 0x2f, 0xc0, 0x2b, 0x13, 0x01]); // ECDHE-RSA/ECDSA-AES128-GCM, TLS_AES_128_GCM
    body.extend([0x01, 0x00]); // null compression
    body.extend((server_name.len() as u16).to_be_bytes());
    body.extend(server_name);
    let mut handshake = vec![0x01]; // ClientHello
    handshake.extend(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend(body);
    let mut record = vec![0x16, 0x03, 0x01]; // handshake record
    record.extend((handshake.len() as u16).to_be_bytes());
    record.extend(handshake);
    record
}

pub fn open_browser(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(url).spawn()?.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-connection server that reads the first bytes, then answers `reply` and hangs up.
    fn serve(reply: &'static [u8]) -> u16 {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut first = [0u8; 5];
            let _ = stream.read_exact(&mut first);
            let _ = stream.write_all(reply);
        });
        port
    }

    #[test]
    fn a_tls_server_is_opened_over_https_and_a_plain_one_over_http() {
        assert!(speaks_tls(serve(&[0x16, 0x03, 0x03, 0x00, 0x2a])), "a ServerHello record");
        assert!(speaks_tls(serve(&[0x15, 0x03, 0x03, 0x00, 0x02, 0x02, 0x28])), "a handshake_failure alert");
        assert!(!speaks_tls(serve(b"HTTP/1.1 400 Bad Request\r\n\r\n")), "plain HTTP");
        assert!(!speaks_tls(serve(b"")), "a server that just hangs up");
        let closed = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        assert!(!speaks_tls(closed), "nothing listening");
    }

    #[test]
    fn the_client_hello_is_one_well_formed_record() {
        let hello = client_hello("localhost");
        assert_eq!(&hello[..3], &[0x16, 0x03, 0x01]);
        assert_eq!(u16::from_be_bytes([hello[3], hello[4]]) as usize, hello.len() - 5, "record length");
        assert_eq!(hello[5], 0x01, "ClientHello");
        assert_eq!(u32::from_be_bytes([0, hello[6], hello[7], hello[8]]) as usize, hello.len() - 9, "handshake length");
        assert!(hello.windows(9).any(|w| w == b"localhost"), "names the host");
    }
}
