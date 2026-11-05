//! Conformance tests for typing over `atomics.v1`: leases, takeover, fencing, input order, resize
//! and echo. Real loopback sockets and the real server, with emulators that echo what they are
//! sent (`Detached`), checked against what reached the terminal.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomics_core::lease::{GridSize, LeaseReason, LeaseState, MAX_INPUT_BYTES, Typist};
use atomics_core::projection::Projections;
use atomics_core::screen::ScreenMirror;
use atomics_core::wire::{self, CAPABILITIES, Envelope, ErrorCode, Message, Role, Seq, Topic, VERSION};

use super::lease::Policy;
use super::screen_tests::entry;
use super::tests::{TestClient, wait};
use super::{ScreenConfig, Server, ServerConfig, SessionConfig, Terminals};
use crate::terminal::Detached;

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

struct NoDiffs;

impl super::DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, atomics_core::diff::FileDiff)> {
        None
    }
}

struct Fixture {
    server: Server,
    /// Keeps the published terminals alive with the server; tests republish through it.
    terminals: Arc<Terminals>,
}

fn start_with(config: ServerConfig, published: &[(&str, &Detached)]) -> Fixture {
    let terminals = Arc::new(Terminals::default());
    terminals.publish(published.iter().map(|(s, t)| entry(s, t)).collect());
    let projections = Arc::new(Projections::default());
    let server =
        Server::start(config, projections, Arc::new(NoDiffs), terminals.clone(), Arc::new(super::NoCommands)).unwrap();
    Fixture { server, terminals }
}

/// A client that follows screens and types into them through `Typist`, the reference model.
struct Typer {
    client: TestClient,
    screens: HashMap<String, ScreenMirror>,
    typists: HashMap<String, Typist>,
    /// Every `lease_state` received, in order.
    leases: Vec<LeaseState>,
    errors: Vec<(ErrorCode, String, Option<String>)>,
    requests: u32,
}

impl Typer {
    fn connect(f: &Fixture, name: &str) -> Self {
        Self::with_capabilities(f, name, CAPABILITIES.iter().map(|c| c.to_string()).collect())
    }

    fn with_capabilities(f: &Fixture, name: &str, capabilities: Vec<String>) -> Self {
        let mut client = TestClient::connect(f.server.addr(), f.server.token());
        client.ws.get_ref().set_nodelay(true).unwrap();
        client.send(&Envelope::new(Message::Hello {
            client_id: name.into(),
            versions: vec![VERSION],
            capabilities,
            prior_epoch: None,
            prior_cursor: None,
        }));
        let mut typer = Self {
            client,
            screens: HashMap::new(),
            typists: HashMap::new(),
            leases: Vec::new(),
            errors: Vec::new(),
            requests: 0,
        };
        typer.until(|t| t.client.welcome.is_some());
        typer
    }

    fn send(&mut self, message: Message) -> String {
        self.requests += 1;
        let id = format!("r{}", self.requests);
        self.client.send(&Envelope::new(message).reply_to(Some(id.clone())));
        id
    }

    fn subscribe(&mut self, session: &str) {
        self.typists.entry(session.into()).or_insert_with(|| Typist::new(session));
        self.send(Message::Subscribe { topics: vec![Topic::Screen { session: session.into(), focused: true }] });
        self.until(|t| t.screen_text(session).is_some() && t.lease(session).is_some());
    }

    fn typist(&mut self, session: &str) -> &mut Typist {
        self.typists.entry(session.into()).or_insert_with(|| Typist::new(session))
    }

    fn step(&mut self) -> Envelope {
        let envelope = self.client.step().expect("the connection ended early");
        self.apply(&envelope);
        envelope
    }

    fn apply(&mut self, envelope: &Envelope) {
        match &envelope.message {
            Message::ScreenSnapshot(s) => drop(self.screens.entry(s.session.clone()).or_default().apply_snapshot(s)),
            Message::ScreenDelta(d) => {
                if self.screens.entry(d.session.clone()).or_default().apply_delta(d).is_err() {
                    let resync = Message::Resync { reason: Some("gap".into()), screens: vec![d.session.clone()] };
                    self.client.send(&Envelope::new(resync));
                }
            }
            Message::LeaseState(state) => {
                if let Some(typist) = self.typists.get_mut(&state.session) {
                    typist.apply(state);
                }
                self.leases.push(state.clone());
            }
            Message::Error { code, message } => self.errors.push((*code, message.clone(), envelope.request_id.clone())),
            _ => {}
        }
    }

    fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while !done(self) {
            assert!(
                Instant::now() < deadline,
                "the client did not catch up: {:?} {:?}",
                self.leases.last(),
                self.errors
            );
            self.step();
        }
    }

    /// Read whatever arrives for a while.
    fn idle(&mut self, period: Duration) {
        let _ = self.client.ws.get_ref().set_read_timeout(Some(Duration::from_millis(10)));
        let until = Instant::now() + period;
        while Instant::now() < until {
            match self.client.ws.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let envelope = wire::decode(text.as_str()).unwrap();
                    self.client.mirror.apply(&envelope).expect("the board follows");
                    self.apply(&envelope);
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if crate::test_support::nothing_yet(&e) => {}
                Err(e) => panic!("the connection ended: {e}"),
            }
        }
        let _ = self.client.ws.get_ref().set_read_timeout(Some(wait(Duration::from_secs(5))));
    }

    fn screen_text(&self, session: &str) -> Option<String> {
        Some(self.screens.get(session)?.screen.as_ref()?.text())
    }

    fn screen_size(&self, session: &str) -> Option<(u16, u16)> {
        let screen = self.screens.get(session)?.screen.as_ref()?;
        Some((screen.cols, screen.rows))
    }

    /// The latest lease state for `session`.
    fn lease(&self, session: &str) -> Option<&LeaseState> {
        self.leases.iter().rev().find(|l| l.session == session)
    }

    /// Wait for the answer to request `id`: the lease state, or the error code.
    fn answer(&mut self, id: &str) -> Result<LeaseState, ErrorCode> {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        loop {
            assert!(Instant::now() < deadline, "no answer to {id}");
            let envelope = self.step();
            if envelope.request_id.as_deref() != Some(id) {
                continue;
            }
            match envelope.message {
                Message::LeaseState(state) => return Ok(state),
                Message::Error { code, .. } => return Err(code),
                other => panic!("unexpected answer {other:?}"),
            }
        }
    }

    fn acquire(&mut self, session: &str, takeover: bool) -> Result<LeaseState, ErrorCode> {
        let message = self.typist(session).acquire(takeover);
        let id = self.send(message);
        self.answer(&id)
    }

    /// Type through the typist; the ids of the frames sent.
    fn type_text(&mut self, session: &str, text: &str) -> Vec<String> {
        let frames = self.typist(session).input(text);
        assert!(!frames.is_empty(), "the typist holds no lease");
        frames.into_iter().map(|m| self.send(m)).collect()
    }

    /// The error answering request `id`.
    fn refusal(&mut self, id: &str) -> ErrorCode {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        loop {
            if let Some((code, ..)) = self.errors.iter().find(|(_, _, r)| r.as_deref() == Some(id)) {
                return *code;
            }
            assert!(Instant::now() < deadline, "no error for {id}");
            self.step();
        }
    }

    /// Send raw input under a token and sequence number of the test's choosing.
    fn raw_input(&mut self, session: &str, lease: u64, seq: u64, data: &str) -> String {
        self.send(Message::TerminalInput {
            session: session.into(),
            lease: Seq(lease),
            seq: Seq(seq),
            data: data.into(),
        })
    }
}

fn token(state: &LeaseState) -> u64 {
    state.lease.expect("the holder is told its token").0
}

#[test]
fn a_holder_types_in_order_and_its_echo_comes_back() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let first = phone.lease("s1").unwrap().clone();
    assert_eq!((first.holder.as_deref(), first.yours, first.lease), (None, false, None), "nobody holds it at first");

    let granted = phone.acquire("s1", false).unwrap();
    assert!(granted.yours && granted.lease.is_some());
    assert_eq!(
        (granted.holder.as_deref(), granted.next, granted.reason),
        (Some("phone"), Some(Seq(1)), Some(LeaseReason::Granted))
    );
    for piece in ["ec", "ho", " hi", "\r\n"] {
        phone.type_text("s1", piece);
    }
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("echo hi")));
    assert_eq!(terminal.written(), "echo hi\r\n");
    assert!(phone.errors.is_empty(), "{:?}", phone.errors);
}

#[test]
fn input_without_the_lease_or_out_of_order_is_refused_and_never_written() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let id = phone.raw_input("s1", 1, 1, "no lease");
    assert_eq!(phone.refusal(&id), ErrorCode::Fenced);

    let lease = token(&phone.acquire("s1", false).unwrap());
    let id = phone.raw_input("s1", lease + 1000, 1, "guessed");
    assert_eq!(phone.refusal(&id), ErrorCode::Fenced);
    let ok = phone.raw_input("s1", lease, 1, "a");
    let replay = phone.raw_input("s1", lease, 1, "a");
    assert_eq!(phone.refusal(&replay), ErrorCode::OutOfOrder);
    let gap = phone.raw_input("s1", lease, 3, "c");
    assert_eq!(phone.refusal(&gap), ErrorCode::OutOfOrder);
    phone.raw_input("s1", lease, 2, "b");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("ab")));
    assert_eq!(terminal.written(), "ab");
    assert!(phone.errors.iter().all(|(_, _, r)| r.as_deref() != Some(ok.as_str())));

    // Racing clients meet fenced input as a matter of course; it never costs the connection.
    for _ in 0..20 {
        phone.raw_input("s1", lease + 1000, 3, "x");
    }
    let id = phone.raw_input("s1", lease, 3, "c");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("abc")));
    assert!(phone.errors.iter().all(|(_, _, r)| r.as_deref() != Some(id.as_str())));
    assert_eq!(terminal.written(), "abc");
}

#[test]
fn opening_a_screen_never_steals_control_and_a_takeover_is_seen_by_everyone() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut laptop = Typer::connect(&f, "laptop");
    laptop.subscribe("s1");
    let old = token(&laptop.acquire("s1", false).unwrap());
    laptop.type_text("s1", "one ");
    laptop.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("one")));

    // A second client opens the screen: it is told who types, and taking control needs takeover.
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let seen = phone.lease("s1").unwrap().clone();
    assert_eq!((seen.holder.as_deref(), seen.yours, seen.lease), (Some("laptop"), false, None));
    assert_eq!(phone.acquire("s1", false), Err(ErrorCode::LeaseHeld));
    let taken = phone.acquire("s1", true).unwrap();
    assert!(taken.yours && token(&taken) > old, "a takeover gets a newer fencing token");
    assert_eq!(taken.reason, Some(LeaseReason::TakenOver));

    // The old holder is told, and its late input is fenced, not written.
    laptop.until(|t| t.lease("s1").is_some_and(|l| l.holder.as_deref() == Some("phone")));
    let lost = laptop.lease("s1").unwrap().clone();
    assert_eq!((lost.yours, lost.lease, lost.reason), (false, None, Some(LeaseReason::TakenOver)));
    assert!(!laptop.typist("s1").may_type());
    let late = laptop.raw_input("s1", old, 2, "late");
    assert_eq!(laptop.refusal(&late), ErrorCode::Fenced);
    phone.type_text("s1", "two");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("one two")));
    assert_eq!(terminal.written(), "one two");
}

#[test]
fn an_expired_lease_fences_late_input_and_renewing_keeps_it() {
    let terminal = Detached::new(40, 6, 100);
    let mut cfg = config();
    cfg.lease = Policy { ttl: Duration::from_millis(400), ..Policy::default() };
    let f = start_with(cfg, &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    let mut viewer = Typer::connect(&f, "viewer");
    phone.subscribe("s1");
    viewer.subscribe("s1");

    // Renewing (and typing) keeps the lease past its time-to-live.
    let lease = token(&phone.acquire("s1", false).unwrap());
    for _ in 0..4 {
        phone.idle(Duration::from_millis(150));
        let renew = phone.typist("s1").renew().unwrap();
        let id = phone.send(renew);
        assert!(phone.answer(&id).unwrap().yours);
    }
    phone.type_text("s1", "a");

    // Then nothing: the lease expires, everyone is told, and late input is not written.
    viewer.until(|t| t.lease("s1").is_some_and(|l| l.reason == Some(LeaseReason::Expired)));
    assert_eq!(viewer.lease("s1").unwrap().holder, None);
    phone.until(|t| t.lease("s1").is_some_and(|l| l.reason == Some(LeaseReason::Expired)));
    let late = phone.raw_input("s1", lease, 2, "late");
    assert_eq!(phone.refusal(&late), ErrorCode::Fenced);
    phone.idle(Duration::from_millis(50));
    assert_eq!(terminal.written(), "a");
}

#[test]
fn input_is_never_replayed_across_a_disconnect() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Typer::connect(&f, "viewer");
    viewer.subscribe("s1");
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let lease = token(&phone.acquire("s1", false).unwrap());
    phone.type_text("s1", "one");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("one")));
    // What the typist would send next, kept as a client that replays naively would keep it.
    let unsent = phone.typist("s1").input("two");
    let mut typist = phone.typist("s1").clone();
    drop(phone);

    // The lease ended with the connection, and everyone following the screen is told.
    viewer.until(|t| t.lease("s1").is_some_and(|l| l.reason == Some(LeaseReason::Disconnected)));
    assert_eq!(viewer.lease("s1").unwrap().holder, None);

    // The same device reconnects: the reference client sends nothing typed before the outage...
    typist.disconnected();
    assert!(typist.input("three").is_empty());
    // ...and a client that resends anyway is fenced: its old lease cannot be carried over.
    let mut again = Typer::connect(&f, "phone");
    again.subscribe("s1");
    for message in unsent {
        let id = again.send(message);
        assert_eq!(again.refusal(&id), ErrorCode::Fenced);
    }
    let id = again.raw_input("s1", lease, 3, "three");
    assert_eq!(again.refusal(&id), ErrorCode::Fenced);
    // A fresh lease works, from 1.
    let fresh = again.acquire("s1", false).unwrap();
    assert!(token(&fresh) > lease);
    again.type_text("s1", " four");
    again.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("one four")));
    assert_eq!(terminal.written(), "one four");
}

#[test]
fn the_phone_does_not_resize_unless_asked_and_its_size_ends_with_its_lease() {
    let terminal = Detached::new(80, 24, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut desk = Typer::connect(&f, "desk");
    desk.subscribe("s1");
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    phone.acquire("s1", false).unwrap();
    phone.type_text("s1", "ls");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("ls")));
    phone.idle(Duration::from_millis(50));
    assert_eq!(terminal.size(), (80, 24), "taking the lease and typing leave the grid alone");
    assert_eq!(phone.screen_size("s1"), Some((80, 24)));

    // "Fit to this screen" is an explicit request; every client sees the new grid.
    let fit = phone.typist("s1").resize(Some(GridSize::new(40, 12))).unwrap();
    phone.send(fit);
    desk.until(|t| t.screen_size("s1") == Some((40, 12)));
    assert_eq!(terminal.size(), (40, 12));
    desk.until(|t| t.lease("s1").is_some_and(|l| l.resized));

    // The desktop's pane changes meanwhile: its size waits for the lease to end.
    terminal.resize(100, 30);
    desk.idle(Duration::from_millis(50));
    assert_eq!(terminal.size(), (40, 12));
    let release = phone.typist("s1").release().unwrap();
    let id = phone.send(release);
    let released = phone.answer(&id).unwrap();
    assert_eq!((released.holder, released.reason), (None, Some(LeaseReason::Released)));
    desk.until(|t| t.screen_size("s1") == Some((100, 30)));
    assert_eq!(terminal.size(), (100, 30));
    desk.until(|t| t.lease("s1").is_some_and(|l| l.holder.is_none() && !l.resized));
}

impl Typer {
    fn offers(&self, capability: &str) -> bool {
        matches!(&self.client.welcome.as_ref().unwrap().message,
            Message::Welcome { capabilities, .. } if capabilities.iter().any(|c| c == capability))
    }
}

#[test]
fn a_host_without_a_desktop_takes_each_terminals_own_size_from_a_clients_pane() {
    let terminal = Detached::new(80, 24, 100);
    let config =
        ServerConfig { headless: true, lease: Policy { ttl: Duration::from_secs(1), ..Policy::default() }, ..config() };
    let f = start_with(config, &[("s1", &terminal)]);
    let mut mac = Typer::connect(&f, "mac");
    assert!(mac.offers(wire::FIT), "a host with no desktop of its own offers fit");
    mac.subscribe("s1");
    // The pane's size, with no lease and nothing typed: the terminal's own size from now on.
    mac.send(Message::TerminalFit { session: "s1".into(), size: GridSize::new(120, 40) });
    mac.until(|t| t.screen_size("s1") == Some((120, 40)));
    assert_eq!(terminal.size(), (120, 40));

    // A phone fits it to its screen for as long as its lease lasts; the pane's next size waits meanwhile.
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    phone.acquire("s1", false).unwrap();
    let fit = phone.typist("s1").resize(Some(GridSize::new(40, 12))).unwrap();
    phone.send(fit);
    mac.until(|t| t.screen_size("s1") == Some((40, 12)));
    mac.send(Message::TerminalFit { session: "s1".into(), size: GridSize::new(130, 40) });
    mac.idle(Duration::from_millis(50));
    assert_eq!(terminal.size(), (40, 12));
    // The phone's lease expires: the terminal returns to the pane's size, never to the size it started with.
    mac.until(|t| t.lease("s1").is_some_and(|l| l.reason == Some(LeaseReason::Expired)));
    mac.until(|t| t.screen_size("s1") == Some((130, 40)));
    assert_eq!(terminal.size(), (130, 40));
    assert!(mac.errors.is_empty(), "{:?}", mac.errors);
}

#[test]
fn a_host_with_a_desktop_keeps_sizing_its_terminals() {
    let terminal = Detached::new(80, 24, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut client = Typer::connect(&f, "mac");
    assert!(!client.offers(wire::FIT), "the desktop's panes size its terminals");
    client.subscribe("s1");
    let id = client.send(Message::TerminalFit { session: "s1".into(), size: GridSize::new(120, 40) });
    assert_eq!(client.refusal(&id), ErrorCode::Unauthorised);
    client.idle(Duration::from_millis(50));
    assert_eq!(terminal.size(), (80, 24));

    // Without `input` there is no fit either, even with no desktop.
    let g = start_with(ServerConfig { headless: true, ..config() }, &[("s1", &terminal)]);
    let screens_only = vec!["board".to_string(), wire::TERMINALS.to_string(), wire::FIT.to_string()];
    let viewer = Typer::with_capabilities(&g, "viewer", screens_only);
    assert!(!viewer.offers(wire::FIT));
}

#[test]
fn resizes_from_a_burst_land_once_and_the_last_one_wins() {
    let terminal = Detached::new(80, 24, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    phone.acquire("s1", false).unwrap();
    for cols in [40, 41, 42, 43, 44] {
        let resize = phone.typist("s1").resize(Some(GridSize::new(cols, 20))).unwrap();
        phone.send(resize);
    }
    phone.until(|t| t.screen_size("s1") == Some((44, 20)));
    phone.idle(Duration::from_millis(150));
    assert_eq!(terminal.size(), (44, 20));
    assert!(phone.errors.is_empty(), "{:?}", phone.errors);
}

#[test]
fn the_desktop_takes_the_lease_back_by_typing() {
    let terminal = Detached::new(80, 24, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let lease = token(&phone.acquire("s1", false).unwrap());
    let fit = phone.typist("s1").resize(Some(GridSize::new(40, 12))).unwrap();
    phone.send(fit);
    phone.until(|t| t.screen_size("s1") == Some((40, 12)));

    assert!(terminal.take_back());
    phone.until(|t| t.lease("s1").is_some_and(|l| l.reason == Some(LeaseReason::Desktop)));
    assert_eq!(phone.lease("s1").unwrap().holder, None);
    assert!(!phone.typist("s1").may_type());
    // The size returns with the lease.
    phone.until(|t| t.screen_size("s1") == Some((80, 24)));
    let late = phone.raw_input("s1", lease, 1, "late");
    assert_eq!(phone.refusal(&late), ErrorCode::Fenced);
    assert_eq!(terminal.written(), "");
}

#[test]
fn unsubscribing_ends_the_lease() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut viewer = Typer::connect(&f, "viewer");
    viewer.subscribe("s1");
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let lease = token(&phone.acquire("s1", false).unwrap());
    viewer.until(|t| t.lease("s1").is_some_and(|l| l.holder.as_deref() == Some("phone")));
    phone.send(Message::Unsubscribe { topics: vec![Topic::Screen { session: "s1".into(), focused: false }] });
    viewer.until(|t| t.lease("s1").is_some_and(|l| l.holder.is_none()));
    // Typing needs the screen: without a subscription the request is refused before the lease.
    let id = phone.raw_input("s1", lease, 1, "x");
    assert_eq!(phone.refusal(&id), ErrorCode::NotFound);
}

#[test]
fn typing_needs_the_input_capability_a_followed_terminal_and_a_controller() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);

    // Without `input` the client is never sent lease states (it could not decode them) and cannot type.
    let screens_only = vec!["board".to_string(), wire::TERMINALS.to_string()];
    let mut old = Typer::with_capabilities(&f, "old", screens_only);
    old.send(Message::Subscribe { topics: vec![Topic::Screen { session: "s1".into(), focused: true }] });
    old.until(|t| t.screen_text("s1").is_some());
    assert_eq!(old.acquire("s1", false), Err(ErrorCode::Unauthorised));
    old.idle(Duration::from_millis(50));
    assert!(old.leases.is_empty());

    let mut phone = Typer::connect(&f, "phone");
    assert_eq!(phone.acquire("s1", false), Err(ErrorCode::NotFound), "subscribe first");
    phone.send(Message::Subscribe { topics: vec![Topic::Screen { session: "nope".into(), focused: true }] });
    assert_eq!(phone.acquire("nope", false), Err(ErrorCode::NotFound));

    // A view-only listener's clients follow screens and see who types, but never get a lease.
    let mut cfg = config();
    cfg.role = Role::Viewer;
    let g = start_with(cfg, &[("s1", &terminal)]);
    let mut viewer = Typer::connect(&g, "viewer");
    viewer.subscribe("s1");
    assert_eq!(viewer.acquire("s1", false), Err(ErrorCode::Unauthorised));
    let id = viewer.raw_input("s1", 1, 1, "x");
    assert_eq!(viewer.refusal(&id), ErrorCode::Unauthorised);
    assert_eq!(terminal.written(), "");
}

#[test]
fn the_echo_is_sent_at_once_not_at_the_screen_cadence() {
    let terminal = Detached::new(40, 6, 100);
    let mut cfg = config();
    // A cadence and a read tick far slower than an echo may take.
    cfg.poll_interval = Duration::from_millis(500);
    cfg.session.tick = Duration::from_millis(500);
    cfg.screens.focused = Duration::from_secs(5);
    cfg.screens.visible = Duration::from_secs(5);
    let f = start_with(cfg, &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    phone.acquire("s1", false).unwrap();
    let mut slowest = Duration::ZERO;
    for c in ["a", "b", "c", "d", "e"] {
        phone.idle(Duration::from_millis(300)); // past the echo window, so each key starts cold
        let started = Instant::now();
        phone.type_text("s1", c);
        let typed = terminal.written();
        phone.until(|t| t.screen_text("s1").is_some_and(|s| s.trim_end().ends_with(&typed)));
        slowest = slowest.max(started.elapsed());
    }
    assert!(slowest < wait(Duration::from_millis(250)), "an echo took {slowest:?}");
}

#[test]
fn a_closed_terminal_is_told_as_free_and_racing_its_close_never_costs_the_connection() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let lease = token(&phone.acquire("s1", false).unwrap());

    // The session's terminal closes: everyone following it is told that nobody holds its lease.
    f.terminals.publish(Vec::new());
    phone.until(|t| {
        t.lease("s1").is_some_and(|l| l.holder.is_none() && !l.yours && l.reason == Some(LeaseReason::Disconnected))
    });
    assert!(!phone.typist("s1").may_type());

    // Input sent before the client heard of it meets not_found, however often: a well-behaved
    // client racing a close is never disconnected for it.
    for _ in 0..=SessionConfig::default().max_errors + 2 {
        let id = phone.raw_input("s1", lease, 1, "late");
        assert_eq!(phone.refusal(&id), ErrorCode::NotFound);
    }
    let id = phone.send(Message::Subscribe { topics: vec![Topic::Screen { session: "s1".into(), focused: true }] });
    assert_eq!(phone.refusal(&id), ErrorCode::NotFound);

    // Still connected: a new terminal for the session streams, and its lease is told afresh.
    let second = Detached::new(40, 6, 100);
    second.feed(b"second terminal");
    f.terminals.publish(vec![entry("s1", &second)]);
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("second terminal")));
    phone.until(|t| t.lease("s1").is_some_and(|l| l.reason.is_none()));
    assert_eq!(terminal.written(), "");
}

#[test]
fn input_a_program_has_not_read_is_bounded_and_answered_with_slow_client() {
    let terminal = Detached::new(40, 6, 100);
    let mut cfg = config();
    cfg.lease = Policy { max_unwritten: 8, ..Policy::default() };
    let f = start_with(cfg, &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    let lease = token(&phone.acquire("s1", false).unwrap());

    terminal.stall(true);
    phone.raw_input("s1", lease, 1, "12345");
    // Above the bound input is refused, not queued, and keeps its number; a client that keeps
    // trying is not disconnected for it.
    for _ in 0..=SessionConfig::default().max_errors + 2 {
        let id = phone.raw_input("s1", lease, 2, "67890");
        assert_eq!(phone.refusal(&id), ErrorCode::SlowClient);
    }
    assert_eq!(terminal.written(), "12345");

    // Once the program reads, the same input number goes through.
    terminal.stall(false);
    phone.raw_input("s1", lease, 2, "ok");
    phone.until(|t| t.screen_text("s1").is_some_and(|s| s.contains("ok")));
    assert_eq!(terminal.written(), "12345ok");
}

#[test]
fn a_paste_the_program_does_not_read_is_typed_again_from_where_slow_client_stopped_it() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    phone.subscribe("s1");
    phone.acquire("s1", false).unwrap();
    let unread = Policy::default().max_unwritten;

    // Twice what may wait unread, sent at once: the host takes the first half.
    terminal.stall(true);
    let paste = "0123456789abcdef".repeat(2 * unread / 16);
    let ids = phone.type_text("s1", &paste);
    let (pieces, taken) = (paste.len() / MAX_INPUT_BYTES, unread / MAX_INPUT_BYTES);
    assert_eq!(phone.refusal(&ids[taken]), ErrorCode::SlowClient);
    let tail = phone.typist("s1").refused(Seq(taken as u64 + 1), ErrorCode::SlowClient).expect("the refused tail");
    assert_eq!(tail, paste[unread..]);
    // Every piece already sent behind it meets out_of_order, more of them than the errors a
    // client may make, and the client stays connected.
    assert!(pieces - taken - 1 > SessionConfig::default().max_errors as usize);
    for (seq, id) in (taken + 1..pieces).zip(&ids[taken + 1..pieces]) {
        assert_eq!(phone.refusal(id), ErrorCode::OutOfOrder);
        assert_eq!(phone.typist("s1").refused(Seq(seq as u64 + 1), ErrorCode::OutOfOrder), None);
    }
    assert_eq!(terminal.written(), paste[..unread]);

    // Once the program reads, the tail goes through under the numbers it was refused at.
    terminal.stall(false);
    phone.type_text("s1", &tail);
    phone.until(|_| terminal.written().len() == paste.len());
    assert_eq!(terminal.written(), paste);
    phone.idle(Duration::from_millis(100));
    assert_eq!(phone.errors.len(), pieces - taken, "nothing typed again was refused: {:?}", phone.errors.last());
}

#[test]
fn subscribing_to_a_terminal_that_never_existed_counts_but_to_one_that_closed_does_not() {
    let terminal = Detached::new(40, 6, 100);
    let mut cfg = config();
    cfg.session.max_errors = 2;
    let f = start_with(cfg, &[("s1", &terminal)]);
    let mut phone = Typer::connect(&f, "phone");
    f.terminals.publish(Vec::new());
    let screen = |s: &str| Message::Subscribe { topics: vec![Topic::Screen { session: s.into(), focused: true }] };
    for _ in 0..5 {
        let id = phone.send(screen("s1"));
        assert_eq!(phone.refusal(&id), ErrorCode::NotFound, "its terminal closed: a race, not a mistake");
    }
    for _ in 0..6 {
        phone.send(screen("nope"));
    }
    let mut refused = 0;
    while let Some(envelope) = phone.client.next() {
        refused += matches!(envelope.message, Message::Error { code: ErrorCode::NotFound, .. }) as u32;
    }
    assert_eq!(refused, 3, "a session that never had a terminal counts: the third ends the connection");
}

#[test]
fn input_is_not_offered_without_terminals() {
    let terminal = Detached::new(40, 6, 100);
    let f = start_with(config(), &[("s1", &terminal)]);
    let typer = Typer::with_capabilities(&f, "odd", vec!["board".into(), wire::INPUT.into()]);
    let Some(Message::Welcome { capabilities, .. }) = typer.client.welcome.as_ref().map(|w| &w.message) else {
        panic!("no welcome");
    };
    assert_eq!(capabilities, &vec!["board".to_string()], "input needs terminals");
}
