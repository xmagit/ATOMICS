//! Terminal screens for subscribed clients (docs/daemon.md, Terminal streaming).
//!
//! The runtime thread publishes which terminals exist ([`Terminals`]); it does nothing else for
//! streaming. This module runs on the protocol thread: it copies a screen only while some client
//! subscribes to it, only when the emulator reports a change, and at most every 16 ms (about 60
//! frames a second, for a focused or a visible pane). The copy happens under the emulator's short lock; comparing,
//! encoding and queueing happen after it is released, and nothing here takes the `State` lock.
//!
//! Each change is one revision: the first copy of a generation is a snapshot, later ones are
//! deltas from the previous revision. A client's outbox keeps a bounded slot per screen; when the
//! slot overflows, or a delta does not follow what the client was sent, the slot drops its deltas
//! and the client's thread sends the current snapshot instead. Snapshots are encoded once per
//! revision and shared by every client that needs one.
//!
//! Input echo is the exception to the cadence: after a lease holder's input the thread is woken,
//! the next change is copied at once, and for a short window after it changes are copied at most
//! `echo_floor` apart while the thread looks every `echo_poll`.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::thread::Thread;
use std::time::{Duration, Instant};

use atomics_core::screen::{HistoryChunk, RowsEncoder, Screen};
use atomics_core::wire::{self, Envelope, ErrorCode, Message, Seq, WireError};

use super::hub::Outbox;
use super::lease::{TerminalControl, micros};
use super::session::Departed;
use crate::terminal::screen::{HistoryLines, Tracker};

/// A terminal the protocol thread can stream. Implemented over the native emulator; tests use the
/// same implementation without a PTY.
pub trait ScreenSource: Send + Sync {
    /// Moves whenever the screen may have changed. Cheap and lock-free.
    fn changes(&self) -> u64;
    /// Copy the live screen under the emulator's short lock.
    fn capture(&self, tracker: &mut Tracker, generations: &AtomicU64) -> Screen;
    /// Copy retained history lines under the emulator's short lock.
    fn history(&self, tracker: &mut Tracker, generations: &AtomicU64, before: u64, count: u32) -> HistoryLines;
}

#[derive(Clone)]
pub struct TerminalEntry {
    pub session: String,
    /// Distinguishes a terminal from its replacement under the same session.
    pub identity: usize,
    pub source: Arc<dyn ScreenSource>,
    /// Its lease, and the PTY input and size it guards.
    pub control: Arc<dyn TerminalControl>,
}

/// The terminals that exist, by session. The runtime thread publishes; the protocol thread reads.
/// Created only when the protocol server runs.
#[derive(Default)]
pub struct Terminals {
    slot: RwLock<Arc<Vec<TerminalEntry>>>,
    /// Sessions whose terminals closed lately.
    closed: Departed,
}

impl Terminals {
    pub fn load(&self) -> Arc<Vec<TerminalEntry>> {
        self.slot.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether the published list names exactly these `(session, identity)` pairs, in order.
    /// Allocates nothing, so the runtime thread can ask on every pass.
    pub fn matches<'a>(&self, current: impl Iterator<Item = (&'a str, usize)>) -> bool {
        let list = self.slot.read().unwrap_or_else(|e| e.into_inner());
        let mut current = current;
        list.iter().all(|e| current.next().is_some_and(|(s, i)| e.session == s && e.identity == i))
            && current.next().is_none()
    }

    pub fn publish(&self, entries: Vec<TerminalEntry>) {
        let before = std::mem::replace(&mut *self.slot.write().unwrap_or_else(|e| e.into_inner()), Arc::new(entries));
        let now = self.load();
        self.closed.record(before.iter().map(|e| e.session.as_str()), |s| now.iter().any(|e| e.session == s));
    }

    /// Whether `session` had a terminal that closed lately, as opposed to never having had one.
    pub fn closed(&self, session: &str) -> bool {
        self.closed.contains(session)
    }

    fn find(&self, session: &str) -> Option<TerminalEntry> {
        self.load().iter().find(|e| e.session == session).cloned()
    }

    /// The terminal a session has now, as its identity and control.
    pub fn control(&self, session: &str) -> Option<(usize, Arc<dyn TerminalControl>)> {
        let list = self.slot.read().unwrap_or_else(|e| e.into_inner());
        list.iter().find(|e| e.session == session).map(|e| (e.identity, e.control.clone()))
    }
}

#[derive(Debug, Clone)]
pub struct ScreenConfig {
    pub focused: Duration,
    pub visible: Duration,
    /// Deltas one client's slot holds before they are replaced by a snapshot.
    pub slot_frames: usize,
    /// Bytes of deltas one client may have waiting across all its screens.
    pub slot_bytes: usize,
    /// After input, how long changes are copied soon instead of at the cadence.
    pub echo_window: Duration,
    /// The least time between two copies in that window (the first change after input is copied
    /// at once).
    pub echo_floor: Duration,
    /// How often the screens thread looks for changes during that window.
    pub echo_poll: Duration,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            // About 60 frames a second. Only a screen that changed is copied, and a client that falls
            // behind gets a snapshot in place of its backlog (slot_frames, slot_bytes).
            focused: Duration::from_millis(16),
            visible: Duration::from_millis(16),
            slot_frames: 32,
            slot_bytes: 2 << 20,
            echo_window: Duration::from_millis(200),
            echo_floor: Duration::from_millis(5),
            echo_poll: Duration::from_micros(250),
        }
    }
}

/// One revision of one screen, published for clients that need a snapshot. Encoded on first use.
pub struct Current {
    pub generation: u64,
    pub revision: u64,
    epoch: String,
    session: String,
    screen: Arc<Screen>,
    frames: OnceLock<Option<Arc<[Arc<str>]>>>,
}

impl Current {
    /// The snapshot frames, or `None` if the screen cannot be encoded within the frame limit.
    pub fn frames(&self) -> Option<Arc<[Arc<str>]>> {
        self.frames
            .get_or_init(|| {
                let encode = |part| wire::encode(&Envelope::host(&self.epoch, Message::ScreenSnapshot(part))).ok();
                let frames = self.screen.snapshot(&self.session, self.generation, self.revision, |part| {
                    encode(part).map(Arc::<str>::from)
                })?;
                Some(frames.into())
            })
            .clone()
    }
}

impl Current {
    /// The `oversize` error a client gets instead of a snapshot that does not fit in frames.
    pub fn oversize(&self) -> Arc<str> {
        let message = format!(
            "the screen of session {} does not fit in a frame (generation {}, revision {}); it is sent again once it does",
            self.session, self.generation, self.revision
        );
        let envelope = WireError::new(ErrorCode::Oversize, message).into_envelope(None, Some(self.epoch.clone()));
        wire::encode(&envelope).expect("an error frame is small").into()
    }
}

/// Where a client's slot finds the current snapshot of its screen.
pub type Feed = Arc<RwLock<Option<Arc<Current>>>>;

#[derive(Default)]
struct StreamState {
    tracker: Tracker,
    last: Option<Arc<Screen>>,
    generation: u64,
    revision: u64,
    seen: Option<u64>,
    captured: Option<Instant>,
    /// The input (`Lease::input_at`) whose echo was last copied at once.
    echoed: u64,
}

struct Stream {
    identity: usize,
    source: Arc<dyn ScreenSource>,
    control: Arc<dyn TerminalControl>,
    feed: Feed,
    state: Mutex<StreamState>,
}

impl Stream {
    fn new(entry: &TerminalEntry) -> Arc<Self> {
        Arc::new(Self {
            identity: entry.identity,
            source: entry.source.clone(),
            control: entry.control.clone(),
            feed: Feed::default(),
            state: Mutex::new(StreamState::default()),
        })
    }

    fn state(&self) -> MutexGuard<'_, StreamState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
struct Subscriber {
    outbox: Arc<Outbox>,
    focused: bool,
}

#[derive(Default)]
struct Watch {
    subscribers: Vec<Subscriber>,
    /// Absent while the session has no terminal; subscriptions wait for one.
    stream: Option<Arc<Stream>>,
}

impl Watch {
    /// Point every subscriber at a new stream (a new terminal): each gets its first snapshot.
    fn start(&mut self, session: &str, entry: &TerminalEntry) -> Arc<Stream> {
        let stream = Stream::new(entry);
        for sub in &self.subscribers {
            sub.outbox.watch_screen(session, &stream.feed);
        }
        self.stream = Some(stream.clone());
        stream
    }
}

pub struct Screens {
    terminals: Arc<Terminals>,
    generations: AtomicU64,
    config: ScreenConfig,
    epoch: String,
    watches: Mutex<HashMap<String, Watch>>,
    /// The thread that calls `poll`, woken when input arrives.
    thread: OnceLock<Thread>,
}

enum Outcome {
    Snapshot,
    Delta { generation: u64, base: u64, revision: u64, frame: Option<Arc<str>> },
}

impl Screens {
    pub fn new(terminals: Arc<Terminals>, epoch: String, config: ScreenConfig) -> Self {
        Self {
            terminals,
            generations: AtomicU64::new(0),
            config,
            epoch,
            watches: Mutex::new(HashMap::new()),
            thread: OnceLock::new(),
        }
    }

    pub fn terminals(&self) -> &Terminals {
        &self.terminals
    }

    /// Name the thread that polls, so input can wake it.
    pub fn set_thread(&self, thread: Thread) {
        let _ = self.thread.set(thread);
    }

    /// Input was just written: look for its echo now rather than at the next poll.
    pub fn wake(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }

    fn watches(&self) -> MutexGuard<'_, HashMap<String, Watch>> {
        self.watches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start sending `session`'s screen to `outbox` (a snapshot first), or update `focused`.
    pub fn subscribe(&self, session: &str, outbox: &Arc<Outbox>, focused: bool) -> Result<(), WireError> {
        let entry = self
            .terminals
            .find(session)
            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "that session has no terminal"))?;
        let mut watches = self.watches();
        let watch = watches.entry(session.to_string()).or_default();
        if let Some(sub) = watch.subscribers.iter_mut().find(|s| Arc::ptr_eq(&s.outbox, outbox)) {
            sub.focused = focused;
            return Ok(());
        }
        let stream = match &watch.stream {
            Some(stream) if stream.identity == entry.identity => stream.clone(),
            _ => watch.start(session, &entry),
        };
        outbox.watch_screen(session, &stream.feed);
        watch.subscribers.push(Subscriber { outbox: outbox.clone(), focused });
        Ok(())
    }

    pub fn unsubscribe(&self, session: &str, outbox: &Arc<Outbox>) {
        outbox.unwatch_screen(session);
        let mut watches = self.watches();
        if let Some(watch) = watches.get_mut(session) {
            watch.subscribers.retain(|s| !Arc::ptr_eq(&s.outbox, outbox));
            if watch.subscribers.is_empty() {
                watches.remove(session);
            }
        }
    }

    /// The client lost track of `session`'s screen: its next frames are the current snapshot.
    pub fn resync(&self, session: &str, outbox: &Arc<Outbox>) -> Result<(), WireError> {
        if !outbox.want_snapshot(session) {
            return Err(WireError::new(ErrorCode::NotFound, "not subscribed to that screen"));
        }
        Ok(())
    }

    /// Retained history of a subscribed screen, as one `history_chunk` that fits in a frame.
    pub fn history(&self, session: &str, generation: u64, before: u64, count: u32) -> Result<HistoryChunk, WireError> {
        let stream = self
            .watches()
            .get(session)
            .and_then(|w| w.stream.clone())
            .ok_or_else(|| WireError::new(ErrorCode::NotFound, "subscribe to that screen first"))?;
        let lines = {
            let mut state = stream.state();
            if state.tracker.generation != generation || state.generation != generation {
                return Err(stale(state.generation));
            }
            let lines = stream.source.history(&mut state.tracker, &self.generations, before, count);
            if lines.generation != generation {
                // The terminal moved on in a way lines cannot follow; its snapshot is on the way.
                return Err(stale(lines.generation));
            }
            lines
        };
        // Newest lines first: halve what is sent until the chunk fits in one frame.
        let mut keep = lines.lines.len();
        loop {
            let skip = lines.lines.len() - keep;
            let mut encoder = RowsEncoder::default();
            for (i, line) in lines.lines[skip..].iter().enumerate() {
                encoder.push(i as u16, line);
            }
            let (styles, wire_lines) = encoder.finish();
            let chunk = HistoryChunk {
                session: session.to_string(),
                generation: Seq(generation),
                cols: lines.cols,
                first: Seq(lines.first + skip as u64),
                oldest: Seq(lines.oldest),
                evicted: lines.evicted && skip == 0,
                styles,
                lines: wire_lines,
            };
            let probe = Envelope::host(&self.epoch, Message::HistoryChunk(chunk));
            match wire::encode(&probe) {
                Ok(_) => {
                    let Message::HistoryChunk(chunk) = probe.message else { unreachable!() };
                    return Ok(chunk);
                }
                Err(e) if keep <= 1 => return Err(e),
                Err(_) => keep /= 2,
            }
        }
    }

    /// One pass of the protocol thread: follow terminal replacements, drop closed clients, and copy
    /// every subscribed screen that changed and is due. True while some screen is in its echo
    /// window, when the caller should look again after `echo_poll` rather than its usual interval.
    /// A pass that finds nothing due allocates nothing, however often it runs.
    pub fn poll(&self) -> bool {
        let terminals = self.terminals.load();
        let mut echoing = false;
        let work: Vec<(String, Arc<Stream>, Vec<Subscriber>)> = {
            let mut watches = self.watches();
            watches.retain(|session, watch| {
                watch.subscribers.retain(|s| !s.outbox.is_closed());
                if watch.subscribers.is_empty() {
                    return false;
                }
                match terminals.iter().find(|e| e.session == *session) {
                    None => watch.stream = None,
                    Some(entry) if watch.stream.as_ref().is_none_or(|s| s.identity != entry.identity) => {
                        watch.start(session, entry);
                    }
                    Some(_) => {}
                }
                true
            });
            let mut due = false;
            for watch in watches.values() {
                let Some(stream) = &watch.stream else { continue };
                let look = self.look(stream, &stream.state(), watch.subscribers.iter().any(|s| s.focused));
                echoing |= look.echoing;
                due |= look.due;
            }
            if !due {
                return echoing;
            }
            watches
                .iter()
                .filter_map(|(session, w)| Some((session.clone(), w.stream.clone()?, w.subscribers.clone())))
                .collect()
        };
        for (session, stream, subscribers) in work {
            echoing |= self.pump(&session, &stream, &subscribers);
        }
        echoing
    }

    /// Whether `stream` is in its echo window, and whether it is due for a copy: new, or changed
    /// and its cadence has passed (none for the first change after input).
    fn look(&self, stream: &Stream, state: &StreamState, focused: bool) -> Look {
        let input = stream.control.lease().input_at();
        let echoing =
            input != 0 && micros(Instant::now()).saturating_sub(input) < self.config.echo_window.as_micros() as u64;
        let cadence = if input > state.echoed {
            Duration::ZERO // the first change after input: its echo
        } else if echoing {
            self.config.echo_floor
        } else if focused {
            self.config.focused
        } else {
            self.config.visible
        };
        let fresh = state.last.is_none() || state.tracker.generation != state.generation;
        let changed = state.seen != Some(stream.source.changes());
        let due = fresh || (changed && state.captured.is_none_or(|at| at.elapsed() >= cadence));
        Look { input, echoing, due }
    }

    /// Copy one screen if it changed and is due. True while it is in its echo window.
    fn pump(&self, session: &str, stream: &Stream, subscribers: &[Subscriber]) -> bool {
        let (outcome, current, echoing) = {
            let mut state = stream.state();
            let Look { input, echoing, due } = self.look(stream, &state, subscribers.iter().any(|s| s.focused));
            if !due {
                return echoing;
            }
            // Read before copying: a change racing the copy moves the counter again and is seen next time.
            let changes = stream.source.changes();
            let screen = stream.source.capture(&mut state.tracker, &self.generations);
            state.seen = Some(changes);
            state.captured = Some(Instant::now());
            state.echoed = state.echoed.max(input);
            let same_generation = state.last.is_some() && state.tracker.generation == state.generation;
            let outcome = if !same_generation {
                state.generation = state.tracker.generation;
                state.revision = 1;
                Outcome::Snapshot
            } else {
                let last = state.last.as_ref().expect("same generation has a last screen");
                let (generation, base) = (state.generation, state.revision);
                let Some(delta) = last.delta(&screen, session, generation, base, base + 1) else {
                    return echoing; // nothing a client would draw differently
                };
                state.revision += 1;
                let frame = wire::encode(&Envelope::host(&self.epoch, Message::ScreenDelta(delta))).ok().map(Arc::from);
                Outcome::Delta { generation, base, revision: base + 1, frame }
            };
            let screen = Arc::new(screen);
            state.last = Some(screen.clone());
            let current = Arc::new(Current {
                generation: state.generation,
                revision: state.revision,
                epoch: self.epoch.clone(),
                session: session.to_string(),
                screen,
                frames: OnceLock::new(),
            });
            (outcome, current, echoing)
        };
        // Publish before queueing, so a client that loads this snapshot skips the delta that led to it.
        *stream.feed.write().unwrap_or_else(|e| e.into_inner()) = Some(current);
        for sub in subscribers {
            match &outcome {
                Outcome::Delta { generation, base, revision, frame: Some(frame) } => sub.outbox.push_screen(
                    session,
                    (*generation, *base, *revision),
                    frame,
                    self.config.slot_frames,
                    self.config.slot_bytes,
                ),
                // A new generation, or a delta too large for one frame: everyone takes the snapshot.
                Outcome::Snapshot | Outcome::Delta { frame: None, .. } => {
                    sub.outbox.want_snapshot(session);
                }
            }
        }
        echoing
    }
}

/// What one look at a stream found.
struct Look {
    /// The last input admitted (`Lease::input_at`).
    input: u64,
    echoing: bool,
    due: bool,
}

fn stale(generation: u64) -> WireError {
    WireError::new(ErrorCode::StaleRevision, format!("the screen is at generation {generation}"))
}
