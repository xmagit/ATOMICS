//! The frame's life with one host, sans I/O: the boot, the encrypted channel through the broker, pairing,
//! `atomics.v1` into the [`RemoteBackend`], and reconnecting with a fresh ticket from the hosting page.
//! The browser glue (`web.rs`) carries out its [`Output`]s and feeds back what happened; the tests drive
//! it against a host built from `atomics_core`, with no browser.
//!
//! Reconnecting: when the channel drops, the driver waits (jittered, 1 s doubling to 30 s), asks the
//! hosting page for a fresh ticket ([`boot::WANTS_TICKET`]), dials again, and resumes with `hello`'s
//! prior epoch and cursor, so the host replays what was missed or sends a snapshot.
//!
//! A native client reaches its host directly instead ([`Driver::direct`]): on this machine, or through a
//! forward to another one, with no broker, ticket or Noise channel. The upgrade's bearer token is the
//! authentication, and the glue finds the address and token afresh for every dial
//! ([`Output::DialDirect`]), so a host that restarted with a new token is reached again.
//!
//! What it trusts: close codes and anything else outside the encrypted channel come through the
//! broker unauthenticated. So the stored key is forgotten only on the host's refusal sealed into the
//! channel (`wire::not_paired`); a bare `not_paired` close stops the frame but keeps the key. Only a
//! failed handshake answer says the host's key differs; a transport message that fails ends the
//! channel and the frame dials again.

use std::fmt;

use atomics_core::noise::{NoiseError, PAIRING_LIFETIME_SECS, Pairing, Secret};
use atomics_core::tunnel::{self, close};
use atomics_core::wire::{self, ErrorCode, Message};
use serde_json::Value;
use zeroize::Zeroizing;

use crate::boot::{self, Boot, FromParent, ParentOrigin, Refused, Ticket};
use crate::connection::{Connection, Event, Failure};
use crate::device::Device;
use crate::remote::RemoteBackend;

/// The `client_id` this client says `hello` with.
pub const CLIENT_ID: &str = "atomics-web";
/// The first wait before dialing again; it doubles up to [`MAX_RETRY_MS`].
const FIRST_RETRY_MS: u64 = 1_000;
const MAX_RETRY_MS: u64 = 30_000;
/// How long the broker's `quota` close keeps the client away.
const QUOTA_RETRY_MS: u64 = 60_000;
/// How long a channel must stay up for the next drop to start the waits over.
const STEADY_MS: u64 = 30_000;
/// How long to wait for a fresh ticket before asking again.
const TICKET_WAIT_MS: u64 = 15_000;

/// What the frame shows about its connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Waiting for the hosting page's boot.
    Waiting,
    Connecting,
    /// Pairing this browser with the host; the code shows once the handshake is done.
    Pairing {
        code: Option<String>,
    },
    /// A pairing boot names a host this browser already holds another key for: pairing would replace
    /// it, so the frame asks first ([`Driver::repair`]).
    ConfirmRepair,
    Live,
    Reconnecting {
        attempt: u32,
    },
    Stopped(Stop),
}

/// Why the frame stopped trying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// No parent origin is configured, so nothing may boot the frame.
    NoParent,
    /// This browser has no key for the host: it never paired here.
    NotHere,
    /// The host said, inside the encrypted channel, that it does not know this browser's key.
    NotPaired,
    /// The channel was closed as not paired, but the host did not say so itself: the key is kept.
    NotPairedUnconfirmed,
    /// The pairing link was used or has expired (or the host withdrew it): only a new one helps.
    NoPairing,
    /// The host's user refused this browser: the codes differed.
    PairRefused,
    /// The pairing's handshake failed: a used or wrong link.
    PairFailed,
    /// The channel dropped while the codes were being compared.
    PairInterrupted,
    /// The rendezvous service revoked the host.
    Revoked,
    /// The host's key is not the one this browser paired with.
    KeyMismatch,
    /// This browser could not start: no randomness, or a malformed key.
    Failed,
    /// A direct link: the host refused this client's token.
    Unauthorised,
    /// The host speaks another major version of `atomics.v1`.
    Version,
}

impl Stop {
    pub fn message(self) -> &'static str {
        match self {
            Stop::NoParent => "Open ATOMICS from the page that hosts it.",
            Stop::NotHere => {
                "This browser is not paired with this computer yet. In ATOMICS on the computer, open Remote devices and pair it."
            }
            Stop::NotPaired => {
                "The computer no longer knows this browser. Pair it again from Remote devices in ATOMICS on the computer."
            }
            Stop::NotPairedUnconfirmed => {
                "The connection was closed as not paired, but the computer did not confirm it, so this browser keeps its key. Reload the page to try again; if this keeps happening, pair again from Remote devices."
            }
            Stop::NoPairing => "This pairing link was used or has expired. Show a new code on the computer.",
            Stop::PairRefused => "The computer refused this browser: the codes did not match.",
            Stop::PairFailed => "The pairing link was not accepted. Start a new pairing from Remote devices.",
            Stop::PairInterrupted => "The connection dropped while pairing. Start a new pairing from Remote devices.",
            Stop::Revoked => "This computer's registration with the rendezvous service was revoked.",
            Stop::KeyMismatch => "The computer's key does not match the one this browser paired with. Pair again.",
            Stop::Failed => "This browser could not start the connection. Reload the page to try again.",
            Stop::Unauthorised => "The host did not accept this window's token. Pair this window with the host again.",
            Stop::Version => {
                "The host speaks another version of the ATOMICS protocol. Update ATOMICS here or on the host."
            }
        }
    }
}

/// What the browser glue does for the driver.
pub enum Output {
    /// Post `{type, v}` to the parent window, at the parent origin exactly.
    Post(&'static str),
    /// Open a WebSocket to `url` offering `protocols` (the second carries the ticket: never log it).
    Dial {
        socket: u64,
        url: String,
        protocols: Vec<Zeroizing<String>>,
    },
    /// A direct client: find the host's address and token now and open a WebSocket to it offering
    /// `atomics.v1`, with the token as its bearer. Messages on it are text frames. A refused token is
    /// reported as [`close::UNAUTHORISED`], a host that cannot be reached as 1006.
    DialDirect {
        socket: u64,
    },
    /// One binary message.
    Send {
        socket: u64,
        message: Vec<u8>,
    },
    Close {
        socket: u64,
        code: u16,
    },
    /// Read the stored device for this host, and answer with [`Driver::device_loaded`].
    LoadDevice(String),
    SaveDevice(Device),
    ForgetDevice(String),
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Output::Post(kind) => write!(f, "Post({kind})"),
            Output::Dial { socket, url, .. } => write!(f, "Dial({socket}, {url})"),
            Output::DialDirect { socket } => write!(f, "DialDirect({socket})"),
            Output::Send { socket, message } => write!(f, "Send({socket}, {} bytes)", message.len()),
            Output::Close { socket, code } => write!(f, "Close({socket}, {code})"),
            Output::LoadDevice(host) => write!(f, "LoadDevice({host})"),
            Output::SaveDevice(device) => write!(f, "SaveDevice({})", device.host_id),
            Output::ForgetDevice(host) => write!(f, "ForgetDevice({host})"),
        }
    }
}

struct Socket {
    id: u64,
    connection: Connection,
    open: bool,
}

#[derive(Default)]
struct Retry {
    attempt: u32,
    /// When to ask for a ticket and dial again.
    due_ms: Option<u64>,
    /// When a fresh ticket was asked for, while it has not come.
    asked_ms: Option<u64>,
    /// When the channel last opened.
    open_ms: Option<u64>,
}

pub struct Driver {
    parent: Option<ParentOrigin>,
    boot: Option<Box<Boot>>,
    pub remote: RemoteBackend,
    device_name: String,
    device: Option<Device>,
    /// A pairing boot's link and the key this browser pairs with, until the host accepts it.
    pairing: Option<(Pairing, Secret)>,
    /// When a pairing boot's link has surely expired: its lifetime after the hosting page got it
    /// ([`Boot::pairing_age_ms`] before the boot). Until then a host busy pairing another device is
    /// asked again.
    pair_until_ms: u64,
    /// The last try found the host pairing another device, and another try is due.
    pair_busy: bool,
    /// [`Output::LoadDevice`] went out and its answer has not come.
    loading: bool,
    /// What this browser holds for a host a pairing boot names, while the frame asks whether to
    /// replace it.
    known: Option<Device>,
    ticket: Option<Ticket>,
    socket: Option<Socket>,
    sockets: u64,
    status: Status,
    retry: Retry,
    now_ms: u64,
    outputs: Vec<Output>,
    /// A native client that reaches its host directly ([`Driver::direct`]).
    direct: bool,
}

#[derive(Debug, Clone, Copy)]
enum End {
    Code(u16),
    Failure(Failure),
    /// The host's refusal, sealed into the channel.
    HostRefused,
    /// The host speaks another major version.
    Version,
}

impl Driver {
    /// A frame whose parent origin is `parent` (none configured: nothing may boot it). `op_prefix` starts
    /// its operation IDs; `device_name` is how a host's pairing prompt names this browser.
    pub fn new(parent: Option<ParentOrigin>, op_prefix: &str, device_name: &str) -> Self {
        let status = if parent.is_some() { Status::Waiting } else { Status::Stopped(Stop::NoParent) };
        let outputs = if parent.is_some() { vec![Output::Post(boot::READY)] } else { Vec::new() };
        Self {
            parent,
            boot: None,
            remote: RemoteBackend::new(op_prefix),
            device_name: device_name.to_string(),
            device: None,
            pairing: None,
            pair_until_ms: 0,
            pair_busy: false,
            loading: false,
            known: None,
            ticket: None,
            socket: None,
            sockets: 0,
            status,
            retry: Retry::default(),
            now_ms: 0,
            outputs,
            direct: false,
        }
    }

    /// A native client of a host it reaches directly, on this machine or through a forward: it dials at
    /// once and again after every drop, with the browser's waits. `op_prefix` starts its operation IDs.
    pub fn direct(op_prefix: &str) -> Self {
        let mut driver = Self::new(None, op_prefix, "");
        driver.direct = true;
        #[cfg(not(target_arch = "wasm32"))]
        driver.remote.enable_native_previews();
        driver.status = Status::Connecting;
        driver.dial();
        driver
    }

    /// Whether this is a direct client ([`Driver::direct`]).
    pub fn is_direct(&self) -> bool {
        self.direct
    }

    /// Dial again now rather than at the next wait's end: the user asked, or what the host needs is back.
    pub fn retry_now(&mut self) {
        if self.socket.is_none() && self.direct {
            if matches!(self.status, Status::Stopped(_)) {
                self.status = Status::Connecting;
                self.retry = Retry::default();
            }
            self.dial();
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn boot(&self) -> Option<&Boot> {
        self.boot.as_deref()
    }

    pub fn take_outputs(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.outputs)
    }

    /// A message event on the window. Anything but the parent's boot or ticket is refused unread.
    pub fn message(&mut self, origin: &str, from_parent: bool, data: &Value) -> Result<(), Refused> {
        let Some(parent) = &self.parent else { return Err(Refused::Origin) };
        match boot::read(parent, origin, from_parent, data)? {
            // One boot per frame; the hosting page makes a new frame to boot again.
            FromParent::Boot(_) if self.boot.is_some() => {}
            FromParent::Boot(boot) => {
                self.outputs.push(Output::Post(boot::BOOTED));
                self.ticket = Some(boot.ticket.clone());
                let host_id = boot.host_id.clone();
                let pairing = boot.pairing.clone();
                let lifetime_ms = (PAIRING_LIFETIME_SECS * 1000).saturating_sub(boot.pairing_age_ms);
                self.boot = Some(boot);
                match pairing {
                    // Pairing: first, whether this browser already holds a key for the host.
                    Some(pairing) => match Secret::generate() {
                        Ok(secret) => {
                            self.pairing = Some((pairing, secret));
                            self.pair_until_ms = self.now_ms + lifetime_ms;
                            self.status = Status::Pairing { code: None };
                            self.load(host_id);
                        }
                        Err(_) => self.status = Status::Stopped(Stop::Failed),
                    },
                    None => {
                        self.status = Status::Connecting;
                        self.load(host_id);
                    }
                }
            }
            FromParent::Ticket { host_id, ticket } => {
                let ours = self.boot.as_ref().is_some_and(|b| b.host_id == host_id);
                if ours && self.retry.asked_ms.take().is_some() && self.socket.is_none() {
                    self.ticket = Some(ticket);
                    self.dial();
                }
            }
        }
        Ok(())
    }

    fn load(&mut self, host_id: String) {
        self.loading = true;
        self.outputs.push(Output::LoadDevice(host_id));
    }

    /// The stored device for the booted host, as [`Output::LoadDevice`] asked.
    pub fn device_loaded(&mut self, device: Option<Device>) {
        if !std::mem::take(&mut self.loading) {
            return;
        }
        let host = self.boot.as_ref().map(|b| b.host_id.clone());
        let device = device.filter(|d| Some(&d.host_id) == host.as_ref());
        match (&self.pairing, device) {
            // Pairing would replace the host key this browser trusts for that host: ask first. The
            // same key needs no question, since only that host can answer the pairing.
            (Some((pairing, _)), Some(known)) if known.host_key != pairing.host_key => {
                self.known = Some(known);
                self.status = Status::ConfirmRepair;
            }
            (Some(_), _) => self.dial(),
            (None, Some(device)) => {
                self.device = Some(device);
                self.dial();
            }
            (None, None) => self.status = Status::Stopped(Stop::NotHere),
        }
    }

    /// The answer to [`Status::ConfirmRepair`]: pair, replacing the key this browser holds for the
    /// host once the host accepts it, or keep that key and connect with it.
    pub fn repair(&mut self, pair: bool) {
        if self.status != Status::ConfirmRepair {
            return;
        }
        let known = self.known.take();
        if pair {
            self.status = Status::Pairing { code: None };
        } else {
            self.pairing = None;
            self.device = known;
            self.status = Status::Connecting;
        }
        self.dial();
    }

    /// Whether the last pairing try found the host pairing another device, and the frame tries again.
    pub fn pairing_busy(&self) -> bool {
        self.pair_busy && matches!(self.status, Status::Pairing { .. })
    }

    fn dial(&mut self) {
        if self.direct {
            self.sockets += 1;
            let id = self.sockets;
            self.outputs.push(Output::DialDirect { socket: id });
            self.socket = Some(Socket { id, connection: Connection::plain(), open: false });
            self.retry.due_ms = None;
            if !matches!(self.status, Status::Reconnecting { .. }) {
                self.status = Status::Connecting;
            }
            return;
        }
        let Some(boot) = &self.boot else { return };
        let Some(ticket) = self.ticket.take() else { return self.ask_ticket() };
        let connection = match (&self.pairing, &self.device) {
            (Some((pairing, secret)), _) => Connection::pair(pairing, secret, &self.device_name),
            (None, Some(device)) => Connection::session(&device.host_id, &device.host_key, &device.secret),
            (None, None) => return,
        };
        let Ok(connection) = connection else {
            self.status = Status::Stopped(Stop::Failed);
            return;
        };
        self.sockets += 1;
        let id = self.sockets;
        let protocols = vec![Zeroizing::new(tunnel::SUBPROTOCOL.to_string()), ticket.subprotocol()];
        self.outputs.push(Output::Dial { socket: id, url: boot.broker.clone(), protocols });
        self.socket = Some(Socket { id, connection, open: false });
        self.retry.due_ms = None;
        if !matches!(self.status, Status::Pairing { .. } | Status::Reconnecting { .. }) {
            self.status = Status::Connecting;
        }
    }

    fn ask_ticket(&mut self) {
        self.outputs.push(Output::Post(boot::WANTS_TICKET));
        self.retry.asked_ms = Some(self.now_ms);
    }

    fn current(&mut self, socket: u64) -> Option<&mut Socket> {
        self.socket.as_mut().filter(|s| s.id == socket)
    }

    /// The WebSocket opened, with the subprotocol the broker chose.
    pub fn socket_open(&mut self, socket: u64, protocol: &str) {
        let direct = self.direct;
        let Some(current) = self.current(socket) else { return };
        let expected = if direct { wire::SUBPROTOCOL } else { tunnel::SUBPROTOCOL };
        if protocol != expected {
            return self.fail(socket, End::Code(close::REFUSED));
        }
        current.open = true;
        // A direct link is open as soon as the upgrade is: the token authenticated it.
        if direct {
            let hello = self.remote.hello(CLIENT_ID);
            self.queue(&hello);
            self.status = Status::Live;
            self.retry.open_ms = Some(self.now_ms);
        }
        self.flush();
    }

    /// One binary message on the WebSocket.
    pub fn socket_message(&mut self, socket: u64, message: &[u8]) {
        let Some(current) = self.current(socket).filter(|s| s.open) else { return };
        let events = match current.connection.receive(message) {
            Ok(events) => events,
            Err(failure) => return self.fail(socket, End::Failure(failure)),
        };
        for event in events {
            match event {
                Event::Code(code) => {
                    self.pair_busy = false;
                    self.status = Status::Pairing { code: Some(code) };
                }
                Event::Paired(_) => {
                    let Some((pairing, secret)) = self.pairing.take() else { continue };
                    let device = Device { host_id: pairing.host_id.clone(), host_key: pairing.host_key, secret };
                    self.outputs.push(Output::SaveDevice(device.clone()));
                    self.outputs.push(Output::Post(boot::PAIRED));
                    self.device = Some(device);
                }
                Event::Open => {
                    let hello = self.remote.hello(CLIENT_ID);
                    self.queue(&hello);
                    self.status = Status::Live;
                    self.retry.open_ms = Some(self.now_ms);
                }
                // A frame this client cannot read (a kind from a newer minor) is skipped.
                Event::Frame(text) => {
                    let envelope = match wire::decode(&text) {
                        Ok(envelope) => envelope,
                        // Waiting cannot help: only an update on one side can.
                        Err(error) if error.code == ErrorCode::UnsupportedVersion => {
                            return self.fail(socket, End::Version);
                        }
                        Err(_) => continue,
                    };
                    if let Message::Error { code: ErrorCode::UnsupportedVersion, .. } = &envelope.message {
                        return self.fail(socket, End::Version);
                    }
                    if wire::is_not_paired(&envelope) {
                        return self.fail(socket, End::HostRefused);
                    }
                    self.remote.receive(envelope);
                }
            }
        }
        self.flush();
    }

    /// The WebSocket closed with `code` (1006 when it dropped without one).
    pub fn socket_closed(&mut self, socket: u64, code: u16) {
        if self.current(socket).is_none() {
            return;
        }
        self.socket = None;
        self.remote.disconnected();
        self.ended(End::Code(code));
    }

    /// The clock moved: milliseconds on any clock that only goes forward.
    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        self.remote.set_now(now_ms);
        if let Some(open) = self.retry.open_ms
            && self.socket.is_some()
            && now_ms.saturating_sub(open) >= STEADY_MS
        {
            self.retry.attempt = 0;
        }
        if self.retry.due_ms.is_some_and(|due| now_ms >= due) {
            self.retry.due_ms = None;
            self.dial();
        } else if self.retry.asked_ms.is_some_and(|asked| now_ms.saturating_sub(asked) >= TICKET_WAIT_MS) {
            self.ask_ticket();
        }
    }

    /// Milliseconds until the driver wants [`tick`](Driver::tick) again, if it waits for anything.
    pub fn wake_in(&self) -> Option<u64> {
        let due = self.retry.due_ms.map(|due| due.saturating_sub(self.now_ms));
        let ask = self.retry.asked_ms.map(|asked| (asked + TICKET_WAIT_MS).saturating_sub(self.now_ms));
        due.into_iter().chain(ask).min()
    }

    /// Send what the backend queued (the UI's commands, a resync, diff requests).
    pub fn flush(&mut self) {
        let Some(socket) = &mut self.socket else { return };
        if !socket.open {
            return;
        }
        let id = socket.id;
        let mut failed = None;
        if socket.connection.is_open() {
            for envelope in self.remote.take_outgoing() {
                let Ok(frame) = wire::encode(&envelope) else { continue };
                if let Err(failure) = socket.connection.send(&frame) {
                    failed = Some(failure);
                    break;
                }
            }
        }
        for message in socket.connection.take_outgoing() {
            self.outputs.push(Output::Send { socket: id, message });
        }
        if let Some(failure) = failed {
            self.fail(id, End::Failure(failure));
        }
    }

    fn queue(&mut self, envelope: &wire::Envelope) {
        if let (Some(socket), Ok(frame)) = (&mut self.socket, wire::encode(envelope)) {
            let _ = socket.connection.send(&frame);
        }
    }

    /// The channel failed on this side: close the socket and decide what next.
    fn fail(&mut self, socket: u64, end: End) {
        self.outputs.push(Output::Close { socket, code: close::REFUSED });
        self.socket = None;
        self.remote.disconnected();
        self.ended(end);
    }

    fn ended(&mut self, end: End) {
        let pairing = self.pairing.is_some();
        let compared = matches!(self.status, Status::Pairing { code: Some(_) });
        // Another device holds the host's pairing link for a while: try again while the link may be valid.
        // A used or expired link (`no_pairing`) cannot come back, so it stops at once.
        let busy = matches!(end, End::Code(close::PAIRING_BUSY)) && pairing && self.now_ms < self.pair_until_ms;
        self.pair_busy = busy;
        let stop = match end {
            End::Version => Some(Stop::Version),
            // A direct link's upgrade carries the token; nothing between can forge a refusal of it.
            End::Code(close::UNAUTHORISED) if self.direct => Some(Stop::Unauthorised),
            // Sealed by the host itself: the one sign to forget the key on.
            End::HostRefused => {
                if let Some(device) = self.device.take() {
                    self.outputs.push(Output::ForgetDevice(device.host_id));
                }
                Some(Stop::NotPaired)
            }
            End::Code(close::NOT_PAIRED) if pairing => Some(Stop::PairRefused),
            End::Code(close::NOT_PAIRED) => Some(Stop::NotPairedUnconfirmed),
            End::Code(close::PAIRING_BUSY) if busy => None,
            // Still busy when the link has surely expired: it is gone by now.
            End::Code(close::PAIRING_BUSY | close::NO_PAIRING) => Some(Stop::NoPairing),
            End::Code(close::REVOKED) => Some(Stop::Revoked),
            End::Failure(Failure::Refused) => Some(Stop::PairRefused),
            End::Failure(Failure::Handshake(NoiseError::Refused)) if pairing => Some(Stop::PairFailed),
            End::Failure(Failure::Handshake(NoiseError::Refused)) => Some(Stop::KeyMismatch),
            _ if pairing && compared => Some(Stop::PairInterrupted),
            _ => None,
        };
        if let Some(stop) = stop {
            self.pairing = None;
            self.retry = Retry::default();
            self.status = Status::Stopped(stop);
            return;
        }
        let wait = match end {
            End::Code(close::QUOTA) => QUOTA_RETRY_MS,
            _ => (FIRST_RETRY_MS << self.retry.attempt.min(5)).min(MAX_RETRY_MS),
        };
        // Between half and all of the wait, so clients that dropped together do not return together.
        let mut byte = [0u8; 1];
        let jitter = getrandom::fill(&mut byte).map_or(255, |()| byte[0]);
        let wait = wait / 2 + wait / 2 * u64::from(jitter) / 255;
        self.retry.attempt += 1;
        self.retry.due_ms = Some(self.now_ms + wait);
        self.retry.open_ms = None;
        self.status =
            if pairing { Status::Pairing { code: None } } else { Status::Reconnecting { attempt: self.retry.attempt } };
    }
}

#[cfg(test)]
mod tests;
