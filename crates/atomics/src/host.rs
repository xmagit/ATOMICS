//! In-process runtime wakeups. Producers signal work directly, independently of egui.

use std::sync::MutexGuard;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::app::State;
use crate::app::projection::{DiffBodies, DiffSummaries};
use crate::ipc::{ControlMessage, ControlOutcome};
use crate::protocol;
use crate::protocol::rendezvous::devices::Devices;
use crate::protocol::rendezvous::{self, Rendezvous, StatusCell, issuer::Published};
use atomics_core::control::ControlRequest;
use atomics_core::projection::Projections;
use atomics_core::tickets::TicketConfig;

/// Owns runtime state and the thread that services it. The desktop is an
/// in-process client; closing the whole application still shuts down the host.
/// `atomics host` runs the same host with no window attached (see `headless`).
pub struct Host {
    state: Arc<Mutex<State>>,
    wake: Wake,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    commands: Sender<ControlMessage>,
    projections: Arc<Projections>,
    /// Only when `[remote] enabled`; otherwise there is no thread and no socket.
    protocol: Option<protocol::Server>,
    remote_lock: Option<std::fs::File>,
    /// Why the server is not running although `[remote] enabled` asks for it.
    protocol_error: Option<String>,
    /// The terminals the protocol may stream; absent (and never published) without the server.
    terminals: Option<Arc<protocol::Terminals>>,
    /// Only with `[remote.rendezvous] enabled` beside a running protocol server.
    rendezvous: Option<Rendezvous>,
}

/// What the desktop and `atomics ctl` show of remote access.
#[derive(Clone, Default)]
pub(crate) struct RemoteView {
    pub enabled: bool,
    pub addr: Option<std::net::SocketAddr>,
    pub error: Option<String>,
    /// With `[remote.rendezvous] enabled`, whether or not it could start.
    pub rendezvous: Option<Arc<StatusCell>>,
    /// The paired devices and the pairing the desktop may offer, while the rendezvous runs.
    pub devices: Option<Arc<Devices>>,
    /// `[remote.rendezvous] pair_url`, the template of the pairing link.
    pub pair_url: String,
    /// Clients paired by a token (`protocol::clients`), while the protocol server runs.
    pub clients: Option<Arc<protocol::clients::PairedClients>>,
}

impl Host {
    pub(crate) fn start(mut state: State, wake: Wake, commands: Sender<ControlMessage>) -> Self {
        let projections = Arc::new(Projections::default());
        let bodies = Arc::new(DiffBodies::with_wake(wake.clone()));
        let remote = state.remote_access();
        let ownership =
            if remote.enabled { remote_lock(&atomics_core::paths::atomics_home()).map(Some) } else { Ok(None) };
        let (remote_lock, ownership_error) = match ownership {
            Ok(lock) => (lock, None),
            Err(error) => (None, Some(error)),
        };
        let started = match ownership_error {
            Some(error) => Err(error),
            None => start_protocol(&mut state, &remote, &projections, &bodies),
        };
        let (protocol, terminals, clients, protocol_error) = match started {
            Ok(Some((server, terminals, clients))) => (Some(server), Some(terminals), Some(clients), None),
            Ok(None) => (None, None, None, None),
            Err(error) => {
                // The desktop is useful without it; say why and carry on. `atomics host` stops instead.
                eprintln!("atomics: the protocol server did not start: {error}");
                (None, None, None, Some(error.to_string()))
            }
        };
        let (rendezvous, profile, status, devices) =
            start_rendezvous(&remote, protocol.as_ref(), state.window().clone(), state.rendezvous_profile());
        state.set_remote_view(RemoteView {
            enabled: remote.enabled,
            addr: protocol.as_ref().map(protocol::Server::addr),
            error: protocol_error.clone(),
            rendezvous: status,
            devices,
            pair_url: remote.rendezvous.pair_url.clone(),
            clients,
        });
        let state = Arc::new(Mutex::new(state));
        let stopped = Arc::new(AtomicBool::new(false));
        let (runtime, signal, stop, published) = (state.clone(), wake.clone(), stopped.clone(), projections.clone());
        let bodies = bodies.clone();
        let published_terminals = terminals.clone();
        let thread = std::thread::Builder::new()
            .name("atomics-runtime".into())
            .spawn(move || {
                let mut summaries = DiffSummaries::default();
                while !stop.load(Ordering::Acquire) {
                    let (deadline, flush) = {
                        let mut state = runtime.lock().unwrap();
                        bodies.refresh_folders(&mut state);
                        let window = state.window().clone();
                        if state.runtime_tick() {
                            window.request_repaint();
                        }
                        if let Some(delay) = state.take_output_delay() {
                            window.request_repaint_after(delay);
                        }
                        if !window.is_attached() {
                            state.drop_client_events();
                        }
                        // Only with the protocol server running: otherwise no terminal is ever streamed.
                        // Before the board, so a client that sees a session can subscribe to its screen.
                        if let Some(terminals) = &terminals {
                            state.publish_terminals(terminals);
                        }
                        // The profile the rendezvous registers through, as the workspace now has it.
                        if let Some(profile) = &profile {
                            profile.set(state.rendezvous_profile());
                        }
                        state.publish_projections(&published, &mut summaries, &bodies);
                        (state.runtime_deadline(), state.take_op_flush())
                    };
                    flush();
                    signal.wait_until(deadline);
                }
            })
            .expect("runtime thread");
        Self {
            state,
            wake,
            stopped,
            thread: Some(thread),
            commands,
            projections,
            protocol,
            remote_lock,
            protocol_error,
            terminals: published_terminals,
            rendezvous,
        }
    }

    /// Borrow the in-process native model. Desktop actions are sent to the host
    /// queue; this guard never services runtime work or waits for a command.
    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// The published projections. Readers load `Arc` snapshots; none of this takes the `State` lock.
    #[allow(dead_code)] // Tests read it directly.
    pub fn projections(&self) -> Arc<Projections> {
        self.projections.clone()
    }

    pub fn wake(&self) {
        self.wake.notify();
    }

    pub fn command(&self, request: ControlRequest) -> mpsc::Receiver<ControlOutcome> {
        let (reply, response) = mpsc::channel();
        let _ = self.commands.send(ControlMessage { request, reply });
        response
    }

    /// Where the protocol server listens, when it is on.
    pub fn protocol_addr(&self) -> Option<std::net::SocketAddr> {
        self.protocol.as_ref().map(protocol::Server::addr)
    }

    /// Why the protocol server failed to start although `[remote] enabled` asked for it. The
    /// error comes from binding and advertising, so it never holds the token.
    pub fn protocol_error(&self) -> Option<&str> {
        self.protocol_error.as_deref()
    }

    /// Whether the runtime thread publishes terminals for streaming (only with the server on).
    #[allow(dead_code)] // Read by tests.
    pub fn streams_terminals(&self) -> bool {
        self.terminals.is_some()
    }

    /// The rendezvous's status, when `[remote.rendezvous]` is enabled.
    pub fn rendezvous_status(&self) -> Option<rendezvous::Status> {
        self.state.lock().unwrap().remote_view().rendezvous.as_ref().map(|cell| cell.get())
    }

    pub fn stop(&mut self) {
        // The tunnel first: its channels are sessions of the server.
        if let Some(rendezvous) = &mut self.rendezvous {
            rendezvous.stop();
        }
        if let Some(server) = &mut self.protocol {
            server.stop();
        }
        self.stopped.store(true, Ordering::Release);
        self.wake.notify();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("runtime thread panicked");
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
        self.state.lock().unwrap().shutdown();
        self.remote_lock.take();
    }
}

/// The protocol server, the terminals it may stream and the clients paired with it.
type Protocol = (protocol::Server, Arc<protocol::Terminals>, Arc<protocol::clients::PairedClients>);

/// `Ok(None)` when `[remote]` is off: then there is no thread, no socket and no journal file.
/// `state` carries out the server's mutations; `config` is its `[remote]`, read once by `start`.
fn start_protocol(
    state: &mut State,
    config: &atomics_core::config::RemoteAccessConfig,
    projections: &Arc<Projections>,
    bodies: &Arc<DiffBodies>,
) -> std::io::Result<Option<Protocol>> {
    if !config.enabled {
        return Ok(None);
    }
    let home = atomics_core::paths::atomics_home();
    let terminals = Arc::new(protocol::Terminals::default());
    let clients = Arc::new(protocol::clients::PairedClients::open(home.join("paired-clients.json"))?);
    let server_config = protocol::ServerConfig {
        port: config.port,
        allowed_origins: config.allowed_origins.clone(),
        advertise: Some(home.join("protocol.json")),
        paired: Some(clients.clone()),
        // `atomics host` has no window whose panes would size its terminals.
        headless: !state.window().is_attached(),
        ..protocol::ServerConfig::default()
    };
    let commands = Arc::new(state.op_sender());
    let server =
        protocol::Server::start(server_config, projections.clone(), bodies.clone(), terminals.clone(), commands)?;
    state.serve_commands(&home.join("protocol-ops.jsonl"));
    Ok(Some((server, terminals, clients)))
}

/// Kept open for the lifetime of all remote services; never remove the lock file.
fn remote_lock(home: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(home)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(home.join("remote.lock"))?;
    file.try_lock()
        .map_err(|error| std::io::Error::other(format!("another ATOMICS owns remote access for this home: {error}")))?;
    Ok(file)
}

type Started =
    (Option<Rendezvous>, Option<Arc<Published<TicketConfig>>>, Option<Arc<StatusCell>>, Option<Arc<Devices>>);

/// The rendezvous, when `[remote.rendezvous] enabled`. Without the protocol server it cannot
/// serve a channel, so it does not start, and its status says why. Its keys are read, and made if
/// need be, on its own thread: the keychain may wait to be unlocked. `profile` is the ticket
/// profile as it is now; the runtime thread publishes its changes.
fn start_rendezvous(
    config: &atomics_core::config::RemoteAccessConfig,
    server: Option<&protocol::Server>,
    window: Window,
    profile: Option<Arc<TicketConfig>>,
) -> Started {
    let settings = &config.rendezvous;
    if !settings.enabled {
        return (None, None, None, None);
    }
    let repaint = window.clone();
    let status = StatusCell::new(move || window.request_repaint());
    let stopped = |note: &str| {
        status.update(|s| {
            s.phase = rendezvous::Phase::Stopped;
            s.note = Some(note.to_string());
        })
    };
    let Some(server) = server else {
        stopped(if config.enabled {
            "the protocol server did not start, so the rendezvous cannot serve a channel"
        } else {
            "the rendezvous needs [remote] enabled = true"
        });
        return (None, None, Some(status), None);
    };
    let home = atomics_core::paths::atomics_home();
    let devices = Devices::open(home.join("devices.json"), move || repaint.request_repaint());
    let published = Arc::new(Published::default());
    published.set(profile);
    let issuer = rendezvous::issuer::ProfileIssuer::new(published.clone(), crate::mcp_client::OAuth::current());
    let keys: rendezvous::KeySource = Box::new(|| {
        let store = crate::credentials::Store::new(crate::relay::Keychain::current());
        rendezvous::keys::DeviceKeys::load_or_create(&store, &atomics_core::paths::atomics_home())
    });
    let started = Rendezvous::start(
        rendezvous::Settings::new(&settings.url, &settings.name),
        server.sessions(),
        devices.clone(),
        Box::new(issuer),
        keys,
        status.clone(),
    );
    match started {
        Ok(rendezvous) => (Some(rendezvous), Some(published), Some(status), Some(devices)),
        Err(error) => {
            stopped(&format!("could not start the rendezvous thread: {error}"));
            (None, None, Some(status), None)
        }
    }
}

/// The runtime thread carries out protocol mutations: the sender wakes it.
impl protocol::Commands for Sender<protocol::OpRequest> {
    fn submit(&self, request: protocol::OpRequest) -> bool {
        self.send(request).is_ok()
    }
}

/// What the runtime may ask of a native window: a repaint, focus, and the size it last had.
/// The desktop attaches its egui context (`app::State::new`); the headless host attaches nothing,
/// so no runtime service needs egui, a display or a GPU to make progress.
pub(crate) trait Surface: Send + Sync {
    fn repaint(&self);
    fn repaint_after(&self, delay: Duration);
    /// Unminimize and focus the window, for a link that names something to show.
    fn raise(&self);
    fn viewport(&self) -> Viewport;
}

/// The window's geometry, as `atomics ctl list` and the restart handoff report it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Viewport {
    /// The content area, in points.
    pub content: [f32; 2],
    pub monitor: Option<[f32; 2]>,
    /// The window's inner size, where the platform reports it.
    pub inner: Option<[f32; 2]>,
    pub maximized: bool,
}

/// The native window attached to the runtime, if any. The default has none: every request
/// is then a no-op and `viewport` is `None`.
#[derive(Clone, Default)]
pub(crate) struct Window(Option<Arc<dyn Surface>>);

impl Window {
    pub fn attach(surface: impl Surface + 'static) -> Self {
        Self(Some(Arc::new(surface)))
    }

    pub fn is_attached(&self) -> bool {
        self.0.is_some()
    }

    pub fn request_repaint(&self) {
        if let Some(surface) = &self.0 {
            surface.repaint();
        }
    }

    pub fn request_repaint_after(&self, delay: Duration) {
        if let Some(surface) = &self.0 {
            surface.repaint_after(delay);
        }
    }

    pub fn raise(&self) {
        if let Some(surface) = &self.0 {
            surface.raise();
        }
    }

    pub fn viewport(&self) -> Option<Viewport> {
        self.0.as_ref().map(|surface| surface.viewport())
    }
}

#[derive(Clone, Default)]
pub struct Wake(Arc<(Mutex<bool>, Condvar)>);

impl Wake {
    pub fn notify(&self) {
        let (pending, ready) = &*self.0;
        *pending.lock().unwrap() = true;
        ready.notify_one();
    }

    pub fn wait_until(&self, deadline: Instant) {
        let (pending, ready) = &*self.0;
        let mut pending = pending.lock().unwrap();
        while !*pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            pending = ready.wait_timeout(pending, remaining).unwrap().0;
        }
        *pending = false;
    }

    pub fn channel<T>(&self) -> (Sender<T>, mpsc::Receiver<T>) {
        let (tx, rx) = mpsc::channel();
        (Sender { tx, wake: self.clone() }, rx)
    }
}

pub struct Sender<T> {
    tx: mpsc::Sender<T>,
    wake: Wake,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), wake: self.wake.clone() }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        self.tx.send(value)?;
        self.wake.notify();
        Ok(())
    }
}

#[cfg(test)]
impl<T> From<mpsc::Sender<T>> for Sender<T> {
    fn from(tx: mpsc::Sender<T>) -> Self {
        Self { tx, wake: Wake::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn messages_wake_a_sleeping_runtime_without_losing_an_early_signal() {
        let wake = Wake::default();
        let (tx, rx) = wake.channel();
        tx.send(1).unwrap();
        let started = Instant::now();
        wake.wait_until(started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.recv().unwrap(), 1);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            tx.send(2).unwrap();
        });
        let started = Instant::now();
        wake.wait_until(started + Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(rx.recv().unwrap(), 2);
        producer.join().unwrap();
    }

    #[test]
    fn runtime_wakes_at_its_deadline_without_a_producer() {
        let wake = Wake::default();
        let deadline = Instant::now() + Duration::from_millis(20);
        wake.wait_until(deadline);
        assert!(Instant::now() >= deadline);
    }
}
