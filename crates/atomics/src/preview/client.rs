//! Native local listeners. One open action accepts a session or no session for future machine views.
//!
//! The listeners and the relay behind them run on a thread of their own, never on the window's frames:
//! the window may stop drawing (another app in front, the window hidden), and a dev server's page loads
//! hundreds of modules, each a round trip. The thread wakes when a socket is ready or a tunnel frame
//! arrives from the host, which the connection's thread hands it directly ([`Tap`]) and which it answers
//! straight onto that connection. The window's turns only open and close listeners, as the board says.
use super::probe::Probe;
use super::saved::{Choice, Store};
use atomics_core::preview::PreviewSource;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;

use super::relay::Relay;
use crate::client::link::Frames;
use atomics_core::preview::Preview;
use atomics_core::wire::{self, Envelope, MAX_QUEUED_FRAMES, MAX_TUNNELS, Message};
use atomics_ui::{Command, PreviewForward, PreviewStatus};
use atomics_web::remote::RemoteBackend;

/// What the window and the relay thread share. Neither holds the lock for longer than it takes to read or change
/// it: socket I/O, encoding and drawing the window happen with it released.
#[derive(Default)]
struct Tunnels {
    /// Shared with the relay thread's turn, which accepts on them with the lock released.
    listeners: HashMap<u16, (PreviewSource, Vec<Arc<TcpListener>>)>,
    probes: HashMap<String, Probe>,
    /// Ports the window asked to check; the relay thread opens their probes, as it alone opens tunnels.
    to_check: Vec<(u16, PreviewSource, u64)>,
    checked: Vec<(u16, Option<bool>)>,
    next: u64,
    /// Whether the host agreed to tunnels on the current connection: until then, and after it ends, the
    /// connection's thread leaves tunnel frames to the window, which drops them.
    enabled: bool,
    /// Tunnel frames from the host, for the relay thread.
    inbox: Vec<Message>,
    /// The host sent more than [`MAX_QUEUED_FRAMES`] before the relay thread took them.
    overflow: bool,
    /// The connection's outgoing frames, by socket: where the relay thread sends, in the order of `queued`.
    /// Without one, frames wait in `outbox` for the window's next turn.
    sink: Option<(u64, Frames)>,
    queued: Vec<Message>,
    outbox: Vec<Message>,
    /// Errors for the window to show.
    errors: Vec<String>,
    /// Counts the ends of the connection's tunnels: the relay thread lets its sockets go when it changes.
    generation: u64,
}

impl Tunnels {
    fn send(&mut self, message: Message) {
        match &self.sink {
            Some(_) => self.queued.push(message),
            None => self.outbox.push(message),
        }
    }

    /// Every tunnel of the connection ends; only the errors still to show stay. The old state is for the caller
    /// to drop once the lock is released.
    fn end(&mut self) -> Tunnels {
        let fresh =
            Tunnels { errors: std::mem::take(&mut self.errors), generation: self.generation + 1, ..Default::default() };
        std::mem::replace(self, fresh)
    }

    fn check(&mut self, port: u16, source: PreviewSource, waiting: u64) {
        if !self.to_check.iter().any(|(p, ..)| *p == port) {
            self.to_check.push((port, source, waiting));
        }
    }

    fn open_probe(&mut self, probe: Probe) {
        self.next += 1;
        let id = format!("probe-{}", self.next);
        let (port, source) = (probe.port, probe.source.clone());
        self.probes.insert(id.clone(), probe);
        self.send(Message::TunnelOpen { id, port, source });
    }

    fn finish_check(&mut self, id: String, scheme: Option<bool>) {
        if let Some(mut probe) = self.probes.remove(&id) {
            self.send(Message::TunnelClose { id, abort: true });
            if scheme.is_none() && probe.opened && !probe.http {
                probe.try_http();
                self.open_probe(probe);
            } else {
                self.checked.push((probe.port, scheme));
            }
        }
    }

    fn close_port(&mut self, port: u16) {
        self.listeners.remove(&port);
        if let Some(id) = self.probes.iter().find(|(_, p)| p.port == port).map(|(id, _)| id.clone()) {
            self.probes.remove(&id);
            self.send(Message::TunnelClose { id, abort: true });
        }
        self.to_check.retain(|(p, ..)| *p != port);
        self.checked.retain(|(p, _)| *p != port);
    }

    /// The part of the relay thread's turn that needs the shared state: the host's frames for the probes, the
    /// probes asked for and those that expired, and what the rest of the turn ([`Turn::run`]) works on.
    fn begin(&mut self, relay: &mut Relay) -> Turn {
        let mut turn = Turn::default();
        if std::mem::take(&mut self.overflow) {
            turn.ended = std::mem::take(relay);
            self.inbox.clear();
            self.errors.push("Preview peer exceeded the receive queue".into());
        }
        for message in std::mem::take(&mut self.inbox) {
            let Some(id) = tunnel_id(&message) else { continue };
            if id.starts_with("probe-") {
                if let Some(probe) = self.probes.get_mut(&id) {
                    if let Some(scheme) = probe.receive(&message) {
                        self.finish_check(id, scheme);
                    } else if matches!(message, Message::TunnelOpened { .. }) {
                        let bytes = if probe.http { &PROBE_HTTP } else { &PROBE_TLS };
                        self.send(Message::TunnelData { id, bytes: String::clone(bytes) });
                    }
                }
                continue;
            }
            if let Message::TunnelRefused { reason, .. } = &message {
                self.errors.push(format!("Preview: {reason}"));
            }
            if matches!(message, Message::TunnelRefused { .. } | Message::TunnelClose { abort: true, .. })
                && let Some(port) = relay.port(&id)
            {
                self.close_port(port);
                self.checked.push((port, None));
            }
            turn.frames.push(message);
        }
        let expired: Vec<_> = self.probes.iter().filter(|(_, p)| p.expired()).map(|(id, _)| id.clone()).collect();
        for id in expired {
            self.finish_check(id, None);
        }
        for (port, source, waiting) in std::mem::take(&mut self.to_check) {
            if relay.len() + self.probes.len() < MAX_TUNNELS && !self.probes.values().any(|probe| probe.port == port) {
                self.open_probe(Probe::new(port, source, waiting));
            }
        }
        turn.room = MAX_TUNNELS.saturating_sub(relay.len() + self.probes.len());
        for (&port, (source, sockets)) in &self.listeners {
            turn.allowed.insert(port, source.clone());
            if turn.room > 0 {
                turn.listeners.extend(sockets.iter().map(|listener| (port, source.clone(), listener.clone())));
            }
        }
        turn.send = std::mem::take(&mut self.queued);
        turn.sink = self.sink.as_ref().map(|(_, frames)| frames.clone());
        turn
    }

    /// What the relay thread waits on until its next turn: listeners while it can take another
    /// connection, and the relay's sockets that can move data.
    fn interest(&self, relay: &Relay) -> Vec<libc::pollfd> {
        let accepting = relay.len() + self.probes.len() < MAX_TUNNELS;
        let listeners = self
            .listeners
            .values()
            .filter(|_| accepting)
            .flat_map(|(_, sockets)| sockets)
            .map(|l| (l.as_raw_fd(), libc::POLLIN));
        listeners.chain(relay.interest()).map(|(fd, events)| libc::pollfd { fd, events, revents: 0 }).collect()
    }
}

/// What the relay thread's turn works on with the lock released: the host's frames for its connections, the
/// listeners to accept on, and the frames to send.
#[derive(Default)]
struct Turn {
    frames: Vec<Message>,
    listeners: Vec<(u16, PreviewSource, Arc<TcpListener>)>,
    /// The connections the relay may still take, and the source each listened port is for.
    room: usize,
    allowed: HashMap<u16, PreviewSource>,
    /// Frames for the host, in order, and the connection they are for; without one, they wait in the outbox.
    send: Vec<Message>,
    sink: Option<Frames>,
    /// The connections a full receive queue ended, closed with the lock released.
    ended: Relay,
}

impl Turn {
    /// The relay's socket I/O and the frames' encoding: the host's frames, new connections, and both ways across
    /// every socket. `accepted` numbers the connections taken.
    fn run(&mut self, relay: &mut Relay, accepted: &mut u64) {
        drop(std::mem::take(&mut self.ended));
        let mut out = std::mem::take(&mut self.send);
        for message in std::mem::take(&mut self.frames) {
            let Some(id) = tunnel_id(&message) else { continue };
            if relay.receive(message).is_err() {
                relay.remove(&id);
                out.push(Message::TunnelClose { id, abort: true });
            }
        }
        for (port, source, listener) in &self.listeners {
            while self.room > 0 {
                let Ok((stream, _)) = listener.accept() else { break };
                *accepted += 1;
                let id = format!("preview-{accepted}");
                if relay.accept(id.clone(), *port, source.clone(), stream).is_ok() {
                    self.room -= 1;
                    out.push(Message::TunnelOpen { id, port: *port, source: source.clone() });
                }
            }
        }
        let allowed = &self.allowed;
        out.extend(relay.poll(|port, source, _| allowed.get(&port) == Some(source)));
        match &self.sink {
            Some(frames) => {
                for message in out {
                    if let Ok(frame) = wire::encode(&Envelope::new(message)) {
                        frames.send(frame);
                    }
                }
            }
            None => self.send = out,
        }
    }
}

fn tunnel_id(message: &Message) -> Option<String> {
    match message {
        Message::TunnelOpened { id }
        | Message::TunnelRefused { id, .. }
        | Message::TunnelData { id, .. }
        | Message::TunnelAck { id, .. }
        | Message::TunnelClose { id, .. } => Some(id.clone()),
        _ => None,
    }
}

/// What a probe sends first, encoded once.
static PROBE_HTTP: LazyLock<String> = LazyLock::new(|| atomics_core::preview::encode(HTTP_HEAD));
static PROBE_TLS: LazyLock<String> = LazyLock::new(|| atomics_core::preview::encode(&client_hello("localhost")));

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
    /// Drop streams immediately, even if the window hasn't handled its disconnect notification yet.
    pub fn closed(&self, socket: u64) {
        let mut tunnels = self.0.lock();
        if tunnels.sink.as_ref().is_some_and(|(id, _)| *id == socket) {
            let ended = tunnels.end();
            drop(tunnels);
            drop(ended);
            self.0.wake();
        }
    }

    /// Take `envelope` if it is a tunnel frame on a connection with tunnels; anything else is given back, for
    /// the window.
    pub fn take(&self, socket: u64, envelope: Envelope) -> Option<Envelope> {
        if !matches!(
            envelope.message,
            Message::TunnelOpened { .. }
                | Message::TunnelRefused { .. }
                | Message::TunnelData { .. }
                | Message::TunnelAck { .. }
                | Message::TunnelClose { .. }
        ) {
            return Some(envelope);
        }
        let mut tunnels = self.0.lock();
        if tunnels.sink.as_ref().is_none_or(|(id, _)| *id != socket) {
            return Some(envelope);
        }
        if tunnels.inbox.len() < MAX_QUEUED_FRAMES {
            tunnels.inbox.push(envelope.message);
        } else {
            tunnels.overflow = true;
        }
        drop(tunnels);
        self.0.wake();
        None
    }
}

pub struct Client {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    choices: BTreeMap<u16, Choice>,
    store: Option<Store>,
    browser: Arc<dyn Fn(&str) + Send + Sync>,
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
        Self {
            shared,
            thread: None,
            choices: BTreeMap::new(),
            store: None,
            browser: Arc::new(|url| {
                let _ = open_browser(url);
            }),
        }
    }

    #[cfg(test)]
    pub fn browser(&mut self, open: impl Fn(&str) + Send + Sync + 'static) {
        self.browser = Arc::new(open);
    }

    /// For the connection's thread: hands tunnel frames to the relay as they arrive.
    pub fn tap(&self) -> Tap {
        Tap(self.shared.clone())
    }

    pub fn restore(&mut self, directory: std::path::PathBuf) {
        let store = Store(directory);
        match store.load() {
            Ok(choices) => self.choices = choices.into_iter().map(|c| (c.preview.port, c)).collect(),
            Err(error) => self.shared.lock().errors.push(format!("Could not load previews: {error}")),
        }
        self.store = Some(store);
    }

    fn save(&self, port: u16) {
        if let Some(store) = &self.store
            && let Err(error) = store.save(&self.choices[&port])
        {
            self.shared.lock().errors.push(format!("Could not save preview: {error}"));
        }
    }

    /// Only the user's choices cross a connection boundary. Every socket and queued byte ends here.
    pub fn reset(&mut self) {
        let ended = self.shared.lock().end();
        drop(ended);
        for choice in self.choices.values_mut() {
            choice.launch_browser = false;
        }
        self.shared.wake();
    }

    #[cfg(test)]
    pub fn open(&mut self, source: PreviewSource, preview: Preview) -> io::Result<String> {
        self.request(source, preview.clone())?;
        let local = self.bind(preview.port, preview.port, false)?;
        let choice = self.choices.get_mut(&preview.port).unwrap();
        choice.local = local;
        choice.waiting_since = None;
        choice.tls = Some(false);
        choice.launch_browser = false;
        self.save(preview.port);
        Ok(preview.url(local))
    }

    fn open_tab(&self, url: String) {
        let browser = self.browser.clone();
        std::thread::spawn(move || browser(&url));
    }

    fn request(&mut self, source: PreviewSource, preview: Preview) -> io::Result<()> {
        preview.validate().map_err(io::Error::other)?;
        if !self.choices.contains_key(&preview.port) && self.choices.len() >= MAX_TUNNELS {
            return Err(io::Error::other("too many previews"));
        }
        let port = preview.port;
        if self.shared.lock().listeners.contains_key(&port)
            && let Some(choice) = self.choices.get_mut(&port)
            && choice.source.as_ref() == Some(&source)
            && let Some(tls) = choice.tls
        {
            choice.preview = preview;
            let url = choice.preview.url_with(choice.local, tls);
            self.save(port);
            self.open_tab(url);
            return Ok(());
        }
        let previous = self.choices.get(&port);
        let local = previous.map_or(port, |choice| choice.local);
        let tls = previous.filter(|c| c.source.as_ref() == Some(&source)).and_then(|c| c.tls);
        self.shared.lock().close_port(port);
        self.choices.insert(
            port,
            Choice {
                source: Some(source),
                preview,
                local,
                tls,
                launch_browser: true,
                waiting_since: Some(atomics_core::store::now()),
                next_attempt: 0,
            },
        );
        self.save(port);
        Ok(())
    }

    fn bind(&mut self, port: u16, preferred: u16, tls: bool) -> io::Result<u16> {
        self.start()?;
        let (local, sockets) = super::bind_loopbacks(preferred, tls)?;
        let source =
            self.choices[&port].source.clone().ok_or_else(|| io::Error::other("preview source unavailable"))?;
        self.shared.lock().listeners.insert(port, (source, sockets.into_iter().map(Arc::new).collect()));
        self.shared.wake();
        Ok(local)
    }

    fn stop(&mut self, port: u16) {
        self.shared.lock().close_port(port);
        if let Some(store) = &self.store
            && let Err(e) = store.forget(port)
        {
            // Keep Stop available if the filesystem refused the deletion.
            if let Some(choice) = self.choices.get_mut(&port) {
                choice.waiting_since = Some(0);
                choice.launch_browser = false;
            }
            self.shared.lock().errors.push(format!("Could not forget preview {port}: {e}"));
        } else {
            self.choices.remove(&port);
        }
        self.shared.wake();
    }

    pub(crate) fn retry(&mut self, port: u16) {
        let now = atomics_core::store::now();
        if let Some(choice) = self.choices.get_mut(&port) {
            choice.waiting_since = Some(now);
            choice.next_attempt = 0;
            self.save(port);
        }
    }

    pub fn waiting(&self) -> bool {
        let now = atomics_core::store::now();
        self.choices.values().any(|c| c.waiting_since.is_some() && !c.paused(now))
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

    fn ready(&mut self, port: u16, tls: bool) -> io::Result<()> {
        let local = self.bind(port, self.choices[&port].local, tls)?;
        let choice = self.choices.get_mut(&port).unwrap();
        choice.local = local;
        choice.tls = Some(tls);
        choice.waiting_since = None;
        if std::mem::take(&mut choice.launch_browser) {
            let url = choice.preview.url_with(local, tls);
            self.open_tab(url);
        }
        self.save(port);
        Ok(())
    }

    /// Only a fresh offer from the same source and a successful probe may restore a listener.
    fn reconcile(&mut self, remote: &RemoteBackend, now: u64, checked: Vec<(u16, Option<bool>)>) {
        for port in self.choices.keys().copied().collect::<Vec<_>>() {
            let choice = &self.choices[&port];
            let offered = choice.source.as_ref().is_some_and(|source| remote.offers_preview(source, port));
            if offered && self.shared.lock().listeners.contains_key(&port) {
                continue;
            }
            if choice.waiting_since.is_none() {
                self.choices.get_mut(&port).unwrap().waiting_since = Some(now);
                self.save(port);
            }
            let choice = &self.choices[&port];
            let Some(source) = choice.source.as_ref().filter(|_| offered && !choice.paused(now)) else {
                self.shared.lock().close_port(port);
                continue;
            };
            if let Some((_, scheme)) = checked.iter().find(|(p, _)| *p == port) {
                if let Some(tls) = *scheme
                    && let Err(error) = self.ready(port, tls)
                {
                    remote.preview_error(format!("Could not bind preview: {error}"));
                }
                self.choices.get_mut(&port).unwrap().next_attempt = now + 3;
            } else if now >= choice.next_attempt {
                let source = source.clone();
                let waiting = now.saturating_sub(choice.waiting_since.unwrap_or(now));
                if let Err(e) = self.start() {
                    self.choices.get_mut(&port).unwrap().next_attempt = now + 3;
                    remote.preview_error(e.to_string());
                } else {
                    self.shared.lock().check(port, source, waiting);
                }
            }
        }
    }

    /// The window's turn: open and close listeners as the board says, hand the relay `link` (the
    /// connection's socket and its outgoing frames), and show what went wrong. The relay itself moves
    /// data between turns.
    pub fn pump(&mut self, remote: &RemoteBackend, link: Option<(u64, &Frames)>) {
        let now = atomics_core::store::now();
        for action in remote.take_preview_actions() {
            match action {
                Command::OpenPreview { source, preview } if remote.offers_preview(&source, preview.port) => {
                    if let Err(e) = self.request(source, preview) {
                        remote.preview_error(e.to_string());
                    }
                }
                Command::StopPreview { port } => self.stop(port),
                Command::RetryPreview { port } => self.retry(port),
                _ => {}
            }
        }
        remote.preview_interest(
            !self.choices.is_empty(),
            self.choices.values().any(|c| matches!(c.source, Some(PreviewSource::Machine(_))) && !c.paused(now)),
        );
        if !remote.tunnels() && self.shared.lock().enabled {
            self.reset();
        }
        let mut tunnels = self.shared.lock();
        tunnels.enabled = remote.tunnels();
        if tunnels.sink.as_ref().map(|(socket, _)| *socket) != link.map(|(socket, _)| socket) {
            tunnels.sink = link.filter(|_| tunnels.enabled).map(|(socket, frames)| (socket, frames.clone()));
            // What waited for another connection's sink is not this one's.
            tunnels.queued.clear();
        }
        let checked = std::mem::take(&mut tunnels.checked);
        drop(tunnels);
        self.reconcile(remote, now, checked);
        let mut tunnels = self.shared.lock();
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
        remote.set_preview_forwards(
            self.choices
                .values()
                .map(|choice| {
                    let status = if tunnels.listeners.contains_key(&choice.preview.port) {
                        PreviewStatus::Open
                    } else if choice.paused(now) {
                        PreviewStatus::Paused
                    } else {
                        PreviewStatus::Waiting
                    };
                    PreviewForward { source: choice.source.clone(), preview: choice.preview.clone(), status }
                })
                .collect(),
        );
        drop(tunnels);
        self.shared.wake();
    }
}

/// The relay thread: a turn, then a wait until a socket is ready or something wakes it. It takes the lock only
/// to begin and to end a turn; the sockets, the encoding and drawing the window are done with it released.
fn run(shared: &Shared, wake_rx: &UnixStream) {
    let mut relay = Relay::default();
    let (mut generation, mut accepted) = (0, 0);
    while !shared.stop.load(Ordering::Acquire) {
        let (mut turn, ended) = {
            let mut tunnels = shared.lock();
            let ended = (tunnels.generation != generation).then(|| {
                generation = tunnels.generation;
                std::mem::take(&mut relay)
            });
            (tunnels.enabled.then(|| tunnels.begin(&mut relay)), ended)
        };
        drop(ended);
        if let Some(turn) = &mut turn {
            turn.run(&mut relay, &mut accepted);
        }
        let unsent = turn.as_mut().map(|turn| std::mem::take(&mut turn.send)).unwrap_or_default();
        let (mut fds, window) = {
            let mut tunnels = shared.lock();
            // Frames of tunnels that ended meanwhile go nowhere.
            if tunnels.generation == generation {
                tunnels.outbox.extend(unsent);
            }
            let window = !tunnels.checked.is_empty()
                || !tunnels.errors.is_empty()
                || (tunnels.sink.is_none() && !tunnels.outbox.is_empty());
            (tunnels.interest(&relay), window)
        };
        drop(turn);
        if window {
            (shared.window)();
        }
        fds.push(libc::pollfd { fd: wake_rx.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        // Every socket the next turn can use is in `fds`; the timeout is only a backstop.
        // SAFETY: `fds` is a valid array of pollfd for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 250) };
        let mut sink = [0u8; 64];
        while matches!((&*wake_rx).read(&mut sink), Ok(n) if n > 0) {}
    }
}

const HTTP_HEAD: &[u8] = b"HEAD / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// A five-byte TLS record header or HTTP status prefix positively identifies the scheme.
pub(super) fn speaks_tls(mut stream: std::net::TcpStream) -> Option<bool> {
    use std::time::Duration;
    let address = stream.peer_addr().ok()?;
    let probe = |stream: &mut std::net::TcpStream, request: &[u8], read_timeout| {
        stream.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
        stream.set_read_timeout(Some(read_timeout)).ok()?;
        stream.write_all(request).ok()?;
        let mut header = [0; 5];
        stream.read_exact(&mut header).ok()?;
        scheme(&header)
    };
    probe(&mut stream, &client_hello("localhost"), Duration::from_secs(5)).or_else(|| {
        drop(stream);
        probe(&mut super::relay::connect_loopback(address).ok()?, HTTP_HEAD, Duration::from_secs(2))
    })
}

pub(super) fn scheme(header: &[u8]) -> Option<bool> {
    match header {
        [0x15 | 0x16, 0x03, 0x00..=0x03, hi, lo] if u16::from_be_bytes([*hi, *lo]) > 0 => Some(true),
        b"HTTP/" => Some(false),
        _ => None,
    }
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
    let url = atomics_ui::markdown::web_url(url).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "only http:// and https:// addresses can be opened")
    })?;
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
    use crate::preview::direct::Previews;
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    #[test]
    fn preview_browser_refuses_external_protocols() {
        for url in ["file:///tmp/test", "javascript:alert(1)", "httpx://example.com"] {
            assert_eq!(open_browser(url).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

    /// A one-connection server that reads the first bytes, then answers `reply` and hangs up.
    fn serve(reply: &'static [u8]) -> SocketAddr {
        serve_on(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap(), reply)
    }

    fn serve_on(listener: TcpListener, reply: &'static [u8]) -> SocketAddr {
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut first = [0u8; 5];
            let _ = stream.read_exact(&mut first);
            let _ = stream.write_all(reply);
        });
        address
    }

    #[test]
    fn a_tls_server_is_opened_over_https_and_a_plain_one_over_http() {
        let identify = |address| speaks_tls(super::super::relay::connect_loopback(address).unwrap());
        assert_eq!(identify(serve(&[0x16, 3, 3, 0, 42])), Some(true));
        assert_eq!(identify(serve(&[0x15, 3, 3, 0, 2])), Some(true));
        assert_eq!(identify(serve(b"HTTP/1.1 400 Bad Request")), Some(false));
        assert_eq!(identify(serve(b"hello")), None);
        assert_eq!(identify(serve(b"")), None);
        let silent = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        assert_eq!(identify(silent.local_addr().unwrap()), None);
    }

    #[test]
    fn a_direct_port_only_preview_refuses_two_services_on_the_same_port() {
        let (v4, v6) = crate::preview::test_ports::pair();
        let port = v6.local_addr().unwrap().port();
        serve_on(v6, b"HTTP/1.1 200 OK\r\nX-Owner: declaring-session\r\n\r\n");
        serve_on(v4, b"HTTP/1.1 200 OK\r\nX-Owner: other-session\r\n\r\n");
        let preview = Preview { port, path: "/demo".into(), ..Default::default() };
        let error = Previews::default().url(&PreviewSource::Session("declaring-session".into()), &preview).unwrap_err();
        assert!(error.to_string().contains("choose a listener"));
    }

    #[test]
    fn a_direct_port_only_preview_can_open_ipv6() {
        let (v6, _ipv4) = crate::preview::test_ports::ipv6_only();
        let address = serve_on(v6, b"HTTP/");
        let preview = Preview { port: address.port(), path: "/demo".into(), ..Default::default() };
        assert_eq!(
            Previews::default().url(&PreviewSource::Session("web".into()), &preview).unwrap(),
            format!("http://{address}/demo")
        );
    }

    #[test]
    fn direct_previews_probe_and_open_the_selected_loopback() {
        let (v4, v6) = crate::preview::test_ports::pair();
        let port = v4.local_addr().unwrap().port();
        serve_on(v4, b"HTTP/");
        serve_on(v6, &[0x16, 3, 3, 0, 42]);
        let preview = Preview { port, path: "/demo".into(), ..Default::default() };
        let previews = Previews::default();
        let open = |address: &str| {
            let row = atomics_core::machine::Port { address: address.into(), port, ..Default::default() };
            previews.url(&PreviewSource::Machine((&row).into()), &preview).unwrap()
        };
        assert_eq!(open("127.0.0.1"), format!("http://127.0.0.1:{port}/demo"));
        let tls = url::Url::parse(&open("::1")).unwrap();
        assert_eq!((tls.scheme(), tls.host_str(), tls.path()), ("https", Some("localhost"), "/demo"));
        assert_ne!(tls.port(), Some(port));
    }

    #[test]
    fn a_forwarded_https_url_owns_both_loopbacks_even_when_ipv6_is_occupied() {
        let occupied = TcpListener::bind(("::1", 0)).unwrap();
        let preferred = occupied.local_addr().unwrap().port();
        let mut client = Client::default();
        client.browser(|_| {});
        client
            .request(PreviewSource::Session("web".into()), Preview { port: preferred, ..Default::default() })
            .unwrap();
        client.ready(preferred, true).unwrap();
        let local = client.choices[&preferred].local;
        assert_ne!(local, preferred);
        for ip in ["127.0.0.1", "::1"] {
            std::net::TcpStream::connect((ip, local)).expect("the forward owns this address");
            assert!(TcpListener::bind((ip, local)).is_err());
        }
    }

    #[test]
    fn paused_previews_require_retry_and_stop_forgets_them_offline() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = Client::default();
        client.restore(dir.path().into());
        let preview = Preview {
            port: 5173,
            path: "/demo?key=preview-canary#private".into(),
            label: "preview-canary".into(),
            ..Default::default()
        };
        client.request(PreviewSource::Session("my-session".into()), preview.clone()).unwrap();
        let now = atomics_core::store::now();
        client.choices.get_mut(&preview.port).unwrap().waiting_since = Some(now - 600);
        client.save(preview.port);
        let file = std::fs::read_to_string(dir.path().join("5173.json")).unwrap();
        assert!(!file.contains("preview-canary") && !file.contains("#private"));
        drop(client);
        let mut client = Client::default();
        client.restore(dir.path().into());
        let remote = RemoteBackend::new("offline");
        client.pump(&remote, None);
        let choice = &client.choices[&preview.port];
        assert!(choice.paused(now));
        assert!(!choice.launch_browser);
        assert_eq!(choice.preview.path, "/demo");
        assert_eq!(choice.preview.title(), "Port 5173");
        assert!(!client.active());
        client.retry(preview.port);
        assert!(!client.choices[&preview.port].paused(now));
        client.stop(preview.port);
        client.restore(dir.path().into());
        assert!(client.choices.is_empty());
    }

    /// The relay thread draws the window, here for an error to show, with the shared state's lock released: the
    /// window's turn, which takes that lock, never waits for it.
    #[test]
    fn the_relay_thread_draws_the_window_with_the_lock_released() {
        use std::sync::atomic::AtomicUsize;
        let shared: Arc<OnceLock<std::sync::Weak<Shared>>> = Arc::default();
        let (drawn, locked) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let window = {
            let (shared, drawn, locked) = (shared.clone(), drawn.clone(), locked.clone());
            Arc::new(move || {
                if shared.get().and_then(std::sync::Weak::upgrade).is_some_and(|s| s.tunnels.try_lock().is_err()) {
                    locked.fetch_add(1, Ordering::Relaxed);
                }
                drawn.fetch_add(1, Ordering::Relaxed);
            })
        };
        let mut client = Client::new(window);
        shared.set(Arc::downgrade(&client.shared)).unwrap();
        client.start().unwrap();
        {
            let mut tunnels = client.shared.lock();
            tunnels.enabled = true;
            tunnels.overflow = true;
        }
        client.shared.wake();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while drawn.load(Ordering::Relaxed) == 0 {
            assert!(std::time::Instant::now() < deadline, "the relay thread did not draw the window");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(locked.load(Ordering::Relaxed), 0, "the window was drawn under the lock");
        assert_eq!(client.shared.lock().errors, ["Preview peer exceeded the receive queue"]);
    }

    /// A port the window asks to check is probed by the relay thread, which alone opens tunnels: it sends the
    /// probe's frames to the connection itself, in order, and the answer comes back to the window.
    #[test]
    fn the_relay_thread_probes_the_ports_the_window_asks_about() {
        let (tx, sent) = std::sync::mpsc::channel();
        let mut client = Client::default();
        client.start().unwrap();
        {
            let mut tunnels = client.shared.lock();
            tunnels.enabled = true;
            tunnels.sink = Some((1, Frames::to(tx)));
            let source = PreviewSource::Session("web".into());
            tunnels.check(5173, source.clone(), 0);
            tunnels.check(5173, source, 0);
        }
        client.shared.wake();
        let next = || {
            let frame = sent.recv_timeout(std::time::Duration::from_secs(10)).expect("a frame for the host");
            wire::decode(&frame).unwrap().message
        };
        let Message::TunnelOpen { id, port: 5173, .. } = next() else { panic!("the probe opens first") };
        client.shared.lock().inbox.push(Message::TunnelOpened { id: id.clone() });
        client.shared.wake();
        assert_eq!(next(), Message::TunnelData { id: id.clone(), bytes: PROBE_TLS.clone() });
        let reply = atomics_core::preview::encode(b"HTTP/1.1 400 Bad Request\r\n");
        client.shared.lock().inbox.push(Message::TunnelData { id: id.clone(), bytes: reply });
        client.shared.wake();
        assert_eq!(next(), Message::TunnelClose { id, abort: true });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while client.shared.lock().checked.is_empty() {
            assert!(std::time::Instant::now() < deadline, "the check did not finish");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(client.shared.lock().checked, [(5173, Some(false))], "a plain server, checked once");
        assert!(sent.try_recv().is_err(), "one probe for the port asked about twice");
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
