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
//! A browser whose frame storage lasts only while it runs keeps a sealed copy of the key on the hosting page
//! ([`crate::keep`]): after a pairing the driver asks for it to be kept before it says `atomics.paired`, and a cold
//! start that finds this frame's store empty but the page's copy there waits on a tap to unlock it
//! ([`Stop::Locked`]). Elsewhere nothing changes: no passkey is asked for, and the frame's store is the key's home.
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
use crate::keep::{Answer, Ask, Passkeys, Path, Sealed};
use crate::remote::RemoteBackend;
use crate::storage::Storage;

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
/// How long the page must be hidden before a live socket is treated as potentially stale.
const LIVE_SOCKET_STALE_AFTER_MS: u64 = 30_000;
/// How long a save to this frame's store, or the hosting page's word that it keeps a sealed copy, may take.
const KEEP_WAIT_MS: u64 = 15_000;

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
    /// This storage context has no device record for the host (first use, cleared or evicted).
    NotHere,
    /// This frame's store has no key for the host, but the hosting page keeps a sealed copy: a tap and the
    /// passkey's user verification open it ([`crate::keep`]).
    Locked,
    /// Storage could not be read. This is never permission to pair or overwrite a key.
    DeviceReadFailed,
    /// The broker refused the hosting page's sign-in or connect ticket; the key is kept.
    SignIn,
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
    Refused,
}

impl Stop {
    pub fn message(self) -> &'static str {
        match self {
            Stop::NoParent => "Open ATOMICS from the page that hosts it.",
            Stop::NotHere => {
                "This browser is not paired with this computer yet. In ATOMICS on the computer, open Remote devices and pair it."
            }
            Stop::Locked => {
                "This browser forgot its pairing with this computer when it closed. Unlock the copy kept for it with your passkey."
            }
            Stop::DeviceReadFailed => {
                "This browser could not read its saved pairing. Try again to read it; your key has not been replaced."
            }
            Stop::SignIn => {
                "Sign in again on the page that hosts ATOMICS, then try again. This browser keeps its pairing."
            }
            Stop::NotPaired => {
                "The computer no longer knows this browser. Pair it again from Remote devices in ATOMICS on the computer."
            }
            Stop::NotPairedUnconfirmed => {
                "The connection was closed as not paired, but the computer did not confirm it. This browser keeps its key. Try again to reconnect with the saved pairing."
            }
            Stop::NoPairing => "This pairing link was used or has expired. Show a new code on the computer.",
            Stop::PairRefused => "The computer refused this browser: the codes did not match.",
            Stop::PairFailed => "The pairing link was not accepted. Start a new pairing from Remote devices.",
            Stop::PairInterrupted => "The connection dropped while pairing. Start a new pairing from Remote devices.",
            Stop::Revoked => "This computer's registration with the rendezvous service was revoked.",
            Stop::KeyMismatch => "The computer's key does not match the one this browser paired with. Pair again.",
            Stop::Failed => "This browser could not start the connection. Reload the page to try again.",
            Stop::Unauthorised => "The host did not accept this window's token. Try again to read the current token.",
            Stop::Refused => "The host refused this connection.",
            Stop::Version => {
                "The host speaks another version of the ATOMICS protocol. Update ATOMICS here or on the host."
            }
        }
    }
}

/// A failed storage read, distinct from a successful read that found no device.
#[derive(Debug, Clone, Copy)]
pub struct DeviceReadError;

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
    /// Save to this frame's store, and answer with [`Driver::device_saved`].
    SaveDevice(Device),
    ForgetDevice(String),
    /// Post `atomics.paired`, saying whether the hosting page keeps a sealed copy.
    Paired {
        kept: bool,
    },
    /// Post `atomics.keep` with the record, to the parent origin exactly.
    Keep(Sealed),
    /// Post `atomics.forget` for this host.
    Forget(String),
    /// Prepare what the page's passkey button does on its next tap, and answer with
    /// [`Driver::passkey_answered`].
    Passkey(Ask),
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
            Output::Paired { kept } => write!(f, "Paired(kept: {kept})"),
            Output::Keep(sealed) => write!(f, "Keep({})", sealed.host_id),
            Output::Forget(host) => write!(f, "Forget({host})"),
            Output::Passkey(ask) => write!(f, "Passkey({ask:?})"),
        }
    }
}

struct Socket {
    id: u64,
    connection: Connection,
    open: bool,
}

/// The sealed copy after a pairing, from the save to this frame's store to the hosting page's word.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Keeping {
    Idle,
    /// This frame's store is saving the device the host accepted.
    Saving {
        since_ms: u64,
    },
    /// The keep screen's button waits for a tap; after a failure, why. `made`: a passkey was just made in its
    /// own window, so whichever the browser offers will do.
    Ask {
        failed: Option<String>,
        made: bool,
    },
    /// The passkey's window is open.
    Enrolling,
    /// The record went to the hosting page, which has not answered yet.
    Handing {
        record: Sealed,
        since_ms: u64,
    },
}

/// What the keep screen shows ([`Driver::keep_prompt`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepPrompt {
    /// The button keeps the pairing; `first`: it makes the passkey first. After a failure, why, and
    /// `another`: the passkey asked for may be gone, so another can be made instead.
    Ask { first: bool, failed: Option<String>, another: bool },
    /// The passkey's window is open.
    Enrolling,
    /// The hosting page has the record and has not answered yet.
    Handing,
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
    /// The last successful read found a device, so a requested pairing can reuse its key.
    had_device: bool,
    /// A pairing boot's link. Read storage before choosing the device key to pair with.
    pairing: Option<Pairing>,
    /// When a pairing boot's link has surely expired: its lifetime after the hosting page got it
    /// ([`Boot::pairing_age_ms`] before the boot). Until then a host busy pairing another device is
    /// asked again.
    pair_until_ms: u64,
    /// The last try found the host pairing another device, and another try is due.
    pair_busy: bool,
    /// [`Output::LoadDevice`] went out and its answer has not come.
    loading: bool,
    /// What the browser's storage did, as the glue last said ([`Driver::storage_noted`]).
    storage: Option<Storage>,
    /// The boot's: the hosting page keeps sealed records, the one it keeps for this host, and whether it sent
    /// one that could not be read.
    keeps: bool,
    sealed: Option<Sealed>,
    sealed_unreadable: bool,
    /// What this browser offers for passkeys ([`Driver::passkeys_noted`]).
    passkeys: Passkeys,
    keeping: Keeping,
    /// The passkey this page knows of, for the next seal: the kept record's, or the one just made or used.
    credential: Option<String>,
    /// Where this frame's key came from, and what came of keeping a new pairing.
    path: Path,
    /// Why the last unlock did not open the hosting page's copy.
    unlock_failed: Option<String>,
    /// What this browser holds for a host a pairing boot names, while the frame asks whether to
    /// replace it.
    known: Option<Device>,
    ticket: Option<Ticket>,
    socket: Option<Socket>,
    sockets: u64,
    status: Status,
    refusal: Option<String>,
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
    Refused,
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
            had_device: false,
            pairing: None,
            pair_until_ms: 0,
            pair_busy: false,
            loading: false,
            storage: None,
            keeps: false,
            sealed: None,
            sealed_unreadable: false,
            passkeys: Passkeys::default(),
            keeping: Keeping::Idle,
            credential: None,
            path: Path::Unread,
            unlock_failed: None,
            known: None,
            ticket: None,
            socket: None,
            sockets: 0,
            status,
            refusal: None,
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
        self.refusal = None;
        if self.status == Status::Stopped(Stop::DeviceReadFailed) {
            if let Some(host) = self.boot.as_ref().map(|b| b.host_id.clone()) {
                self.status = Status::Connecting;
                self.load(host);
            }
            return;
        }
        if self.socket.is_none() && self.direct {
            if matches!(self.status, Status::Stopped(_)) {
                self.status = Status::Connecting;
                self.retry = Retry::default();
            }
            self.dial();
        } else if self.socket.is_none()
            && !self.direct
            && matches!(
                self.status,
                Status::Reconnecting { .. }
                    | Status::Stopped(Stop::SignIn | Stop::NotPairedUnconfirmed | Stop::Refused)
            )
        {
            self.status = if self.pairing.is_some() {
                Status::Pairing { code: None }
            } else {
                Status::Reconnecting { attempt: self.retry.attempt }
            };
            self.retry.due_ms = None;
            self.ask_ticket();
        }
    }

    /// Returning from the background may leave a socket that looks open but can no longer deliver.
    /// Keep a live session through short tab switches, preserving its screen and typing lease.
    /// Replace only an ordinary session, never interrupt code comparison or retry a security refusal.
    pub fn resumed(&mut self, hidden_ms: u64) {
        if self.status == Status::Live && hidden_ms < LIVE_SOCKET_STALE_AFTER_MS {
            return;
        }
        if self.direct || self.loading || self.pairing.is_some() || self.device.is_none() {
            return;
        }
        if !matches!(
            self.status,
            Status::Live | Status::Connecting | Status::Reconnecting { .. } | Status::Stopped(Stop::SignIn)
        ) {
            return;
        }
        if let Some(socket) = self.socket.take() {
            self.outputs.push(Output::Close { socket: socket.id, code: close::NORMAL });
            self.remote.disconnected();
        }
        self.ticket = None;
        self.status = Status::Reconnecting { attempt: self.retry.attempt };
        self.retry.due_ms = None;
        self.ask_ticket();
    }

    /// One safe diagnostic line for the pairing and recovery screens. Never contains key material.
    pub fn pairing_reason(&self) -> Option<&'static str> {
        match self.status {
            Status::Pairing { .. } if self.device.is_some() && self.known.is_some() => {
                Some("Reason: host key changed; pairing was requested.")
            }
            Status::Pairing { .. } if self.had_device => {
                Some("Reason: pairing was requested; reusing the stored device key.")
            }
            Status::Pairing { .. } | Status::Stopped(Stop::NotHere) => Some("Reason: no key stored for this host."),
            Status::Stopped(Stop::Locked) => {
                Some("Reason: no key in this frame's store; the page keeps a sealed copy.")
            }
            Status::ConfirmRepair | Status::Stopped(Stop::KeyMismatch) => Some("Reason: host key changed."),
            Status::Stopped(Stop::DeviceReadFailed) => Some("Reason: could not read the stored key."),
            Status::Stopped(Stop::NotPaired) => Some("Reason: host refused this key inside the encrypted connection."),
            Status::Stopped(Stop::NotPairedUnconfirmed) => {
                Some("Reason: connection refused pairing without host confirmation; key kept.")
            }
            Status::Stopped(Stop::NoPairing | Stop::PairRefused | Stop::PairFailed | Stop::PairInterrupted) => {
                Some("Reason: the requested pairing did not complete; any stored key is kept.")
            }
            _ => None,
        }
    }

    /// What the device store says about its storage: after each read, save and forget, and before
    /// the read's answer, so a pairing that follows names it.
    pub fn storage_noted(&mut self, storage: Storage) {
        self.storage = Some(storage);
    }

    /// The storage line under [`Driver::pairing_reason`], wherever that shows: what the frame's store did, then
    /// [`Driver::key_line`]. Never contains key material.
    pub fn storage_reason(&self) -> Option<String> {
        self.pairing_reason()?;
        Some(match &self.storage {
            Some(storage) => format!("{}. {}", storage.line(), self.key_line()),
            None => self.key_line(),
        })
    }

    /// Where this frame's key came from on this page (its store, the page's sealed copy, or no passkey to open
    /// that with), what this browser offers for passkeys, and what the hosting page keeps.
    pub fn key_line(&self) -> String {
        let page = match (self.keeps, &self.sealed, self.sealed_unreadable) {
            (false, ..) => "page keeps no copies".to_string(),
            (true, _, true) => "page's copy for this host unreadable".to_string(),
            (true, Some(sealed), _) => format!("page keeps a sealed copy from {}", utc(sealed.paired_at)),
            (true, None, _) => "page keeps no copy for this host".to_string(),
        };
        let unlock = self.unlock_failed.as_ref().map(|why| format!("; last unlock: {why}")).unwrap_or_default();
        format!("Key: {}; {}; {page}{unlock}", self.path.line(), self.passkeys.line())
    }

    /// How a pairing names this browser to the host: its name, and the short form of what its storage held and
    /// what could keep the key (`c`: the page's copy, `-` for a page that keeps none; `a`: a usable passkey), so
    /// the host's device list says what each pairing found.
    fn pairing_name(&self) -> String {
        let Some(storage) = &self.storage else { return self.device_name.clone() };
        let copy = match (self.keeps, &self.sealed, self.sealed_unreadable) {
            (false, ..) => '-',
            (true, _, true) => 'x',
            (true, Some(_), _) => '1',
            (true, None, _) => '0',
        };
        let passkey = match (self.passkeys.known, self.passkeys.unusable()) {
            (false, _) => '?',
            (true, None) => '1',
            (true, Some(_)) => '0',
        };
        format!("{} {}", self.device_name, storage.tag(&format!(" c{copy} a{passkey}")))
    }

    /// What this browser offers for passkeys, once the glue has asked. A frame waiting to unlock a copy no
    /// passkey can open here says so instead.
    pub fn passkeys_noted(&mut self, passkeys: Passkeys) {
        self.passkeys = passkeys;
        if self.status == Status::Stopped(Stop::Locked)
            && let Some(why) = self.passkeys.cannot_unlock()
        {
            self.path = Path::NoPasskey(why.into());
            self.status = Status::Stopped(Stop::NotHere);
            self.outputs.push(Output::Passkey(Ask::Done));
        }
    }

    /// What the keep screen shows after a pairing, while there is something to keep.
    pub fn keep_prompt(&self) -> Option<KeepPrompt> {
        match &self.keeping {
            Keeping::Ask { failed, made } => Some(KeepPrompt::Ask {
                first: self.credential.is_none() && !made,
                another: failed.is_some() && self.credential.is_some(),
                failed: failed.clone(),
            }),
            Keeping::Enrolling => Some(KeepPrompt::Enrolling),
            Keeping::Handing { .. } => Some(KeepPrompt::Handing),
            Keeping::Idle | Keeping::Saving { .. } => None,
        }
    }

    /// Why the last tap on [`Stop::Locked`]'s button did not unlock the copy.
    pub fn unlock_failed(&self) -> Option<&str> {
        self.unlock_failed.as_deref()
    }

    /// This frame's store saved the device [`Output::SaveDevice`] named and read it back, or could not (`Err` names
    /// the step). After a pairing, the sealed copy follows where it can help: on a browser whose frame storage
    /// ends with it, with a page that keeps copies and a passkey with PRF. Anywhere else the pairing is reported at
    /// once, as before.
    pub fn device_saved(&mut self, saved: Result<(), String>) {
        if !matches!(self.keeping, Keeping::Saving { .. }) {
            return;
        }
        let local = saved.err().map(|step| format!("this frame's store failed: {step}"));
        let why = if !self.keeps {
            Some("the page keeps no copies")
        } else if self.passkeys.known && !self.passkeys.per_session {
            Some("this browser keeps the frame's storage")
        } else {
            self.passkeys.unusable()
        };
        match why {
            Some(why) => {
                self.finish_keep(false, Some(local.map_or_else(|| why.to_string(), |l| format!("{why}; {l}"))))
            }
            None => {
                self.keeping = Keeping::Ask { failed: local, made: false };
                self.ask_keep();
            }
        }
    }

    fn ask_keep(&mut self) {
        let Some(device) = self.device.clone() else { return self.finish_keep(false, Some("no device".into())) };
        let made = matches!(self.keeping, Keeping::Ask { made: true, .. });
        let (credential, create_in_frame) = (self.credential.clone(), self.passkeys.create_in_frame);
        self.outputs.push(Output::Passkey(Ask::Keep { device, credential, made, create_in_frame }));
    }

    /// The pairing is reported; `kept`, whether the hosting page keeps a sealed copy, and why not.
    fn finish_keep(&mut self, kept: bool, why: Option<String>) {
        let asked = !matches!(self.keeping, Keeping::Idle | Keeping::Saving { .. });
        self.keeping = Keeping::Idle;
        if asked {
            self.outputs.push(Output::Passkey(Ask::Done));
        }
        self.outputs.push(Output::Paired { kept });
        self.path = Path::Paired { kept, why };
    }

    /// The keep screen's "Not now": this browser keeps the pairing until it closes.
    pub fn keep_declined(&mut self) {
        if matches!(self.keeping, Keeping::Ask { .. } | Keeping::Enrolling) {
            self.finish_keep(false, Some("not now".into()));
        }
    }

    /// The passkey's window was closed without saying: "Done" on the keep screen means a passkey was made there.
    pub fn enrolled_elsewhere(&mut self, made: bool) {
        if self.keeping == Keeping::Enrolling {
            let failed = (!made).then(|| "making the passkey was cancelled".to_string());
            self.keeping = Keeping::Ask { failed, made };
            self.ask_keep();
        }
    }

    /// After a failure with the passkey this page knew of: make another instead, on the next tap.
    pub fn use_another_passkey(&mut self) {
        if let Keeping::Ask { failed, .. } = &self.keeping {
            self.keeping = Keeping::Ask { failed: failed.clone(), made: false };
            self.credential = None;
            self.ask_keep();
        }
    }

    /// What came of a tap on the page's passkey button.
    pub fn passkey_answered(&mut self, answer: Answer) {
        if self.status == Status::Stopped(Stop::Locked) {
            return self.unlock_answered(answer);
        }
        let made = match &self.keeping {
            Keeping::Ask { made, .. } => *made,
            Keeping::Enrolling => false,
            _ => return,
        };
        match answer {
            Answer::Sealed(record) => {
                let ours =
                    self.device.as_ref().is_some_and(|d| d.host_id == record.host_id && d.host_key == record.host_key);
                if !ours {
                    self.keeping = Keeping::Ask { failed: Some("the sealed copy named another host".into()), made };
                    return;
                }
                self.credential = Some(record.credential.clone());
                self.outputs.push(Output::Keep(record.clone()));
                self.keeping = Keeping::Handing { record, since_ms: self.now_ms };
            }
            Answer::Enrolling => self.keeping = Keeping::Enrolling,
            Answer::Enrolled(credential) => {
                if credential.is_some() {
                    self.credential = credential;
                }
                self.keeping = Keeping::Ask { failed: None, made: true };
                self.ask_keep();
            }
            Answer::NoPrf => self.finish_keep(false, Some("the passkey gives no PRF output".into())),
            Answer::Failed(failure) => {
                self.keeping = Keeping::Ask { failed: Some(failure.text()), made };
                // A frame that may not make a passkey after all makes it in a window of its own next time.
                if failure.step == "create" && self.passkeys.create_in_frame {
                    self.passkeys.create_in_frame = false;
                    self.ask_keep();
                }
            }
            Answer::Unlocked(_) => {}
        }
    }

    /// An unlock's answer. A failure never forgets either copy: only the host's sealed refusal does.
    fn unlock_answered(&mut self, answer: Answer) {
        let Some(sealed) = self.sealed.clone() else { return };
        match answer {
            Answer::Unlocked(secret) => {
                let device = Device { host_id: sealed.host_id.clone(), host_key: sealed.host_key, secret };
                self.unlock_failed = None;
                self.credential = Some(sealed.credential.clone());
                self.path = Path::Unsealed;
                self.outputs.push(Output::Passkey(Ask::Done));
                // Back into this frame's store, so a reload while the browser runs needs no passkey.
                self.outputs.push(Output::SaveDevice(device.clone()));
                self.had_device = true;
                self.device = Some(device);
                // The boot's ticket may have expired while the passkey waited: dial with a fresh one.
                self.ticket = None;
                self.status = Status::Connecting;
                self.dial();
            }
            Answer::NoPrf => {
                self.unlock_failed = Some("the passkey gives no PRF output here".into());
                self.path = Path::NoPasskey("no PRF output".into());
            }
            Answer::Failed(failure) => self.unlock_failed = Some(failure.text()),
            Answer::Sealed(_) | Answer::Enrolling | Answer::Enrolled(_) => {}
        }
    }

    /// The hosting page's answer to [`Output::Keep`].
    fn kept(&mut self, host_id: &str, ok: bool) {
        let Keeping::Handing { record, .. } = &self.keeping else { return };
        if record.host_id != host_id {
            return;
        }
        if ok {
            self.sealed = Some(record.clone());
            self.finish_keep(true, None);
        } else {
            self.keeping = Keeping::Ask { failed: Some("the page could not keep the sealed copy".into()), made: false };
        }
    }

    /// No key in this frame's store and no pairing asked for: unlock the page's copy if a passkey may open it.
    fn no_key_here(&mut self) {
        match (self.sealed.clone(), self.passkeys.cannot_unlock()) {
            (Some(sealed), None) => {
                self.path = Path::Locked;
                self.status = Status::Stopped(Stop::Locked);
                self.outputs.push(Output::Passkey(Ask::Unlock(sealed)));
            }
            (Some(_), Some(why)) => {
                self.path = Path::NoPasskey(why.into());
                self.status = Status::Stopped(Stop::NotHere);
            }
            (None, _) => {
                self.path = Path::Nothing;
                self.status = Status::Stopped(Stop::NotHere);
            }
        }
    }

    pub fn refusal_reason(&self) -> Option<&str> {
        self.refusal.as_deref()
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
                self.keeps = boot.keeps;
                self.sealed = boot.sealed.clone();
                self.sealed_unreadable = boot.sealed_unreadable;
                self.credential = boot.sealed.as_ref().map(|s| s.credential.clone());
                let lifetime_ms = (PAIRING_LIFETIME_SECS * 1000).saturating_sub(boot.pairing_age_ms);
                self.boot = Some(boot);
                self.pairing = pairing;
                self.pair_until_ms = self.now_ms + lifetime_ms;
                self.status = Status::Connecting;
                self.load(host_id);
            }
            FromParent::Ticket { host_id, ticket } => {
                let ours = self.boot.as_ref().is_some_and(|b| b.host_id == host_id);
                if ours && self.retry.asked_ms.take().is_some() && self.socket.is_none() {
                    self.ticket = Some(ticket);
                    self.dial();
                }
            }
            FromParent::Kept { host_id, ok } => self.kept(&host_id, ok),
        }
        Ok(())
    }

    fn load(&mut self, host_id: String) {
        self.loading = true;
        self.outputs.push(Output::LoadDevice(host_id));
    }

    /// The stored device for the booted host, as [`Output::LoadDevice`] asked.
    pub fn device_loaded(&mut self, device: Result<Option<Device>, DeviceReadError>) {
        if !std::mem::take(&mut self.loading) {
            return;
        }
        let Ok(device) = device else {
            self.status = Status::Stopped(Stop::DeviceReadFailed);
            return;
        };
        let host = self.boot.as_ref().map(|b| b.host_id.clone());
        if device.as_ref().is_some_and(|d| Some(&d.host_id) != host.as_ref()) {
            self.status = Status::Stopped(Stop::DeviceReadFailed);
            return;
        }
        self.had_device = device.is_some();
        match (&self.pairing, device) {
            // Pairing would replace the host key this browser trusts for that host: ask first. The
            // same key needs no question, since only that host can answer the pairing.
            (Some(pairing), Some(known)) if known.host_key != pairing.host_key => {
                self.known = Some(known);
                self.status = Status::ConfirmRepair;
            }
            (Some(pairing), device) => {
                self.device = match device {
                    Some(device) => Some(device),
                    None => match Secret::generate() {
                        Ok(secret) => {
                            Some(Device { host_id: pairing.host_id.clone(), host_key: pairing.host_key, secret })
                        }
                        Err(_) => {
                            self.status = Status::Stopped(Stop::Failed);
                            return;
                        }
                    },
                };
                self.status = Status::Pairing { code: None };
                self.dial();
            }
            (None, Some(device)) => {
                self.path = Path::FrameStore;
                self.device = Some(device);
                self.dial();
            }
            (None, None) => self.no_key_here(),
        }
    }

    /// The answer to [`Status::ConfirmRepair`]: pair, replacing the key this browser holds for the
    /// host once the host accepts it, or keep that key and connect with it.
    pub fn repair(&mut self, pair: bool) {
        if self.status != Status::ConfirmRepair {
            return;
        }
        if pair {
            self.device = self.known.clone();
            self.status = Status::Pairing { code: None };
        } else {
            self.pairing = None;
            self.device = self.known.take();
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
        let name = self.pairing_name();
        let connection = match (&self.pairing, &self.device) {
            (Some(pairing), Some(device)) => Connection::pair(pairing, &device.secret, &name),
            (None, Some(device)) => Connection::session(&device.host_id, &device.host_key, &device.secret),
            (_, None) => return,
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
                    let Some(pairing) = self.pairing.take() else { continue };
                    let Some(mut device) = self.device.take() else { continue };
                    device.host_key = pairing.host_key;
                    self.known = None;
                    // `atomics.paired` waits for the save, and for a sealed copy where one can help.
                    self.outputs.push(Output::SaveDevice(device.clone()));
                    self.keeping = Keeping::Saving { since_ms: self.now_ms };
                    self.device = Some(device);
                }
                Event::Open => {
                    let hello = self.remote.hello(CLIENT_ID);
                    self.queue(&hello);
                    self.status = Status::Live;
                    self.retry.open_ms = Some(self.now_ms);
                }
                Event::Frame(text) => {
                    if !self.frame(socket, wire::decode(&text)) {
                        return;
                    }
                }
            }
        }
        self.flush();
    }

    /// One frame on a direct link, already decoded with [`wire::decode`] off the window's thread, so the window
    /// does not parse it. Anything but an open direct link ignores it.
    pub fn socket_frame(&mut self, socket: u64, frame: Result<wire::Envelope, wire::WireError>) {
        if self.current(socket).is_none_or(|s| !s.open || !s.connection.is_plain()) {
            return;
        }
        if self.frame(socket, frame) {
            self.flush();
        }
    }

    /// One decoded `atomics.v1` frame; false when it ended the channel. A frame this client cannot read (a kind
    /// from a newer minor) is skipped.
    fn frame(&mut self, socket: u64, frame: Result<wire::Envelope, wire::WireError>) -> bool {
        let envelope = match frame {
            Ok(envelope) => envelope,
            // Waiting cannot help: only an update on one side can.
            Err(error) if error.code == ErrorCode::UnsupportedVersion => {
                self.fail(socket, End::Version);
                return false;
            }
            Err(_) => return true,
        };
        if let Message::Error { code: ErrorCode::UnsupportedVersion, .. } = &envelope.message {
            self.fail(socket, End::Version);
            return false;
        }
        if wire::is_not_paired(&envelope) {
            self.fail(socket, End::HostRefused);
            return false;
        }
        if let Message::Error { message, .. } = &envelope.message
            && envelope.request_id.is_none()
        {
            self.refusal = Some(format!("The host refused this connection: {message}"));
            self.remote.receive(envelope);
            self.fail(socket, End::Refused);
            return false;
        }
        self.remote.receive(envelope);
        true
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
        let keeping_since = match &self.keeping {
            Keeping::Saving { since_ms } | Keeping::Handing { since_ms, .. } => Some(*since_ms),
            _ => None,
        };
        if keeping_since.is_some_and(|since| now_ms.saturating_sub(since) >= KEEP_WAIT_MS) {
            let why = if matches!(self.keeping, Keeping::Saving { .. }) {
                "this frame's store did not answer"
            } else {
                "the page did not answer"
            };
            self.finish_keep(false, Some(why.into()));
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
        let keep = match &self.keeping {
            Keeping::Saving { since_ms } | Keeping::Handing { since_ms, .. } => {
                Some((since_ms + KEEP_WAIT_MS).saturating_sub(self.now_ms))
            }
            _ => None,
        };
        due.into_iter().chain(ask).chain(keep).min()
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
            End::Refused => Some(Stop::Refused),
            // A direct link's upgrade carries the token; nothing between can forge a refusal of it.
            End::Code(close::UNAUTHORISED) if self.direct => Some(Stop::Unauthorised),
            End::Code(close::UNAUTHORISED) => Some(Stop::SignIn),
            // Sealed by the host itself: the one sign to forget the key on.
            End::HostRefused => {
                if let Some(device) = self.device.take() {
                    self.outputs.push(Output::ForgetDevice(device.host_id.clone()));
                    if self.keeps {
                        self.outputs.push(Output::Forget(device.host_id));
                    }
                }
                self.sealed = None;
                if self.keeping != Keeping::Idle {
                    self.keeping = Keeping::Idle;
                    self.outputs.push(Output::Passkey(Ask::Done));
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
            if stop != Stop::SignIn {
                self.pairing = None;
            }
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

/// Milliseconds since the epoch as `2026-11-07 10:18Z`.
fn utc(ms: u64) -> String {
    let (days, minute) = ((ms / 86_400_000) as i64, (ms / 60_000) % 1440);
    // Days to a civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02} {:02}:{:02}Z", minute / 60, minute % 60)
}

#[cfg(test)]
mod tests;
