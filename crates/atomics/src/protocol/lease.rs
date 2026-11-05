//! Who may type into and resize one terminal: its controller lease (docs/daemon.md, Terminal input
//! and leases).
//!
//! Every terminal has one [`Lease`], held by at most one client connection. A grant takes a
//! fencing token from a counter for the whole process, so a token never comes back. Input names the
//! token and a sequence number that starts at 1 in each lease and must go up by one; anything else
//! is refused before a byte reaches the PTY. A lease ends when its holder releases it, another
//! client takes over, it goes a whole time-to-live without input, a resize or a renew, its
//! connection ends, or the desktop types. Input naming an ended lease is fenced: late input never
//! lands, and a new connection can never reuse a lease, so nothing is replayed across a disconnect.
//!
//! The desktop is not a lease holder. It types and resizes as it always has; its typing takes the
//! lease back. A holder resizes only when it asks, its size lasts only as long as its lease, and
//! its resizes are debounced. When its lease ends the terminal returns to its own size, and a
//! resize of that made in the meantime waits until then. The desktop's pane gives a terminal its
//! own size; on a host with no desktop, a client's pane does ([`fit`](dyn TerminalControl::fit)).
//!
//! Every change happens under the lease's own lock, and input is written to the PTY under it, so
//! a takeover can never be overtaken by input admitted under the old lease. Nothing here takes
//! the `State` lock or an outbox lock; the emulator lock is taken inside it only to resize.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use atomics_core::lease::{GridSize, LeaseReason, LeaseState};
use atomics_core::wire::{ErrorCode, Seq, WireError};

/// Fencing tokens: one counter for every terminal in the process, so tokens are never reused.
static TOKENS: AtomicU64 = AtomicU64::new(0);

/// One client connection, as a lease holder.
#[derive(Debug, Clone)]
pub struct Client {
    pub id: u64,
    /// Shown to other clients as the holder: a paired device's name, else the `client_id` it
    /// said hello with.
    pub label: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    /// How long a lease lasts without input, a resize or a renew.
    pub ttl: Duration,
    /// The least time between two resizes of the holder's; a later one waits and the latest wins.
    pub debounce: Duration,
    /// The most admitted input a terminal's PTY may not have taken yet. Input beyond it is refused
    /// with `slow_client` until the program reads.
    pub max_unwritten: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self { ttl: Duration::from_secs(30), debounce: Duration::from_millis(100), max_unwritten: 1 << 20 }
    }
}

/// What the protocol needs to let a lease holder type into one terminal. Implemented over the
/// native PTY; tests implement it over an emulator that echoes what it is sent.
pub trait TerminalControl: Send + Sync {
    /// The same lease for every handle of one terminal.
    fn lease(&self) -> &Lease;
    /// Write admitted input. Called with the lease locked, so bytes reach the PTY in the order
    /// they were admitted.
    fn write(&self, bytes: Vec<u8>);
    /// Bytes written here that the PTY has not taken yet: a program that does not read its input
    /// leaves them queued.
    fn unwritten(&self) -> usize {
        0
    }
    /// Resize to the holder's `size`, or with `None` to the desktop's own size. Called with the
    /// lease locked.
    fn resize(&self, size: Option<GridSize>);
    /// Make `size` the terminal's own size, the one `resize(None)` returns to, as a desktop's pane
    /// does. Called with the lease locked.
    fn set_own_size(&self, size: GridSize);
}

struct Holder {
    client: Client,
    token: u64,
    next: u64,
    expires: Instant,
    /// The holder's own size is in force.
    sized: bool,
    resized_at: Option<Instant>,
    /// A resize that waits for the debounce.
    pending: Option<Option<GridSize>>,
}

#[derive(Default)]
struct State {
    holder: Option<Holder>,
    /// Why the lease last changed.
    reason: Option<LeaseReason>,
}

/// One terminal's lease. See the module docs.
#[derive(Default)]
pub struct Lease {
    state: Mutex<State>,
    /// Moves on every change a client would show: a grant, an end, the holder's size.
    version: AtomicU64,
    /// Someone holds it. Lets the desktop type without taking a lock when nobody does.
    held: AtomicBool,
    /// When input was last admitted (see [`micros`]); 0 for never.
    input_at: AtomicU64,
}

/// A point in time as microseconds since the first one asked for, plus one, so 0 means never.
pub fn micros(at: Instant) -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    let base = *BASE.get_or_init(Instant::now);
    at.saturating_duration_since(base).as_micros() as u64 + 1
}

struct Locked<'a> {
    lease: &'a Lease,
    state: MutexGuard<'a, State>,
    changed: bool,
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // Still under the lock: a reader of `version` under the lock sees the state it names.
        if self.changed {
            self.lease.version.fetch_add(1, Ordering::AcqRel);
        }
        self.lease.held.store(self.state.holder.is_some(), Ordering::Release);
    }
}

impl Lease {
    fn lock(&self) -> Locked<'_> {
        Locked { lease: self, state: self.state.lock().unwrap_or_else(|e| e.into_inner()), changed: false }
    }

    /// Moves whenever what [`view`](dyn TerminalControl::view) shows other clients changes.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Whether a client holds the lease. Lock-free.
    pub fn held(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// When input was last admitted, as [`micros`]; 0 for never.
    pub fn input_at(&self) -> u64 {
        self.input_at.load(Ordering::Acquire)
    }

    /// Who holds the lease (its label) and whether its own size is in force, for the desktop to show.
    /// Takes the lock only while someone holds it.
    pub fn holder(&self) -> Option<(Arc<str>, bool)> {
        if !self.held() {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.holder.as_ref().map(|h| (h.client.label.clone(), h.sized))
    }
}

fn fenced(message: String) -> WireError {
    WireError::new(ErrorCode::Fenced, message)
}

fn why_ended(reason: Option<LeaseReason>) -> &'static str {
    match reason {
        Some(LeaseReason::Expired) => "it expired",
        Some(LeaseReason::Released) => "it was released",
        Some(LeaseReason::Disconnected) => "its connection ended",
        Some(LeaseReason::Desktop) => "the desktop took the terminal back",
        Some(LeaseReason::TakenOver) => "another client took over",
        Some(LeaseReason::Granted | LeaseReason::Unknown) | None => "nobody holds this terminal's lease",
    }
}

impl dyn TerminalControl + '_ {
    /// End the current lease, if any, and return the terminal to the desktop's size if the holder
    /// had resized it.
    fn end(&self, locked: &mut Locked<'_>, reason: LeaseReason) {
        if let Some(holder) = locked.state.holder.take() {
            locked.state.reason = Some(reason);
            locked.changed = true;
            if holder.sized {
                self.resize(None);
            }
        }
    }

    fn lapse(&self, locked: &mut Locked<'_>, now: Instant) {
        if locked.state.holder.as_ref().is_some_and(|h| h.expires <= now) {
            self.end(locked, LeaseReason::Expired);
        }
    }

    /// The lease `who` names with `token`, renewed; ended leases (expired ones included) are fenced.
    fn holder<'l>(
        &self,
        locked: &'l mut Locked<'_>,
        who: &Client,
        token: u64,
        now: Instant,
        policy: &Policy,
    ) -> Result<&'l mut Holder, WireError> {
        self.lapse(locked, now);
        let refusal = match &locked.state.holder {
            Some(h) if h.client.id == who.id && h.token == token => None,
            Some(h) if h.client.id == who.id => Some(format!("lease {token} has ended; this client holds {}", h.token)),
            Some(h) => Some(format!("lease {token} has ended: {} holds this terminal", h.client.label)),
            None => Some(format!("lease {token} has ended: {}", why_ended(locked.state.reason))),
        };
        if let Some(message) = refusal {
            return Err(fenced(message));
        }
        let holder = locked.state.holder.as_mut().expect("checked above");
        holder.expires = now + policy.ttl;
        Ok(holder)
    }

    /// Take the lease. Holding it already renews it; while another client holds it, only
    /// `takeover` (an explicit user action) displaces that client.
    pub fn acquire(&self, who: &Client, takeover: bool, now: Instant, policy: &Policy) -> Result<(), WireError> {
        let mut locked = self.lease().lock();
        self.lapse(&mut locked, now);
        match &mut locked.state.holder {
            Some(h) if h.client.id == who.id => {
                h.expires = now + policy.ttl;
                return Ok(());
            }
            Some(h) if !takeover => {
                return Err(WireError::new(
                    ErrorCode::LeaseHeld,
                    format!("{} is typing in this terminal; take over to type", h.client.label),
                ));
            }
            _ => {}
        }
        let displaced = locked.state.holder.is_some();
        self.end(&mut locked, LeaseReason::TakenOver);
        locked.state.holder = Some(Holder {
            client: who.clone(),
            token: TOKENS.fetch_add(1, Ordering::Relaxed) + 1,
            next: 1,
            expires: now + policy.ttl,
            sized: false,
            resized_at: None,
            pending: None,
        });
        locked.state.reason = Some(if displaced { LeaseReason::TakenOver } else { LeaseReason::Granted });
        locked.changed = true;
        Ok(())
    }

    pub fn renew(&self, who: &Client, token: u64, now: Instant, policy: &Policy) -> Result<(), WireError> {
        let mut locked = self.lease().lock();
        self.holder(&mut locked, who, token, now, policy).map(|_| ())
    }

    pub fn release(&self, who: &Client, token: u64, now: Instant, policy: &Policy) -> Result<(), WireError> {
        let mut locked = self.lease().lock();
        self.holder(&mut locked, who, token, now, policy)?;
        self.end(&mut locked, LeaseReason::Released);
        Ok(())
    }

    /// Write `bytes` if they are the next input of `who`'s current lease and the PTY has taken
    /// enough of the earlier input. Refused input is not applied and does not use up its number.
    pub fn input(
        &self,
        who: &Client,
        (token, seq): (u64, u64),
        bytes: Vec<u8>,
        now: Instant,
        policy: &Policy,
    ) -> Result<(), WireError> {
        let mut locked = self.lease().lock();
        let holder = self.holder(&mut locked, who, token, now, policy)?;
        if seq != holder.next {
            let what = if seq < holder.next { "was already applied" } else { "skips input" };
            return Err(WireError::new(
                ErrorCode::OutOfOrder,
                format!("input {seq} {what}; the next input in lease {token} is {}", holder.next),
            ));
        }
        let queued = self.unwritten();
        if queued.saturating_add(bytes.len()) > policy.max_unwritten {
            return Err(WireError::new(
                ErrorCode::SlowClient,
                format!(
                    "the terminal has not read {queued} bytes of earlier input yet (at most {} may wait); \
                     send input {seq} again once it has",
                    policy.max_unwritten
                ),
            ));
        }
        holder.next += 1;
        self.lease().input_at.store(micros(now), Ordering::Release);
        self.write(bytes);
        Ok(())
    }

    /// Resize for the holder, or with `None` return to the desktop's size. Debounced: within the
    /// debounce of the last resize the request waits, and the latest one waiting is applied.
    pub fn resize_for(
        &self,
        who: &Client,
        token: u64,
        size: Option<GridSize>,
        now: Instant,
        policy: &Policy,
    ) -> Result<(), WireError> {
        let mut locked = self.lease().lock();
        let holder = self.holder(&mut locked, who, token, now, policy)?;
        if holder.resized_at.is_some_and(|at| now.saturating_duration_since(at) < policy.debounce) {
            holder.pending = Some(size);
            return Ok(());
        }
        self.apply(&mut locked, size, now);
        Ok(())
    }

    fn apply(&self, locked: &mut Locked<'_>, size: Option<GridSize>, now: Instant) {
        let Some(holder) = locked.state.holder.as_mut() else { return };
        holder.pending = None;
        if size.is_none() && !holder.sized {
            return; // already at the desktop's size
        }
        holder.resized_at = Some(now);
        if holder.sized != size.is_some() {
            holder.sized = size.is_some();
            locked.changed = true;
        }
        self.resize(size);
    }

    /// Housekeeping any client's thread may do: end an expired lease, and apply a debounced
    /// resize that is due. Lock-free while nobody holds the lease.
    pub fn tick(&self, now: Instant, policy: &Policy) {
        if !self.lease().held() {
            return;
        }
        let mut locked = self.lease().lock();
        self.lapse(&mut locked, now);
        let due = locked.state.holder.as_ref().and_then(|h| {
            let waited = h.resized_at.is_none_or(|at| now.saturating_duration_since(at) >= policy.debounce);
            h.pending.filter(|_| waited)
        });
        if let Some(size) = due {
            self.apply(&mut locked, size, now);
        }
    }

    /// `who` stopped following the terminal or went away: its lease, if it holds it, ends.
    pub fn disconnect(&self, who: &Client, reason: LeaseReason) {
        if !self.lease().held() {
            return;
        }
        let mut locked = self.lease().lock();
        if locked.state.holder.as_ref().is_some_and(|h| h.client.id == who.id) {
            self.end(&mut locked, reason);
        }
    }

    /// The desktop typed: a client's lease ends, and its size with it. False when nobody held it.
    pub fn take_back(&self) -> bool {
        if !self.lease().held() {
            return false;
        }
        let mut locked = self.lease().lock();
        let held = locked.state.holder.is_some();
        self.end(&mut locked, LeaseReason::Desktop);
        held
    }

    /// The desktop's own size changed. `remember` stores it where `resize(None)` finds it; it is
    /// applied now unless a holder's size is in force, and otherwise when that lease ends.
    pub fn local_resize(&self, remember: impl FnOnce()) {
        let locked = self.lease().lock();
        remember();
        if !locked.state.holder.as_ref().is_some_and(|h| h.sized) {
            self.resize(None);
        }
    }

    /// A client's pane gives the terminal its own size, on a host with no desktop to: applied as
    /// the desktop's own resize is ([`local_resize`](Self::local_resize)).
    pub fn fit(&self, size: GridSize) {
        self.local_resize(|| self.set_own_size(size));
    }

    /// The lease as `who` should see it, and the version that state belongs to.
    pub fn view(&self, session: &str, who: &Client, policy: &Policy) -> (u64, LeaseState) {
        let locked = self.lease().lock();
        let holder = locked.state.holder.as_ref();
        let mine = holder.filter(|h| h.client.id == who.id);
        let state = LeaseState {
            session: session.to_string(),
            holder: holder.map(|h| h.client.label.to_string()),
            yours: mine.is_some(),
            lease: mine.map(|h| Seq(h.token)),
            next: mine.map(|h| Seq(h.next)),
            ttl_ms: mine.map(|_| policy.ttl.as_millis().min(u128::from(u32::MAX)) as u32),
            resized: holder.is_some_and(|h| h.sized),
            reason: locked.state.reason,
        };
        (self.lease().version(), state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal that records what reaches it. While `stalled`, its program reads nothing.
    #[derive(Default)]
    struct Probe {
        lease: Lease,
        written: Mutex<Vec<u8>>,
        sizes: Mutex<Vec<Option<GridSize>>>,
        /// The own size a client's pane gave it, which `resize(None)` stands for.
        own: Mutex<Option<GridSize>>,
        stalled: AtomicBool,
        unwritten: std::sync::atomic::AtomicUsize,
    }

    impl TerminalControl for Probe {
        fn lease(&self) -> &Lease {
            &self.lease
        }
        fn write(&self, bytes: Vec<u8>) {
            if self.stalled.load(Ordering::Relaxed) {
                self.unwritten.fetch_add(bytes.len(), Ordering::Relaxed);
            }
            self.written.lock().unwrap().extend(bytes);
        }
        fn unwritten(&self) -> usize {
            self.unwritten.load(Ordering::Relaxed)
        }
        fn resize(&self, size: Option<GridSize>) {
            self.sizes.lock().unwrap().push(size);
        }
        fn set_own_size(&self, size: GridSize) {
            *self.own.lock().unwrap() = Some(size);
        }
    }

    impl Probe {
        fn control(&self) -> &dyn TerminalControl {
            self
        }
        fn written(&self) -> String {
            String::from_utf8(self.written.lock().unwrap().clone()).unwrap()
        }
        fn sizes(&self) -> Vec<Option<GridSize>> {
            self.sizes.lock().unwrap().clone()
        }
        fn token(&self, who: &Client) -> u64 {
            self.control().view("s", who, &Policy::default()).1.lease.expect("holds the lease").0
        }
    }

    fn client(id: u64, label: &str) -> Client {
        Client { id, label: label.into() }
    }

    fn code(result: Result<(), WireError>) -> ErrorCode {
        result.expect_err("refused").code
    }

    #[test]
    fn only_the_holder_types_and_only_in_order() {
        let (probe, policy, now) = (Probe::default(), Policy::default(), Instant::now());
        let (phone, laptop) = (client(1, "phone"), client(2, "laptop"));
        let t = probe.control();
        assert_eq!(code(t.input(&phone, (1, 1), b"x".to_vec(), now, &policy)), ErrorCode::Fenced, "no lease yet");
        t.acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        t.input(&phone, (token, 1), b"l".to_vec(), now, &policy).unwrap();
        t.input(&phone, (token, 2), b"s".to_vec(), now, &policy).unwrap();
        // A replay and a gap are both refused, and the stream stays where it was.
        assert_eq!(code(t.input(&phone, (token, 2), b"s".to_vec(), now, &policy)), ErrorCode::OutOfOrder);
        assert_eq!(code(t.input(&phone, (token, 4), b"!".to_vec(), now, &policy)), ErrorCode::OutOfOrder);
        // Another connection cannot type with the holder's token, nor a guessed one.
        assert_eq!(code(t.input(&laptop, (token, 3), b"!".to_vec(), now, &policy)), ErrorCode::Fenced);
        assert_eq!(code(t.input(&phone, (token + 1, 3), b"!".to_vec(), now, &policy)), ErrorCode::Fenced);
        t.input(&phone, (token, 3), b"\r".to_vec(), now, &policy).unwrap();
        assert_eq!(probe.written(), "ls\r");
        assert_ne!(probe.lease.input_at(), 0, "input is recorded for the echo");
    }

    #[test]
    fn input_the_program_has_not_read_is_bounded_and_refused_input_keeps_its_number() {
        let (probe, now) = (Probe::default(), Instant::now());
        let policy = Policy { max_unwritten: 10, ..Policy::default() };
        let phone = client(1, "phone");
        probe.control().acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        probe.stalled.store(true, Ordering::Relaxed);
        probe.control().input(&phone, (token, 1), b"123456".to_vec(), now, &policy).unwrap();
        let refused = probe.control().input(&phone, (token, 2), b"7890ab".to_vec(), now, &policy);
        assert_eq!(code(refused), ErrorCode::SlowClient);
        assert_eq!(probe.written(), "123456", "refused input is not written");
        // Once the program reads, the same input number is admitted.
        probe.unwritten.store(0, Ordering::Relaxed);
        probe.control().input(&phone, (token, 2), b"7890ab".to_vec(), now, &policy).unwrap();
        assert_eq!(probe.written(), "1234567890ab");
        assert!(Policy::default().max_unwritten >= 1 << 20);
    }

    #[test]
    fn opening_never_steals_control_and_a_takeover_fences_the_old_holder() {
        let (probe, policy, now) = (Probe::default(), Policy::default(), Instant::now());
        let (phone, laptop) = (client(1, "phone"), client(2, "laptop"));
        let t = probe.control();
        t.acquire(&laptop, false, now, &policy).unwrap();
        let old = probe.token(&laptop);
        let version = probe.lease.version();
        let refused = t.acquire(&phone, false, now, &policy).unwrap_err();
        assert_eq!(refused.code, ErrorCode::LeaseHeld);
        assert!(refused.message.contains("laptop"), "{}", refused.message);
        assert_eq!(probe.lease.version(), version, "a refused acquire changes nothing");
        // Acquiring again is idempotent for the holder.
        t.acquire(&laptop, false, now, &policy).unwrap();
        assert_eq!(probe.token(&laptop), old);

        t.acquire(&phone, true, now, &policy).unwrap();
        let new = probe.token(&phone);
        assert!(new > old, "fencing tokens only go up");
        let (_, seen) = t.view("s", &laptop, &policy);
        assert_eq!((seen.holder.as_deref(), seen.yours, seen.lease), (Some("phone"), false, None));
        assert_eq!(seen.reason, Some(LeaseReason::TakenOver));
        let late = t.input(&laptop, (old, 1), b"rm -rf".to_vec(), now, &policy).unwrap_err();
        assert_eq!(late.code, ErrorCode::Fenced);
        assert!(late.message.contains("phone"), "{}", late.message);
        t.input(&phone, (new, 1), b"ok".to_vec(), now, &policy).unwrap();
        assert_eq!(probe.written(), "ok");
    }

    #[test]
    fn an_expired_lease_fences_late_input_and_activity_keeps_it_alive() {
        let probe = Probe::default();
        let policy = Policy { ttl: Duration::from_secs(10), ..Policy::default() };
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let phone = client(1, "phone");
        let t = probe.control();
        t.acquire(&phone, false, start, &policy).unwrap();
        let token = probe.token(&phone);
        t.input(&phone, (token, 1), b"a".to_vec(), at(8), &policy).unwrap();
        t.renew(&phone, token, at(16), &policy).unwrap();
        t.input(&phone, (token, 2), b"b".to_vec(), at(25), &policy).unwrap();
        // Nothing for ten seconds: the next input is late and is not written.
        let late = t.input(&phone, (token, 3), b"c".to_vec(), at(35), &policy).unwrap_err();
        assert_eq!(late.code, ErrorCode::Fenced);
        assert!(late.message.contains("expired"), "{}", late.message);
        assert_eq!(probe.written(), "ab");
        assert_eq!(t.view("s", &phone, &policy).1.reason, Some(LeaseReason::Expired));
        assert_eq!(code(t.renew(&phone, token, at(36), &policy)), ErrorCode::Fenced);

        // `tick` ends an expired lease with nobody typing, so other clients see it go.
        t.acquire(&phone, false, at(40), &policy).unwrap();
        let version = probe.lease.version();
        t.tick(at(45), &policy);
        assert!(probe.lease.held());
        t.tick(at(50), &policy);
        assert!(!probe.lease.held() && probe.lease.version() > version);
    }

    #[test]
    fn a_disconnect_or_release_ends_the_lease_for_good() {
        let (probe, policy, now) = (Probe::default(), Policy::default(), Instant::now());
        let (phone, again) = (client(1, "phone"), client(3, "phone"));
        let t = probe.control();
        t.acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        t.input(&phone, (token, 1), b"a".to_vec(), now, &policy).unwrap();
        t.disconnect(&phone, LeaseReason::Disconnected);
        assert!(!probe.lease.held());
        // The same device on a new connection cannot carry on with the old lease.
        assert_eq!(code(t.input(&again, (token, 2), b"b".to_vec(), now, &policy)), ErrorCode::Fenced);
        t.acquire(&again, false, now, &policy).unwrap();
        let next = probe.token(&again);
        assert_ne!(next, token);
        assert_eq!(code(t.input(&again, (token, 2), b"b".to_vec(), now, &policy)), ErrorCode::Fenced);
        t.release(&again, next, now, &policy).unwrap();
        assert_eq!(t.view("s", &again, &policy).1.reason, Some(LeaseReason::Released));
        assert_eq!(code(t.release(&again, next, now, &policy)), ErrorCode::Fenced);
        assert_eq!(probe.written(), "a");
        // A disconnect of someone who does not hold it changes nothing.
        t.acquire(&phone, false, now, &policy).unwrap();
        t.disconnect(&again, LeaseReason::Disconnected);
        assert!(probe.lease.held());
    }

    #[test]
    fn the_holder_resizes_only_when_asked_and_its_size_ends_with_its_lease() {
        let (probe, policy, now) = (Probe::default(), Policy::default(), Instant::now());
        let (phone, laptop) = (client(1, "phone"), client(2, "laptop"));
        let t = probe.control();
        t.acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        t.input(&phone, (token, 1), b"a".to_vec(), now, &policy).unwrap();
        assert!(probe.sizes().is_empty(), "taking the lease and typing never resize");
        let small = GridSize::new(40, 20);
        t.resize_for(&phone, token, Some(small), now, &policy).unwrap();
        assert_eq!(probe.sizes(), vec![Some(small)]);
        assert!(t.view("s", &laptop, &policy).1.resized);
        // The desktop's own resize waits while the holder's size is in force.
        let remembered = AtomicBool::new(false);
        t.local_resize(|| remembered.store(true, Ordering::Relaxed));
        assert!(remembered.load(Ordering::Relaxed));
        assert_eq!(probe.sizes(), vec![Some(small)]);
        // Ending the lease, however it ends, returns to the desktop's size.
        t.acquire(&laptop, true, now, &policy).unwrap();
        assert_eq!(probe.sizes(), vec![Some(small), None]);
        assert!(!t.view("s", &laptop, &policy).1.resized);
        // With no holder's size in force, a desktop resize applies at once.
        t.local_resize(|| {});
        assert_eq!(probe.sizes().len(), 3);
        // A holder that never resized leaves the size alone when its lease ends.
        t.disconnect(&laptop, LeaseReason::Disconnected);
        assert_eq!(probe.sizes().len(), 3);
    }

    #[test]
    fn every_way_a_resized_lease_ends_restores_the_desktop_size() {
        let policy = Policy { ttl: Duration::from_secs(5), ..Policy::default() };
        let now = Instant::now();
        let phone = client(1, "phone");
        type End<'a> = (&'a str, &'a dyn Fn(&dyn TerminalControl, u64));
        let ends: [End; 5] = [
            ("release", &|t, token| t.release(&phone, token, now, &policy).unwrap()),
            ("disconnect", &|t, _| t.disconnect(&phone, LeaseReason::Disconnected)),
            ("desktop", &|t, _| assert!(t.take_back())),
            ("expiry", &|t, _| t.tick(now + Duration::from_secs(6), &policy)),
            ("unresize", &|t, token| t.resize_for(&phone, token, None, now + Duration::from_secs(1), &policy).unwrap()),
        ];
        for (name, end) in ends {
            let probe = Probe::default();
            let t = probe.control();
            t.acquire(&phone, false, now, &policy).unwrap();
            let token = probe.token(&phone);
            t.resize_for(&phone, token, Some(GridSize::new(30, 10)), now, &policy).unwrap();
            end(t, token);
            assert_eq!(probe.sizes(), vec![Some(GridSize::new(30, 10)), None], "{name}");
        }
    }

    #[test]
    fn a_clients_own_size_waits_for_a_holders_size_and_is_what_the_terminal_returns_to() {
        let policy = Policy { ttl: Duration::from_secs(5), ..Policy::default() };
        let (probe, now) = (Probe::default(), Instant::now());
        let phone = client(1, "phone");
        let t = probe.control();
        t.fit(GridSize::new(120, 40));
        assert_eq!(probe.sizes(), vec![None], "with no holder's size in force, at once");
        assert_eq!(*probe.own.lock().unwrap(), Some(GridSize::new(120, 40)));
        t.acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        t.resize_for(&phone, token, Some(GridSize::new(40, 12)), now, &policy).unwrap();
        // The pane changes size while the phone's size is in force: it waits for the lease to end.
        t.fit(GridSize::new(130, 40));
        assert_eq!(probe.sizes(), vec![None, Some(GridSize::new(40, 12))]);
        t.tick(now + Duration::from_secs(6), &policy);
        assert_eq!(probe.sizes(), vec![None, Some(GridSize::new(40, 12)), None], "the expiry returns to it");
        assert_eq!(*probe.own.lock().unwrap(), Some(GridSize::new(130, 40)));
        assert!(!probe.lease.held());
    }

    #[test]
    fn resizes_are_debounced_and_the_latest_one_wins() {
        let (probe, policy) = (Probe::default(), Policy::default());
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let phone = client(1, "phone");
        let t = probe.control();
        t.acquire(&phone, false, start, &policy).unwrap();
        let token = probe.token(&phone);
        for (i, at) in [0, 10, 20, 30, 40].into_iter().enumerate() {
            t.resize_for(&phone, token, Some(GridSize::new(40 + i as u16, 20)), ms(at), &policy).unwrap();
        }
        assert_eq!(probe.sizes(), vec![Some(GridSize::new(40, 20))], "a burst applies once at first");
        t.tick(ms(60), &policy);
        assert_eq!(probe.sizes().len(), 1, "and waits for the debounce");
        t.tick(ms(100), &policy);
        assert_eq!(probe.sizes(), vec![Some(GridSize::new(40, 20)), Some(GridSize::new(44, 20))]);
        t.tick(ms(500), &policy);
        assert_eq!(probe.sizes().len(), 2, "nothing more is waiting");
    }

    #[test]
    fn the_desktop_types_freely_and_takes_a_held_lease_back() {
        let (probe, policy, now) = (Probe::default(), Policy::default(), Instant::now());
        let phone = client(1, "phone");
        let t = probe.control();
        let version = probe.lease.version();
        assert!(!t.take_back(), "nothing to take back");
        assert_eq!(probe.lease.version(), version);
        t.acquire(&phone, false, now, &policy).unwrap();
        let token = probe.token(&phone);
        assert!(t.take_back());
        let (_, seen) = t.view("s", &phone, &policy);
        assert_eq!((seen.holder, seen.reason), (None, Some(LeaseReason::Desktop)));
        let late = t.input(&phone, (token, 1), b"x".to_vec(), now, &policy).unwrap_err();
        assert!(late.message.contains("desktop"), "{}", late.message);
        assert_eq!(probe.written(), "");
    }
}
