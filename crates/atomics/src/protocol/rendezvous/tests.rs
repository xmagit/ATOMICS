//! The rendezvous against a fake service: an in-process WebSocket server that admits sockets with
//! passes a fake issuer minted, speaks the tunnel's frames, and closes with every code. Browsers
//! on its channels speak `atomics_core::noise` as a web client does: pairing through the
//! desktop's link and code, sessions of paired devices, and every way those are refused. The
//! profile issuer is tested against an in-process MCP server.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use atomics_core::diff::FileDiff;
use atomics_core::noise::{self, Access, Initiator, PairAnswer, PairRequest, Pairing, PublicKey, Secret};
use atomics_core::projection::Projections;
use atomics_core::session::AgentStatus;
use atomics_core::tunnel::service::{self, FromHost};
use atomics_core::tunnel::{self, SUBPROTOCOL, close};
use atomics_core::wire::{self, CAPABILITIES, Envelope, Message, Role, Topic, VERSION};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::{Value, json};
use tungstenite::WebSocket;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};

use super::devices::{Devices, PairingView};
use super::issuer::{self, IssueError, IssueKind, Issuer, Pass, ProfileIssuer, Published};
use super::keys::DeviceKeys;
use super::{Ended, Next, Phase, Rendezvous, Settings, Status, StatusCell, after};
use crate::protocol::tests::wait;
use crate::protocol::{DiffSource, Server, ServerConfig, SessionConfig, Terminals};

/// The host id the fake issuer hands out.
const HOST: &str = "0190a1b2-c3d4-8e5f-a617-283940516273";

/// What the fake issuer and the fake service share: the registered key and the passes.
#[derive(Default)]
struct Ledger {
    signing_key: Option<Vec<u8>>,
    registrations: Vec<(String, u32, String)>,
    challenges: usize,
    /// Minted and not yet used.
    passes: HashSet<String>,
    minted: usize,
    bad_signatures: usize,
    /// The next refusals of each call, in order.
    register_fails: VecDeque<IssueError>,
    challenge_fails: VecDeque<IssueError>,
    pass_fails: VecDeque<IssueError>,
    /// Upgrades to refuse with 401 before admitting any.
    refuse_upgrades: usize,
    upgrades: usize,
}

type Shared = Arc<Mutex<Ledger>>;

struct FakeIssuer(Shared);

impl Issuer for FakeIssuer {
    fn label(&self) -> String {
        "Example service".into()
    }

    fn register(&mut self, keys: &DeviceKeys, name: &str, major: u32) -> Result<String, IssueError> {
        let mut ledger = self.0.lock().unwrap();
        if let Some(error) = ledger.register_fails.pop_front() {
            return Err(error);
        }
        ledger.signing_key = Some(URL_SAFE_NO_PAD.decode(keys.signing_public()).unwrap());
        ledger.registrations.push((name.into(), major, keys.noise_public()));
        Ok(HOST.into())
    }

    fn challenge(&mut self, host_id: &str) -> Result<String, IssueError> {
        let mut ledger = self.0.lock().unwrap();
        assert_eq!(host_id, HOST);
        if let Some(error) = ledger.challenge_fails.pop_front() {
            return Err(error);
        }
        ledger.challenges += 1;
        Ok(format!("challenge-{}", ledger.challenges))
    }

    fn pass(&mut self, host_id: &str, challenge: &str, signature: &str) -> Result<Pass, IssueError> {
        let mut ledger = self.0.lock().unwrap();
        if let Some(error) = ledger.pass_fails.pop_front() {
            return Err(error);
        }
        let key = ledger.signing_key.clone().expect("registered first");
        let signed = UnparsedPublicKey::new(&ED25519, key)
            .verify(tunnel::pass_message(host_id, challenge).as_bytes(), &URL_SAFE_NO_PAD.decode(signature).unwrap());
        if signed.is_err() {
            ledger.bad_signatures += 1;
            return Err(IssueError::new(IssueKind::Challenge, "bad signature"));
        }
        ledger.minted += 1;
        let pass = format!("pass-{}", ledger.minted);
        ledger.passes.insert(pass.clone());
        Ok(Pass(pass.into()))
    }
}

/// One admitted host socket, driven by the test as the service.
struct Admitted {
    ws: WebSocket<TcpStream>,
    authorization: String,
    origin: Option<String>,
    path: String,
}

impl Admitted {
    fn send(&mut self, frame: Vec<u8>) {
        self.ws.send(tungstenite::Message::binary(frame)).expect("the host is there");
    }

    fn expiry_in(&mut self, ms: i64) {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
        self.send(service::expiry(now + ms));
    }

    /// The host's next frame within `limit`; `None` on timeout or close.
    fn next(&mut self, limit: Duration) -> Option<FromHost> {
        let deadline = Instant::now() + wait(limit);
        while Instant::now() < deadline {
            match self.ws.read() {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    return Some(service::parse(&bytes).expect("the host sends only frames a service accepts"));
                }
                Ok(tungstenite::Message::Close(_)) => return None,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if crate::test_support::nothing_yet(&e) => {}
                Err(_) => return None,
            }
        }
        None
    }

    /// The next body on `channel`, or the code the host closed it with; other frames are passed
    /// over. A body the host sends is never plaintext `atomics.v1`.
    fn body(&mut self, channel: u32) -> Result<Vec<u8>, u16> {
        loop {
            match self.next(Duration::from_secs(5)).expect("a frame from the host") {
                FromHost::Data { channel: c, body } if c == channel => {
                    let text = std::str::from_utf8(&body).ok();
                    assert!(text.and_then(|t| wire::decode(t).ok()).is_none(), "plaintext on channel {channel}");
                    return Ok(body);
                }
                FromHost::Close { channel: c, code } if c == channel => return Err(code),
                _ => {}
            }
        }
    }

    /// The code the host closes `channel` with, passing over what it sends first; the channel
    /// then leaves, as the service would make it.
    fn closed(&mut self, channel: u32) -> u16 {
        loop {
            if let Err(code) = self.body(channel) {
                self.send(service::left(channel, code));
                return code;
            }
        }
    }

    /// A browser's frame, sealed into its messages.
    fn seal(&mut self, channel: u32, noise: &mut noise::Channel, frame: &[u8]) {
        for message in noise.seal(frame).unwrap() {
            self.send(tunnel::data(channel, &message));
        }
    }

    /// The next whole frame the host sent on `channel`, or its close code.
    fn open(&mut self, channel: u32, noise: &mut noise::Channel) -> Result<Vec<u8>, u16> {
        loop {
            if let Some(frame) = noise.open(&self.body(channel)?).expect("the host's messages authenticate") {
                return Ok(frame);
            }
        }
    }

    fn envelope(&mut self, channel: u32, noise: &mut noise::Channel) -> Envelope {
        let frame = self.open(channel, noise).expect("a frame, not a close");
        wire::decode(std::str::from_utf8(&frame).unwrap()).expect("atomics.v1 inside the channel")
    }

    fn hello(&mut self, channel: u32, noise: &mut noise::Channel) {
        let hello = Envelope::new(Message::Hello {
            client_id: "browser".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            prior_epoch: None,
            prior_cursor: None,
        });
        self.seal(channel, noise, serde_json::to_string(&hello).unwrap().as_bytes());
    }

    fn message(&mut self, channel: u32, noise: &mut noise::Channel, message: Message) {
        let envelope = Envelope::new(message).reply_to(Some("r1".into()));
        self.seal(channel, noise, serde_json::to_string(&envelope).unwrap().as_bytes());
    }

    /// A paired device's session on a new channel: the established channel, or the close code.
    fn connect(&mut self, channel: u32, host_key: &PublicKey, device: &Secret) -> Result<noise::Channel, u16> {
        self.send(service::joined(channel));
        let (initiator, first) = Initiator::connect(HOST, host_key, device).unwrap();
        self.send(tunnel::data(channel, &first));
        let answer = self.body(channel)?;
        Ok(initiator.finish(&answer).expect("the host's answer authenticates").channel)
    }

    /// A device answering `link` on a new channel, as far as the code: it shows `code` and has
    /// sent its name; the host's answer is next.
    fn pair(&mut self, channel: u32, link: &str, device: &Secret, name: &str) -> Result<(noise::Channel, String), u16> {
        let pairing = Pairing::read(link.split_once('#').expect("a fragment").1).expect("a link the browser reads");
        assert_eq!(pairing.host_id, HOST);
        self.send(service::joined(channel));
        let (initiator, first) = Initiator::pair(&pairing, device).unwrap();
        self.send(tunnel::data(channel, &first));
        let answer = self.body(channel)?;
        let mut done = initiator.finish(&answer).expect("the host proved it holds the psk");
        let request = serde_json::to_vec(&PairRequest { name: name.into() }).unwrap();
        self.seal(channel, &mut done.channel, &request);
        Ok((done.channel, done.code))
    }

    /// A session the host does not know, once its `hello` decrypts: the host's sealed refusal, then
    /// the code it closes the channel with.
    fn refused(&mut self, channel: u32, noise: &mut noise::Channel) -> u16 {
        self.hello(channel, noise);
        let refusal = self.envelope(channel, noise);
        assert!(wire::is_not_paired(&refusal), "the refusal comes sealed: {refusal:?}");
        self.closed(channel)
    }

    /// The host's answer to a pairing.
    fn pair_answer(&mut self, channel: u32, noise: &mut noise::Channel) -> PairAnswer {
        serde_json::from_slice(&self.open(channel, noise).expect("an answer")).expect("a pairing answer")
    }

    fn close(&mut self, code: u16) {
        use tungstenite::protocol::CloseFrame;
        let _ = self.ws.close(Some(CloseFrame { code: code.into(), reason: "".into() }));
        let _ = self.ws.flush();
    }
}

/// The fake service: admits an upgrade only with an unused pass from the ledger, the
/// subprotocol, and no `Origin`.
struct FakeService {
    addr: SocketAddr,
    admitted: mpsc::Receiver<Admitted>,
}

impl FakeService {
    fn start(ledger: Shared) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, admitted) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let (mut authorization, mut origin, mut path) = (String::new(), None, String::new());
                let ledger = ledger.clone();
                #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
                let check = |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
                    let header =
                        |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
                    authorization = header("authorization").unwrap_or_default();
                    origin = header("origin");
                    path = request.uri().path().to_string();
                    let refuse = |status: u16| {
                        tungstenite::http::Response::builder().status(status).body(Some("refused".to_string())).unwrap()
                    };
                    let mut ledger = ledger.lock().unwrap();
                    ledger.upgrades += 1;
                    if ledger.refuse_upgrades > 0 {
                        ledger.refuse_upgrades -= 1;
                        return Err(refuse(401));
                    }
                    let pass = authorization.strip_prefix("Bearer ").unwrap_or_default();
                    if !ledger.passes.remove(pass) {
                        return Err(refuse(401));
                    }
                    if origin.is_some() {
                        return Err(refuse(403));
                    }
                    if !header("sec-websocket-protocol").is_some_and(|p| p.split(',').any(|p| p.trim() == SUBPROTOCOL))
                    {
                        return Err(refuse(400));
                    }
                    response.headers_mut().insert("Sec-WebSocket-Protocol", SUBPROTOCOL.parse().unwrap());
                    Ok(response)
                };
                // Bound first: the callback borrows the captured headers until the result is gone.
                let accepted = tungstenite::accept_hdr(stream, check);
                let Ok(ws) = accepted else { continue };
                ws.get_ref().set_read_timeout(Some(Duration::from_millis(10))).unwrap();
                if tx.send(Admitted { ws, authorization, origin, path }).is_err() {
                    break;
                }
            }
        });
        Self { addr, admitted }
    }

    fn url(&self) -> String {
        format!("ws://{}/atomics/tunnel", self.addr)
    }

    fn accept(&self) -> Admitted {
        self.admitted.recv_timeout(wait(Duration::from_secs(10))).expect("the host dialed")
    }

    fn quiet_for(&self, duration: Duration) -> bool {
        self.admitted.recv_timeout(duration).is_err()
    }
}

struct NoDiffs;

impl DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, FileDiff)> {
        None
    }
}

/// A protocol server, a rendezvous to the fake service, and what they share.
struct Fixture {
    rendezvous: Rendezvous,
    status: Arc<StatusCell>,
    ledger: Shared,
    devices: Arc<Devices>,
    _server: Server,
    _dir: crate::test_support::TempDir,
}

impl Drop for Fixture {
    /// `stop` gives the channels' threads 2 s, as a quit should, and a loaded machine can take
    /// longer. Each writes `devices.json` as its session ends, and `_dir` goes next.
    fn drop(&mut self) {
        self.rendezvous.stop();
        self.rendezvous.crew.wait(Instant::now() + wait(Duration::from_secs(60)));
    }
}

const PAIR_URL: &str = "https://service.example/atomics/pair#{pairing}";

impl Fixture {
    /// The host's Noise key, as it registered it.
    fn host_key(&self) -> PublicKey {
        let ledger = self.ledger.lock().unwrap();
        PublicKey::parse(&ledger.registrations.last().expect("registered").2).unwrap()
    }

    /// "Pair a device" on the desktop, once the host is registered: the link it shows.
    fn offer(&self) -> String {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while self.devices.unready().is_some() {
            assert!(Instant::now() < deadline, "never ready to pair");
            std::thread::sleep(Duration::from_millis(5));
        }
        self.devices.offer(PAIR_URL).unwrap();
        match self.devices.view() {
            PairingView::Open { link, .. } => link.to_string(),
            _ => panic!("the link is shown"),
        }
    }

    /// `devices.json` as it is on disk now.
    fn allowlist(&self) -> String {
        std::fs::read_to_string(self._dir.path().join("devices.json")).expect("the allowlist")
    }

    /// Wait until the desktop's pairing window is free for the next device.
    fn link_free(&self) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while !matches!(self.devices.view(), PairingView::Open { busy: false, .. }) {
            assert!(Instant::now() < deadline, "the link was never given back");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Wait until no session of the paired devices is open.
    fn none_connected(&self) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while self.devices.list().iter().any(|(_, connected)| *connected) {
            assert!(Instant::now() < deadline, "a device stayed connected");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The desktop's prompt for a device that proved the link: its name and code.
    fn prompt(&self) -> (String, String) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        loop {
            if let PairingView::Confirm { name, code, .. } = self.devices.view() {
                return (name, code);
            }
            assert!(Instant::now() < deadline, "the desktop never asked");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Pair `device` through the desktop's link and code, with `access`; the channel then goes
    /// on as its session.
    fn paired(&self, socket: &mut Admitted, channel: u32, device: &Secret, access: Access) -> noise::Channel {
        let link = self.offer();
        let (mut secure, code) = socket.pair(channel, &link, device, "Test phone").unwrap();
        assert_eq!(self.prompt(), ("Test phone".to_string(), code), "the desktop shows the device's own code");
        self.devices.decide(Some(access));
        assert_eq!(socket.pair_answer(channel, &mut secure), PairAnswer { paired: true, may: Some(access) });
        secure
    }
}

fn quick(url: &str) -> Settings {
    Settings {
        tick: Duration::from_millis(20),
        backoff: (Duration::from_millis(20), Duration::from_millis(100)),
        retry: Duration::from_millis(50),
        throttle_wait: Duration::from_millis(50),
        quota_wait: Duration::from_secs(60),
        registration_grace: Duration::from_millis(200),
        renew: (Duration::from_millis(50), Duration::from_secs(10)),
        idle_timeout: Duration::from_secs(10),
        connect_timeout: Duration::from_secs(5),
        ..Settings::new(url, "test host")
    }
}

fn fixture(service: &FakeService, ledger: &Shared, tune: impl FnOnce(&mut Settings)) -> Fixture {
    fixture_serving(service, ledger, tune, Arc::new(Terminals::default()), Arc::new(crate::protocol::NoCommands))
}

/// A fixture whose server streams `terminals` and hands its mutations to `commands`.
fn fixture_serving(
    service: &FakeService,
    ledger: &Shared,
    tune: impl FnOnce(&mut Settings),
    terminals: Arc<Terminals>,
    commands: Arc<dyn crate::protocol::Commands>,
) -> Fixture {
    let projections = Arc::new(Projections::default());
    projections.publish_sessions(vec![crate::protocol::tests::session("a", "fix the parser", AgentStatus::Working)]);
    let config = ServerConfig {
        poll_interval: Duration::from_millis(5),
        session: SessionConfig { tick: Duration::from_millis(5), ..SessionConfig::default() },
        ..ServerConfig::default()
    };
    let server = Server::start(config, projections, Arc::new(NoDiffs), terminals, commands).unwrap();
    let (dir, _, store) = crate::credentials::tests::machine(false);
    let home = dir.path().join("home");
    let mut settings = quick(&service.url());
    tune(&mut settings);
    let status = StatusCell::new(|| {});
    let devices = Devices::open(dir.path().join("devices.json"), || {});
    let rendezvous = Rendezvous::start(
        settings,
        server.sessions(),
        devices.clone(),
        Box::new(FakeIssuer(ledger.clone())),
        Box::new(move || DeviceKeys::load_or_create(&store, &home)),
        status.clone(),
    )
    .unwrap();
    Fixture { rendezvous, status, ledger: ledger.clone(), devices, _server: server, _dir: dir }
}

fn until(status: &StatusCell, what: &str, done: impl Fn(&Status) -> bool) -> Status {
    let deadline = Instant::now() + wait(Duration::from_secs(10));
    loop {
        let now = status.get();
        if done(&now) {
            return now;
        }
        assert!(Instant::now() < deadline, "never {what}: {now:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_host_registers_dials_with_a_pass_and_serves_a_paired_device_encrypted() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    // The upgrade: the pass as a bearer, no Origin, the configured path.
    assert_eq!(socket.authorization, "Bearer pass-1");
    assert_eq!((socket.origin.as_deref(), socket.path.as_str()), (None, "/atomics/tunnel"));
    {
        let ledger = host.ledger.lock().unwrap();
        assert_eq!(ledger.registrations, [("test host".to_string(), 1, ledger.registrations[0].2.clone())]);
        assert_eq!((ledger.bad_signatures, ledger.passes.len()), (0, 0), "the one pass was spent on the upgrade");
    }
    let online = until(&host.status, "online", |s| s.online);
    assert_eq!(
        (online.phase, online.registered, online.host_id.as_deref(), online.service.as_str(), online.host.as_str()),
        (Phase::Online, true, Some(HOST), "Example service", "127.0.0.1")
    );
    socket.expiry_in(300_000);
    // Pair a device that may type and approve; its pairing channel goes on as its session.
    let device = Secret::generate().unwrap();
    let mut secure = host.paired(&mut socket, 1, &device, Access::Control);
    socket.hello(1, &mut secure);
    let welcome = socket.envelope(1, &mut secure);
    let Message::Welcome { role, may, .. } = &welcome.message else { panic!("welcome first: {welcome:?}") };
    assert_eq!((*role, may.len()), (Role::Controller, 5), "{may:?}");
    let snapshot = socket.envelope(1, &mut secure);
    assert!(matches!(snapshot.message, Message::StateSnapshot { .. }), "{snapshot:?}");
    until(&host.status, "one channel", |s| s.channels == 1);
    // Data for a channel that never joined, and frames from a newer service, change nothing.
    socket.send(tunnel::data(9, b"{}"));
    socket.send(vec![7, 0, 0, 0, 1]);
    socket.send(service::left(1, close::NORMAL));
    until(&host.status, "no channel", |s| s.channels == 0 && s.online);
    // A later session of the same device: the host knows its key.
    let mut again = socket.connect(2, &host.host_key(), &device).expect("a known device");
    socket.hello(2, &mut again);
    let welcome = socket.envelope(2, &mut again);
    assert!(matches!(welcome.message, Message::Welcome { role: Role::Controller, .. }), "{welcome:?}");
    let list = host.devices.list();
    assert_eq!((list.len(), list[0].0.name.as_str(), list[0].1), (1, "Test phone", true), "listed, and connected");
}

/// Keeps the mutations the server hands over.
#[derive(Default)]
struct Recorder(Mutex<Vec<crate::protocol::OpRequest>>);

impl crate::protocol::Commands for Recorder {
    fn submit(&self, request: crate::protocol::OpRequest) -> bool {
        self.0.lock().unwrap().push(request);
        true
    }
}

#[test]
fn a_paired_device_types_and_commands_under_its_paired_name() {
    let terminal = crate::terminal::Detached::new(40, 6, 100);
    let terminals = Arc::new(Terminals::default());
    terminals.publish(vec![crate::protocol::screen_tests::entry("s1", &terminal)]);
    let recorder = Arc::new(Recorder::default());
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture_serving(&service, &ledger, |_| {}, terminals, recorder.clone());
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let mut secure = host.paired(&mut socket, 1, &device, Access::Control);
    socket.hello(1, &mut secure); // its client_id is "browser"
    socket.message(
        1,
        &mut secure,
        Message::Subscribe { topics: vec![Topic::Screen { session: "s1".into(), focused: true }] },
    );
    socket.message(1, &mut secure, atomics_core::lease::Typist::new("s1").acquire(false));
    let held = loop {
        if let Message::LeaseState(state) = socket.envelope(1, &mut secure).message
            && state.yours
        {
            break state;
        }
    };
    assert_eq!(held.holder.as_deref(), Some("Test phone"), "others see who types: the device, not its client_id");
    socket.message(
        1,
        &mut secure,
        Message::ApprovalDecide(atomics_core::ops::ApprovalDecide {
            op: "op-1".into(),
            approval: "a-1".into(),
            decision: atomics_core::approvals::Decision::Allow,
            message: None,
        }),
    );
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while recorder.0.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the mutation never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    let requests = recorder.0.lock().unwrap();
    let sent = requests[0].device.clone().expect("from a paired device");
    assert_eq!((requests[0].client.as_str(), sent.name.as_str()), ("browser", "Test phone"));
    assert_eq!(sent.id, super::devices::digest(&device.public()), "known by its key, for the journal");
}

#[test]
fn a_view_only_device_is_a_viewer_and_may_change_nothing() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let mut secure = host.paired(&mut socket, 1, &device, Access::View);
    socket.hello(1, &mut secure);
    let welcome = socket.envelope(1, &mut secure);
    let Message::Welcome { role, may, capabilities, .. } = &welcome.message else { panic!("{welcome:?}") };
    assert_eq!(*role, Role::Viewer);
    assert!(may.is_empty(), "whatever it negotiated: {capabilities:?} {may:?}");
    socket.send(service::left(1, close::NORMAL));
    let mut again = socket.connect(2, &host.host_key(), &device).unwrap();
    socket.hello(2, &mut again);
    assert!(matches!(socket.envelope(2, &mut again).message, Message::Welcome { role: Role::Viewer, .. }));
}

#[test]
fn an_unknown_device_is_refused_inside_the_channel_and_a_forged_one_before_any_json() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.handshake_timeout = Duration::from_millis(300));
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    until(&host.status, "online", |s| s.online);
    // A key the host never paired: the host answers, and once the device's first message decrypts
    // it refuses it inside the channel, then closes with `not_paired`.
    let mut unknown = socket.connect(1, &host.host_key(), &Secret::generate().unwrap()).expect("an answer");
    assert_eq!(socket.refused(1, &mut unknown), close::NOT_PAIRED);
    // Silent after the answer: no refusal, only the idle close.
    let _silent = socket.connect(6, &host.host_key(), &Secret::generate().unwrap()).expect("an answer");
    assert_eq!(socket.closed(6), close::IDLE);
    // A handshake meant for another host's key fails to decrypt.
    let device = Secret::generate().unwrap();
    assert_eq!(socket.connect(2, &Secret::generate().unwrap().public(), &device).err(), Some(close::REFUSED));
    socket.send(service::left(2, close::REFUSED));
    // Plaintext `atomics.v1`, as an H5a browser would send it.
    socket.send(service::joined(3));
    let hello = Envelope::new(Message::Hello {
        client_id: "plain".into(),
        versions: vec![VERSION],
        capabilities: vec![],
        prior_epoch: None,
        prior_cursor: None,
    });
    socket.send(tunnel::data(3, serde_json::to_string(&hello).unwrap().as_bytes()));
    assert_eq!(socket.closed(3), close::REFUSED);
    // No pairing is open.
    let link = format!("https://x.example/p#{}", *Pairing::new(HOST, host.host_key()).unwrap().fragment());
    assert_eq!(socket.pair(4, &link, &device, "Guess").err(), Some(close::NO_PAIRING));
    socket.send(service::left(4, close::NO_PAIRING));
    // Silence after joining.
    socket.send(service::joined(5));
    assert_eq!(socket.closed(5), close::IDLE);
}

#[test]
fn a_wrong_psk_fails_and_the_link_stays_for_the_right_device() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let link = host.offer();
    // The same link with another psk: the browser refuses the host's answer, and whatever it
    // sends next fails at the host, which gives the link back.
    let (address, fragment) = link.split_once('#').unwrap();
    let mut parts: Vec<String> = fragment.split('.').map(str::to_string).collect();
    parts[3] = Secret::generate().unwrap().public().text();
    let wrong = Pairing::read(&parts.join(".")).unwrap();
    socket.send(service::joined(1));
    let device = Secret::generate().unwrap();
    let (initiator, first) = Initiator::pair(&wrong, &device).unwrap();
    socket.send(tunnel::data(1, &first));
    let answer = socket.body(1).unwrap();
    assert_eq!(initiator.finish(&answer).err(), Some(noise::NoiseError::Refused));
    assert!(matches!(host.devices.view(), PairingView::Open { busy: true, .. }), "held while it may prove the psk");
    socket.send(tunnel::data(1, &[0x42; 64]));
    assert_eq!(socket.closed(1), close::REFUSED);
    assert!(
        matches!(host.devices.view(), PairingView::Open { busy: false, .. }),
        "given back the moment its first message fails"
    );
    // One without the psk that leaves after the answer gives it back as it leaves.
    socket.send(service::joined(3));
    let (_, first) = Initiator::pair(&wrong, &Secret::generate().unwrap()).unwrap();
    socket.send(tunnel::data(3, &first));
    socket.body(3).unwrap();
    assert!(matches!(host.devices.view(), PairingView::Open { busy: true, .. }));
    socket.send(service::left(3, close::NORMAL));
    host.link_free();
    // The real device pairs with the same link.
    let (mut secure, code) = socket.pair(2, &format!("{address}#{fragment}"), &device, "Phone").unwrap();
    assert_eq!(host.prompt().1, code);
    host.devices.decide(Some(Access::Control));
    assert!(socket.pair_answer(2, &mut secure).paired);
}

#[test]
fn stopping_waits_for_the_channels_so_their_last_writes_land_first() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let mut host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let mut secure = host.paired(&mut socket, 1, &device, Access::Control);
    socket.hello(1, &mut secure);
    let welcome = socket.envelope(1, &mut secure);
    assert!(matches!(welcome.message, Message::Welcome { .. }), "{welcome:?}");
    // Later than the attach's last-seen time, so only the detach can write one this late.
    std::thread::sleep(Duration::from_millis(5));
    let stopping = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    host.rendezvous.stop();
    // The session's thread has ended: its device is no longer connected, and the last-seen time
    // it wrote as it ended is on disk, before anything that owns the home goes.
    let list = host.devices.list();
    assert_eq!((list.len(), list[0].1), (1, false), "the session ended within stop");
    let stored: Value = serde_json::from_str(&host.allowlist()).unwrap();
    assert!(stored["devices"][0]["seen_ms"].as_u64().unwrap() >= stopping, "{stored}");
}

#[test]
fn a_device_racing_the_link_shows_another_code_and_is_refused() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let link = host.offer();
    // A script that read the link answers first: the desktop shows its code, not the phone's.
    let racer = Secret::generate().unwrap();
    let (mut raced, racer_code) = socket.pair(1, &link, &racer, "Phone").unwrap();
    let (name, shown) = host.prompt();
    assert_eq!((name.as_str(), shown.as_str()), ("Phone", racer_code.as_str()));
    // The phone itself is refused while that pairing waits: one at a time, and it may ask again.
    let phone = Secret::generate().unwrap();
    assert_eq!(socket.pair(2, &link, &phone, "Phone").err(), Some(close::PAIRING_BUSY));
    socket.send(service::left(2, close::PAIRING_BUSY));
    // The phone shows no code the user could match, so the user refuses.
    host.devices.decide(None);
    assert_eq!(socket.pair_answer(1, &mut raced), PairAnswer { paired: false, may: None });
    assert_eq!(socket.closed(1), close::NOT_PAIRED);
    assert!(host.devices.list().is_empty(), "never added");
    let mut later = socket.connect(3, &host.host_key(), &racer).expect("an answer");
    assert_eq!(socket.refused(3, &mut later), close::NOT_PAIRED);
    // And the link is used up: no longer busy, and asking again cannot help.
    assert_eq!(socket.pair(4, &link, &phone, "Phone").err(), Some(close::NO_PAIRING));
}

#[test]
fn a_replayed_session_fails_and_the_service_sees_only_ciphertext() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let _paired = host.paired(&mut socket, 1, &device, Access::Control);
    socket.send(service::left(1, close::NORMAL));
    // A recorded session: its first frame and its hello, as the service saw them.
    socket.send(service::joined(2));
    let (initiator, first) = Initiator::connect(HOST, &host.host_key(), &device).unwrap();
    socket.send(tunnel::data(2, &first));
    let answer = socket.body(2).unwrap();
    let mut session = initiator.finish(&answer).unwrap().channel;
    let hello = Envelope::new(Message::Hello {
        client_id: "browser".into(),
        versions: vec![VERSION],
        capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        prior_epoch: None,
        prior_cursor: None,
    });
    let recorded = session.seal(serde_json::to_string(&hello).unwrap().as_bytes()).unwrap();
    for message in &recorded {
        socket.send(tunnel::data(2, message));
    }
    assert!(matches!(socket.envelope(2, &mut session).message, Message::Welcome { .. }));
    // Replayed on a new channel: the host answers with a new ephemeral, so the hello fails.
    socket.send(service::joined(3));
    socket.send(tunnel::data(3, &first));
    socket.body(3).expect("an answer with fresh keys");
    for message in &recorded {
        socket.send(tunnel::data(3, message));
    }
    assert_eq!(socket.closed(3), close::REFUSED, "nothing replayed is served");
    // Replayed within the live session: the counter nonce refuses it.
    for message in &recorded {
        socket.send(tunnel::data(2, message));
    }
    assert_eq!(socket.closed(2), close::REFUSED);
}

#[test]
fn a_replayed_handshake_marks_nothing_connected_and_writes_nothing() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.handshake_timeout = Duration::from_millis(300));
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let _paired = host.paired(&mut socket, 1, &device, Access::Control);
    socket.send(service::left(1, close::NORMAL));
    host.none_connected();
    let before = host.allowlist();
    // A session's first frame as the service saw it, sent again on new channels: each gets an
    // answer, and with nothing that decrypts after it, an idle close.
    let (_, first) = Initiator::connect(HOST, &host.host_key(), &device).unwrap();
    for channel in [2, 3] {
        socket.send(service::joined(channel));
        socket.send(tunnel::data(channel, &first));
        socket.body(channel).expect("an answer");
        assert!(!host.devices.list()[0].1, "not connected on a handshake alone");
        assert_eq!(socket.closed(channel), close::IDLE);
    }
    assert_eq!(host.allowlist(), before, "devices.json is not written");
    // The device itself, with its first message, is.
    let mut session = socket.connect(4, &host.host_key(), &device).unwrap();
    socket.hello(4, &mut session);
    assert!(matches!(socket.envelope(4, &mut session).message, Message::Welcome { .. }));
    assert!(host.devices.list()[0].1, "connected");
}

#[test]
fn revoking_a_device_ends_its_session_and_its_next_one() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let device = Secret::generate().unwrap();
    let mut secure = host.paired(&mut socket, 1, &device, Access::Control);
    socket.hello(1, &mut secure);
    assert!(matches!(socket.envelope(1, &mut secure).message, Message::Welcome { .. }));
    let key = device.public();
    host.devices.revoke(&key);
    // The open session ends at once, with the refusal sealed into it.
    loop {
        let frame = socket.open(1, &mut secure).expect("the refusal before the close");
        if wire::is_not_paired(&wire::decode(std::str::from_utf8(&frame).unwrap()).unwrap()) {
            break;
        }
    }
    assert_eq!(socket.closed(1), close::NOT_PAIRED);
    let mut next = socket.connect(2, &host.host_key(), &device).expect("an answer");
    assert_eq!(socket.refused(2, &mut next), close::NOT_PAIRED);
    assert!(host.devices.list().is_empty());
}

#[test]
fn a_host_the_service_revoked_stops_and_offers_no_pairing() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let link = host.offer();
    assert!(link.contains(HOST));
    service.accept().close(close::REVOKED);
    let stopped = until(&host.status, "stopped", |s| s.phase == Phase::Stopped);
    assert!(stopped.note.as_deref().unwrap().contains("revoked"), "{stopped:?}");
    assert!(service.quiet_for(Duration::from_millis(300)), "no new dial");
    assert!(host.devices.unready().is_some(), "no pairing without a registration");
    assert!(matches!(host.devices.view(), PairingView::Idle { .. }), "the open link is withdrawn");
}

#[test]
fn the_pass_is_renewed_in_band_at_half_its_life_and_every_renewal_is_a_fresh_pass() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.renew = (Duration::from_millis(50), Duration::from_secs(10)));
    let mut socket = service.accept();
    let started = Instant::now();
    socket.expiry_in(600);
    let renewal = loop {
        if let FromHost::Renew(pass) = socket.next(Duration::from_secs(5)).expect("a renewal") {
            break pass;
        }
    };
    assert!(started.elapsed() >= Duration::from_millis(200), "not before half its life: {:?}", started.elapsed());
    assert_eq!(renewal, "pass-2");
    assert!(host.ledger.lock().unwrap().passes.remove(&renewal), "a pass the service has not seen");
    // The service extends the admission; the next renewal follows the new expiry.
    socket.expiry_in(400);
    let next = loop {
        if let FromHost::Renew(pass) = socket.next(Duration::from_secs(5)).expect("another renewal") {
            break pass;
        }
    };
    assert_eq!(next, "pass-3");
    assert_eq!(host.ledger.lock().unwrap().bad_signatures, 0);
    assert!(host.status.get().online);
}

#[test]
fn a_failed_renewal_is_retried_and_a_revoked_one_stops_the_host() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    host.ledger.lock().unwrap().challenge_fails.push_back(IssueError::new(IssueKind::Failed, "busy"));
    socket.expiry_in(100);
    let pass = loop {
        if let FromHost::Renew(pass) = socket.next(Duration::from_secs(5)).expect("a renewal after the retry") {
            break pass;
        }
    };
    assert_eq!(pass, "pass-2");
    host.ledger.lock().unwrap().pass_fails.push_back(IssueError::new(IssueKind::Revoked, "host revoked"));
    socket.expiry_in(100);
    let stopped = until(&host.status, "stopped", |s| s.phase == Phase::Stopped);
    assert!(stopped.note.as_deref().unwrap().contains("revoked"), "{stopped:?}");
    assert!(!stopped.online);
    assert!(service.quiet_for(Duration::from_millis(300)), "a revoked host does not dial again");
}

#[test]
fn every_close_code_is_answered_as_the_tunnel_spec_says() {
    // Dial again with a new pass.
    for code in [
        close::UNAUTHORISED,
        close::HOST_OFFLINE,
        close::SLOW,
        close::NORMAL,
        close::GOING_AWAY,
        close::PROTOCOL_ERROR,
        close::TOO_BIG,
    ] {
        let ledger = Shared::default();
        let service = FakeService::start(ledger.clone());
        let host = fixture(&service, &ledger, |_| {});
        let mut first = service.accept();
        assert_eq!(first.authorization, "Bearer pass-1");
        first.close(code);
        let second = service.accept();
        assert_eq!(second.authorization, "Bearer pass-2", "{code}: a fresh pass for every dial");
        until(&host.status, "online again", |s| s.online);
    }
    // Stop: re-pair, or another connection of this host took over.
    for (code, why) in [(close::REVOKED, "revoked"), (close::REPLACED, "replaced")] {
        let ledger = Shared::default();
        let service = FakeService::start(ledger.clone());
        let host = fixture(&service, &ledger, |_| {});
        service.accept().close(code);
        let stopped = until(&host.status, "stopped", |s| s.phase == Phase::Stopped);
        assert!(stopped.note.as_deref().unwrap().contains(why), "{code}: {stopped:?}");
        assert!(service.quiet_for(Duration::from_millis(300)), "{code}: no new dial");
    }
    // Quota: tell the user and wait long.
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    service.accept().close(close::QUOTA);
    let waiting = until(&host.status, "waiting", |s| s.phase == Phase::Waiting);
    assert!(waiting.note.as_deref().unwrap().contains("quota"), "{waiting:?}");
    assert!(service.quiet_for(Duration::from_millis(300)), "no dial until the quota wait has passed");
}

#[test]
fn a_dropped_socket_or_a_silent_service_dials_again() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.idle_timeout = Duration::from_millis(300));
    let first = service.accept();
    drop(first); // no close frame: the TCP connection just goes
    let _second = service.accept();
    let admitted = Instant::now();
    until(&host.status, "online", |s| s.online);
    // Silence past the idle timeout: the second socket stays open but says nothing.
    let third = service.accept();
    assert!(admitted.elapsed() >= Duration::from_millis(250), "{:?}", admitted.elapsed());
    assert_eq!(third.authorization, "Bearer pass-3");
}

#[test]
fn a_refused_upgrade_is_retried_with_a_new_pass() {
    let ledger = Shared::default();
    ledger.lock().unwrap().refuse_upgrades = 2;
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let socket = service.accept();
    assert_eq!(socket.authorization, "Bearer pass-3", "two refused, the third admitted");
    assert_eq!(host.ledger.lock().unwrap().upgrades, 3);
    until(&host.status, "online", |s| s.online);
}

#[test]
fn the_host_closes_channels_it_cannot_serve_and_ones_that_break_the_protocol() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.max_channels = 1);
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    socket.send(service::joined(1));
    socket.send(service::joined(2));
    let mut closed = Vec::new();
    // Over the host's own channel limit; then a first frame that is no hello.
    while closed.is_empty() {
        if let Some(FromHost::Close { channel, code }) = socket.next(Duration::from_secs(5)) {
            closed.push((channel, code));
        }
    }
    assert_eq!(closed, [(2, close::QUOTA)]);
    socket.send(tunnel::data(1, b"{\"kind\":\"ping\"}"));
    let refused = loop {
        if let FromHost::Close { channel: 1, code } = socket.next(Duration::from_secs(5)).expect("the channel's end") {
            break code;
        }
    };
    assert_eq!(refused, close::REFUSED);
    // The slot is free once the host closed the channel, before the service says it left.
    socket.send(service::joined(3));
    socket.send(tunnel::data(3, b"{\"kind\":\"ping\"}"));
    let next = loop {
        if let FromHost::Close { channel: 3, code } = socket.next(Duration::from_secs(5)).expect("the channel's end") {
            break code;
        }
    };
    assert_eq!(next, close::REFUSED, "served, not over the limit");
    socket.send(service::left(1, close::NORMAL));
    socket.send(service::left(3, close::NORMAL));
    until(&host.status, "no channel", |s| s.channels == 0);
    assert!(host.status.get().online, "a channel's end is not the tunnel's");
}

#[test]
fn issuer_refusals_wait_register_again_or_stop() {
    // Not the person's own agent: nothing is dialed, and the reason is shown.
    let ledger = Shared::default();
    ledger.lock().unwrap().register_fails.push_back(IssueError::new(IssueKind::Owner, "not a personal agent"));
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| s.retry = Duration::from_secs(60));
    let waiting = until(&host.status, "waiting", |s| s.phase == Phase::Waiting);
    assert!(waiting.note.as_deref().unwrap().contains("not a personal agent") && !waiting.registered, "{waiting:?}");
    assert!(service.quiet_for(Duration::from_millis(200)));
    drop(host);

    // Registered but not applied yet, then gone: the host registers again after the grace. Ten
    // refusals at least 30 ms apart (60 ms, jittered down to half) outlast a 100 ms grace.
    let ledger = Shared::default();
    for _ in 0..10 {
        ledger.lock().unwrap().challenge_fails.push_back(IssueError::new(IssueKind::Unknown, "no such host"));
    }
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |s| {
        s.backoff = (Duration::from_millis(60), Duration::from_millis(60));
        s.registration_grace = Duration::from_millis(100);
    });
    service.accept();
    assert!(host.ledger.lock().unwrap().registrations.len() >= 2, "registered again once the grace passed");

    // A refused challenge is tried once more at once.
    let ledger = Shared::default();
    ledger.lock().unwrap().pass_fails.push_back(IssueError::new(IssueKind::Challenge, "nonce spent"));
    let service = FakeService::start(ledger.clone());
    let _host = fixture(&service, &ledger, |_| {});
    assert_eq!(service.accept().authorization, "Bearer pass-1");
    assert_eq!(ledger.lock().unwrap().challenges, 2);

    // Revoked at the pass: stop.
    let ledger = Shared::default();
    ledger.lock().unwrap().challenge_fails.push_back(IssueError::new(IssueKind::Revoked, "host revoked"));
    let service = FakeService::start(ledger.clone());
    let host = fixture(&service, &ledger, |_| {});
    let stopped = until(&host.status, "stopped", |s| s.phase == Phase::Stopped);
    assert!(stopped.note.as_deref().unwrap().contains("revoked"));
    assert!(service.quiet_for(Duration::from_millis(200)));
}

#[test]
fn stopping_closes_the_tunnel_normally_and_ends_every_channel() {
    let ledger = Shared::default();
    let service = FakeService::start(ledger.clone());
    let mut host = fixture(&service, &ledger, |_| {});
    let mut socket = service.accept();
    socket.expiry_in(300_000);
    let mut secure = host.paired(&mut socket, 1, &Secret::generate().unwrap(), Access::View);
    socket.hello(1, &mut secure);
    socket.envelope(1, &mut secure);
    let started = Instant::now();
    host.rendezvous.stop();
    assert!(started.elapsed() < wait(Duration::from_secs(2)), "{:?}", started.elapsed());
    let code = loop {
        match socket.ws.read() {
            Ok(tungstenite::Message::Close(frame)) => break frame.map(|f| u16::from(f.code)),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if crate::test_support::nothing_yet(&e) => {}
            Err(e) => panic!("no close frame: {e}"),
        }
    };
    assert_eq!(code, Some(close::NORMAL));
    let status = host.status.get();
    assert!(!status.online && status.channels == 0, "{status:?}");
}

#[test]
fn close_codes_map_to_dial_again_wait_or_stop() {
    let settings = Settings::new("wss://r.example/t", "h");
    let next = |code| after(Ended::Closed(Some(code)), &settings).unwrap();
    assert!(matches!(next(close::UNAUTHORISED), Next::Again(_)));
    assert!(matches!(next(close::REVOKED), Next::Stop(_)));
    assert!(matches!(next(close::REPLACED), Next::Stop(_)));
    assert_eq!(next(close::QUOTA), Next::Wait(settings.quota_wait, after_quota(&settings)));
    for code in [close::HOST_OFFLINE, close::SLOW, close::NORMAL, close::ABNORMAL, close::INTERNAL, 1002, 4999] {
        assert!(matches!(next(code), Next::Again(_)), "{code}");
    }
    assert_eq!(after(Ended::Stopped, &settings), None);
    assert!(matches!(after(Ended::Revoked("x".into()), &settings), Some(Next::Stop(_))));
    assert!(matches!(after(Ended::Idle, &settings), Some(Next::Again(_))));
}

fn after_quota(settings: &Settings) -> String {
    match after(Ended::Closed(Some(close::QUOTA)), settings).unwrap() {
        Next::Wait(_, note) => note,
        other => panic!("{other:?}"),
    }
}

#[test]
fn backoff_doubles_with_jitter_to_its_limit_and_resets() {
    let mut backoff = super::Backoff::new((Duration::from_millis(100), Duration::from_millis(400)));
    let waits: Vec<_> = (0..5).map(|_| backoff.next()).collect();
    for (wait, step) in waits.iter().zip([100, 200, 400, 400, 400]) {
        assert!(*wait >= Duration::from_millis(step / 2) && *wait <= Duration::from_millis(step), "{waits:?}");
    }
    backoff.reset();
    assert!(backoff.next() <= Duration::from_millis(100));
}

#[test]
fn the_host_name_defaults_to_the_machine_and_fits_the_service() {
    assert_eq!(super::host_name("  laptop  "), "laptop");
    assert_eq!(super::host_name(&"x".repeat(200)).chars().count(), 80);
    assert_eq!(super::host_name("a\u{7}b"), "ab");
    assert!(!super::host_name("").is_empty());
}

#[test]
fn the_rendezvous_modules_name_no_product() {
    let products = [["re", "lay"].concat(), ["atom", "isk"].concat()];
    for (name, source) in [
        ("mod.rs", include_str!("mod.rs")),
        ("channel.rs", include_str!("channel.rs")),
        ("devices.rs", include_str!("devices.rs")),
        ("issuer.rs", include_str!("issuer.rs")),
        ("keys.rs", include_str!("keys.rs")),
        ("socket.rs", include_str!("socket.rs")),
        ("tunnel.rs", include_str!("tunnel.rs")),
        ("tests.rs", include_str!("tests.rs")),
        ("atomics-core tunnel.rs", include_str!("../../../../atomics-core/src/tunnel.rs")),
        ("atomics-core noise.rs", include_str!("../../../../atomics-core/src/noise.rs")),
        ("atomics-core noise tests", include_str!("../../../../atomics-core/src/noise/tests.rs")),
        ("pairing window", include_str!("../../app/pairing.rs")),
        ("journal", include_str!("../journal.rs")),
        ("ops", include_str!("../ops.rs")),
        ("advert", include_str!("../../../../atomics-core/src/advert.rs")),
        ("mutations", include_str!("../../app/mutations.rs")),
    ] {
        for product in &products {
            assert!(!source.to_lowercase().contains(product), "{name} names a product");
        }
    }
}

/// A streamable HTTP MCP server with the three tools of a reference profile. `owner` says
/// whether whoami names a personal agent's owner; `refuse` makes the pass tool refuse with a
/// code, echoing the signature it got.
fn issuer_server(owner: bool, refuse: Option<&'static str>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    std::thread::spawn(move || {
        let mut signing: Option<Vec<u8>> = None;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            let ok = |content: Value| json!({"structuredContent": content, "content": [{"type": "text", "text": content.to_string()}]});
            let answer = match body["method"].as_str().unwrap_or_default() {
                "initialize" => Some(json!({"protocolVersion": "2025-06-18", "capabilities": {}})),
                "tools/call" => {
                    let (name, args) =
                        (body["params"]["name"].as_str().unwrap().to_string(), body["params"]["arguments"].clone());
                    recorded.lock().unwrap().push(json!({"tool": name, "arguments": args}));
                    Some(match name.as_str() {
                        "whoami" => ok(if owner {
                            json!({"userId": "agent-1", "kind": "personalAgent", "displayName": "Example agent", "ownerUserId": "human-1"})
                        } else {
                            json!({"userId": "agent-2", "kind": "agent", "displayName": "Product agent"})
                        }),
                        "register_host" => {
                            signing = Some(URL_SAFE_NO_PAD.decode(args["signingPublicKey"].as_str().unwrap()).unwrap());
                            ok(json!({"hostId": HOST.to_uppercase(), "operationId": "op-1", "status": "Accepted"}))
                        }
                        "host_pass" if args.get("nonce").is_none() => {
                            ok(json!({"hostId": HOST, "step": "nonce", "nonce": "n-1"}))
                        }
                        "host_pass" => {
                            let signature = args["signature"].as_str().unwrap();
                            if let Some(code) = refuse {
                                json!({"isError": true, "structuredContent": {"code": code, "message": "refused"},
                                    "content": [{"type": "text", "text": format!("refused signature {signature}")}]})
                            } else {
                                let message = tunnel::pass_message(
                                    args["hostId"].as_str().unwrap(),
                                    args["nonce"].as_str().unwrap(),
                                );
                                let verified = UnparsedPublicKey::new(&ED25519, signing.clone().unwrap())
                                    .verify(message.as_bytes(), &URL_SAFE_NO_PAD.decode(signature).unwrap())
                                    .is_ok();
                                ok(
                                    json!({"hostId": HOST, "step": "pass", "pass": if verified { "pass-1" } else { "unverified" }}),
                                )
                            }
                        }
                        other => panic!("unexpected tool {other}"),
                    })
                }
                _ => None,
            };
            let response = match answer {
                Some(result) => {
                    let text = json!({"jsonrpc": "2.0", "id": body["id"], "result": result}).to_string();
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    )
                }
                None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            };
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (url, calls)
}

fn profile(url: &str) -> atomics_core::tickets::TicketConfig {
    let mut config: atomics_core::tickets::TicketConfig = toml::from_str(&format!(
        r#"system = "example"
[server]
url = "{url}"
[rendezvous]
label = "Example rendezvous"
errors = {{ code = "code", unknown = ["host_unknown"], revoked = ["host_revoked"], challenge = ["nonce_invalid", "signature_invalid"], owner = ["owner_required"], throttled = ["rate_limited"] }}
[operations.whoami]
tool = "whoami"
result = {{ id = "userId", kind = "kind", name = "displayName", owner_id = "ownerUserId" }}
[operations.rendezvous_register]
tool = "register_host"
arguments = {{ idempotencyKey = "{{nonce}}", signingPublicKey = "{{signing_key}}", noisePublicKey = "{{noise_key}}", displayName = "{{host_name}}", protocolMajor = "{{protocol_major}}" }}
result = {{ host_id = "hostId" }}
[operations.rendezvous_challenge]
tool = "host_pass"
arguments = {{ hostId = "{{host_id}}" }}
result = {{ challenge = "nonce" }}
[operations.rendezvous_pass]
tool = "host_pass"
arguments = {{ hostId = "{{host_id}}", nonce = "{{challenge}}", signature = "{{signature}}" }}
result = {{ pass = "pass" }}"#
    ))
    .unwrap();
    config.timeout_secs = 5;
    config
}

fn keys() -> (crate::test_support::TempDir, DeviceKeys) {
    let (dir, _, store) = crate::credentials::tests::machine(false);
    let keys = DeviceKeys::load_or_create(&store, &dir.path().join("home")).unwrap();
    (dir, keys)
}

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn the_profile_issuer_registers_as_the_personal_agent_and_buys_a_signed_pass() {
    let (url, calls) = issuer_server(true, None);
    let published = Arc::new(Published::default());
    published.set(Some(Arc::new(profile(&url))));
    let mut issuer = ProfileIssuer::new(published, crate::mcp_client::OAuth::current()).with_env(no_env);
    let (_dir, keys) = keys();
    assert_eq!(issuer.label(), "Example rendezvous");
    let host = issuer.register(&keys, "laptop", 1).unwrap();
    assert_eq!(host, HOST, "the id is lowercase and hyphenated, as it is signed");
    let pass = issuer::buy_pass(&mut issuer, &keys, &host).unwrap();
    assert_eq!(&*pass.0, "pass-1", "the server verified the signature");
    let calls = calls.lock().unwrap();
    let tools: Vec<_> = calls.iter().map(|c| c["tool"].as_str().unwrap().to_string()).collect();
    assert_eq!(tools, ["whoami", "register_host", "host_pass", "host_pass"]);
    let register = &calls[1]["arguments"];
    assert_eq!(register["protocolMajor"], 1, "a number, not text");
    assert_eq!(
        (register["displayName"].as_str(), register["signingPublicKey"].as_str()),
        (Some("laptop"), Some(keys.signing_public().as_str()))
    );
    assert_eq!(register["noisePublicKey"].as_str(), Some(keys.noise_public().as_str()));
    assert!(uuid::Uuid::parse_str(register["idempotencyKey"].as_str().unwrap()).is_ok());
    assert_eq!(calls[2]["arguments"], json!({"hostId": HOST}), "the first step names only the host");
    assert_eq!(calls[3]["arguments"]["nonce"], "n-1");
}

#[test]
fn the_profile_issuer_refuses_an_agent_without_an_owner_before_registering() {
    let (url, calls) = issuer_server(false, None);
    let published = Arc::new(Published::default());
    published.set(Some(Arc::new(profile(&url))));
    let mut issuer = ProfileIssuer::new(published, crate::mcp_client::OAuth::current()).with_env(no_env);
    let (_dir, keys) = keys();
    let error = issuer.register(&keys, "laptop", 1).unwrap_err();
    assert_eq!(error.kind, IssueKind::Owner);
    assert!(error.message.contains("Product agent") && error.message.contains("personal agent"), "{}", error.message);
    let tools: Vec<_> = calls.lock().unwrap().iter().map(|c| c["tool"].as_str().unwrap().to_string()).collect();
    assert_eq!(tools, ["whoami"], "nothing registered");
}

#[test]
fn the_profile_issuer_classifies_refusals_by_code_and_never_shows_the_signature() {
    for (code, kind) in [
        ("host_unknown", IssueKind::Unknown),
        ("host_revoked", IssueKind::Revoked),
        ("rate_limited", IssueKind::Throttled),
        ("invalid_request", IssueKind::Failed),
    ] {
        let (url, _) = issuer_server(true, Some(code));
        let published = Arc::new(Published::default());
        published.set(Some(Arc::new(profile(&url))));
        let mut issuer = ProfileIssuer::new(published, crate::mcp_client::OAuth::current()).with_env(no_env);
        let (_dir, keys) = keys();
        let host = issuer.register(&keys, "laptop", 1).unwrap();
        let signature = keys.sign_challenge(&host, "n-1");
        let error = issuer.pass(&host, "n-1", &signature).err().expect("refused");
        assert_eq!(error.kind, kind, "{code}");
        assert!(!error.message.contains(&signature), "{code}: {}", error.message);
    }
    // A refused challenge twice in a row is not retried a third time.
    let (url, calls) = issuer_server(true, Some("nonce_invalid"));
    let published = Arc::new(Published::default());
    published.set(Some(Arc::new(profile(&url))));
    let mut issuer = ProfileIssuer::new(published, crate::mcp_client::OAuth::current()).with_env(no_env);
    let (_dir, keys) = keys();
    let host = issuer.register(&keys, "laptop", 1).unwrap();
    assert_eq!(issuer::buy_pass(&mut issuer, &keys, &host).err().unwrap().kind, IssueKind::Challenge);
    assert_eq!(
        calls.lock().unwrap().iter().filter(|c| c["tool"] == "host_pass").count(),
        4,
        "two challenges, two passes"
    );
}

#[test]
fn the_profile_issuer_needs_a_profile_with_every_operation() {
    let published = Arc::new(Published::default());
    let mut issuer = ProfileIssuer::new(published.clone(), crate::mcp_client::OAuth::current()).with_env(no_env);
    let (_dir, keys) = keys();
    assert_eq!(issuer.register(&keys, "h", 1).unwrap_err().kind, IssueKind::Setup);
    assert_eq!(issuer.label(), "the rendezvous service");
    let mut config = profile("http://127.0.0.1:9/mcp");
    config.operations.rendezvous_pass = None;
    published.set(Some(Arc::new(config)));
    let error = issuer.challenge(HOST).unwrap_err();
    assert_eq!(error.kind, IssueKind::Setup);
    assert!(error.message.contains("operations.rendezvous_pass"), "{}", error.message);
    let mut config = profile("http://127.0.0.1:9/mcp");
    config.operations.rendezvous_register.as_mut().unwrap().result.host_id.clear();
    config.operations.whoami = None;
    published.set(Some(Arc::new(config)));
    assert!(issuer.register(&keys, "h", 1).unwrap_err().message.contains("operations.whoami"));
}
