//! The event stream every client follows: it turns published projections into numbered events,
//! keeps a bounded replay ring, and fans frames out to per-client queues.
//!
//! Nothing here does I/O, and nothing here touches the host's `State`. A client's queue is
//! bounded; when it overflows the client is marked lagged, its backlog is dropped, and its own
//! thread sends a fresh snapshot instead. A reader that stops reading can therefore never make the
//! hub (or the runtime thread that publishes) wait. Terminal screens use the same outbox with a
//! slot per screen (`screens.rs`), outside the board's queue, so a busy terminal never pushes an
//! approval out and board events are always sent first. Whatever queues something for a client
//! wakes its thread, so it is sent at once. Board frames carry their cursor and what they are
//! about, so a client that narrowed its topics is sent a `skipped` in place of an event.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use atomics_core::projection::{Loaded, Projections};
use atomics_core::wire::{self, Cursor, Disposition, Envelope, Message, Resume, Seq, WireError};

use super::link::Waker;
use super::screens::{Current, Feed};

/// Replay ring bounds: entries, bytes and age. It is an optimisation, not persistence.
#[derive(Debug, Clone)]
pub struct RingLimits {
    pub entries: usize,
    pub bytes: usize,
    pub age: Duration,
}

impl Default for RingLimits {
    fn default() -> Self {
        Self { entries: 200, bytes: 8 << 20, age: Duration::from_secs(300) }
    }
}

/// What a board event is about, for a client that narrowed its topics (`session.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Sent to everyone: sessions, approvals, snapshot parts.
    Open,
    Diff(String),
    Plan(String),
    /// The host's own state: only to a client that negotiated `host`.
    Host,
}

fn scope_of(message: &Message) -> Scope {
    match message {
        Message::PlanUpsert { plan, .. } => Scope::Plan(plan.id.clone()),
        Message::PlanRemoved { id, .. } => Scope::Plan(id.clone()),
        Message::TaskReport { plan, .. } => Scope::Plan(plan.clone()),
        Message::DiffSummary { summary, .. } => Scope::Diff(summary.worktree.clone()),
        Message::HostUpsert { .. } => Scope::Host,
        _ => Scope::Open,
    }
}

/// One encoded board frame, shared by every client queue and the replay ring.
#[derive(Debug)]
pub struct Frame {
    pub text: String,
    /// The event's cursor; 0 for a snapshot part.
    pub cursor: u64,
    pub scope: Scope,
}

#[derive(Default)]
struct Queue {
    frames: VecDeque<Arc<Frame>>,
    bytes: usize,
    lagged: bool,
    screens: HashMap<String, Slot>,
    /// Delta bytes waiting across all screen slots.
    screen_bytes: usize,
}

/// One subscribed screen in one client's outbox: deltas that follow what the client was sent, or
/// a note that it needs the current snapshot instead.
struct Slot {
    feed: Feed,
    deltas: Vec<Arc<str>>,
    bytes: usize,
    snapshot: bool,
    /// The generation and revision the client will hold once everything queued is sent.
    at: Option<(u64, u64)>,
    /// The client was told this screen does not fit in a frame; it is told again only after a
    /// snapshot of it has been sent.
    oversize: bool,
    /// Snapshots of this screen a newer copy overtook while they were encoded, in a row.
    overtaken: u32,
}

/// How often in a row a snapshot overtaken while it was encoded is encoded again from the newer
/// copy before it is sent as it is.
const SNAPSHOT_RETRIES: u32 = 3;

impl Slot {
    fn supersede(&mut self, total: &mut usize) {
        *total -= self.bytes;
        self.deltas.clear();
        self.bytes = 0;
        self.snapshot = true;
    }

    /// Whether to encode the snapshot again because a newer copy was published while it was
    /// encoded. After [`SNAPSHOT_RETRIES`] in a row the one encoded is sent as it is, so a screen
    /// that changes faster than it encodes still reaches the client.
    fn encode_again(&mut self, overtaken: bool) -> bool {
        if overtaken && self.overtaken < SNAPSHOT_RETRIES {
            self.overtaken += 1;
            return true;
        }
        self.overtaken = 0;
        false
    }
}

/// One client's bounded outbound queue. The hub pushes; the client's own thread drains.
pub struct Outbox {
    queue: Mutex<Queue>,
    max_frames: usize,
    max_bytes: usize,
    closed: AtomicBool,
    /// Ends the client thread's wait for its next frame.
    waker: OnceLock<Arc<Waker>>,
}

impl Outbox {
    pub(super) fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            max_frames,
            max_bytes,
            closed: AtomicBool::new(false),
            waker: OnceLock::new(),
        }
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake the client's thread whenever something is queued for it.
    pub fn set_waker(&self, waker: Arc<Waker>) {
        let _ = self.waker.set(waker);
    }

    /// Never blocks.
    fn wake(&self) {
        if let Some(waker) = self.waker.get() {
            waker.wake();
        }
    }

    /// Never blocks. A full queue drops the backlog and marks the client lagged.
    fn push(&self, frame: &Arc<Frame>) {
        {
            let mut q = self.queue();
            if q.lagged {
                return;
            }
            if q.frames.len() >= self.max_frames || q.bytes + frame.text.len() > self.max_bytes {
                q.frames.clear();
                q.bytes = 0;
                q.lagged = true;
            } else {
                q.bytes += frame.text.len();
                q.frames.push_back(frame.clone());
            }
        }
        self.wake();
    }

    /// Replace the board backlog with `frames` (a snapshot's parts), queued together so live
    /// events follow the last part. Called by the client's own thread or before it starts, which
    /// takes the queue next, so there is no one to wake.
    fn replace_board(&self, frames: &[Arc<Frame>]) {
        let mut q = self.queue();
        q.frames.clear();
        q.frames.extend(frames.iter().cloned());
        q.bytes = frames.iter().map(|f| f.text.len()).sum();
        q.lagged = false;
    }

    fn mark_lagged(&self) {
        {
            let mut q = self.queue();
            q.frames.clear();
            q.bytes = 0;
            q.lagged = true;
        }
        self.wake();
    }

    /// Everything queued (board events first, then screens), or that the client must be
    /// resynchronised first. A screen that needs a snapshot gets the one its feed holds now; until
    /// the first copy is published it waits. A snapshot that cannot fit in frames is answered once
    /// with an `oversize` error naming the session, and tried again at the screen's next change.
    ///
    /// Snapshots are encoded with the queue unlocked, so the threads that push to it never wait
    /// for an encoding. One the feed replaced meanwhile is not sent and the next call takes the
    /// newer, up to [`SNAPSHOT_RETRIES`] times in a row; then the older one is sent, and the newer
    /// one still follows it.
    pub fn take(&self) -> Taken {
        let wanted: Vec<(String, Arc<Current>)> = {
            let q = self.queue();
            if q.lagged {
                return Taken::Lagged;
            }
            q.screens
                .iter()
                .filter(|(_, slot)| slot.snapshot)
                .filter_map(|(session, slot)| Some((session.clone(), current(&slot.feed)?)))
                .collect()
        };
        // Encoded once per revision and shared by every client (`Current::frames`).
        let encoded: Vec<_> =
            wanted.into_iter().map(|(session, current)| (session, current.frames(), current)).collect();
        let mut q = self.queue();
        if q.lagged {
            return Taken::Lagged;
        }
        q.bytes = 0;
        let board: Vec<Arc<Frame>> = q.frames.drain(..).collect();
        let mut frames: Vec<Arc<str>> = Vec::new();
        let mut sent = 0;
        for slot in q.screens.values_mut().filter(|slot| !slot.snapshot) {
            sent += slot.bytes;
            slot.bytes = 0;
            frames.append(&mut slot.deltas);
        }
        q.screen_bytes -= sent;
        let mut again = false;
        for (session, snapshot, encoded_from) in encoded {
            let Some(slot) = q.screens.get_mut(&session).filter(|slot| slot.snapshot) else { continue };
            // A newer copy (or a new terminal) was published meanwhile: its delta was not queued,
            // so only its own snapshot brings the client up to date. Sent as it is after a few
            // tries, the older snapshot keeps the slot waiting for that one.
            let overtaken = !current(&slot.feed).is_some_and(|now| Arc::ptr_eq(&now, &encoded_from));
            if slot.encode_again(overtaken) {
                again = true;
                continue;
            }
            slot.snapshot = overtaken;
            again |= overtaken;
            match snapshot {
                Some(snapshot) => {
                    frames.extend(snapshot.iter().cloned());
                    slot.at = Some((encoded_from.generation, encoded_from.revision));
                    slot.oversize = false;
                }
                None => {
                    // Not followed by deltas: the next change asks for a snapshot again.
                    slot.at = None;
                    if !std::mem::replace(&mut slot.oversize, true) {
                        frames.push(encoded_from.oversize());
                    }
                }
            }
        }
        drop(q);
        if again {
            self.wake();
        }
        Taken::Frames { board, screens: frames }
    }

    /// Follow a screen, starting with a snapshot from `feed` (also when its terminal was replaced).
    pub fn watch_screen(&self, session: &str, feed: &Feed) {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let old = q.screens.insert(
                session.to_string(),
                Slot {
                    feed: feed.clone(),
                    deltas: Vec::new(),
                    bytes: 0,
                    snapshot: true,
                    at: None,
                    oversize: false,
                    overtaken: 0,
                },
            );
            q.screen_bytes -= old.map_or(0, |s| s.bytes);
        }
        self.wake();
    }

    pub fn unwatch_screen(&self, session: &str) {
        let mut q = self.queue();
        if let Some(slot) = q.screens.remove(session) {
            q.screen_bytes -= slot.bytes;
        }
    }

    /// Replace whatever is queued for this screen with its current snapshot. False when the
    /// client does not follow it.
    pub fn want_snapshot(&self, session: &str) -> bool {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let Some(slot) = q.screens.get_mut(session) else { return false };
            slot.supersede(&mut q.screen_bytes);
        }
        self.wake();
        true
    }

    /// Queue the delta from `base` to `revision` if it follows what this client holds. A delta
    /// that does not follow, or that would overfill the slot, turns the slot into a snapshot.
    pub fn push_screen(
        &self,
        session: &str,
        (generation, base, revision): (u64, u64, u64),
        frame: &Arc<str>,
        max_frames: usize,
        max_bytes: usize,
    ) {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let Some(slot) = q.screens.get_mut(session) else { return };
            if slot.snapshot {
                return; // the snapshot it will get already contains this change
            }
            match slot.at {
                Some(at) if at == (generation, base) => {
                    if slot.deltas.len() >= max_frames || q.screen_bytes + frame.len() > max_bytes {
                        slot.supersede(&mut q.screen_bytes);
                    } else {
                        slot.deltas.push(frame.clone());
                        slot.bytes += frame.len();
                        q.screen_bytes += frame.len();
                        slot.at = Some((generation, revision));
                    }
                }
                // Loaded a snapshot that already includes this delta.
                Some((g, r)) if g == generation && r >= revision => return,
                _ => slot.supersede(&mut q.screen_bytes),
            }
        }
        self.wake();
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

pub enum Taken {
    /// Board frames first, then screen frames.
    Frames {
        board: Vec<Arc<Frame>>,
        screens: Vec<Arc<str>>,
    },
    Lagged,
}

/// The copy a screen's feed holds now.
fn current(feed: &Feed) -> Option<Arc<Current>> {
    feed.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// How long after its last client a host is looked at as often as with one.
const UNWATCHED_AFTER: Duration = Duration::from_secs(60);

pub struct Hub {
    epoch: String,
    head: u64,
    /// What the events so far add up to; its `cursor` is `head`.
    state: Loaded,
    seen: u64,
    ring: VecDeque<(Instant, Arc<Frame>)>,
    ring_bytes: usize,
    limits: RingLimits,
    /// The current state's snapshot parts, shared by every client that needs them.
    snapshot: Option<Arc<[Arc<Frame>]>>,
    /// The most a snapshot part may hold.
    frame_budget: usize,
    clients: Vec<Arc<Outbox>>,
    /// When a client was last connected, or when the host started.
    watched_at: Instant,
    queue_frames: usize,
    queue_bytes: usize,
}

pub struct Attached {
    pub resume: Resume,
    pub outbox: Arc<Outbox>,
    pub epoch: String,
}

impl Hub {
    pub fn new(epoch: String, limits: RingLimits, queue_frames: usize, queue_bytes: usize) -> Self {
        let state = Projections::default().load();
        Self {
            epoch,
            head: 0,
            state,
            seen: u64::MAX,
            ring: VecDeque::new(),
            ring_bytes: 0,
            limits,
            snapshot: None,
            frame_budget: wire::MAX_FRAME_BYTES,
            clients: Vec::new(),
            watched_at: Instant::now(),
            queue_frames,
            queue_bytes,
        }
    }

    /// Cut snapshots into parts of at most `bytes` (the frame limit by default).
    pub fn with_frame_budget(mut self, bytes: usize) -> Self {
        self.frame_budget = bytes;
        self
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    fn oldest_retained(&self) -> u64 {
        self.ring.front().map_or(self.head + 1, |(_, frame)| frame.cursor)
    }

    /// Whether the board as published holds session `id`.
    pub fn has_session(&self, id: &str) -> bool {
        self.state.sessions.data.iter().any(|s| s.id == id)
    }

    /// A folder subscription may wait for its first diff worker result, but may name only a root
    /// the host has published, never an arbitrary path supplied by a client.
    pub fn has_worktree(&self, root: &str) -> bool {
        self.state.diffs.data.iter().any(|d| d.worktree == root)
            || self
                .state
                .host
                .data
                .projects
                .iter()
                .any(|p| p.root == root || p.worktrees.iter().any(|w| w.path == root))
            || self.state.sessions.data.iter().any(|s| s.worktree.as_deref().unwrap_or(&s.cwd) == root)
    }

    /// Read the published projections and turn what changed into events for every client.
    /// Whether a client is connected, or was a moment ago: one that comes back replays what it missed.
    pub fn watched(&self) -> bool {
        !self.clients.is_empty() || self.watched_at.elapsed() < UNWATCHED_AFTER
    }

    pub fn poll(&mut self, projections: &Projections) {
        self.expire_ring();
        self.clients.retain(|c| !c.is_closed());
        if !self.clients.is_empty() {
            self.watched_at = Instant::now();
        }
        if projections.cursor() == self.seen {
            return;
        }
        let loaded = projections.load();
        self.seen = loaded.cursor;
        let events = self.diff(&loaded, |id| projections.resolution(id));
        self.snapshot = None;
        if events.is_empty() {
            // Only the order or the revisions moved: no event, but the next snapshot shows them.
            self.state = Loaded { cursor: self.head, ..loaded };
            return;
        }
        for message in events {
            self.head += 1;
            let scope = scope_of(&message);
            let envelope = Envelope::host(&self.epoch, with_cursor(message, Seq(self.head)));
            match wire::encode(&envelope) {
                Ok(text) => self.publish(Arc::new(Frame { text, cursor: self.head, scope })),
                Err(_) => {
                    // An event too large to send cannot be replayed either: everyone resyncs.
                    self.ring.clear();
                    self.ring_bytes = 0;
                    for client in &self.clients {
                        client.mark_lagged();
                    }
                }
            }
        }
        self.state = Loaded { cursor: self.head, ..loaded };
    }

    fn publish(&mut self, frame: Arc<Frame>) {
        for client in &self.clients {
            client.push(&frame);
        }
        self.ring_bytes += frame.text.len();
        self.ring.push_back((Instant::now(), frame));
        while self.ring.len() > self.limits.entries || self.ring_bytes > self.limits.bytes {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some((_, frame)) = self.ring.pop_front() {
            self.ring_bytes -= frame.text.len();
        }
    }

    fn expire_ring(&mut self) {
        while self.ring.front().is_some_and(|(at, _)| at.elapsed() > self.limits.age) {
            self.drop_oldest();
        }
    }

    /// Events (cursor placeholders, filled in by the caller) for the change from `self.state` to
    /// `new`. `resolution` says how an approval that left ended, when it was answered.
    fn diff(&self, new: &Loaded, resolution: impl Fn(&str) -> Option<Disposition>) -> Vec<Message> {
        let old = &self.state;
        let mut out = Vec::new();
        let zero = Seq(0);
        if new.sessions.revision != old.sessions.revision {
            let before: BTreeMap<_, _> = old.sessions.data.iter().map(|s| (&s.id, s)).collect();
            let after: BTreeMap<_, _> = new.sessions.data.iter().map(|s| (&s.id, s)).collect();
            for (id, s) in &after {
                if before.get(id) != Some(s) {
                    out.push(Message::SessionUpsert { cursor: zero, session: (*s).clone() });
                }
            }
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                out.push(Message::SessionRemoved { cursor: zero, id: (*id).clone() });
            }
        }
        if new.plans.revision != old.plans.revision {
            let before: BTreeMap<_, _> = old.plans.data.iter().map(|p| (&p.id, p)).collect();
            let after: BTreeMap<_, _> = new.plans.data.iter().map(|p| (&p.id, p)).collect();
            for (id, p) in &after {
                if before.get(id) != Some(p) {
                    out.push(Message::PlanUpsert { cursor: zero, plan: (*p).clone() });
                }
            }
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                out.push(Message::PlanRemoved { cursor: zero, id: (*id).clone() });
            }
        }
        if new.approvals.revision != old.approvals.revision {
            let before: BTreeMap<_, _> = old.approvals.data.iter().map(|a| (&a.id, a)).collect();
            let after: BTreeMap<_, _> = new.approvals.data.iter().map(|a| (&a.id, a)).collect();
            for (id, a) in &after {
                if before.get(id) != Some(a) {
                    out.push(Message::ApprovalPending { cursor: zero, approval: (*a).clone() });
                }
            }
            // An answered approval says how; one that went otherwise (the hook gave up, the agent
            // moved on, it was answered in the terminal) is withdrawn.
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                let disposition = resolution(id).unwrap_or(Disposition::Withdrawn);
                out.push(Message::ApprovalResolved { cursor: zero, id: (*id).clone(), disposition });
            }
        }
        if new.diffs.revision != old.diffs.revision {
            let before: BTreeMap<_, _> = old.diffs.data.iter().map(|d| (&d.worktree, d)).collect();
            let after: BTreeMap<_, _> = new.diffs.data.iter().map(|d| (&d.worktree, d)).collect();
            for (id, d) in &after {
                if before.get(id) != Some(d) {
                    out.push(Message::DiffSummary { cursor: zero, summary: (*d).clone() });
                }
            }
            for (id, d) in before.iter().filter(|(id, _)| !after.contains_key(**id)) {
                let tombstone = atomics_core::projection::DiffSummary::tombstone(id, d.revision + 1);
                out.push(Message::DiffSummary { cursor: zero, summary: tombstone });
            }
        }
        if new.host.revision != old.host.revision && new.host.data != old.host.data {
            out.push(Message::HostUpsert { cursor: zero, host: new.host.data.clone() });
        }
        out
    }

    /// The current state as `state_snapshot` frames (one, or parts when the board does not fit in
    /// one), shared between clients at the same cursor. `Err` only when a single session, plan,
    /// approval or diff summary is itself over the budget.
    pub fn snapshot_frames(&mut self) -> Result<Arc<[Arc<Frame>]>, WireError> {
        if let Some(frames) = &self.snapshot {
            return Ok(frames.clone());
        }
        let frames: Arc<[Arc<Frame>]> = wire::snapshot_frames(&self.epoch, &self.state, self.frame_budget)?
            .into_iter()
            .map(|text| Arc::new(Frame { text, cursor: 0, scope: Scope::Open }))
            .collect();
        self.snapshot = Some(frames.clone());
        Ok(frames)
    }

    /// Register a client. In one step: decide replay or reset, queue the frames it needs first, and
    /// start feeding it live events, so nothing published in between is lost or repeated.
    pub fn attach(&mut self, prior_epoch: Option<&str>, prior_cursor: Option<Cursor>) -> Result<Attached, WireError> {
        self.expire_ring();
        let resume =
            wire::decide_resume(&self.epoch, Seq(self.head), Seq(self.oldest_retained()), prior_epoch, prior_cursor);
        let outbox = Arc::new(Outbox::new(self.queue_frames, self.queue_bytes));
        match resume {
            Resume::Replay { after } => {
                let mut q = outbox.queue();
                for (_, frame) in self.ring.iter().filter(|(_, frame)| frame.cursor > after.0) {
                    q.bytes += frame.text.len();
                    q.frames.push_back(frame.clone());
                }
            }
            Resume::Reset { .. } => outbox.replace_board(&self.snapshot_frames()?),
        }
        self.clients.push(outbox.clone());
        Ok(Attached { resume, outbox, epoch: self.epoch.clone() })
    }

    /// For a lagged or resyncing client: drop its backlog and queue the current snapshot, which
    /// already includes everything published so far.
    pub fn resync(&mut self, outbox: &Outbox) -> Result<(), WireError> {
        outbox.replace_board(&self.snapshot_frames()?);
        Ok(())
    }
}

fn with_cursor(mut message: Message, to: Cursor) -> Message {
    match &mut message {
        Message::SessionUpsert { cursor, .. }
        | Message::SessionRemoved { cursor, .. }
        | Message::PlanUpsert { cursor, .. }
        | Message::PlanRemoved { cursor, .. }
        | Message::TaskReport { cursor, .. }
        | Message::ApprovalPending { cursor, .. }
        | Message::ApprovalResolved { cursor, .. }
        | Message::DiffSummary { cursor, .. }
        | Message::HostUpsert { cursor, .. } => *cursor = to,
        _ => {}
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_overtaken_snapshot_is_encoded_again_only_a_few_times_in_a_row() {
        let mut slot = Slot {
            feed: Feed::default(),
            deltas: Vec::new(),
            bytes: 0,
            snapshot: true,
            at: None,
            oversize: false,
            overtaken: 0,
        };
        for _ in 0..SNAPSHOT_RETRIES {
            assert!(slot.encode_again(true));
        }
        assert!(!slot.encode_again(true), "then the one encoded is sent as it is");
        assert!(slot.encode_again(true), "and the count starts again");
        assert!(!slot.encode_again(false), "one not overtaken is sent");
        assert_eq!(slot.overtaken, 0);
    }
}
