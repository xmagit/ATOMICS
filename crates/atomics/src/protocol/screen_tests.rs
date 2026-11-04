//! Conformance tests for terminal screens over `atomics.v1`: real loopback sockets, the real
//! server and real alacritty emulators (without a PTY), checked against `ScreenMirror`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_core::projection::Projections;
use atomics_core::screen::{Screen, ScreenGap, ScreenMirror};
use atomics_core::wire::{self, Envelope, ErrorCode, Message, Seq, Topic, VERSION};

use super::hub::{Outbox, Taken};
use super::screens::{ScreenConfig, Screens};
use super::tests::{TestClient, wait};
use super::{ScreenSource, Server, ServerConfig, SessionConfig, TerminalEntry, Terminals};
use crate::terminal::Detached;
use crate::terminal::screen::Tracker;

fn config() -> ServerConfig {
    ServerConfig {
        poll_interval: Duration::from_millis(5),
        session: SessionConfig { tick: Duration::from_millis(5), ..SessionConfig::default() },
        screens: ScreenConfig {
            focused: Duration::from_millis(10),
            visible: Duration::from_millis(20),
            ..ScreenConfig::default()
        },
        ..ServerConfig::default()
    }
}

pub(super) fn entry(session: &str, terminal: &Detached) -> TerminalEntry {
    let handle = Arc::new(terminal.handle.clone());
    TerminalEntry {
        session: session.into(),
        identity: terminal.handle.identity(),
        source: handle.clone(),
        control: handle,
    }
}

struct Fixture {
    server: Server,
    terminals: Arc<Terminals>,
    projections: Arc<Projections>,
}

fn start_with(config: ServerConfig, published: &[(&str, &Detached)]) -> Fixture {
    let terminals = Arc::new(Terminals::default());
    terminals.publish(published.iter().map(|(s, t)| entry(s, t)).collect());
    let projections = Arc::new(Projections::default());
    let server =
        Server::start(config, projections.clone(), Arc::new(NoDiffs), terminals.clone(), Arc::new(super::NoCommands))
            .unwrap();
    Fixture { server, terminals, projections }
}

struct NoDiffs;

impl super::DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, atomics_core::diff::FileDiff)> {
        None
    }
}

/// What the emulator shows now, by an independent copy (its own line numbering).
fn truth(terminal: &Detached) -> Screen {
    terminal.handle.capture(&mut Tracker::default(), &AtomicU64::new(0))
}

/// The same cells, cursor, modes, title and palette; line IDs are compared separately.
fn same_view(a: &Screen, b: &Screen) -> bool {
    (a.cols, a.rows, &a.lines, a.cursor, a.modes, &a.title, &a.palette)
        == (b.cols, b.rows, &b.lines, b.cursor, b.modes, &b.title, &b.palette)
}

/// A client that keeps a mirror of every screen it follows.
struct Viewer {
    client: TestClient,
    screens: HashMap<String, ScreenMirror>,
    snapshots: usize,
    deltas: usize,
    /// When set, the next delta is read and thrown away, as if lost.
    drop_next_delta: bool,
    gaps: Vec<ScreenGap>,
    /// Screens whose snapshot was asked for; gaps before it arrives are expected.
    awaiting: std::collections::HashSet<String>,
    errors: Vec<(ErrorCode, Option<String>)>,
    /// The text of each error, in the same order.
    messages: Vec<String>,
    chunks: Vec<atomics_core::screen::HistoryChunk>,
}

impl Viewer {
    fn connect(f: &Fixture) -> Self {
        Self::with_capabilities(f, wire::CAPABILITIES.iter().map(|c| c.to_string()).collect())
    }

    fn with_capabilities(f: &Fixture, capabilities: Vec<String>) -> Self {
        let mut client = TestClient::connect(f.server.addr(), f.server.token());
        client.send(&Envelope::new(Message::Hello {
            client_id: "screens".into(),
            versions: vec![VERSION],
            capabilities,
            prior_epoch: None,
            prior_cursor: None,
        }));
        let mut viewer = Self {
            client,
            screens: HashMap::new(),
            snapshots: 0,
            deltas: 0,
            drop_next_delta: false,
            gaps: Vec::new(),
            awaiting: Default::default(),
            errors: Vec::new(),
            messages: Vec::new(),
            chunks: Vec::new(),
        };
        viewer.until(|v| v.client.mirror.epoch.is_some());
        viewer
    }

    fn subscribe(&mut self, session: &str, focused: bool) {
        let topics = vec![Topic::Screen { session: session.into(), focused }];
        self.client.send(&Envelope::new(Message::Subscribe { topics }));
    }

    fn screen(&self, session: &str) -> Option<&Screen> {
        self.screens.get(session)?.screen.as_ref()
    }

    /// Read one frame and apply it. A gap asks for that screen's snapshot, as a client must.
    fn step(&mut self) {
        let envelope = self.client.step().expect("the connection ended early");
        match envelope.message {
            Message::ScreenSnapshot(s) => {
                self.snapshots += 1;
                self.awaiting.remove(&s.session);
                self.screens.entry(s.session.clone()).or_default().apply_snapshot(&s).expect("a valid snapshot");
            }
            Message::ScreenDelta(d) => {
                self.deltas += 1;
                if std::mem::take(&mut self.drop_next_delta) {
                    return;
                }
                if let Err(gap) = self.screens.entry(d.session.clone()).or_default().apply_delta(&d)
                    && self.awaiting.insert(d.session.clone())
                {
                    self.gaps.push(gap);
                    let resync = Message::Resync { reason: Some("gap".into()), screens: vec![d.session.clone()] };
                    self.client.send(&Envelope::new(resync));
                }
            }
            Message::HistoryChunk(chunk) => self.chunks.push(chunk),
            Message::Error { code, message } => {
                self.errors.push((code, envelope.request_id));
                self.messages.push(message);
            }
            _ => {}
        }
    }

    fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while !done(self) {
            assert!(Instant::now() < deadline, "the viewer did not catch up");
            self.step();
        }
    }

    fn until_shows(&mut self, session: &str, terminal: &Detached) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        loop {
            let want = truth(terminal);
            if self.screen(session).is_some_and(|s| same_view(s, &want)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the screen did not converge:\n{:?}\nvs\n{}",
                self.screen(session).map(Screen::text),
                want.text()
            );
            self.step();
        }
    }

    /// Frames for a while, or until the host goes quiet.
    fn drain_for(&mut self, period: Duration) {
        let _ = self.client.ws.get_ref().set_read_timeout(Some(Duration::from_millis(20)));
        let until = Instant::now() + period;
        while Instant::now() < until {
            match self.client.ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let envelope = wire::decode(t.as_str()).unwrap();
                    match envelope.message {
                        Message::ScreenSnapshot(_) => self.snapshots += 1,
                        Message::ScreenDelta(_) => self.deltas += 1,
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if crate::test_support::nothing_yet(&e) => {}
                Err(e) => panic!("the connection ended: {e}"),
            }
        }
        let _ = self.client.ws.get_ref().set_read_timeout(Some(wait(Duration::from_secs(5))));
    }
}

#[test]
fn a_subscribed_screen_gets_a_snapshot_then_deltas_that_track_the_emulator() {
    let terminal = Detached::new(40, 8, 500);
    terminal.feed(b"$ ls\r\nREADME.md  src\r\n$ ");
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);
    assert_eq!((viewer.snapshots, viewer.deltas), (1, 0));
    assert_eq!(viewer.screen("s1").unwrap().lines[1].text(), "README.md  src");

    for i in 0..30 {
        terminal.feed(format!("cargo build step {i}\r\n").as_bytes());
        std::thread::sleep(Duration::from_millis(3));
    }
    viewer.until_shows("s1", &terminal);
    assert!(viewer.deltas > 0, "later changes travel as deltas");
    assert_eq!(viewer.snapshots, 1, "and no further snapshot is needed");
    assert!(viewer.gaps.is_empty());
    // Scrolled lines moved into history with stable IDs: the mirror's top line is the host's.
    let host = viewer.screen("s1").unwrap();
    assert_eq!(host.first_line, host.history);
    assert!(host.history > 0);
}

#[test]
fn wide_combining_coloured_cells_cursor_and_modes_arrive_exactly() {
    let terminal = Detached::new(30, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", false);
    viewer.until(|v| v.screen("s1").is_some());
    terminal
        .feed("\x1b]0;editor\x07中文 e\u{301}\u{302} \x1b[1;3;4;38;5;196;48;2;10;20;30mstyled\x1b[0m\r\n".as_bytes());
    terminal.feed(b"\x1b[7mrev\x1b[0m \x1b[9mstrike\x1b[0m \x1b[2mdim\x1b[0m\x1b[?2004h\x1b[?1h\x1b[4 q");
    viewer.until_shows("s1", &terminal);
    let s = viewer.screen("s1").unwrap();
    assert_eq!(s.lines[0].text(), "中文 e\u{301}\u{302} styled");
    assert_eq!((s.lines[0].cells[0].width, s.lines[0].cells[1].width), (2, 0));
    assert_eq!(s.title.as_deref(), Some("editor"));
    assert!(s.modes.bracketed_paste && s.modes.app_cursor);
    assert_eq!(s.cursor.shape, atomics_core::screen::CursorShape::Underline);
}

#[test]
fn resize_and_the_alternate_screen_start_a_new_generation_with_a_snapshot() {
    let terminal = Detached::new(20, 5, 100);
    terminal.feed(b"main screen\r\n");
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);
    let first = viewer.screens["s1"].generation;

    terminal.resize(32, 7);
    viewer.until_shows("s1", &terminal);
    let resized = viewer.screens["s1"].generation;
    assert!(resized > first);
    assert_eq!((viewer.screen("s1").unwrap().cols, viewer.screen("s1").unwrap().rows), (32, 7));

    terminal.feed(b"\x1b[?1049h\x1b[Hfull screen app");
    viewer.until_shows("s1", &terminal);
    assert!(viewer.screen("s1").unwrap().modes.alt_screen);
    assert!(viewer.screens["s1"].generation > resized);
    terminal.feed(b"\x1b[?1049l");
    viewer.until_shows("s1", &terminal);
    assert_eq!(viewer.snapshots, 4, "one snapshot per generation");
    assert!(viewer.gaps.is_empty(), "a new generation never looks like a gap");
}

#[test]
fn a_dropped_delta_is_detected_and_answered_with_a_snapshot() {
    let terminal = Detached::new(30, 12, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);

    viewer.drop_next_delta = true;
    terminal.feed(b"lost line\r\n");
    viewer.until(|v| !v.drop_next_delta);
    for i in 0..5 {
        terminal.feed(format!("after {i}\r\n").as_bytes());
        std::thread::sleep(Duration::from_millis(25));
    }
    viewer.until_shows("s1", &terminal);
    assert!(matches!(viewer.gaps.as_slice(), [ScreenGap::Missing { .. }, ..]), "{:?}", viewer.gaps);
    assert_eq!(viewer.snapshots, 2, "the gap was repaired by one snapshot");
    assert!(viewer.screen("s1").unwrap().text().contains("lost line"), "the lost change is in the snapshot");
}

/// The per-client slot: a client whose deltas pile up gets one current snapshot instead.
#[test]
fn a_backlog_of_deltas_is_replaced_by_one_current_snapshot() {
    let terminal = Detached::new(30, 6, 100);
    let terminals = Arc::new(Terminals::default());
    terminals.publish(vec![entry("s1", &terminal)]);
    let screens = Screens::new(
        terminals,
        "e".into(),
        ScreenConfig {
            focused: Duration::ZERO,
            visible: Duration::ZERO,
            slot_frames: 4,
            slot_bytes: 1 << 20,
            ..ScreenConfig::default()
        },
    );
    let outbox = Arc::new(Outbox::new(64, 1 << 20));
    screens.subscribe("s1", &outbox, true).unwrap();
    screens.poll();
    let mut mirror = ScreenMirror::default();
    let apply = |frames: Vec<Arc<str>>, mirror: &mut ScreenMirror| -> Vec<&'static str> {
        frames
            .iter()
            .map(|f| match wire::decode(f).unwrap().message {
                Message::ScreenSnapshot(s) => {
                    mirror.apply_snapshot(&s).unwrap();
                    "snapshot"
                }
                Message::ScreenDelta(d) => {
                    mirror.apply_delta(&d).unwrap();
                    "delta"
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    };
    let Taken::Frames { screens: frames, .. } = outbox.take() else { panic!() };
    assert_eq!(apply(frames, &mut mirror), ["snapshot"]);

    // A reader keeping up gets deltas.
    terminal.feed(b"one\r\n");
    screens.poll();
    let Taken::Frames { screens: frames, .. } = outbox.take() else { panic!() };
    assert_eq!(apply(frames, &mut mirror), ["delta"]);

    // One that does not keep up: more deltas than its slot holds.
    for i in 0..10 {
        terminal.feed(format!("busy {i}\r\n").as_bytes());
        screens.poll();
    }
    let Taken::Frames { screens: frames, .. } = outbox.take() else { panic!() };
    assert_eq!(apply(frames, &mut mirror), ["snapshot"], "superseded deltas are discarded");
    assert!(same_view(mirror.screen.as_ref().unwrap(), &truth(&terminal)));

    // And it follows with deltas again from that snapshot.
    terminal.feed(b"calm\r\n");
    screens.poll();
    let Taken::Frames { screens: frames, .. } = outbox.take() else { panic!() };
    assert_eq!(apply(frames, &mut mirror), ["delta"]);
    assert!(same_view(mirror.screen.as_ref().unwrap(), &truth(&terminal)));
}

#[test]
fn a_persistently_slow_client_is_disconnected_without_slowing_the_emulator_or_others() {
    let terminal = Arc::new(Detached::new(120, 40, 1000));
    let mut cfg = config();
    cfg.write_timeout = Duration::from_millis(300);
    let f = start_with(cfg, &[("s1", &terminal)]);
    let mut slow = Viewer::connect(&f);
    slow.subscribe("s1", true);
    slow.until(|v| v.screen("s1").is_some());
    let mut good = Viewer::connect(&f);
    good.subscribe("s1", true);
    good.until(|v| v.screen("s1").is_some());
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    // The healthy client reads on its own thread throughout.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, terminal) = (stop.clone(), terminal.clone());
        std::thread::spawn(move || {
            let until = Instant::now() + wait(Duration::from_secs(60));
            while Instant::now() < until {
                let _ = good.client.ws.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
                match good.client.ws.read() {
                    Ok(tungstenite::Message::Text(t)) => match wire::decode(t.as_str()).unwrap().message {
                        Message::ScreenSnapshot(s) => {
                            good.screens.entry(s.session.clone()).or_default().apply_snapshot(&s).unwrap();
                        }
                        Message::ScreenDelta(d) => {
                            good.screens.entry(d.session.clone()).or_default().apply_delta(&d).unwrap();
                        }
                        _ => {}
                    },
                    Ok(_) => {}
                    Err(_) => {}
                }
                if stop.load(Ordering::Acquire) && good.screen("s1").is_some_and(|s| same_view(s, &truth(&terminal))) {
                    return true;
                }
            }
            false
        })
    };

    // `slow` never reads again; the emulator (standing in for the PTY thread) keeps writing.
    let line = format!("\x1b[3{}m{}\x1b[0m\r\n", 1, "x".repeat(110));
    let (mut worst, mut writes) = (Duration::ZERO, 0);
    let deadline = Instant::now() + wait(Duration::from_secs(40));
    while f.server.active_clients() > 1 {
        assert!(Instant::now() < deadline, "the stalled client was never dropped");
        let started = Instant::now();
        terminal.feed(line.replace('1', &((writes % 7) + 1).to_string()).as_bytes());
        worst = worst.max(started.elapsed());
        writes += 1;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(worst < wait(Duration::from_millis(250)), "the emulator waited {worst:?} for a slow client");
    assert_eq!(f.server.active_clients(), 1, "only the stalled client is dropped");
    stop.store(true, Ordering::Release);
    assert!(reader.join().unwrap(), "the healthy client converged");
    drop(slow);
}

#[test]
fn history_returns_retained_lines_and_says_when_they_were_evicted() {
    let terminal = Detached::new(20, 4, 10);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);

    // History of a screen not subscribed to is not served.
    viewer.client.send(
        &Envelope::new(Message::HistoryGet(atomics_core::screen::HistoryGet {
            session: "s1".into(),
            generation: Seq(1),
            before: Seq(0),
            count: 5,
        }))
        .reply_to(Some("h0".into())),
    );
    viewer.until(|v| !v.errors.is_empty());
    assert_eq!(viewer.errors.pop(), Some((ErrorCode::NotFound, Some("h0".into()))));

    viewer.subscribe("s1", true);
    viewer.until(|v| v.screen("s1").is_some());
    for i in 0..30 {
        terminal.feed(format!("line {i}\r\n").as_bytes());
        std::thread::sleep(Duration::from_millis(15));
    }
    viewer.until_shows("s1", &terminal);
    let (generation, top) = {
        let s = viewer.screen("s1").unwrap();
        (viewer.screens["s1"].generation, s.first_line)
    };
    let ask = |viewer: &mut Viewer, id: &str, generation: u64, count: u32| {
        let request = atomics_core::screen::HistoryGet {
            session: "s1".into(),
            generation: Seq(generation),
            before: Seq(top),
            count,
        };
        viewer.client.send(&Envelope::new(Message::HistoryGet(request)).reply_to(Some(id.into())));
    };

    ask(&mut viewer, "h1", generation, 4);
    viewer.until(|v| !v.chunks.is_empty());
    let chunk = viewer.chunks.pop().unwrap();
    let lines: Vec<String> = chunk.decode().unwrap().iter().map(|l| l.text()).collect();
    assert_eq!(lines, ["line 23", "line 24", "line 25", "line 26"]);
    assert_eq!((chunk.first, chunk.evicted), (Seq(top - 4), false));

    ask(&mut viewer, "h2", generation, 500);
    viewer.until(|v| !v.chunks.is_empty());
    let chunk = viewer.chunks.pop().unwrap();
    assert!(chunk.evicted, "asking past the oldest retained line says it is gone");
    assert_eq!(chunk.lines.len(), 10, "everything retained is sent");
    assert_eq!(chunk.first, chunk.oldest);

    ask(&mut viewer, "h3", generation + 7, 4);
    viewer.until(|v| !v.errors.is_empty());
    assert_eq!(viewer.errors.pop(), Some((ErrorCode::StaleRevision, Some("h3".into()))));
}

#[test]
fn screens_need_the_terminals_capability_and_an_existing_terminal() {
    let terminal = Detached::new(20, 4, 10);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut without = Viewer::with_capabilities(&f, vec!["board".into()]);
    without.subscribe("s1", true);
    without.until(|v| !v.errors.is_empty());
    assert_eq!(without.errors[0].0, ErrorCode::Unauthorised);

    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("nope", true);
    viewer.until(|v| !v.errors.is_empty());
    assert_eq!(viewer.errors[0].0, ErrorCode::NotFound);
    let resync = Message::Resync { reason: None, screens: vec!["s1".into()] };
    viewer.client.send(&Envelope::new(resync));
    viewer.until(|v| v.errors.len() == 2);
    assert_eq!(viewer.errors[1].0, ErrorCode::NotFound, "resync of a screen not followed is refused");
}

#[test]
fn idle_and_unsubscribed_screens_send_nothing() {
    let terminal = Detached::new(20, 4, 10);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", false);
    viewer.until_shows("s1", &terminal);
    viewer.drain_for(Duration::from_millis(300));
    let (snapshots, deltas) = (viewer.snapshots, viewer.deltas);
    assert_eq!((snapshots, deltas), (1, 0), "an idle terminal sends no cell traffic");

    viewer.client.send(&Envelope::new(Message::Unsubscribe {
        topics: vec![Topic::Screen { session: "s1".into(), focused: false }],
    }));
    viewer.drain_for(Duration::from_millis(50));
    for i in 0..10 {
        terminal.feed(format!("unseen {i}\r\n").as_bytes());
    }
    viewer.drain_for(Duration::from_millis(300));
    assert_eq!((viewer.snapshots, viewer.deltas), (snapshots, deltas), "an unsubscribed pane gets no screen traffic");
}

#[test]
fn a_replaced_terminal_is_followed_with_a_new_generation() {
    let first = Detached::new(20, 4, 10);
    first.feed(b"first terminal");
    let f = start_with(config(), &[("s1", &first)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &first);
    let generation = viewer.screens["s1"].generation;

    let second = Detached::new(20, 4, 10);
    second.feed(b"second terminal");
    f.terminals.publish(vec![entry("s1", &second)]);
    viewer.until_shows("s1", &second);
    assert!(viewer.screens["s1"].generation > generation);
    assert!(viewer.gaps.is_empty());
}

#[test]
fn two_clients_share_one_copy_and_see_the_same_screen() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut a = Viewer::connect(&f);
    let mut b = Viewer::connect(&f);
    a.subscribe("s1", true);
    b.subscribe("s1", false);
    a.until(|v| v.screen("s1").is_some());
    b.until(|v| v.screen("s1").is_some());
    for i in 0..20 {
        terminal.feed(format!("shared {i}\r\n").as_bytes());
        std::thread::sleep(Duration::from_millis(4));
    }
    a.until_shows("s1", &terminal);
    b.until_shows("s1", &terminal);
    let (sa, sb) = (a.screen("s1").unwrap(), b.screen("s1").unwrap());
    assert_eq!(sa, sb, "line IDs included");
    assert_eq!(a.screens["s1"].generation, b.screens["s1"].generation);
}

/// The emulator lock is taken only to copy: while a test holds it (as a long PTY read or a frame
/// being painted would), board events still reach clients and the copy waits its turn.
#[test]
fn copying_waits_for_the_emulator_lock_without_holding_up_the_board() {
    let terminal = Arc::new(Detached::new(20, 4, 10));
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until(|v| v.screen("s1").is_some());

    let held = Arc::new(Mutex::new(()));
    let gate = held.lock().unwrap();
    let locker = {
        let (terminal, held) = (terminal.clone(), held.clone());
        std::thread::spawn(move || {
            let _emulator = terminal.lock();
            let _wait = held.lock().unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    terminal.touch();
    f.projections.publish_sessions(vec![super::tests::session(
        "a",
        "board",
        atomics_core::session::AgentStatus::Working,
    )]);
    viewer.until(|v| v.client.mirror.sessions.contains_key("a"));
    drop(gate);
    locker.join().unwrap();
    terminal.feed(b"after the lock");
    viewer.until_shows("s1", &terminal);
}

#[test]
fn a_huge_title_is_cut_when_copied_and_the_screen_still_arrives() {
    let terminal = Detached::new(40, 6, 10);
    // A 300 KB OSC 0 title: with it, no snapshot part would fit in a frame.
    terminal.feed(format!("\x1b]0;{}\x07$ ready", "t".repeat(300 * 1024)).as_bytes());
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);
    let screen = viewer.screen("s1").unwrap();
    assert_eq!(screen.lines[0].text(), "$ ready");
    assert_eq!(screen.title.as_deref(), Some("t".repeat(atomics_core::screen::MAX_TITLE_BYTES).as_str()));
    assert!(viewer.errors.is_empty(), "{:?}", viewer.messages);

    // Marks stacked on one cell are cut too.
    terminal.feed(format!("\r\ne{}", "\u{301}".repeat(200)).as_bytes());
    viewer.until_shows("s1", &terminal);
    let cell = &viewer.screen("s1").unwrap().lines[1].cells[0];
    assert_eq!(cell.combining.as_deref().map(<[char]>::len), Some(atomics_core::screen::MAX_COMBINING));
}

/// Copies a terminal's screen and, while `bloated`, gives it a title no frame can carry: a screen
/// too large to send, as a source without the copy's caps would make.
struct Bloated {
    inner: Arc<dyn ScreenSource>,
    bloated: AtomicBool,
}

impl ScreenSource for Bloated {
    fn changes(&self) -> u64 {
        self.inner.changes()
    }

    fn capture(&self, tracker: &mut Tracker, generations: &AtomicU64) -> Screen {
        let mut screen = self.inner.capture(tracker, generations);
        if self.bloated.load(Ordering::Acquire) {
            screen.title = Some("x".repeat(wire::MAX_FRAME_BYTES));
        }
        screen
    }

    fn history(
        &self,
        tracker: &mut Tracker,
        generations: &AtomicU64,
        before: u64,
        count: u32,
    ) -> crate::terminal::screen::HistoryLines {
        self.inner.history(tracker, generations, before, count)
    }
}

#[test]
fn a_screen_too_large_to_send_is_answered_once_with_oversize_and_sent_once_it_fits() {
    let big = Detached::new(40, 6, 10);
    big.feed(b"big screen");
    let other = Detached::new(40, 6, 10);
    other.feed(b"other screen");
    let source = Arc::new(Bloated { inner: Arc::new(big.handle.clone()), bloated: AtomicBool::new(true) });
    let f = start_with(config(), &[("s2", &other)]);
    let bloated = TerminalEntry {
        session: "big".into(),
        identity: big.handle.identity(),
        source: source.clone(),
        control: Arc::new(big.handle.clone()),
    };
    f.terminals.publish(vec![entry("s2", &other), bloated]);
    let mut viewer = Viewer::connect(&f);
    viewer.subscribe("big", true);
    viewer.subscribe("s2", false);
    let oversize = |v: &Viewer| v.errors.iter().filter(|(code, _)| *code == ErrorCode::Oversize).count();
    viewer.until(|v| oversize(v) == 1 && v.screen("s2").is_some());
    assert!(viewer.messages.iter().any(|m| m.contains("session big")), "{:?}", viewer.messages);
    assert!(viewer.screen("big").is_none());

    // It keeps changing while too large: the client was told, and other screens keep streaming.
    for i in 0..5 {
        big.feed(format!("\r\nmore {i}").as_bytes());
        std::thread::sleep(Duration::from_millis(30));
    }
    other.feed(b"\r\nstill streaming");
    viewer.until_shows("s2", &other);
    assert_eq!(oversize(&viewer), 1, "told once, not at every change");

    source.bloated.store(false, Ordering::Release);
    big.feed(b"\r\nfits again");
    viewer.until(|v| v.screen("big").is_some_and(|s| s.text().contains("fits again")));
    assert_eq!(oversize(&viewer), 1);
}

#[test]
fn a_subscribe_list_is_checked_whole_before_anything_is_subscribed() {
    let terminal = Detached::new(20, 4, 10);
    terminal.feed(b"here");
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    let topics = vec![
        Topic::Screen { session: "s1".into(), focused: true },
        Topic::Screen { session: "nope".into(), focused: true },
    ];
    viewer.client.send(&Envelope::new(Message::Subscribe { topics }));
    viewer.until(|v| !v.errors.is_empty());
    assert_eq!(viewer.errors[0].0, ErrorCode::NotFound);
    viewer.drain_for(Duration::from_millis(200));
    assert_eq!(viewer.snapshots, 0, "the screen listed before the refused one was not subscribed");
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);
}

#[test]
fn one_subscription_limit_spans_screens_and_board_topics() {
    let terminal = Detached::new(20, 4, 10);
    terminal.feed(b"here");
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Viewer::connect(&f);
    // Board topics fill all places but one, and the screen takes that one.
    let topic = |i: usize| match i % 2 {
        0 => Topic::Plan { id: format!("p{i}") },
        _ => Topic::Diff { worktree: format!("/w/{i}") },
    };
    let board: Vec<Topic> = (0..wire::MAX_SUBSCRIPTIONS - 1).map(topic).collect();
    viewer.client.send(&Envelope::new(Message::Subscribe { topics: board }));
    viewer.subscribe("s1", true);
    viewer.until_shows("s1", &terminal);
    assert!(viewer.errors.is_empty(), "{:?}", viewer.messages);

    // Full, whatever the kind of the topic asked for; following the screen again takes no place.
    let one_more = || Message::Subscribe { topics: vec![Topic::Plan { id: "one more".into() }] };
    viewer.client.send(&Envelope::new(one_more()));
    viewer.until(|v| !v.errors.is_empty());
    assert_eq!(viewer.errors[0].0, ErrorCode::TooMany);
    viewer.subscribe("s1", false);

    // Leaving the screen frees its place for a board topic.
    let screen = vec![Topic::Screen { session: "s1".into(), focused: false }];
    viewer.client.send(&Envelope::new(Message::Unsubscribe { topics: screen }));
    viewer.client.send(&Envelope::new(one_more()));
    viewer.client.send(&Envelope::new(Message::Ping { nonce: Some("after".into()) }));
    loop {
        match viewer.client.step().expect("connected").message {
            Message::Pong { .. } => break,
            Message::Error { code, message } => panic!("refused {code:?}: {message}"),
            _ => {}
        }
    }
    assert_eq!(viewer.errors.len(), 1, "only the subscription over the limit was refused");
}
