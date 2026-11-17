//! Native local listeners. One open action accepts a session or no session for future machine views.
//!
//! The listeners and the relay behind them run on a thread of their own, never on the window's frames:
//! the window may stop drawing (another app in front, the window hidden), and a dev server's page loads
//! hundreds of modules, each a round trip. The thread wakes when a socket is ready or a tunnel frame
//! arrives from the host, which the connection's thread hands it directly ([`Tap`]) and which it answers
//! straight onto that connection. The window's turns only open and close listeners, as the board says.
use super::probe::Probe;
use super::saved::{Choice, Store};
use std::collections::{BTreeMap, HashMap};
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

/// What the window and the relay thread share.
#[derive(Default)]
struct Tunnels {
    listeners: HashMap<u16, TcpListener>,
    probes: HashMap<String, Probe>,
    checked: Vec<(u16, Option<bool>)>,
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

    fn check(&mut self, port: u16) {
        if self.len() >= MAX_TUNNELS || self.probes.values().any(|probe| probe.port == port) {
            return;
        }
        self.next += 1;
        let id = format!("probe-{}", self.next);
        self.probes.insert(id.clone(), Probe::new(port));
        self.send(Message::TunnelOpen { id, port });
    }

    fn len(&self) -> usize {
        self.relay.len() + self.probes.len()
    }

    fn finish_check(&mut self, id: String, scheme: Option<bool>) {
        if let Some(probe) = self.probes.remove(&id) {
            self.checked.push((probe.port, scheme));
            self.send(Message::TunnelClose { id, abort: true });
        }
    }

    fn close_port(&mut self, port: u16) {
        self.listeners.remove(&port);
        if let Some(id) = self.probes.iter().find(|(_, p)| p.port == port).map(|(id, _)| id.clone()) {
            self.finish_check(id, None);
        }
        self.checked.retain(|(p, _)| *p != port);
    }

    /// One turn of the relay: the host's frames, new connections, and both ways across every socket.
    fn turn(&mut self) {
        if std::mem::take(&mut self.overflow) {
            self.relay = Relay::default();
            self.inbox.clear();
            self.errors.push("Preview peer exceeded the receive queue".into());
        }
        for message in std::mem::take(&mut self.inbox) {
            let id = match &message {
                Message::TunnelOpened { id }
                | Message::TunnelRefused { id, .. }
                | Message::TunnelData { id, .. }
                | Message::TunnelAck { id, .. }
                | Message::TunnelClose { id, .. } => id.clone(),
                _ => continue,
            };
            if id.starts_with("probe-") {
                if let Some(probe) = self.probes.get_mut(&id) {
                    if let Some(scheme) = probe.receive(&message) {
                        self.finish_check(id, scheme);
                    } else if matches!(message, Message::TunnelOpened { .. }) {
                        self.send(Message::TunnelData {
                            id,
                            bytes: atomics_core::preview::encode(&client_hello("localhost")),
                        });
                    }
                }
                continue;
            }
            if let Message::TunnelRefused { reason, .. } = &message {
                self.errors.push(format!("Preview: {reason}"));
            }
            if self.relay.receive(message).is_err() {
                self.relay.remove(&id);
                self.send(Message::TunnelClose { id, abort: true });
            }
        }
        let expired: Vec<_> = self
            .probes
            .iter()
            .filter(|(_, p)| p.expired())
            .map(|(id, p)| (id.clone(), p.opened.then_some(false)))
            .collect();
        for (id, scheme) in expired {
            self.finish_check(id, scheme);
        }
        let mut opened = Vec::new();
        for (&port, listener) in &self.listeners {
            while self.len() < MAX_TUNNELS {
                match listener.accept() {
                    Ok((stream, _)) => {
                        self.next += 1;
                        let id = format!("preview-{}", self.next);
                        if self.relay.accept(id.clone(), port, stream).is_ok() {
                            opened.push(Message::TunnelOpen { id, port });
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
        let accepting = self.len() < MAX_TUNNELS;
        let listeners = self.listeners.values().filter(|_| accepting).map(|l| (l.as_raw_fd(), libc::POLLIN));
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
    /// Drop streams immediately, even if the window hasn't handled its disconnect notification yet.
    pub fn closed(&self, socket: u64) {
        let mut tunnels = self.0.lock();
        if tunnels.sink.as_ref().is_some_and(|(id, _)| *id == socket) {
            let errors = std::mem::take(&mut tunnels.errors);
            *tunnels = Tunnels { errors, ..Default::default() };
            drop(tunnels);
            self.0.wake();
        }
    }

    /// Take `frame` if it is a tunnel frame on a connection with tunnels; `false` leaves it to the window.
    pub fn take(&self, socket: u64, frame: &[u8]) -> bool {
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
        if tunnels.sink.as_ref().is_none_or(|(id, _)| *id != socket) {
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
        {
            let mut tunnels = self.shared.lock();
            let errors = std::mem::take(&mut tunnels.errors);
            *tunnels = Tunnels { errors, ..Default::default() };
        }
        for choice in self.choices.values_mut() {
            choice.launch_browser = false;
        }
        self.shared.wake();
    }

    #[cfg(test)]
    pub fn open(&mut self, session: Option<String>, preview: Preview) -> io::Result<String> {
        self.request(session, preview.clone())?;
        let local = self.bind(preview.port, preview.port)?;
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

    fn request(&mut self, session: Option<String>, preview: Preview) -> io::Result<()> {
        preview.validate().map_err(io::Error::other)?;
        if !self.choices.contains_key(&preview.port) && self.choices.len() >= MAX_TUNNELS {
            return Err(io::Error::other("too many previews"));
        }
        let port = preview.port;
        if self.shared.lock().listeners.contains_key(&port)
            && let Some(choice) = self.choices.get_mut(&port)
            && choice.session == session
            && let Some(tls) = choice.tls
        {
            choice.preview = preview;
            let url = choice.preview.url_with(choice.local, tls);
            self.save(port);
            self.open_tab(url);
            return Ok(());
        }
        let local = self.choices.get(&port).map_or(port, |choice| choice.local);
        self.shared.lock().close_port(port);
        self.choices.insert(
            port,
            Choice {
                session,
                preview,
                local,
                tls: None,
                launch_browser: true,
                waiting_since: Some(atomics_core::store::now()),
                next_attempt: 0,
            },
        );
        self.save(port);
        Ok(())
    }

    fn bind(&mut self, port: u16, preferred: u16) -> io::Result<u16> {
        self.start()?;
        let socket = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, preferred))
            .or_else(|_| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)))?;
        socket.set_nonblocking(true)?;
        let local = socket.local_addr()?.port();
        self.shared.lock().listeners.insert(port, socket);
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

    /// Stays visible while the VM is stopped and while the saved port has no board row.
    pub fn show(&mut self, ui: &mut egui::Ui, remote: &RemoteBackend) {
        if self.choices.is_empty() {
            return;
        }
        let now = atomics_core::store::now();
        let mut stop = None;
        let mut retry = None;
        egui::Panel::top("preview-forwards").show(ui, |ui| {
            egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                for (&port, choice) in &self.choices {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!("Preview · {}", choice.preview.title()));
                        if choice.paused(now) {
                            ui.label("Paused after 10 minutes");
                            if ui.button("Retry").clicked() {
                                retry = Some(port);
                            }
                        } else if let Some(tls) = choice.tls.filter(|_| self.shared.lock().listeners.contains_key(&port)) {
                            let url = choice.preview.url_with(choice.local, tls);
                            ui.hyperlink_to(&url, &url);
                            if tls {
                                ui.label("Certificate help").on_hover_text(
                                    "If the browser rejects the certificate, configure the dev server with a certificate \
                                     for localhost and trust its development CA on this Mac (for example, with mkcert).",
                                );
                            }
                        } else {
                            ui.label(if remote.live() { "Waiting for preview…" } else { "Waiting for host…" });
                        }
                        if ui.button("Stop").clicked() {
                            stop = Some(port);
                        }
                    });
                }
            });
        });
        if let Some(port) = stop {
            self.stop(port);
        }
        if let Some(port) = retry {
            self.retry(port);
        }
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
        let local = self.bind(port, self.choices[&port].local)?;
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
            let offered = remote.offers_preview(choice.session.as_deref(), port);
            if offered && self.shared.lock().listeners.contains_key(&port) {
                continue;
            }
            if choice.waiting_since.is_none() {
                self.choices.get_mut(&port).unwrap().waiting_since = Some(now);
                self.save(port);
            }
            let choice = &self.choices[&port];
            if !offered || choice.paused(now) {
                self.shared.lock().close_port(port);
                continue;
            }
            if let Some((_, scheme)) = checked.iter().find(|(p, _)| *p == port) {
                if let Some(tls) = *scheme
                    && let Err(error) = self.ready(port, tls)
                {
                    remote.preview_error(format!("Could not bind preview: {error}"));
                }
                self.choices.get_mut(&port).unwrap().next_attempt = now + 3;
            } else if now >= choice.next_attempt {
                if let Err(e) = self.start() {
                    self.choices.get_mut(&port).unwrap().next_attempt = now + 3;
                    remote.preview_error(e.to_string());
                } else {
                    self.shared.lock().check(port);
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
                Command::OpenPreview { session, preview }
                    if remote.offers_preview(session.as_deref(), preview.port) =>
                {
                    if let Err(e) = self.request(session, preview) {
                        remote.preview_error(e.to_string());
                    }
                }
                Command::StopPreview { port } => self.stop(port),
                _ => {}
            }
        }
        remote.preview_interest(
            !self.choices.is_empty(),
            self.choices.values().any(|c| c.session.is_none() && !c.paused(now)),
        );
        if !remote.tunnels() && self.shared.lock().enabled {
            self.reset();
        }
        let mut tunnels = self.shared.lock();
        tunnels.enabled = remote.tunnels();
        if tunnels.sink.as_ref().map(|(socket, _)| *socket) != link.map(|(socket, _)| socket) {
            tunnels.sink = link.filter(|_| tunnels.enabled).map(|(socket, frames)| (socket, frames.clone()));
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
            if !tunnels.checked.is_empty()
                || !tunnels.errors.is_empty()
                || (tunnels.sink.is_none() && !tunnels.outbox.is_empty())
            {
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
    let mut header = [0; 5];
    stream.read_exact(&mut header).is_ok() && tls_record(&header)
}

pub(super) fn tls_record(header: &[u8]) -> bool {
    matches!(header, [0x15 | 0x16, 0x03, 0x00..=0x03, hi, lo] if u16::from_be_bytes([*hi,*lo]) > 0)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn preview_browser_refuses_external_protocols() {
        for url in ["file:///tmp/test", "javascript:alert(1)", "httpx://example.com"] {
            assert_eq!(open_browser(url).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
    }

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
        assert!(!speaks_tls(serve(&[0x16, b'n', b'o', b'p', b'e'])), "a non-TLS binary reply");
        assert!(!speaks_tls(serve(b"HTTP/1.1 400 Bad Request\r\n\r\n")), "plain HTTP");
        assert!(!speaks_tls(serve(b"")), "a server that just hangs up");
        let closed = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        assert!(!speaks_tls(closed), "nothing listening");
    }

    #[test]
    fn paused_previews_require_retry_and_stop_forgets_them_offline() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = Client::default();
        client.restore(dir.path().into());
        let preview = Preview { port: 5173, path: "/demo".into(), ..Default::default() };
        client.request(Some("my-session".into()), preview.clone()).unwrap();
        let now = atomics_core::store::now();
        client.choices.get_mut(&preview.port).unwrap().waiting_since = Some(now - 600);
        client.save(preview.port);
        drop(client);
        let mut client = Client::default();
        client.restore(dir.path().into());
        let remote = RemoteBackend::new("offline");
        client.pump(&remote, None);
        let choice = &client.choices[&preview.port];
        assert!(choice.paused(now));
        assert!(!choice.launch_browser);
        assert_eq!(choice.preview.path, "/demo");
        assert!(!client.active());
        client.retry(preview.port);
        assert!(!client.choices[&preview.port].paused(now));
        client.stop(preview.port);
        client.restore(dir.path().into());
        assert!(client.choices.is_empty());
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
