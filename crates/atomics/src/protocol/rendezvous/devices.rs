//! The devices this host trusts, and the one pairing that may be open (design §6.1 and §8).
//!
//! **The allowlist** is `devices.json` in the ATOMICS home, readable by this user only: each
//! device's Noise static public key, its name, what it may do ([`Access`]), and when it paired and
//! was last seen. It holds no secret, but whoever can write it can add a device. A session channel
//! whose key is not in it is refused inside the encrypted channel once the device's first message
//! decrypts (`channel.rs`).
//!
//! **A pairing** is offered from the desktop ("Pair a device"): a link with a fresh psk, for five
//! minutes, one at a time. A device that answers it goes through these stages:
//! - *open*: the link waits for a device;
//! - *handshake*: one device at a time completes the handshake and proves the psk with its first
//!   message (others get `pairing_busy` meanwhile, and a browser tries again while its link may still
//!   be valid). The handshake alone proves nothing about the psk, so the link goes back as soon as
//!   that first message fails or the device leaves, and at the latest after [`HANDSHAKE_WINDOW`];
//! - *claimed*: the desktop shows the device's name and the code, and asks whether the codes match;
//! - the user's answer pairs the device (view-only, or may type and approve) or refuses it. Either
//!   way the link is used up, as it is when it expires or the device leaves first. A device that
//!   answers a used-up, expired or withdrawn link gets `no_pairing`, never `pairing_busy`
//!   ([`Unavailable`]).
//!
//! **Revoking** a device removes it and ends its open sessions at once ([`Live`]).
//!
//! The channels' threads and the desktop share this behind one short lock; the file is written
//! under a lock of its own, never the first.

use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::noise::{Access, Pairing, PublicKey, Secret};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::super::link::Waker;
use super::super::{Peer, PeerDevice};

/// How long a device that answered the link has to finish the handshake and prove the psk.
pub const HANDSHAKE_WINDOW: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Its Noise static public key: what its sessions are recognised by.
    pub key: PublicKey,
    pub name: String,
    pub access: Access,
    /// Unix milliseconds.
    pub paired_ms: u64,
    pub seen_ms: u64,
}

impl Device {
    /// Its sessions' peer: the role its access gives, its name and its key's digest.
    pub fn peer(&self) -> Peer {
        Peer {
            native: false,
            role: self.access.role(),
            device: Some(PeerDevice { name: self.name.clone(), id: digest(&self.key) }),
        }
    }
}

/// Sixteen hex digits of the key's SHA-256: names a device in the operation journal.
pub fn digest(key: &PublicKey) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, key.as_bytes());
    digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    v: u32,
    devices: Vec<Device>,
}

/// One open session of a paired device. Revoking the device marks it and wakes its link.
pub struct Live {
    revoked: AtomicBool,
    waker: Arc<Waker>,
}

impl Live {
    pub fn revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
}

/// What a pairing's channel hears from the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Pending,
    Paired(Access),
    Refused,
    /// The pairing ended without an answer: it expired, or was cancelled.
    Gone,
}

/// Why a device that answered the link cannot pair now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The link is open, but another device is pairing with it: it may come free.
    Busy,
    /// No link is open: none was offered, or it was used, expired or was withdrawn.
    Closed,
}

impl Unavailable {
    /// The code the device's channel is closed with.
    pub fn close_code(self) -> u16 {
        match self {
            Unavailable::Busy => atomics_core::tunnel::close::PAIRING_BUSY,
            Unavailable::Closed => atomics_core::tunnel::close::NO_PAIRING,
        }
    }

    fn of(inner: &Inner) -> Self {
        if inner.offer.is_some() { Unavailable::Busy } else { Unavailable::Closed }
    }
}

/// What the desktop's pairing window shows.
pub enum PairingView {
    /// No pairing is open; `note` says how the last one ended.
    Idle { note: Option<String> },
    /// The link waits for a device; `busy` while one is completing the handshake.
    Open { link: Zeroizing<String>, left: Duration, busy: bool },
    /// A device proved the link's secret: compare `code` with the one it shows.
    Confirm { name: String, code: String, left: Duration },
}

struct Offer {
    pairing: Pairing,
    link: Zeroizing<String>,
    expires: Instant,
    stage: Stage,
}

enum Stage {
    Open,
    Handshake {
        attempt: u64,
        since: Instant,
    },
    /// `waker` ends the wait of the device's channel when the desktop answers.
    Claimed {
        attempt: u64,
        name: String,
        code: String,
        key: PublicKey,
        waker: Arc<Waker>,
    },
}

impl Stage {
    /// Tell a claimed pairing's channel that its pairing was answered or ended.
    fn wake(&self) {
        if let Stage::Claimed { waker, .. } = self {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct Inner {
    devices: Vec<Device>,
    /// Sessions open now, by device key.
    live: Vec<(PublicKey, Weak<Live>)>,
    /// The host id and Noise key that links name: known once the host is registered.
    host_id: Option<String>,
    key: Option<PublicKey>,
    offer: Option<Offer>,
    /// The desktop's answers, until the claimed pairing's channel reads them.
    answers: Vec<(u64, Option<Access>)>,
    attempts: u64,
    note: Option<String>,
    /// Why the allowlist could not be read or written.
    problem: Option<String>,
    /// Bumped by every change to `devices`, so the file is never overwritten by an older list.
    generation: u64,
}

pub struct Devices {
    /// Itself, for the timer that ends an offer.
    me: Weak<Devices>,
    path: PathBuf,
    lifetime: Duration,
    inner: Mutex<Inner>,
    /// The generation last written to the file.
    written: Mutex<u64>,
    changed: Box<dyn Fn() + Send + Sync>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// What a device may do, in the desktop's words.
pub fn access_words(access: Access) -> &'static str {
    match access {
        Access::Control => "may type and approve",
        Access::View => "view only",
    }
}

impl Devices {
    /// The allowlist at `path`, read now. `changed` runs (a repaint) after each change.
    pub fn open(path: PathBuf, changed: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Self::with_lifetime(path, Duration::from_secs(atomics_core::noise::PAIRING_LIFETIME_SECS), changed)
    }

    pub fn with_lifetime(path: PathBuf, lifetime: Duration, changed: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        let mut inner = Inner::default();
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
                Ok(stored) if stored.v == 1 => inner.devices = stored.devices,
                _ => {
                    inner.problem = Some(format!(
                        "{} is malformed, so no device is trusted; pairing again replaces it",
                        path.display()
                    ))
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => inner.problem = Some(format!("could not read {}: {error}", path.display())),
        }
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            path,
            lifetime,
            inner: Mutex::new(inner),
            written: Mutex::new(0),
            changed: Box::new(changed),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `edit` under the lock, then (outside it) write the list if `edit` changed it, and
    /// repaint.
    fn edit<T>(&self, edit: impl FnOnce(&mut Inner) -> T) -> T {
        let (out, list) = {
            let mut inner = self.lock();
            let before = inner.generation;
            let out = edit(&mut inner);
            let list = (inner.generation != before).then(|| (inner.generation, inner.devices.clone()));
            (out, list)
        };
        if let Some((generation, devices)) = list {
            let result = self.save(generation, devices);
            self.lock().problem = result.err().map(|e| format!("could not save {}: {e}", self.path.display()));
        }
        (self.changed)();
        out
    }

    fn save(&self, generation: u64, devices: Vec<Device>) -> io::Result<()> {
        let mut written = self.written.lock().unwrap_or_else(|e| e.into_inner());
        if *written >= generation {
            return Ok(()); // a newer list is already on disk
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_vec_pretty(&Stored { v: 1, devices }).map_err(io::Error::other)?;
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(&text)?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.path)?;
        *written = generation;
        Ok(())
    }

    /// The Noise key links name; from the device keys.
    pub fn set_key(&self, key: PublicKey) {
        self.edit(|inner| inner.key = Some(key));
    }

    /// The host id links name, or none while unregistered. An open link that names another
    /// host id is dropped.
    pub fn set_host(&self, host_id: Option<&str>) {
        self.edit(|inner| {
            if inner.host_id.as_deref() != host_id {
                inner.host_id = host_id.map(str::to_string);
                if let Some(offer) = inner.offer.take() {
                    offer.stage.wake();
                    inner.note = Some("The pairing link was withdrawn: this host's registration changed.".into());
                }
            }
        });
    }

    /// Why a pairing cannot be offered now, if it cannot.
    pub fn unready(&self) -> Option<String> {
        let inner = self.lock();
        match (&inner.host_id, &inner.key) {
            (Some(_), Some(_)) => None,
            _ => Some("this host is not registered with the service yet".into()),
        }
    }

    /// Open a pairing with a link made from `template` (`[remote.rendezvous] pair_url`). One
    /// already open stays as it is. When its time is up the offer, its psk and its link are
    /// dropped, whether or not anything is drawn then.
    pub fn offer(&self, template: &str) -> Result<(), String> {
        let made = self.edit(|inner| {
            Self::expire(inner);
            if inner.offer.is_some() {
                return Ok(false);
            }
            let (Some(host_id), Some(key)) = (inner.host_id.clone(), inner.key) else {
                return Err("this host is not registered with the service yet".to_string());
            };
            let pairing = Pairing::new(&host_id, key).map_err(|e| e.to_string())?;
            let link = pairing.link(template)?;
            inner.offer = Some(Offer { pairing, link, expires: Instant::now() + self.lifetime, stage: Stage::Open });
            inner.note = None;
            Ok(true)
        })?;
        if made {
            let (me, lifetime) = (self.me.clone(), self.lifetime);
            // Without the thread the offer still ends at its time, when next looked at.
            let _ = std::thread::Builder::new().name("atomics-pairing-expiry".into()).spawn(move || {
                std::thread::sleep(lifetime);
                if let Some(devices) = me.upgrade() {
                    devices.edit(Self::expire);
                }
            });
        }
        Ok(())
    }

    /// Withdraw the open pairing, at any stage: a device waiting on it is refused.
    pub fn cancel(&self) {
        self.edit(|inner| {
            if let Some(offer) = inner.offer.take() {
                offer.stage.wake();
                inner.note = Some("Pairing cancelled.".into());
            }
        });
    }

    /// The desktop's answer to a claimed pairing: `Some` pairs the device with that access,
    /// `None` refuses it. Either uses the link up.
    #[cfg(test)]
    pub fn decide(&self, access: Option<Access>) {
        self.decide_checked(None, access);
    }

    pub fn decide_code(&self, code: &str, access: Option<Access>) -> bool {
        self.decide_checked(Some(code), access)
    }

    fn decide_checked(&self, expected: Option<&str>, access: Option<Access>) -> bool {
        let mut decided = false;
        self.edit(|inner| {
            Self::expire(inner);
            if !matches!(inner.offer.as_ref().map(|offer| &offer.stage), Some(Stage::Claimed { code, .. }) if expected.is_none_or(|expected| expected == code)) {
                return;
            }
            let Some(Offer { stage: Stage::Claimed { attempt, name, key, waker, .. }, .. }) = inner.offer.take() else {
                return;
            };
            decided = true;
            inner.answers.push((attempt, access));
            waker.wake();
            inner.note = Some(match access {
                Some(access) => {
                    let now = now_ms();
                    inner.devices.retain(|d| d.key != key);
                    inner.devices.push(Device { key, name: name.clone(), access, paired_ms: now, seen_ms: now });
                    inner.generation += 1;
                    format!("Paired “{name}” ({}).", access_words(access))
                }
                None => format!("Refused “{name}”."),
            });
        });
        decided
    }

    pub fn view(&self) -> PairingView {
        let mut inner = self.lock();
        Self::expire(&mut inner);
        let Some(offer) = &inner.offer else { return PairingView::Idle { note: inner.note.clone() } };
        let left = offer.expires.saturating_duration_since(Instant::now());
        match &offer.stage {
            Stage::Claimed { name, code, .. } => PairingView::Confirm { name: name.clone(), code: code.clone(), left },
            Stage::Open => PairingView::Open { link: offer.link.clone(), left, busy: false },
            Stage::Handshake { since, .. } => {
                PairingView::Open { link: offer.link.clone(), left, busy: since.elapsed() < HANDSHAKE_WINDOW }
            }
        }
    }

    /// The paired devices, each with whether it has a session open now.
    pub fn list(&self) -> Vec<(Device, bool)> {
        let mut inner = self.lock();
        inner.live.retain(|(_, live)| live.strong_count() > 0);
        let inner = &*inner;
        inner.devices.iter().map(|d| (d.clone(), inner.live.iter().any(|(key, _)| *key == d.key))).collect()
    }

    /// Why the allowlist could not be read or saved, if so.
    pub fn problem(&self) -> Option<String> {
        self.lock().problem.clone()
    }

    /// Remove a device, and end its open sessions now.
    pub fn revoke(&self, key: &PublicKey) {
        self.edit(|inner| {
            let before = inner.devices.len();
            inner.devices.retain(|d| d.key != *key);
            if inner.devices.len() != before {
                inner.generation += 1;
            }
            for (_, live) in inner.live.iter().filter(|(k, _)| k == key) {
                if let Some(live) = live.upgrade() {
                    live.revoked.store(true, Ordering::Release);
                    live.waker.wake();
                }
            }
            inner.live.retain(|(k, _)| k != key);
        });
    }

    /// A pairing ends once its time is up.
    fn expire(inner: &mut Inner) {
        if let Some(offer) = inner.offer.take_if(|offer| Instant::now() >= offer.expires) {
            offer.stage.wake();
            let claimed = matches!(offer.stage, Stage::Claimed { .. });
            inner.note = Some(if claimed {
                "The pairing expired before it was answered.".into()
            } else {
                "The pairing link expired unused.".into()
            });
        }
    }

    /// A device answered the link: the psk for its handshake, unless no link is open (none was
    /// offered, or it expired or was used) or another device is pairing now.
    pub(super) fn begin(&self) -> Result<(u64, Secret), Unavailable> {
        self.edit(|inner| {
            Self::expire(inner);
            inner.attempts += 1;
            let attempt = inner.attempts;
            let offer = inner.offer.as_mut().ok_or(Unavailable::Closed)?;
            let free = match offer.stage {
                Stage::Open => true,
                Stage::Handshake { since, .. } => since.elapsed() >= HANDSHAKE_WINDOW,
                Stage::Claimed { .. } => false,
            };
            if !free {
                return Err(Unavailable::Busy);
            }
            offer.stage = Stage::Handshake { attempt, since: Instant::now() };
            Ok((attempt, offer.pairing.psk().clone()))
        })
    }

    /// The device of `attempt` failed or left: before it proved the psk, the link is open again;
    /// after, it is used up.
    pub(super) fn abandon(&self, attempt: u64) {
        self.edit(|inner| {
            let (handshake, claimed) = match inner.offer.as_ref().map(|offer| &offer.stage) {
                Some(Stage::Handshake { attempt: a, .. }) => (*a == attempt, None),
                Some(Stage::Claimed { attempt: a, name, .. }) if *a == attempt => (false, Some(name.clone())),
                _ => (false, None),
            };
            if let Some(offer) = inner.offer.as_mut().filter(|_| handshake) {
                offer.stage = Stage::Open;
            }
            if let Some(name) = claimed {
                inner.note = Some(format!("“{name}” left before the codes were compared. Pair again."));
                inner.offer = None;
            }
            inner.answers.retain(|(a, _)| *a != attempt);
        });
    }

    /// The device of `attempt` proved the psk: ask the desktop. Refused when its pairing ended, or
    /// another device took the link over after [`HANDSHAKE_WINDOW`].
    pub(super) fn claim(
        &self,
        attempt: u64,
        name: String,
        code: String,
        key: PublicKey,
        waker: Arc<Waker>,
    ) -> Result<(), Unavailable> {
        self.edit(|inner| {
            Self::expire(inner);
            match inner.offer.as_mut() {
                Some(offer) if matches!(offer.stage, Stage::Handshake { attempt: a, .. } if a == attempt) => {
                    offer.stage = Stage::Claimed { attempt, name, code, key, waker };
                    Ok(())
                }
                _ => Err(Unavailable::of(inner)),
            }
        })
    }

    /// The desktop's answer for `attempt`, once there is one.
    pub(super) fn answer(&self, attempt: u64) -> Answer {
        let mut inner = self.lock();
        if let Some(at) = inner.answers.iter().position(|(a, _)| *a == attempt) {
            return match inner.answers.remove(at).1 {
                Some(access) => Answer::Paired(access),
                None => Answer::Refused,
            };
        }
        let expired = inner.offer.as_ref().is_some_and(|offer| Instant::now() >= offer.expires);
        let pending = matches!(
            inner.offer.as_ref().map(|offer| &offer.stage),
            Some(Stage::Claimed { attempt: a, .. }) if *a == attempt
        );
        drop(inner);
        if expired {
            self.edit(Self::expire);
        }
        if pending && !expired { Answer::Pending } else { Answer::Gone }
    }

    /// A session for `key`: its device (what it may do, its name) and its revocation handle, if
    /// the key is paired. Marks it connected and writes its last-seen time, so the channel calls it
    /// only once the device's first transport message decrypted: a replayed handshake changes
    /// nothing here.
    pub(super) fn attach(&self, key: &PublicKey, waker: Arc<Waker>) -> Option<(Device, Arc<Live>)> {
        self.edit(|inner| {
            let device = inner.devices.iter_mut().find(|d| d.key == *key)?;
            device.seen_ms = now_ms();
            let device = device.clone();
            inner.generation += 1;
            let live = Arc::new(Live { revoked: AtomicBool::new(false), waker });
            inner.live.retain(|(_, live)| live.strong_count() > 0);
            inner.live.push((*key, Arc::downgrade(&live)));
            Some((device, live))
        })
    }

    /// A session for `key` ended: it was last seen now.
    pub(super) fn detach(&self, key: &PublicKey) {
        self.edit(|inner| {
            if let Some(device) = inner.devices.iter_mut().find(|d| d.key == *key) {
                device.seen_ms = now_ms();
                inner.generation += 1;
            }
            inner.live.retain(|(_, live)| live.strong_count() > 0);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_ID: &str = "0190a1b2-c3d4-8e5f-a617-283940516273";
    const TEMPLATE: &str = "https://service.example/atomics/pair#{pairing}";

    fn devices(dir: &crate::test_support::TempDir, lifetime: Duration) -> Arc<Devices> {
        let devices = Devices::with_lifetime(dir.path().join("devices.json"), lifetime, || {});
        devices.set_key(Secret::generate().unwrap().public());
        devices
    }

    fn waker() -> Arc<Waker> {
        crate::protocol::link::Wait::new().unwrap().waker()
    }

    #[test]
    fn a_pairing_needs_a_registered_host_and_a_safe_template() {
        let dir = crate::test_support::tempdir().unwrap();
        let devices = devices(&dir, Duration::from_secs(300));
        assert!(devices.unready().is_some());
        assert!(devices.offer(TEMPLATE).unwrap_err().contains("not registered"));
        devices.set_host(Some(HOST_ID));
        assert!(devices.unready().is_none());
        assert!(devices.offer("https://service.example/pair?p={pairing}").unwrap_err().contains("fragment"));
        assert!(matches!(devices.view(), PairingView::Idle { note: None }));
        devices.offer(TEMPLATE).unwrap();
        let PairingView::Open { link, busy: false, .. } = devices.view() else { panic!("open") };
        assert!(link.starts_with("https://service.example/atomics/pair#1.0190a1b2-"));
        devices.offer(TEMPLATE).unwrap();
        let PairingView::Open { link: again, .. } = devices.view() else { panic!("still open") };
        assert_eq!(*link, *again, "a second click shows the same link");
        // Registering again under another id withdraws it.
        devices.set_host(Some("another-host"));
        assert!(matches!(devices.view(), PairingView::Idle { note: Some(_) }));
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed), "a withdrawn link is not busy");
        devices.offer(TEMPLATE).unwrap();
        devices.cancel();
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed), "nor is a cancelled one");
    }

    #[test]
    fn one_device_pairs_at_a_time_and_an_answer_uses_the_link_up() {
        let dir = crate::test_support::tempdir().unwrap();
        let devices = devices(&dir, Duration::from_secs(300));
        devices.set_host(Some(HOST_ID));
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed), "no link open");
        devices.offer(TEMPLATE).unwrap();
        let (first, psk) = devices.begin().expect("the link is open");
        assert_eq!(devices.begin().err(), Some(Unavailable::Busy), "another device waits for the first");
        assert!(matches!(devices.view(), PairingView::Open { busy: true, .. }));
        devices.abandon(first);
        let (second, again) = devices.begin().expect("a failed handshake gives the link back");
        assert_eq!(psk.as_bytes(), again.as_bytes(), "the same link");
        let key = Secret::generate().unwrap().public();
        assert_eq!(
            devices.claim(first, "Old".into(), "000000".into(), key, waker()),
            Err(Unavailable::Busy),
            "a stale attempt claims nothing: another device holds the link"
        );
        devices.claim(second, "Phone".into(), "123456".into(), key, waker()).unwrap();
        let PairingView::Confirm { name, code, .. } = devices.view() else { panic!("asks") };
        assert_eq!((name.as_str(), code.as_str()), ("Phone", "123456"));
        assert_eq!(devices.answer(second), Answer::Pending);
        assert_eq!(devices.begin().err(), Some(Unavailable::Busy), "no second pairing while one is claimed");
        devices.decide(Some(Access::View));
        assert_eq!(devices.answer(second), Answer::Paired(Access::View));
        assert!(
            matches!(devices.view(), PairingView::Idle { note: Some(note) } if note.contains("Paired “Phone” (view only)"))
        );
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed), "the link is used up");
        assert_eq!(devices.list().len(), 1);

        // Refused: nothing is added.
        devices.offer(TEMPLATE).unwrap();
        let (attempt, _) = devices.begin().unwrap();
        let other = Secret::generate().unwrap().public();
        devices.claim(attempt, "Racer".into(), "654321".into(), other, waker()).unwrap();
        devices.decide(None);
        assert_eq!(devices.answer(attempt), Answer::Refused);
        assert_eq!(devices.list().len(), 1);
        assert!(devices.attach(&other, waker()).is_none(), "a refused device is unknown");

        // A device that leaves after proving the psk uses the link up too.
        devices.offer(TEMPLATE).unwrap();
        let (attempt, _) = devices.begin().unwrap();
        devices.claim(attempt, "Leaver".into(), "111111".into(), other, waker()).unwrap();
        devices.abandon(attempt);
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed));
        assert_eq!(devices.answer(attempt), Answer::Gone);
    }

    #[test]
    fn a_link_expires_unused_or_unanswered() {
        let dir = crate::test_support::tempdir().unwrap();
        let devices = devices(&dir, Duration::from_millis(50));
        devices.set_host(Some(HOST_ID));
        devices.offer(TEMPLATE).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while devices.lock().offer.is_some() {
            assert!(Instant::now() < deadline, "the psk and link outlive the offer");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(devices.lock().note.as_deref().is_some_and(|n| n.contains("expired")), "dropped with nothing drawn");
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed), "an expired link is not busy");
        assert!(matches!(devices.view(), PairingView::Idle { note: Some(note) } if note.contains("expired")));
        devices.offer(TEMPLATE).unwrap();
        let (attempt, _) = devices.begin().unwrap();
        devices.claim(attempt, "Slow".into(), "222222".into(), Secret::generate().unwrap().public(), waker()).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(devices.answer(attempt), Answer::Gone);
        assert_eq!(devices.begin().err(), Some(Unavailable::Closed));
        devices.decide(Some(Access::Control));
        assert!(devices.list().is_empty(), "too late to pair");
    }

    #[test]
    fn the_allowlist_is_a_private_file_and_revoking_ends_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::tempdir().unwrap();
        let devices = devices(&dir, Duration::from_secs(300));
        devices.set_host(Some(HOST_ID));
        devices.offer(TEMPLATE).unwrap();
        let (attempt, _) = devices.begin().unwrap();
        let key = Secret::generate().unwrap().public();
        devices.claim(attempt, "Tablet".into(), "333333".into(), key, waker()).unwrap();
        devices.decide(Some(Access::Control));
        let path = dir.path().join("devices.json");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&key.text()) && text.contains("\"control\"") && text.contains("Tablet"), "{text}");
        // Read back by a new instance.
        let reread = Devices::open(path.clone(), || {});
        assert_eq!(reread.list()[0].0.access, Access::Control);
        let wait = crate::protocol::link::Wait::new().unwrap();
        let (device, live) = reread.attach(&key, wait.waker()).unwrap();
        assert_eq!(device.access, Access::Control);
        let peer = device.peer();
        assert_eq!(peer.role, atomics_core::wire::Role::Controller);
        let named = peer.device.unwrap();
        assert_eq!((named.name.as_str(), named.id.len()), ("Tablet", 16));
        assert!(!named.id.contains(&key.text()[..8]), "a digest, not the key");
        assert!(reread.list()[0].1, "connected");
        reread.revoke(&key);
        assert!(live.revoked(), "the open session is told");
        assert!(reread.list().is_empty());
        assert!(reread.attach(&key, waker()).is_none());
        assert!(!std::fs::read_to_string(&path).unwrap().contains(&key.text()));
        // A malformed file trusts no one, and says so.
        std::fs::write(&path, "not json").unwrap();
        let broken = Devices::open(path, || {});
        assert!(broken.list().is_empty() && broken.problem().unwrap().contains("malformed"));
    }
}
