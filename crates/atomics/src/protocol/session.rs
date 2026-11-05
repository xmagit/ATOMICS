//! One client's conversation with the host, over any `Link`.
//!
//! The first frame must be a `hello`. After the `welcome` the loop alternates between draining the
//! client's outbound queue and reading one frame, so a client that stops reading stalls only its
//! own thread, and that thread gives up after the link's write timeout.
//!
//! The wait for the client's next frame ends early whenever something is queued for it (the
//! outbox wakes the link), so a frame leaves as soon as it is ready.
//!
//! A client may narrow the board's live updates to the diffs and plans it subscribes to. The
//! events it left out are sent as `skipped`, so its cursor still advances one by one, and when it
//! widens its topics again it gets a fresh snapshot for what it missed.
//!
//! A client that negotiated `input` may hold the lease of a screen it follows (`lease.rs`). Its
//! input is written to the PTY from this thread as soon as the frame is read, and the screens
//! thread is woken to copy the echo. The loop also tells the client whenever a followed screen's
//! lease changes hands, and when the connection ends, so does any lease it held. On a host with
//! no desktop, a client that negotiated `fit` gives a terminal it follows its own size.
//!
//! A mutation passes two gates here, the `mutations` capability and the client's grants (its
//! `welcome`'s `may`), and goes to the runtime thread (`ops.rs`), which journals and carries it
//! out. Its outcomes come back on this client's own channel, which wakes the loop, and leave as
//! `command_result`s answering the request that sent it. A refusal at the gates is a
//! `command_result` too, so a client's pending operation always ends.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use atomics_core::diff::FileDiff;
use atomics_core::lease::{LeaseReason, LeaseState};
use atomics_core::ops::{Command, CommandResult, Grant, Outcome};
use atomics_core::projection::DiffFileBody;
use atomics_core::wire::{
    self, CAPABILITIES, Envelope, ErrorCode, INPUT, Limits, MAX_PENDING_OPS, MAX_SUBSCRIPTIONS, MUTATIONS, Message,
    Role, Seq, TERMINALS, Topic, VERSION, WireError,
};

use super::hub::{Hub, Outbox, Scope, Taken};
use super::lease::{Client, Policy, TerminalControl};
use super::link::{Link, LinkError};
use super::ops::{Commands, OpReply, OpRequest};
use super::screens::Screens;

/// Where `diff_get` finds hunks. Implemented over data the runtime thread published, never `State`.
pub trait DiffSource: Send + Sync {
    /// Reference-counted by folder subscription, including disconnects. Runtime work stays on the host.
    fn watch_folder(&self, _worktree: &str, _watching: bool) {}
    fn folder_revision(&self, _worktree: &str) -> Option<u64> {
        None
    }
    fn folder(
        &self,
        _worktree: &str,
        _path: &str,
        _offset: u32,
        _budget: usize,
    ) -> Result<atomics_core::folder::Page, WireError> {
        Err(WireError::new(ErrorCode::NotFound, "file listing is not ready"))
    }
    fn content(&self, _worktree: &str, _path: &str, _budget: usize) -> Result<(String, bool), WireError> {
        Err(WireError::new(ErrorCode::NotFound, "no such worktree file"))
    }
    /// The diff revision and the file whose new or old path is `path`.
    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)>;
    /// Whether `worktree`'s diff is no longer at `revision`, or went away lately: a file a client
    /// was told of at that revision may be gone. A source that cannot tell says no.
    fn moved_on(&self, _worktree: &str, _revision: u64) -> bool {
        false
    }
}

/// Names that went away lately (closed terminals, removed worktrees), so a request naming one is
/// a race a client meets, not a mistake. Keeps the newest few; older ones are forgotten.
#[derive(Default)]
pub struct Departed(Mutex<VecDeque<String>>);

impl Departed {
    const KEPT: usize = 64;

    /// The names were `before`; `present` says which exist now. Remember those that left and
    /// forget those that came back.
    pub fn record<'a>(&self, before: impl IntoIterator<Item = &'a str>, present: impl Fn(&str) -> bool) {
        let mut gone = self.0.lock().unwrap_or_else(|e| e.into_inner());
        gone.retain(|name| !present(name));
        for name in before {
            if !present(name) && !gone.iter().any(|g| g == name) {
                gone.push_back(name.to_string());
            }
        }
        while gone.len() > Self::KEPT {
            gone.pop_front();
        }
    }

    pub fn contains(&self, name: &str) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|g| g == name)
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub handshake_timeout: Duration,
    pub tick: Duration,
    pub ping_interval: Duration,
    pub idle_timeout: Duration,
    /// Errors a client may cause before it is disconnected.
    pub max_errors: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            tick: Duration::from_millis(25),
            ping_interval: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(60),
            max_errors: 8,
        }
    }
}

pub struct Shared {
    pub hub: Arc<Mutex<Hub>>,
    pub projections: Arc<atomics_core::projection::Projections>,
    pub diffs: Arc<dyn DiffSource>,
    pub screens: Arc<Screens>,
    /// Where mutations are carried out.
    pub commands: Arc<dyn Commands>,
    pub stop: Arc<AtomicBool>,
    pub config: SessionConfig,
    pub lease: Policy,
    /// No desktop gives the terminals their size: a client may (`fit`).
    pub headless: bool,
    pub queue_frames: usize,
    /// The most bytes a frame this session builds (a diff page) may hold.
    pub frame_budget: usize,
}

/// Who a session serves: what it may do and, for a paired device, who it is. A loopback client
/// is known only by its `hello`.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Native transports can create a local TCP listener; browser transports cannot.
    pub native: bool,
    pub role: Role,
    pub device: Option<PeerDevice>,
}

/// A paired device, as the host's allowlist knows it rather than as its `hello` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDevice {
    /// The name it was paired under: the lease holder other clients see, and who the journal
    /// says sent an operation.
    pub name: String,
    /// A digest of its key, which keeps its operations apart from another device's.
    pub id: String,
}

impl Peer {
    pub fn local(role: Role) -> Self {
        Self { role, device: None, native: true }
    }
}

/// Connection numbers: a lease belongs to one connection and dies with it.
static CONNECTIONS: AtomicU64 = AtomicU64::new(0);

fn lock(hub: &Mutex<Hub>) -> MutexGuard<'_, Hub> {
    hub.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why a session ended; for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Stopped,
    Closed,
    /// Refused in the handshake, or disconnected for misbehaving.
    Rejected(ErrorCode),
    Slow,
    Idle,
}

struct Out<'a> {
    link: &'a mut dyn Link,
    epoch: String,
    last_sent: Instant,
    manages_host: bool,
}

impl Out<'_> {
    fn raw(&mut self, frame: &str) -> Result<(), LinkError> {
        self.last_sent = Instant::now();
        if !self.manages_host && frame.contains("\"host\"") {
            let mut value: serde_json::Value = serde_json::from_str(frame).map_err(|_| LinkError::Closed)?;
            if let Some(pairing) =
                value.pointer_mut("/host/management/remote/pairing").and_then(serde_json::Value::as_object_mut)
            {
                pairing.remove("link");
            }
            return self.link.send(&value.to_string());
        }
        self.link.send(frame)
    }

    fn envelope(&mut self, envelope: &Envelope) -> Result<(), LinkError> {
        match wire::encode(envelope) {
            Ok(frame) => self.raw(&frame),
            Err(e) => self.error(e, envelope.request_id.clone()),
        }
    }

    fn error(&mut self, error: WireError, request_id: Option<String>) -> Result<(), LinkError> {
        let envelope = error.into_envelope(request_id, Some(self.epoch.clone()));
        let frame = wire::encode(&envelope).expect("an error frame is small");
        self.raw(&frame)
    }
}

fn link_ended(error: LinkError) -> Ended {
    match error {
        LinkError::Stalled => Ended::Slow,
        _ => Ended::Closed,
    }
}

pub fn serve(link: &mut dyn Link, peer: &Peer, shared: &Shared) -> Ended {
    let epoch = lock(&shared.hub).epoch().to_string();
    let mut out = Out { link, epoch, last_sent: Instant::now(), manages_host: false };
    let ended = run(&mut out, peer, shared);
    use atomics_core::tunnel::close;
    let (code, reason) = match &ended {
        Ended::Stopped => (close::NORMAL, "host is shutting down"),
        Ended::Rejected(_) => (close::REFUSED, "protocol error"),
        Ended::Slow => (close::SLOW, "client is too slow"),
        Ended::Idle => (close::IDLE, "idle"),
        Ended::Closed => (close::NORMAL, "closed"),
    };
    out.link.close_with(code, reason);
    ended
}

fn run(out: &mut Out<'_>, peer: &Peer, shared: &Shared) -> Ended {
    let role = peer.role;
    let config = &shared.config;
    // The handshake: the first frame is a hello.
    let started = Instant::now();
    let hello = loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ended::Stopped;
        }
        if started.elapsed() > config.handshake_timeout {
            let _ = out.error(WireError::new(ErrorCode::Malformed, "no hello"), None);
            return Ended::Rejected(ErrorCode::Malformed);
        }
        match out.link.recv(config.tick) {
            Ok(Some(frame)) => break frame,
            Ok(None) => {}
            Err(e) => return refuse_link(out, e),
        }
    };
    let envelope = match wire::decode(&hello) {
        Ok(e) => e,
        Err(e) => {
            let code = e.code;
            let _ = out.error(e, None);
            return Ended::Rejected(code);
        }
    };
    let Message::Hello { client_id, versions, capabilities, prior_epoch, prior_cursor } = &envelope.message else {
        let _ = out.error(WireError::new(ErrorCode::Malformed, "the first message must be hello"), envelope.request_id);
        return Ended::Rejected(ErrorCode::Malformed);
    };
    let version = match wire::negotiate(versions, &[VERSION]) {
        Ok(v) => v,
        Err(e) => {
            let _ = out.error(e, envelope.request_id.clone());
            return Ended::Rejected(ErrorCode::UnsupportedVersion);
        }
    };
    let attached = match lock(&shared.hub).attach(prior_epoch.as_deref(), *prior_cursor) {
        Ok(a) => a,
        Err(e) => {
            let code = e.code;
            let _ = out.error(e, envelope.request_id.clone());
            return Ended::Rejected(code);
        }
    };
    let outbox = attached.outbox;
    let waker = out.link.waker();
    if let Some(waker) = &waker {
        outbox.set_waker(waker.clone());
    }
    let mut capabilities = wire::common_capabilities(capabilities, CAPABILITIES);
    let terminals = capabilities.iter().any(|c| c == TERMINALS);
    // `input` needs `terminals`; alone it is not offered.
    if !terminals {
        capabilities.retain(|c| c != INPUT);
    }
    if !capabilities.iter().any(|c| c == wire::HOST) {
        capabilities.retain(|c| c != wire::EXPLORER);
    }
    // Where a desktop gives the terminals their size, it stays theirs.
    if !shared.headless || !capabilities.iter().any(|c| c == INPUT) {
        capabilities.retain(|c| c != wire::FIT);
    }
    if !peer.native || role != Role::Controller {
        capabilities.retain(|c| c != wire::TUNNEL);
    }
    let may = role.may(&capabilities);
    out.manages_host = may.contains(&atomics_core::ops::Grant::Host);
    let (results_tx, results) = mpsc::channel();
    let mut follows = Follows {
        board: Narrowed::default(),
        screens: HashSet::new(),
        terminals,
        input: terminals && capabilities.iter().any(|c| c == INPUT),
        fit: capabilities.iter().any(|c| c == wire::FIT),
        tunnel: capabilities.iter().any(|c| c == wire::TUNNEL),
        tunnels: Default::default(),
        mutations: capabilities.iter().any(|c| c == MUTATIONS),
        host: capabilities.iter().any(|c| c == wire::HOST),
        panels: capabilities.iter().any(|c| c == wire::PANELS),
        explorer: capabilities.iter().any(|c| c == wire::EXPLORER),
        complete: capabilities.iter().any(|c| c == wire::PLAN_COMPLETE),
        folders: HashMap::new(),
        trace: capabilities.iter().any(|c| c == wire::TRACE),
        follows_trace: false,
        trace_revision: None,
        trace_page: None,
        may: may.clone(),
        client_id: client_id.clone(),
        device: peer.device.clone(),
        // Others see a paired device by the name it was paired under, not one it chose itself.
        me: Client {
            id: CONNECTIONS.fetch_add(1, Ordering::Relaxed) + 1,
            label: peer.device.as_ref().map_or(client_id.as_str(), |device| device.name.as_str()).into(),
        },
        told: HashMap::new(),
        slowed: HashMap::new(),
        ops: HashMap::new(),
        results,
        reply: OpReply::new(results_tx, waker),
    };
    let welcome = Envelope::host(
        &attached.epoch,
        Message::Welcome {
            version,
            capabilities,
            limits: Limits { max_queued_frames: shared.queue_frames as u32, ..Limits::default() },
            role,
            resume: attached.resume,
            may,
        },
    )
    .reply_to(envelope.request_id.clone());
    let result = (|| {
        out.envelope(&welcome).map_err(link_ended)?;
        conversation(out, shared, &outbox, &mut follows)
    })();
    outbox.close();
    for (worktree, _) in follows.folders.keys() {
        shared.diffs.watch_folder(worktree, false);
    }
    // Whatever lease this connection held ends with it: input sent on it can no longer land, and
    // a later connection starts without one.
    for session in &follows.screens {
        if let Some((_, control)) = shared.screens.terminals().control(session) {
            control.disconnect(&follows.me, LeaseReason::Disconnected);
        }
    }
    match result {
        Ok(()) => Ended::Closed,
        Err(ended) => ended,
    }
}

fn refuse_link(out: &mut Out<'_>, error: LinkError) -> Ended {
    let code = match error {
        LinkError::Oversize => ErrorCode::Oversize,
        LinkError::Binary => ErrorCode::Malformed,
        other => return link_ended(other),
    };
    let message = if code == ErrorCode::Oversize { "frame is over the size limit" } else { "frames are JSON text" };
    let _ = out.error(WireError::new(code, message), None);
    Ended::Rejected(code)
}

/// The board topics a client narrowed its live updates to. An empty set admits every item of
/// its kind; sessions and approvals are never narrowed.
#[derive(Debug, Default, Clone)]
struct Narrowed {
    diffs: HashSet<String>,
    plans: HashSet<String>,
}

impl Narrowed {
    /// The set a `diff` or `plan` topic goes in, and its name there; `None` for a screen.
    fn entry<'a>(&mut self, topic: &'a Topic) -> Option<(&mut HashSet<String>, &'a str)> {
        match topic {
            Topic::Diff { worktree } => Some((&mut self.diffs, worktree.as_str())),
            Topic::Plan { id } => Some((&mut self.plans, id.as_str())),
            Topic::Screen { .. } | Topic::Folder { .. } | Topic::Trace => None,
        }
    }

    /// Whether a client that narrowed itself to `self` gets an event about `scope`; `host` is whether
    /// it negotiated the `host` capability, without which it is never sent the host's own state.
    fn admits(&self, scope: &Scope, host: bool) -> bool {
        match scope {
            Scope::Open => true,
            Scope::Host => host,
            Scope::Diff(worktree) => self.diffs.is_empty() || self.diffs.contains(worktree),
            Scope::Plan(id) => self.plans.is_empty() || self.plans.contains(id),
        }
    }

    /// Whether `self` admits something `before` did not: the events for it were skipped, so the
    /// client needs a fresh snapshot.
    fn widens(&self, before: &Narrowed) -> bool {
        let wider =
            |now: &HashSet<String>, was: &HashSet<String>| !was.is_empty() && (now.is_empty() || !now.is_subset(was));
        wider(&self.diffs, &before.diffs) || wider(&self.plans, &before.plans)
    }
}

/// What one client follows, what it negotiated, and who it is to a lease.
struct Follows {
    board: Narrowed,
    /// Sessions whose screens it subscribed to.
    screens: HashSet<String>,
    terminals: bool,
    /// It negotiated `input`: it is told who holds leases, and may hold one if it may `type`.
    input: bool,
    /// It negotiated `fit`: its panes give the terminals their own size.
    fit: bool,
    tunnel: bool,
    tunnels: crate::preview::relay::Relay,
    /// It negotiated `mutations`.
    mutations: bool,
    /// It negotiated `host`: it is sent `host_upsert`.
    host: bool,
    panels: bool,
    explorer: bool,
    complete: bool,
    folders: HashMap<(String, String), Option<u64>>,
    trace: bool,
    follows_trace: bool,
    trace_revision: Option<u64>,
    /// One immutable page chain per connection; newer live events cannot invalidate it.
    trace_page: Option<Arc<atomics_core::projection::Snapshot<atomics_core::trace_feed::TraceData>>>,
    /// What its role and capabilities let it change, as its `welcome` said.
    may: Vec<Grant>,
    /// The ID it said hello with, which names its operations in the journal.
    client_id: String,
    /// The paired device this session serves, if any.
    device: Option<PeerDevice>,
    me: Client,
    /// The lease last told for each followed screen: the terminal's identity and lease version.
    told: HashMap<String, (usize, u64)>,
    /// Input refused with `slow_client`, by followed screen: its lease token and number. What the
    /// client had already sent behind it meets `out_of_order`, which does not count either.
    slowed: HashMap<String, (u64, u64)>,
    /// Its operations under way, and the request each answers.
    ops: HashMap<String, (String, Option<String>)>,
    /// Their outcomes, from the runtime thread.
    results: mpsc::Receiver<CommandResult>,
    reply: OpReply,
}

impl Follows {
    /// Topics of every kind, which `MAX_SUBSCRIPTIONS` bounds together.
    fn count(&self) -> usize {
        self.board.diffs.len()
            + self.board.plans.len()
            + self.screens.len()
            + self.folders.len()
            + usize::from(self.follows_trace)
    }

    fn has(&self, topic: &Topic) -> bool {
        match topic {
            Topic::Folder { worktree, path } => self.folders.contains_key(&(worktree.clone(), path.clone())),
            Topic::Trace => self.follows_trace,
            Topic::Diff { worktree } => self.board.diffs.contains(worktree),
            Topic::Plan { id } => self.board.plans.contains(id),
            Topic::Screen { session, .. } => self.screens.contains(session),
        }
    }
}

/// Refusals a well-behaved client meets as a matter of course: another one races it for a lease,
/// or it types faster than the program reads. They never count towards disconnecting it.
fn expected(code: ErrorCode) -> bool {
    matches!(code, ErrorCode::Fenced | ErrorCode::LeaseHeld | ErrorCode::SlowClient)
}

/// Why a request was refused, and whether it counts towards disconnecting the client.
struct Refusal {
    error: WireError,
    counts: bool,
}

impl Refusal {
    /// What the request named went away meanwhile (its terminal closed, its diff file changed):
    /// a well-behaved client meets that too, so it does not count. Something that never existed
    /// is not gone, and counts.
    fn gone(error: WireError) -> Self {
        Self { error, counts: false }
    }
}

impl From<WireError> for Refusal {
    fn from(error: WireError) -> Self {
        let counts = !expected(error.code);
        Self { error, counts }
    }
}

fn no_terminal() -> WireError {
    WireError::new(ErrorCode::NotFound, "that session has no terminal")
}

fn conversation(
    out: &mut Out<'_>,
    shared: &Shared,
    outbox: &Arc<Outbox>,
    subscriptions: &mut Follows,
) -> Result<(), Ended> {
    let config = &shared.config;
    let mut errors = 0u32;
    let mut last_heard = Instant::now();
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Err(Ended::Stopped);
        }
        let sessions = shared.projections.sessions.load();
        for message in subscriptions.tunnels.poll(|port| previewable(&sessions.data, port)) {
            out.envelope(&Envelope::host(&out.epoch, message)).map_err(link_ended)?;
        }
        match outbox.take() {
            Taken::Frames { board, screens } => {
                for frame in board {
                    if subscriptions.board.admits(&frame.scope, subscriptions.host) {
                        out.raw(&frame.text).map_err(link_ended)?;
                    } else {
                        // Left out, but its cursor still counts.
                        let skipped = Envelope::host(&out.epoch, Message::Skipped { cursor: Seq(frame.cursor) });
                        out.envelope(&skipped).map_err(link_ended)?;
                    }
                }
                for frame in screens {
                    out.raw(&frame).map_err(link_ended)?;
                }
            }
            Taken::Lagged => resync(out, shared, outbox)?,
        }
        while let Ok(result) = subscriptions.results.try_recv() {
            let request_id = if result.outcome.is_final() {
                subscriptions.ops.remove(&result.op).and_then(|(_, id)| id)
            } else {
                subscriptions.ops.get(&result.op).and_then(|(_, id)| id.clone())
            };
            let reply = Envelope::host(&out.epoch, Message::CommandResult(result)).reply_to(request_id);
            out.envelope(&reply).map_err(link_ended)?;
        }
        if subscriptions.follows_trace {
            let revision = shared.projections.trace.load().revision;
            if subscriptions.trace_revision != Some(revision) {
                out.envelope(&Envelope::host(&out.epoch, Message::TraceChanged { revision: Seq(revision) }))
                    .map_err(link_ended)?;
                subscriptions.trace_revision = Some(revision);
            }
        }
        if subscriptions.input {
            tell_leases(out, shared, subscriptions).map_err(link_ended)?;
        }
        // Folder feeds run on the host connection, independently of any UI frame. A replacement
        // starts at offset zero, so clients discard pages from an older listing.
        for ((worktree, path), sent) in &mut subscriptions.folders {
            let revision = shared.diffs.folder_revision(worktree);
            if revision.is_some() && revision != *sent {
                match shared.diffs.folder(worktree, path, 0, shared.frame_budget.saturating_sub(2048)) {
                    Ok(page) => {
                        *sent = Some(page.revision.0);
                        out.envelope(&Envelope::host(&out.epoch, Message::FolderPage(page))).map_err(link_ended)?;
                    }
                    Err(error) => {
                        *sent = revision;
                        out.error(error, None).map_err(link_ended)?;
                    }
                }
            }
        }
        match out.link.recv(config.tick) {
            Ok(Some(frame)) => {
                last_heard = Instant::now();
                let envelope = match wire::decode(&frame) {
                    Ok(e) => e,
                    Err(e) => {
                        errors += 1;
                        let code = e.code;
                        out.error(e, None).map_err(link_ended)?;
                        if code == ErrorCode::Oversize || errors > config.max_errors {
                            return Err(Ended::Rejected(code));
                        }
                        continue;
                    }
                };
                if let Err(e) = handle(out, shared, outbox, subscriptions, envelope) {
                    let (Refusal { error, counts }, op, request_id) = match e {
                        Handled::Link(ended) => return Err(ended),
                        Handled::Refused(refusal, request_id) => (refusal, None, request_id),
                        Handled::Result(op, refusal, request_id) => (refusal, Some(op), request_id),
                    };
                    let code = error.code;
                    if counts {
                        errors += 1;
                    }
                    match op {
                        // A refused mutation still ends with a result for its operation.
                        Some(op) => {
                            let result = CommandResult { op, outcome: Outcome::refused(error) };
                            let reply = Envelope::host(&out.epoch, Message::CommandResult(result)).reply_to(request_id);
                            out.envelope(&reply).map_err(link_ended)?;
                        }
                        None => out.error(error, request_id).map_err(link_ended)?,
                    }
                    if errors > config.max_errors {
                        return Err(Ended::Rejected(code));
                    }
                }
            }
            Ok(None) => {
                if last_heard.elapsed() > config.idle_timeout {
                    return Err(Ended::Idle);
                }
                if out.last_sent.elapsed() > config.ping_interval {
                    let ping = Envelope::host(&out.epoch, Message::Ping { nonce: None });
                    out.envelope(&ping).map_err(link_ended)?;
                }
            }
            Err(e) => {
                return Err(refuse_link(out, e));
            }
        }
    }
}

/// Tell the client who holds each screen it follows, when that changed, after the leases'
/// housekeeping (an expired lease ends, a debounced resize lands).
fn tell_leases(out: &mut Out<'_>, shared: &Shared, follows: &mut Follows) -> Result<(), LinkError> {
    let now = Instant::now();
    for session in &follows.screens {
        let Some((identity, control)) = shared.screens.terminals().control(session) else {
            // Its terminal went away, and the lease with it: nobody holds it now. Said once; a
            // new terminal for the session is told about afresh.
            if follows.told.remove(session).is_some() {
                let state = LeaseState {
                    session: session.clone(),
                    holder: None,
                    yours: false,
                    lease: None,
                    next: None,
                    ttl_ms: None,
                    resized: false,
                    reason: Some(LeaseReason::Disconnected),
                };
                out.envelope(&Envelope::host(&out.epoch, Message::LeaseState(state)))?;
            }
            continue;
        };
        control.tick(now, &shared.lease);
        if follows.told.get(session) == Some(&(identity, control.lease().version())) {
            continue;
        }
        let (version, state) = control.view(session, &follows.me, &shared.lease);
        follows.told.insert(session.clone(), (identity, version));
        out.envelope(&Envelope::host(&out.epoch, Message::LeaseState(state)))?;
    }
    Ok(())
}

/// The terminal of a screen this client follows, if it may type into it.
fn controllable(
    shared: &Shared,
    follows: &Follows,
    session: &str,
) -> Result<(usize, Arc<dyn TerminalControl>), Refusal> {
    if !follows.input {
        return Err(WireError::new(ErrorCode::Unauthorised, "the input capability was not negotiated").into());
    }
    if !follows.may.contains(&Grant::Type) {
        return Err(WireError::new(ErrorCode::Unauthorised, "this client is view-only").into());
    }
    if !follows.screens.contains(session) {
        return Err(WireError::new(ErrorCode::NotFound, "subscribe to that screen first").into());
    }
    // It follows the screen, so a missing terminal closed meanwhile.
    shared.screens.terminals().control(session).ok_or_else(|| Refusal::gone(no_terminal()))
}

/// Answer a lease request with the lease as this client now sees it.
fn reply_lease(
    out: &mut Out<'_>,
    shared: &Shared,
    follows: &mut Follows,
    session: &str,
    (identity, control): (usize, Arc<dyn TerminalControl>),
    request_id: Option<String>,
) -> Result<(), Handled> {
    let (version, state) = control.view(session, &follows.me, &shared.lease);
    follows.told.insert(session.to_string(), (identity, version));
    Ok(out.envelope(&Envelope::host(&out.epoch, Message::LeaseState(state)).reply_to(request_id))?)
}

fn resync(out: &mut Out<'_>, shared: &Shared, outbox: &Outbox) -> Result<(), Ended> {
    if let Err(e) = lock(&shared.hub).resync(outbox) {
        // One item of the board is over the frame limit; say so rather than leave the client stale.
        let code = e.code;
        let _ = out.error(e, None);
        return Err(Ended::Rejected(code));
    }
    Ok(())
}

enum Handled {
    Link(Ended),
    Refused(Refusal, Option<String>),
    /// A mutation refused before it reached the journal: answered with a `command_result`.
    Result(String, Refusal, Option<String>),
}

/// Pass a mutation through the gates and hand it to the runtime thread. A repeat of an operation
/// this connection still waits for is answered `accepted` and not sent again: the outcome it
/// waits for answers the latest request.
fn mutate(
    out: &mut Out<'_>,
    shared: &Shared,
    follows: &mut Follows,
    command: Command,
    request_id: Option<String>,
) -> Result<(), Handled> {
    let op = command.op().to_string();
    let refuse = |code: ErrorCode, message: &str| {
        Handled::Result(op.clone(), WireError::new(code, message).into(), request_id.clone())
    };
    if !follows.mutations {
        return Err(refuse(ErrorCode::Unauthorised, "the mutations capability was not negotiated"));
    }
    if matches!(&command, Command::HostCommand(c) if matches!(c.action,
        atomics_core::ops::HostAction::Edit { .. } | atomics_core::ops::HostAction::EditorHints { .. }
        | atomics_core::ops::HostAction::PullRequestDiff { .. } | atomics_core::ops::HostAction::StartChief))
        && (!follows.host || !follows.panels)
    {
        return Err(refuse(ErrorCode::Unauthorised, "the host and panels capabilities were not negotiated"));
    }
    if !follows.may.contains(&command.grant()) {
        let why = if follows.may.is_empty() { "this client is view-only" } else { "this client may not do that" };
        return Err(refuse(ErrorCode::Unauthorised, why));
    }
    let fingerprint = super::journal::fingerprint(&command);
    if let Some((previous, answers)) = follows.ops.get_mut(&op) {
        if *previous != fingerprint {
            return Err(refuse(ErrorCode::Conflict, "this operation ID was used for another command"));
        }
        *answers = request_id.clone();
        let result = CommandResult { op, outcome: Outcome::Accepted };
        return Ok(out.envelope(&Envelope::host(&out.epoch, Message::CommandResult(result)).reply_to(request_id))?);
    }
    if follows.ops.len() >= MAX_PENDING_OPS {
        return Err(refuse(ErrorCode::TooMany, "too many operations under way; wait for their results"));
    }
    let request = OpRequest {
        client: follows.client_id.clone(),
        device: follows.device.clone(),
        command,
        reply: follows.reply.clone(),
    };
    if !shared.commands.submit(request) {
        return Err(refuse(ErrorCode::Internal, "the host is stopping"));
    }
    follows.ops.insert(op, (fingerprint, request_id));
    Ok(())
}

impl From<LinkError> for Handled {
    fn from(e: LinkError) -> Self {
        Handled::Link(link_ended(e))
    }
}

fn handle(
    out: &mut Out<'_>,
    shared: &Shared,
    outbox: &Arc<Outbox>,
    subscriptions: &mut Follows,
    envelope: Envelope,
) -> Result<(), Handled> {
    let id = envelope.request_id.clone();
    let refuse = |code: ErrorCode, message: &str| Handled::Refused(WireError::new(code, message).into(), id.clone());
    let refused = |error: WireError| Handled::Refused(error.into(), id.clone());
    let refusal = |refusal: Refusal| Handled::Refused(refusal, id.clone());
    let gone = |error: WireError| Handled::Refused(Refusal::gone(error), id.clone());
    let no_terminals = || refuse(ErrorCode::Unauthorised, "the terminals capability was not negotiated");
    match envelope.message {
        Message::TunnelOpen { id: tunnel, port } => {
            if !subscriptions.tunnel {
                return Err(refuse(ErrorCode::Unauthorised, "the tunnel capability was not negotiated"));
            }
            let permitted = previewable(&shared.projections.sessions.load().data, port);
            let result = if permitted {
                subscriptions.tunnels.open(tunnel.clone(), port)
            } else {
                Err("port is not previewable by this client".into())
            };
            if let Err(reason) = result {
                out.envelope(&Envelope::host(&out.epoch, Message::TunnelRefused { id: tunnel, reason }))?;
            }
            Ok(())
        }
        message @ (Message::TunnelData { .. } | Message::TunnelAck { .. } | Message::TunnelClose { .. }) => {
            if !subscriptions.tunnel {
                return Err(refuse(ErrorCode::Unauthorised, "the tunnel capability was not negotiated"));
            }
            let tunnel = match &message {
                Message::TunnelData { id, .. } | Message::TunnelAck { id, .. } | Message::TunnelClose { id, .. } => {
                    id.clone()
                }
                _ => unreachable!(),
            };
            if let Err(reason) = subscriptions.tunnels.receive(message) {
                subscriptions.tunnels.remove(&tunnel);
                out.envelope(&Envelope::host(&out.epoch, Message::TunnelRefused { id: tunnel, reason }))?;
            }
            Ok(())
        }
        Message::Subscribe { topics } => {
            // The whole list is checked first, so a refused request subscribes to nothing.
            let mut fresh = HashSet::new();
            for topic in &topics {
                if let Topic::Folder { worktree, .. } = topic {
                    if !subscriptions.explorer {
                        return Err(refuse(ErrorCode::Unauthorised, "the explorer capability was not negotiated"));
                    }
                    if !lock(&shared.hub).has_worktree(worktree) {
                        return Err(gone(WireError::new(ErrorCode::NotFound, "that worktree is no longer open")));
                    }
                }
                if matches!(topic, Topic::Trace) && !subscriptions.trace {
                    return Err(refuse(ErrorCode::Unauthorised, "the trace capability was not negotiated"));
                }
                if let Topic::Screen { session, .. } = topic {
                    if !subscriptions.terminals {
                        return Err(no_terminals());
                    }
                    let terminals = shared.screens.terminals();
                    if terminals.control(session).is_none() {
                        // Its terminal may have closed since the client heard of it, or (a session on the
                        // board) not have started yet: the client could not know, and asks again when the
                        // session changes.
                        let expected = terminals.closed(session) || lock(&shared.hub).has_session(session);
                        return Err(if expected { gone(no_terminal()) } else { refused(no_terminal()) });
                    }
                }
                if !subscriptions.has(topic) {
                    fresh.insert(topic.key());
                }
            }
            // One limit spans screens and board topics.
            if subscriptions.count() + fresh.len() > MAX_SUBSCRIPTIONS {
                return Err(refuse(ErrorCode::TooMany, "too many subscriptions"));
            }
            let mut screens: Vec<String> = Vec::new();
            for topic in &topics {
                if let Topic::Screen { session, focused } = topic {
                    if let Err(error) = shared.screens.subscribe(session, outbox, *focused) {
                        // Its terminal closed since the check: undo what this request added.
                        for session in screens {
                            subscriptions.screens.remove(&session);
                            shared.screens.unsubscribe(&session, outbox);
                        }
                        return Err(gone(error));
                    }
                    if subscriptions.screens.insert(session.clone()) {
                        screens.push(session.clone());
                    }
                }
            }
            if topics.contains(&Topic::Trace) {
                subscriptions.follows_trace = true;
                subscriptions.trace_revision = None;
            }
            // Board topics cannot fail, so they go in once every screen has.
            let before = subscriptions.board.clone();
            for topic in &topics {
                if let Topic::Folder { worktree, path } = topic
                    && subscriptions.folders.insert((worktree.clone(), path.clone()), None).is_none()
                {
                    shared.diffs.watch_folder(worktree, true);
                }
                if let Some((set, name)) = subscriptions.board.entry(topic) {
                    set.insert(name.to_string());
                }
            }
            if subscriptions.board.widens(&before) {
                outbox_resync(shared, outbox).map_err(refused)?;
            }
            Ok(())
        }
        Message::Unsubscribe { topics } => {
            let before = subscriptions.board.clone();
            for topic in topics {
                if let Topic::Folder { worktree, path } = &topic
                    && subscriptions.folders.remove(&(worktree.clone(), path.clone())).is_some()
                {
                    shared.diffs.watch_folder(worktree, false);
                }
                if topic == Topic::Trace {
                    subscriptions.follows_trace = false;
                    subscriptions.trace_page = None;
                    subscriptions.trace_revision = None;
                }
                if let Topic::Screen { session, .. } = &topic
                    && subscriptions.screens.remove(session)
                {
                    shared.screens.unsubscribe(session, outbox);
                    subscriptions.told.remove(session);
                    subscriptions.slowed.remove(session);
                    // Typing needs the screen: no longer following it ends the lease.
                    if let Some((_, control)) = shared.screens.terminals().control(session) {
                        control.disconnect(&subscriptions.me, LeaseReason::Disconnected);
                    }
                }
                if let Some((set, name)) = subscriptions.board.entry(&topic) {
                    set.remove(name);
                }
            }
            // Dropping the last topic of a kind goes back to every item of it.
            if subscriptions.board.widens(&before) {
                outbox_resync(shared, outbox).map_err(refused)?;
            }
            Ok(())
        }
        Message::HistoryGet(request) => {
            if !subscriptions.terminals {
                return Err(no_terminals());
            }
            let followed = subscriptions.screens.contains(&request.session);
            let chunk = shared
                .screens
                .history(&request.session, request.generation.0, request.before.0, request.count)
                // A followed screen without a stream is one whose terminal closed meanwhile.
                .map_err(|e| if e.code == ErrorCode::NotFound && followed { gone(e) } else { refused(e) })?;
            let reply = Envelope::host(&out.epoch, Message::HistoryChunk(chunk)).reply_to(id);
            Ok(out.envelope(&reply)?)
        }
        Message::TraceGet { revision, offset, limit } => {
            if !subscriptions.trace {
                return Err(refuse(ErrorCode::Unauthorised, "the trace capability was not negotiated"));
            }
            let current = subscriptions
                .trace_page
                .as_ref()
                .filter(|page| page.revision == revision.0)
                .cloned()
                .unwrap_or_else(|| shared.projections.trace.load());
            if current.revision != revision.0 {
                return Err(gone(WireError::new(
                    ErrorCode::StaleRevision,
                    "trace changed; request its latest revision",
                )));
            }
            let room = shared
                .frame_budget
                .min(wire::MAX_FRAME_BYTES)
                .saturating_sub(256 + id.as_deref().map_or(0, wire::json_len));
            let page = current.data.page(current.revision, offset, limit, room).map_err(refused)?;
            subscriptions.trace_page = page.next_offset.is_some().then_some(current);
            let reply = Envelope::host(&out.epoch, Message::TracePage(page)).reply_to(id);
            Ok(out.envelope(&reply)?)
        }
        Message::DiffGet { worktree, path, revision, offset } => {
            let Some((current, file)) = shared.diffs.file(&worktree, &path) else {
                // The diff may have changed since the revision the client was told of the file in.
                let error = WireError::new(ErrorCode::NotFound, "no such diff file");
                let told = revision.is_some_and(|r| shared.diffs.moved_on(&worktree, r.0));
                return Err(if told { gone(error) } else { refused(error) });
            };
            if revision.is_some_and(|r| r != Seq(current)) {
                return Err(refuse(ErrorCode::StaleRevision, &format!("the diff is at revision {current}")));
            }
            // The envelope, the worktree and the paging fields go alongside the lines.
            let room = shared
                .frame_budget
                .saturating_sub(1024 + wire::json_len(&worktree) + id.as_deref().map_or(0, wire::json_len));
            let (page, next) = wire::diff_page(&file, offset.map_or(0, |o| o as usize), room).map_err(refused)?;
            let body = DiffFileBody { worktree, revision: current, file: page };
            let message = Message::DiffFile { body, truncated: next.is_some(), next_offset: next.map(|n| n as u32) };
            let reply = Envelope::host(&out.epoch, message).reply_to(id);
            match wire::encode(&reply) {
                Ok(frame) => Ok(out.raw(&frame)?),
                Err(e) => Err(Handled::Refused(e.into(), reply.request_id)),
            }
        }
        Message::FolderGet { worktree, path, revision, offset } => {
            if !subscriptions.explorer {
                return Err(refuse(ErrorCode::Unauthorised, "the explorer capability was not negotiated"));
            }
            if !subscriptions.folders.contains_key(&(worktree.clone(), path.clone())) {
                return Err(refuse(ErrorCode::NotFound, "subscribe to that folder first"));
            }
            let budget = shared
                .frame_budget
                .saturating_sub(256 + wire::json_len(&out.epoch) + id.as_deref().map_or(0, wire::json_len));
            let page = shared.diffs.folder(&worktree, &path, offset, budget).map_err(gone)?;
            if page.revision != revision {
                return Err(gone(WireError::new(ErrorCode::StaleRevision, "folder listing changed; start again")));
            }
            Ok(out.envelope(&Envelope::host(&out.epoch, Message::FolderPage(page)).reply_to(id))?)
        }
        Message::FileGet { worktree, path } => {
            if !subscriptions.explorer {
                return Err(refuse(ErrorCode::Unauthorised, "the explorer capability was not negotiated"));
            }
            // JSON escaping takes at most six bytes for each byte of text.
            let overhead = 256
                + wire::json_len(&out.epoch)
                + wire::json_len(&worktree)
                + wire::json_len(&path)
                + id.as_deref().map_or(0, wire::json_len);
            let budget = shared.frame_budget.saturating_sub(overhead) / 6;
            let (content, truncated) = shared.diffs.content(&worktree, &path, budget).map_err(gone)?;
            Ok(out.envelope(
                &Envelope::host(&out.epoch, Message::FileContent { worktree, path, content, truncated }).reply_to(id),
            )?)
        }
        Message::LeaseAcquire { session, takeover } => {
            let terminal = controllable(shared, subscriptions, &session).map_err(refusal)?;
            terminal.1.acquire(&subscriptions.me, takeover, Instant::now(), &shared.lease).map_err(refused)?;
            reply_lease(out, shared, subscriptions, &session, terminal, id)
        }
        Message::LeaseRenew { session, lease } => {
            let terminal = controllable(shared, subscriptions, &session).map_err(refusal)?;
            terminal.1.renew(&subscriptions.me, lease.0, Instant::now(), &shared.lease).map_err(refused)?;
            reply_lease(out, shared, subscriptions, &session, terminal, id)
        }
        Message::LeaseRelease { session, lease } => {
            let terminal = controllable(shared, subscriptions, &session).map_err(refusal)?;
            terminal.1.release(&subscriptions.me, lease.0, Instant::now(), &shared.lease).map_err(refused)?;
            reply_lease(out, shared, subscriptions, &session, terminal, id)
        }
        Message::TerminalInput { session, lease, seq, data } => {
            let (_, control) = controllable(shared, subscriptions, &session).map_err(refusal)?;
            let typed = (lease.0, seq.0);
            let admitted = control.input(&subscriptions.me, typed, data.into_bytes(), Instant::now(), &shared.lease);
            if let Err(error) = admitted {
                // A client sends a long paste at once, so pieces it sent behind one refused as
                // `slow_client` meet `out_of_order`; it types them again from the refused one.
                let behind = |&(token, refused): &(u64, u64)| token == lease.0 && seq.0 > refused;
                let counts = match error.code {
                    ErrorCode::OutOfOrder => !subscriptions.slowed.get(&session).is_some_and(behind),
                    code => !expected(code),
                };
                if error.code == ErrorCode::SlowClient {
                    subscriptions.slowed.insert(session, typed);
                }
                return Err(refusal(Refusal { error, counts }));
            }
            subscriptions.slowed.remove(&session);
            // The echo is copied as soon as it shows; queueing it wakes this thread to send it.
            shared.screens.wake();
            Ok(())
        }
        Message::TerminalResize { session, lease, size } => {
            let (_, control) = controllable(shared, subscriptions, &session).map_err(refusal)?;
            control.resize_for(&subscriptions.me, lease.0, size, Instant::now(), &shared.lease).map_err(refused)
        }
        Message::TerminalFit { session, size } => {
            if !subscriptions.fit {
                return Err(refuse(ErrorCode::Unauthorised, "the fit capability was not negotiated"));
            }
            let (_, control) = controllable(shared, subscriptions, &session).map_err(refusal)?;
            control.fit(size);
            Ok(())
        }
        Message::Ack { cursor } => {
            let head = lock(&shared.hub).head();
            if cursor.0 > head {
                return Err(refuse(ErrorCode::StaleRevision, "ack is ahead of the host's cursor"));
            }
            Ok(())
        }
        Message::Resync { screens, .. } if !screens.is_empty() => {
            for session in &screens {
                shared.screens.resync(session, outbox).map_err(refused)?;
            }
            Ok(())
        }
        Message::Resync { .. } => outbox_resync(shared, outbox).map_err(refused),
        Message::Ping { nonce } => {
            let pong = Envelope::host(&out.epoch, Message::Pong { nonce }).reply_to(id);
            Ok(out.envelope(&pong)?)
        }
        Message::Pong { .. } => Ok(()),
        message @ (Message::ApprovalDecide(_)
        | Message::PlanCommand(_)
        | Message::ReviewCommand(_)
        | Message::SessionCommand(_)
        | Message::HostCommand(_)) => {
            if matches!(&message, Message::PlanCommand(c) if c.action == atomics_core::ops::PlanAction::Complete)
                && !subscriptions.complete
            {
                let Message::PlanCommand(c) = message else { unreachable!() };
                return Err(Handled::Result(
                    c.op,
                    WireError::new(ErrorCode::Unauthorised, "the plan_complete capability was not negotiated").into(),
                    id,
                ));
            }
            let command = Command::from_message(message).expect("a mutation");
            mutate(out, shared, subscriptions, command, id)
        }
        Message::Hello { .. } => Err(refuse(ErrorCode::Malformed, "already greeted")),
        _ => Err(refuse(ErrorCode::Malformed, "that message is sent by the host, not to it")),
    }
}

fn outbox_resync(shared: &Shared, outbox: &Outbox) -> Result<(), WireError> {
    lock(&shared.hub).resync(outbox)
}

/// All sessions in this projection are visible to this authenticated connection.
fn previewable(sessions: &[atomics_core::projection::SessionInfo], port: u16) -> bool {
    sessions
        .iter()
        .any(|s| s.status != atomics_core::session::AgentStatus::Exited && s.previews.iter().any(|p| p.port == port))
}
