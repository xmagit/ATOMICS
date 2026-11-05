use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_core::diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
use atomics_core::mirror::{Applied, Mirror};
use atomics_core::plan::PlanStatus;
use atomics_core::projection::{ApprovalInfo, DiffFileSummary, DiffSummary, PlanInfo, Projections, SessionInfo};
use atomics_core::session::AgentStatus;
use atomics_core::wire::{self, CAPABILITIES, Envelope, ErrorCode, Message, ResetReason, Resume, Seq, VERSION};
use tungstenite::client::IntoClientRequest;
use tungstenite::{HandshakeError, WebSocket};

use super::hub::{Hub, RingLimits, Taken};
use super::{DiffSource, Server, ServerConfig, SessionConfig, Terminals};

pub(crate) fn wait(base: Duration) -> Duration {
    crate::test_support::scaled_timeout(base)
}

pub(crate) fn session(id: &str, title: &str, status: AgentStatus) -> SessionInfo {
    SessionInfo {
        previews: Vec::new(),
        id: id.into(),
        agent: "claude".into(),
        title: title.into(),
        cwd: "/w/a".into(),
        worktree: Some("/w/a".into()),
        branch: Some("fix/a".into()),
        status,
        chief: false,
        task: None,
        pending_approvals: 0,
    }
}

fn plan(id: &str, goal: &str) -> PlanInfo {
    PlanInfo {
        ticket_tree: None,
        id: id.into(),
        status: PlanStatus::Open,
        goal: goal.into(),
        project: None,
        approved: false,
        integration: None,
        tasks: Vec::new(),
        digest: String::new(),
        ticket: None,
    }
}

fn approval(id: &str) -> ApprovalInfo {
    ApprovalInfo {
        id: id.into(),
        session: "a".into(),
        tool: "Bash".into(),
        command: Some("git status".into()),
        cwd: "/w/a".into(),
        reason: None,
        warning: None,
    }
}

fn summary(worktree: &str, revision: u64) -> DiffSummary {
    DiffSummary {
        worktree: worktree.into(),
        revision,
        base_ref: "main".into(),
        base: "abc123".into(),
        partial: false,
        files: vec![DiffFileSummary {
            old_path: None,
            new_path: Some("a.rs".into()),
            status: FileStatus::Added,
            binary: false,
            additions: 1,
            deletions: 0,
            omitted: 0,
        }],
        comments: Vec::new(),
    }
}

fn file_diff() -> FileDiff {
    FileDiff {
        old_path: None,
        new_path: Some("a.rs".into()),
        status: FileStatus::Added,
        binary: false,
        hunks: vec![Hunk {
            header: "@@ -0,0 +1 @@".into(),
            old_start: 0,
            new_start: 1,
            lines: vec![DiffLine {
                kind: LineKind::Added,
                old_no: None,
                new_no: Some(1),
                text: "fn main() {}".into(),
                emphasis: Vec::new(),
            }],
        }],
        omitted: 0,
    }
}

#[derive(Default)]
struct FakeDiffs(Mutex<HashMap<String, (u64, FileDiff)>>);

impl DiffSource for FakeDiffs {
    fn folder_revision(&self, worktree: &str) -> Option<u64> {
        self.0.lock().unwrap().get(worktree).map(|(revision, _)| *revision)
    }

    fn folder(
        &self,
        worktree: &str,
        path: &str,
        offset: u32,
        budget: usize,
    ) -> Result<atomics_core::folder::Page, wire::WireError> {
        let revision =
            self.folder_revision(worktree).ok_or_else(|| wire::WireError::new(ErrorCode::NotFound, "no listing"))?;
        let paths: Vec<_> = (0..300).map(|i| format!("src/file{i:03}.rs")).collect();
        atomics_core::folder::page(paths.iter().map(String::as_str), worktree, path, revision, offset, budget)
    }
    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)> {
        let files = self.0.lock().unwrap();
        let (revision, file) = files.get(worktree)?;
        (file.new_path.as_deref() == Some(path)).then(|| (*revision, file.clone()))
    }

    fn moved_on(&self, worktree: &str, revision: u64) -> bool {
        self.0.lock().unwrap().get(worktree).is_some_and(|(current, _)| *current != revision)
    }
}

fn populate(p: &Projections) {
    p.publish_sessions(vec![session("a", "fix the parser", AgentStatus::Working)]);
    p.publish_plans(vec![plan("p1", "ship it")]);
    p.publish_approvals(vec![approval("x1")]);
    p.publish_diffs(vec![summary("/w/a", 1)]);
}

fn config() -> ServerConfig {
    ServerConfig {
        poll_interval: Duration::from_millis(5),
        session: SessionConfig { tick: Duration::from_millis(5), ..SessionConfig::default() },
        ..ServerConfig::default()
    }
}

struct Fixture {
    server: Server,
    projections: Arc<Projections>,
    diffs: Arc<FakeDiffs>,
}

fn start_with(config: ServerConfig) -> Fixture {
    let projections = Arc::new(Projections::default());
    let diffs = Arc::new(FakeDiffs::default());
    diffs.0.lock().unwrap().insert("/w/a".into(), (1, file_diff()));
    let server = Server::start(
        config,
        projections.clone(),
        diffs.clone(),
        Arc::new(Terminals::default()),
        Arc::new(super::NoCommands),
    )
    .unwrap();
    Fixture { server, projections, diffs }
}

fn start() -> Fixture {
    let fixture = start_with(config());
    populate(&fixture.projections);
    fixture
}

#[test]
fn folder_subscriptions_page_follow_changes_and_stop_on_unsubscribe() {
    use wire::Topic;
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| !m.diffs.is_empty());
    let topic = Topic::Folder { worktree: "/w/a".into(), path: "src".into() };
    client.send(&Envelope::new(Message::Subscribe { topics: vec![topic.clone()] }));
    let page = loop {
        if let Message::FolderPage(page) = client.next().unwrap().message {
            break page;
        }
    };
    assert_eq!(page.entries.len(), atomics_core::folder::PAGE_ENTRIES);
    assert_eq!((page.offset, page.next_offset), (0, Some(128)));
    client.send(&Envelope::new(Message::FolderGet {
        worktree: "/w/a".into(),
        path: "src".into(),
        revision: page.revision,
        offset: 128,
    }));
    let second = loop {
        if let Message::FolderPage(page) = client.next().unwrap().message {
            break page;
        }
    };
    assert_eq!(second.offset, 128);
    assert!(page.entries.last().unwrap().path < second.entries[0].path);
    f.diffs.0.lock().unwrap().get_mut("/w/a").unwrap().0 = 2;
    let changed = loop {
        if let Message::FolderPage(page) = client.next().unwrap().message {
            break page;
        }
    };
    assert_eq!((changed.revision, changed.offset), (Seq(2), 0));
    client.send(&Envelope::new(Message::FolderGet {
        worktree: "/w/a".into(),
        path: "src".into(),
        revision: Seq(1),
        offset: 128,
    }));
    assert!(matches!(client.next().unwrap().message, Message::Error { code: ErrorCode::StaleRevision, .. }));
    client.send(&Envelope::new(Message::Unsubscribe { topics: vec![topic] }));
    client.send(&Envelope::new(Message::Ping { nonce: Some("unsubscribed".into()) }));
    while !matches!(client.next().unwrap().message, Message::Pong { .. }) {}
    f.diffs.0.lock().unwrap().get_mut("/w/a").unwrap().0 = 3;
    client.send(&Envelope::new(Message::Ping { nonce: Some("after-change".into()) }));
    assert!(matches!(client.next().unwrap().message, Message::Pong { .. }));

    let mut old = TestClient::connect(f.server.addr(), f.server.token());
    old.send(&Envelope::new(Message::Hello {
        client_id: "old-explorer".into(),
        versions: vec![VERSION],
        capabilities: vec!["board".into(), "diffs".into()],
        prior_epoch: None,
        prior_cursor: None,
    }));
    old.sync_until(|m| !m.diffs.is_empty());
    old.send(&Envelope::new(Message::Subscribe {
        topics: vec![Topic::Folder { worktree: "/w/a".into(), path: String::new() }],
    }));
    assert!(matches!(old.next().unwrap().message, Message::Error { code: ErrorCode::Unauthorised, .. }));
}

fn board_matches(m: &Mirror, p: &Projections) -> bool {
    fn sorted<T: Clone>(v: &[T], key: impl Fn(&T) -> String) -> Vec<T> {
        let mut v = v.to_vec();
        v.sort_by_key(|x| key(x));
        v
    }
    let l = p.load();
    m.sessions.values().cloned().collect::<Vec<_>>() == sorted(&l.sessions.data, |s| s.id.clone())
        && m.plans.values().cloned().collect::<Vec<_>>() == sorted(&l.plans.data, |s| s.id.clone())
        && m.approvals.values().cloned().collect::<Vec<_>>() == sorted(&l.approvals.data, |s| s.id.clone())
        && m.diffs.values().cloned().collect::<Vec<_>>() == sorted(&l.diffs.data, |s| s.worktree.clone())
}

/// A native client over a real loopback socket.
pub(crate) struct TestClient {
    pub(crate) ws: WebSocket<TcpStream>,
    pub mirror: Mirror,
    pub welcome: Option<Envelope>,
    pub resolutions: HashMap<String, wire::Disposition>,
}

impl TestClient {
    pub(crate) fn connect(addr: SocketAddr, token: &str) -> Self {
        Self::try_connect(addr, Some(token), None, true).expect("upgrade")
    }

    pub(crate) fn try_connect(
        addr: SocketAddr,
        token: Option<&str>,
        origin: Option<&str>,
        subprotocol: bool,
    ) -> Result<Self, tungstenite::Error> {
        let stream = TcpStream::connect(addr).map_err(tungstenite::Error::Io)?;
        stream.set_read_timeout(Some(wait(Duration::from_secs(5)))).unwrap();
        let mut request = format!("ws://{addr}").into_client_request().unwrap();
        let headers = request.headers_mut();
        if let Some(token) = token {
            headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
        }
        if let Some(origin) = origin {
            headers.insert("Origin", origin.parse().unwrap());
        }
        if subprotocol {
            headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
        }
        match tungstenite::client(request, stream) {
            Ok((ws, _)) => Ok(Self { ws, mirror: Mirror::default(), welcome: None, resolutions: HashMap::new() }),
            Err(HandshakeError::Failure(e)) => Err(e),
            Err(HandshakeError::Interrupted(_)) => panic!("the upgrade timed out"),
        }
    }

    pub(crate) fn hello(&mut self) {
        let (prior_epoch, prior_cursor) = self.mirror.resume_point();
        self.send(&Envelope::new(Message::Hello {
            client_id: "test".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            prior_epoch,
            prior_cursor: prior_cursor.map(Seq),
        }));
    }

    pub(crate) fn send(&mut self, envelope: &Envelope) {
        self.send_text(&serde_json::to_string(envelope).unwrap());
    }

    pub(crate) fn send_text(&mut self, text: &str) {
        self.ws.send(tungstenite::Message::text(text)).expect("send");
    }

    /// The next text frame, or `None` on close, error or timeout. Each of those says so on stderr,
    /// so a failing test tells a read that waited in vain from a connection that ended. An
    /// interrupted read has not ended: a socket read with a timeout stops with EINTR even with
    /// SA_RESTART.
    pub(crate) fn next(&mut self) -> Option<Envelope> {
        let started = Instant::now();
        loop {
            match self.ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    return Some(wire::decode(t.as_str()).expect("the host sends valid frames"));
                }
                Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(tungstenite::Message::Close(frame)) => {
                    eprintln!("test client: the host closed the connection after {:?}: {frame:?}", started.elapsed());
                    return None;
                }
                Err(e) => {
                    eprintln!("test client: the read ended after {:?}: {e}", started.elapsed());
                    return None;
                }
                Ok(_) => {}
            }
        }
    }

    /// Read one frame and apply it to the mirror.
    pub(crate) fn step(&mut self) -> Option<Envelope> {
        let envelope = self.next()?;
        if let Message::ApprovalResolved { id, disposition, .. } = &envelope.message {
            self.resolutions.insert(id.clone(), *disposition);
        }
        if matches!(envelope.message, Message::Welcome { .. }) {
            self.welcome = Some(envelope.clone());
        }
        self.mirror.apply(&envelope).unwrap_or_else(|gap| panic!("gap {gap:?} at {:?}", envelope.message));
        Some(envelope)
    }

    pub(crate) fn sync_until(&mut self, done: impl Fn(&Mirror) -> bool) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while !done(&self.mirror) {
            assert!(Instant::now() < deadline, "the client did not catch up: {:?}", self.mirror);
            assert!(self.step().is_some(), "the connection ended early");
        }
    }

    pub(crate) fn resume(&self) -> Option<Resume> {
        match self.welcome.as_ref()?.message {
            Message::Welcome { resume, .. } => Some(resume),
            _ => None,
        }
    }
}

fn status_of(error: tungstenite::Error) -> u16 {
    match error {
        tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

#[test]
fn a_loopback_client_gets_the_board_and_a_diff() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    assert!(f.server.addr().ip().is_loopback());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    assert_eq!(
        client.resume(),
        Some(Resume::Reset { cursor: Seq(client.mirror.cursor), reason: ResetReason::NewClient })
    );
    assert_eq!(client.mirror.diffs["/w/a"].files[0].new_path.as_deref(), Some("a.rs"));

    client.send(
        &Envelope::new(Message::DiffGet {
            worktree: "/w/a".into(),
            path: "a.rs".into(),
            revision: Some(Seq(1)),
            offset: None,
        })
        .reply_to(Some("r1".into())),
    );
    let reply = client.step().unwrap();
    assert_eq!(reply.request_id.as_deref(), Some("r1"));
    let Message::DiffFile { body, truncated, next_offset } = reply.message else {
        panic!("expected diff_file, got {:?}", reply.message)
    };
    assert_eq!((body.revision, body.file), (1, file_diff()));
    assert_eq!((truncated, next_offset), (false, None), "a small file is one page");

    // A later change arrives as an event and the copy follows it.
    f.projections.publish_sessions(vec![session("a", "fix the parser", AgentStatus::Done)]);
    client.sync_until(|m| board_matches(m, &f.projections));
    assert_eq!(client.mirror.sessions["a"].status, AgentStatus::Done);
}

#[test]
fn a_stale_or_unknown_diff_request_is_an_error_not_silence() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for (path, revision, code) in
        [("a.rs", Some(Seq(9)), ErrorCode::StaleRevision), ("nope.rs", None, ErrorCode::NotFound)]
    {
        client.send(&Envelope::new(Message::DiffGet {
            worktree: "/w/a".into(),
            path: path.into(),
            revision,
            offset: None,
        }));
        let Message::Error { code: got, .. } = client.step().unwrap().message else { panic!("expected an error") };
        assert_eq!(got, code);
    }
}

#[test]
fn a_diff_file_that_never_existed_counts_but_one_a_later_revision_dropped_does_not() {
    let mut cfg = config();
    cfg.session.max_errors = 2;
    let f = start_with(cfg);
    populate(&f.projections);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    let get = |path: &str, revision: u64| {
        Envelope::new(Message::DiffGet {
            worktree: "/w/a".into(),
            path: path.into(),
            revision: Some(Seq(revision)),
            offset: None,
        })
    };
    // Told of at revision 0 and gone at revision 1: a race, however often it happens.
    for _ in 0..5 {
        client.send(&get("gone.rs", 0));
        let code = loop {
            if let Message::Error { code, .. } = client.step().expect("still connected").message {
                break code;
            }
        };
        assert_eq!(code, ErrorCode::NotFound);
    }
    for _ in 0..6 {
        client.send(&get("nope.rs", 1));
    }
    let mut refused = 0;
    while let Some(envelope) = client.next() {
        refused += matches!(envelope.message, Message::Error { code: ErrorCode::NotFound, .. }) as u32;
    }
    assert_eq!(refused, 3, "a file the diff never had counts: the third ends the connection");
}

#[test]
fn upgrades_without_the_token_a_good_origin_or_the_subprotocol_fail() {
    let mut cfg = config();
    cfg.allowed_origins = vec!["https://app.example".into()];
    let f = start_with(cfg);
    let (addr, token) = (f.server.addr(), f.server.token().to_string());
    assert_eq!(status_of(TestClient::try_connect(addr, None, None, true).err().unwrap()), 401);
    assert_eq!(status_of(TestClient::try_connect(addr, Some("wrong"), None, true).err().unwrap()), 401);
    assert_eq!(
        status_of(TestClient::try_connect(addr, Some(&token), Some("https://evil.example"), true).err().unwrap()),
        403
    );
    assert_eq!(status_of(TestClient::try_connect(addr, Some(&token), None, false).err().unwrap()), 400);
    assert!(TestClient::try_connect(addr, Some(&token), Some("https://app.example"), true).is_ok());
    assert!(TestClient::try_connect(addr, Some(&token), None, true).is_ok());
}

/// A client that reaches the server through a forward on another port (an ssh forward from a Mac to its
/// VM) still names the loopback; a page a DNS-rebinding attack drives names its own host and is refused.
#[test]
fn the_host_header_must_name_the_loopback_on_any_port() {
    for (host, ok) in [
        ("127.0.0.1:47800", true),
        ("localhost:1", true),
        ("LOCALHOST:65535", true),
        ("127.0.0.1", false),
        ("127.0.0.1:", false),
        ("127.0.0.1:x", false),
        ("evil.example:47800", false),
        ("127.0.0.1.evil.example:80", false),
        ("[::1]:47800", false),
    ] {
        assert_eq!(super::loopback_host(host), ok, "{host}");
    }
    let f = start();
    let token = f.server.token().to_string();
    let upgrade = |host: &str| {
        let stream = TcpStream::connect(f.server.addr()).unwrap();
        stream.set_read_timeout(Some(wait(Duration::from_secs(5)))).unwrap();
        let mut request = format!("ws://{}", f.server.addr()).into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert("Host", host.parse().unwrap());
        headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
        match tungstenite::client(request, stream) {
            Ok(_) => 101,
            Err(HandshakeError::Failure(e)) => status_of(e),
            Err(HandshakeError::Interrupted(_)) => panic!("the upgrade timed out"),
        }
    };
    assert_eq!(upgrade("127.0.0.1:1"), 101, "a forward's own port");
    assert_eq!(upgrade("rebound.example:80"), 403);
}

/// A paired client's token gets in across restarts, as a controller under its own name; the per-start
/// token still works, and a token that was replaced or unpaired does not.
#[test]
fn a_paired_client_gets_in_with_its_own_token() {
    let dir = tempfile::tempdir().unwrap();
    let clients = Arc::new(super::clients::PairedClients::open(dir.path().join("paired-clients.json")).unwrap());
    let (_, token) = clients.pair("Mac").unwrap();
    let f = start_with(ServerConfig { paired: Some(clients.clone()), ..config() });
    let mut client = TestClient::connect(f.server.addr(), &token);
    client.hello();
    client.step();
    let Some(Message::Welcome { role, .. }) = client.welcome.clone().map(|w| w.message) else { panic!("welcome") };
    assert_eq!(role, wire::Role::Controller);
    assert!(TestClient::try_connect(f.server.addr(), Some(f.server.token()), None, true).is_ok());
    let (_, replaced) = clients.pair("Mac").unwrap();
    assert_eq!(status_of(TestClient::try_connect(f.server.addr(), Some(&token), None, true).err().unwrap()), 401);
    assert!(TestClient::try_connect(f.server.addr(), Some(&replaced), None, true).is_ok());
    clients.unpair("Mac").unwrap();
    assert_eq!(status_of(TestClient::try_connect(f.server.addr(), Some(&replaced), None, true).err().unwrap()), 401);
    // Without paired clients only this start's token gets in.
    let plain = start();
    assert_eq!(status_of(TestClient::try_connect(plain.server.addr(), Some(&token), None, true).err().unwrap()), 401);
}

#[test]
fn malformed_unknown_and_version_mismatched_frames_get_typed_errors() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for (frame, code) in [
        ("{not json", ErrorCode::Malformed),
        ("[1,2]", ErrorCode::Malformed),
        (r#"{"v":{"major":1,"minor":0},"kind":"teleport"}"#, ErrorCode::UnknownKind),
        (r#"{"v":{"major":2,"minor":0},"kind":"ping"}"#, ErrorCode::UnsupportedVersion),
        (r#"{"v":{"major":1,"minor":0},"kind":"state_snapshot"}"#, ErrorCode::Malformed),
        (
            r#"{"v":{"major":1,"minor":0},"kind":"welcome","version":{"major":1,"minor":0},"capabilities":[],"limits":{"max_frame_bytes":1,"max_subscriptions":1,"max_pending_ops":1,"max_queued_frames":1},"role":"controller","resume":{"mode":"replay","after":"0"}}"#,
            ErrorCode::Malformed,
        ),
    ] {
        client.send_text(frame);
        let Message::Error { code: got, .. } = client.step().unwrap().message else {
            panic!("expected an error for {frame}")
        };
        assert_eq!(got, code, "{frame}");
    }
    // The connection survives a bad frame and still answers.
    client.send(&Envelope::new(Message::Ping { nonce: Some("n".into()) }));
    assert!(matches!(client.step().unwrap().message, Message::Pong { nonce: Some(n) } if n == "n"));
}

#[test]
fn a_hello_the_host_cannot_serve_is_refused_before_any_state_is_sent() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client
        .send_text(r#"{"v":{"major":9,"minor":0},"kind":"hello","client_id":"c","versions":[{"major":9,"minor":0}]}"#);
    let Message::Error { code, .. } = client.next().unwrap().message else { panic!("expected an error") };
    assert_eq!(code, ErrorCode::UnsupportedVersion);
    assert!(client.next().is_none(), "the host closes after refusing the version");

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.send(&Envelope::new(Message::Ping { nonce: None }));
    let Message::Error { code, .. } = client.next().unwrap().message else { panic!("expected an error") };
    assert_eq!(code, ErrorCode::Malformed, "the first message must be hello");
    assert!(client.next().is_none());
}

#[test]
fn oversize_and_binary_frames_are_rejected_and_the_server_carries_on() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    client.send_text(&format!(
        r#"{{"v":{{"major":1,"minor":0}},"kind":"ping","nonce":"{}"}}"#,
        "x".repeat(wire::MAX_FRAME_BYTES)
    ));
    // The error frame can lose a race with the close; when it arrives it names the limit.
    while let Some(envelope) = client.next() {
        if let Message::Error { code, .. } = envelope.message {
            assert_eq!(code, ErrorCode::Oversize);
        }
    }

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    client.ws.send(tungstenite::Message::binary(vec![1, 2, 3])).unwrap();
    while let Some(envelope) = client.next() {
        if let Message::Error { code, .. } = envelope.message {
            assert_eq!(code, ErrorCode::Malformed);
        }
    }

    let mut again = TestClient::connect(f.server.addr(), f.server.token());
    again.hello();
    again.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
}

#[test]
fn a_client_that_keeps_sending_garbage_is_disconnected() {
    let mut cfg = config();
    cfg.session.max_errors = 3;
    let f = start_with(cfg);
    populate(&f.projections);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for _ in 0..20 {
        client.send_text("garbage");
    }
    let mut errors = 0;
    while let Some(envelope) = client.next() {
        errors += matches!(envelope.message, Message::Error { .. }) as u32;
    }
    assert!((1..20).contains(&errors), "the host stopped answering after a few errors, got {errors}");
}

#[test]
fn reconnect_replays_what_was_missed() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    let held = client.mirror.clone();
    drop(client);

    f.projections.publish_sessions(vec![
        session("a", "fix the parser", AgentStatus::Done),
        session("b", "second", AgentStatus::Idle),
    ]);
    f.projections.publish_approvals(Vec::new());
    f.projections.publish_diffs(Vec::new());
    f.projections.publish_plans(vec![plan("p1", "ship it, really")]);

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.mirror = held.clone();
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections));
    assert_eq!(client.resume(), Some(Resume::Replay { after: Seq(held.cursor) }));
    assert!(client.mirror.cursor > held.cursor);
    assert!(client.mirror.diffs.is_empty() && client.mirror.approvals.is_empty());
}

#[test]
fn reconnect_resets_when_replay_is_not_possible() {
    let mut cfg = config();
    cfg.ring = RingLimits { entries: 2, ..RingLimits::default() };
    let f = start_with(cfg);
    populate(&f.projections);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    let held = client.mirror.clone();
    drop(client);

    // Too far behind for a ring of two events.
    for i in 0..6 {
        f.projections.publish_sessions(vec![session("a", &format!("title {i}"), AgentStatus::Working)]);
        std::thread::sleep(Duration::from_millis(30));
    }
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.mirror = held.clone();
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > held.cursor);
    assert!(
        matches!(client.resume(), Some(Resume::Reset { reason: ResetReason::CursorTooOld, .. })),
        "{:?}",
        client.resume()
    );

    // A cursor from the future, and one from another boot.
    for (epoch, cursor, reason) in [
        (client.mirror.epoch.clone().unwrap(), 10_000, ResetReason::CursorAhead),
        ("some-earlier-boot".to_string(), 1, ResetReason::EpochChanged),
    ] {
        let mut other = TestClient::connect(f.server.addr(), f.server.token());
        other.send(&Envelope::new(Message::Hello {
            client_id: "t".into(),
            versions: vec![VERSION],
            capabilities: Vec::new(),
            prior_epoch: Some(epoch),
            prior_cursor: Some(Seq(cursor)),
        }));
        other.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
        assert!(matches!(other.resume(), Some(Resume::Reset { reason: got, .. }) if got == reason));
    }
}

#[test]
fn a_client_can_ask_for_a_resync() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    client.mirror.sessions.clear();
    client.send(&Envelope::new(Message::Resync { reason: Some("test".into()), screens: Vec::new() }));
    client.sync_until(|m| board_matches(m, &f.projections));
}

#[test]
fn a_reader_that_stops_reading_is_dropped_without_delaying_anyone() {
    let mut cfg = config();
    cfg.queue_frames = 8;
    cfg.write_timeout = Duration::from_millis(300);
    let f = start_with(cfg);
    populate(&f.projections);
    let addr = f.server.addr();
    let token = f.server.token().to_string();

    let mut slow = TestClient::connect(addr, &token);
    slow.hello();
    slow.sync_until(|m| m.cursor > 0);
    let mut good = TestClient::connect(addr, &token);
    good.hello();
    good.sync_until(|m| m.cursor > 0);
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    // The well-behaved client reads on its own thread for the whole run.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, projections) = (stop.clone(), f.projections.clone());
        std::thread::spawn(move || {
            let until = Instant::now() + wait(Duration::from_secs(60));
            loop {
                let _ = good.ws.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
                if let Ok(tungstenite::Message::Text(t)) = good.ws.read() {
                    let envelope = wire::decode(t.as_str()).unwrap();
                    good.mirror.apply(&envelope).ok();
                }
                if (stop.load(Ordering::Acquire) && board_matches(&good.mirror, &projections)) || Instant::now() > until
                {
                    return good.mirror;
                }
            }
        })
    };

    // `slow` never reads again. The publisher (standing in for the runtime thread) must not feel it.
    let big = "x".repeat(200_000);
    let (mut worst, mut i) = (Duration::ZERO, 0);
    let deadline = Instant::now() + wait(Duration::from_secs(40));
    while f.server.active_clients() > 1 {
        assert!(Instant::now() < deadline, "the stalled client was never dropped");
        i += 1;
        let started = Instant::now();
        f.projections.publish_sessions(vec![session("a", &format!("{i}{big}"), AgentStatus::Working)]);
        worst = worst.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(worst < wait(Duration::from_millis(500)), "publishing waited {worst:?} on a slow reader");
    assert_eq!(f.server.active_clients(), 1, "only the stalled client is dropped");

    stop.store(true, Ordering::Release);
    let mirror = reader.join().unwrap();
    assert!(board_matches(&mirror, &f.projections), "the healthy client converged");
    drop(slow);
}

fn hub(queue_frames: usize) -> Hub {
    Hub::new("e1".into(), RingLimits::default(), queue_frames, 1 << 20)
}

#[test]
fn a_full_queue_drops_the_backlog_and_resyncs_with_a_snapshot() {
    let p = Projections::default();
    let mut hub = hub(2);
    hub.poll(&p);
    let attached = hub.attach(None, None).unwrap();
    for i in 0..5 {
        p.publish_sessions(vec![session("a", &format!("t{i}"), AgentStatus::Working)]);
        hub.poll(&p);
    }
    assert!(matches!(attached.outbox.take(), Taken::Lagged), "five events into a queue of two");
    hub.resync(&attached.outbox).unwrap();
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!("resynced") };
    assert_eq!(frames.len(), 1, "a snapshot replaces the backlog");
    let mut mirror = Mirror::default();
    mirror.apply(&wire::decode(&frames[0].text).unwrap()).unwrap();
    assert_eq!((mirror.cursor, mirror.sessions["a"].title.as_str()), (hub.head(), "t4"));

    // Events after the resync are delivered normally again.
    p.publish_sessions(vec![session("a", "t5", AgentStatus::Done)]);
    hub.poll(&p);
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!("live again") };
    mirror.apply(&wire::decode(&frames[0].text).unwrap()).unwrap();
    assert_eq!(mirror.sessions["a"].title, "t5");
}

#[test]
fn removals_become_events_and_a_removed_diff_is_a_tombstone() {
    let p = Projections::default();
    let mut hub = hub(64);
    populate(&p);
    hub.poll(&p);
    let attached = hub.attach(None, None).unwrap();
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!() };
    let mut mirror = Mirror::default();
    mirror.apply(&wire::decode(&frames[0].text).unwrap()).unwrap();
    p.publish_sessions(Vec::new());
    p.publish_plans(Vec::new());
    p.publish_approvals(Vec::new());
    p.publish_diffs(Vec::new());
    hub.poll(&p);
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!() };
    for frame in frames {
        mirror.apply(&wire::decode(&frame.text).unwrap()).unwrap();
    }
    assert!(board_matches(&mirror, &p));
    assert_eq!(mirror.cursor, hub.head());
}

#[test]
fn the_hosts_own_state_reaches_a_client_that_asked_for_it_and_is_skipped_for_one_that_did_not() {
    use atomics_core::projection::{HostInfo, SignInInfo, SignInState};
    let f = start();
    let hello = |client: &mut TestClient, capabilities: &[&str]| {
        client.send(&Envelope::new(Message::Hello {
            client_id: "test".into(),
            versions: vec![VERSION],
            capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            prior_epoch: None,
            prior_cursor: None,
        }));
    };
    let mut new = TestClient::connect(f.server.addr(), f.server.token());
    hello(&mut new, CAPABILITIES);
    let older: Vec<&str> = CAPABILITIES.iter().copied().filter(|c| *c != wire::HOST).collect();
    let mut old = TestClient::connect(f.server.addr(), f.server.token());
    hello(&mut old, &older);
    for client in [&mut new, &mut old] {
        client.sync_until(|m| m.epoch.is_some());
    }
    let Some(Message::Welcome { may, .. }) = old.welcome.as_ref().map(|w| w.message.clone()) else { panic!() };
    assert!(!may.contains(&atomics_core::ops::Grant::Host), "host_command needs the host capability: {may:?}");

    let expired = HostInfo {
        projects: Vec::new(),
        resources: Some(atomics_core::projection::ResourceInfo { load: Some(4.2), cores: 8, ..Default::default() }),
        workspace: Some("team".into()),
        tickets: None,
        workspaces: vec!["team".into()],
        sign_in: Some(SignInInfo {
            system: "tracker".into(),
            server: "tickets.example.com".into(),
            state: SignInState::Expired,
            reason: Some("the server refused the saved sign-in".into()),
            url: None,
            rejected: None,
            browser_failed: false,
        }),
        ..HostInfo::default()
    };
    f.projections.publish_host(expired.clone());
    new.sync_until(|m| m.host == expired);
    // The older client is sent `skipped` in its place, so its cursor still follows, and then the
    // events after it as usual.
    let head = f.projections.cursor();
    f.projections.publish_sessions(vec![session("b", "next", AgentStatus::Idle)]);
    old.sync_until(|m| m.sessions.contains_key("b"));
    assert_eq!(old.mirror.host, HostInfo::default(), "nothing of the host's own state reached it");
    assert!(old.mirror.cursor > head);
    // A snapshot carries it to everyone, which an older client ignores.
    old.send(&Envelope::new(Message::Resync { reason: None, screens: Vec::new() }));
    old.sync_until(|m| m.host == expired);
}

#[test]
fn an_idle_client_is_pinged_and_a_silent_one_is_closed() {
    let mut cfg = config();
    cfg.session.ping_interval = Duration::from_millis(100);
    cfg.session.idle_timeout = Duration::from_millis(600);
    let f = start_with(cfg);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    let mut pinged = false;
    while let Some(envelope) = client.next() {
        pinged |= matches!(envelope.message, Message::Ping { .. });
    }
    assert!(pinged, "the host pings an idle client");
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_client_limit_is_enforced() {
    let mut cfg = config();
    cfg.max_clients = 1;
    let f = start_with(cfg);
    let first = TestClient::connect(f.server.addr(), f.server.token());
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() < 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(TestClient::try_connect(f.server.addr(), Some(f.server.token()), None, true).is_err());
    drop(first);
}

#[test]
fn the_advertisement_is_private_and_goes_away_with_the_server() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_support::tempdir().unwrap();
    let path = dir.path().join("state").join("protocol.json");
    let mut f = start_with(ServerConfig { advertise: Some(path.clone()), ..config() });
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let advert: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(advert["token"], f.server.token());
    assert_eq!(advert["url"], format!("ws://{}", f.server.addr()));
    assert_eq!(advert["pid"], std::process::id());
    assert!(advert["started_at"].as_u64().unwrap() > 0);
    if let Some(start) = atomics_core::advert::process_start(std::process::id()) {
        assert_eq!(advert["pid_start"], start, "the start time tells this process from a later one with its pid");
    }
    assert!(atomics_core::advert::read(&path).is_ok(), "a client accepts the live file");
    let addr = f.server.addr();
    f.server.stop();
    assert!(!path.exists());
    assert!(TcpStream::connect(addr).is_err(), "the listener is closed");
}

fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + wait(Duration::from_secs(10));
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_connection_that_never_finishes_the_upgrade_is_cut_off_and_cannot_take_client_places() {
    use std::io::Write;
    let mut cfg = config();
    cfg.upgrade_deadline = Duration::from_millis(700);
    cfg.max_pre_auth = 2;
    cfg.max_clients = 1;
    let f = start_with(cfg);
    populate(&f.projections);
    let (addr, token) = (f.server.addr(), f.server.token().to_string());
    let mut real = TestClient::connect(addr, &token);
    real.hello();
    real.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    eventually("the real client holds the only client place", || f.server.active_clients() == 1);

    // One peer trickles a request a byte at a time, faster than any per-read timeout; one is silent.
    let started = Instant::now();
    let mut trickler = TcpStream::connect(addr).unwrap();
    let silent = TcpStream::connect(addr).unwrap();
    trickler.write_all(b"GET / HTTP/1.1\r\nX-Pad: ").unwrap();
    let writer = {
        let mut stream = trickler.try_clone().unwrap();
        std::thread::spawn(move || {
            while stream.write_all(b"a").is_ok() {
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    eventually("both peers are counted as upgrading", || f.server.pending_upgrades() == 2);
    assert_eq!(f.server.active_clients(), 1, "unauthenticated peers take no client place");

    // The pre-auth places are full, so a third connection is refused outright...
    assert!(TestClient::try_connect(addr, Some(&token), None, true).is_err());
    // ...but the authenticated client is not affected.
    f.projections.publish_sessions(vec![session("a", "still served", AgentStatus::Done)]);
    real.sync_until(|m| board_matches(m, &f.projections));

    // The total deadline ends both, the trickler included, and frees their places.
    eventually("the upgrades were not cut off", || f.server.pending_upgrades() == 0);
    assert!(started.elapsed() < wait(Duration::from_secs(5)), "cut off after {:?}", started.elapsed());
    writer.join().unwrap(); // it stops when a write fails: the server closed the connection
    drop((trickler, silent));

    // An authenticated client over the client limit is refused with 503, and gives its pre-auth
    // place back.
    assert_eq!(status_of(TestClient::try_connect(addr, Some(&token), None, true).err().unwrap()), 503);
    eventually("the refused upgrade left the pool", || f.server.pending_upgrades() == 0);
    drop(real);
    eventually("the client place is free again", || f.server.active_clients() == 0);
    assert!(TestClient::try_connect(addr, Some(&token), None, true).is_ok());
}

fn populate_but_sessions(p: &Projections) {
    p.publish_plans(vec![plan("p1", "ship it")]);
    p.publish_approvals(vec![approval("x1")]);
    p.publish_diffs(vec![summary("/w/a", 1)]);
}

#[test]
fn a_board_over_one_frame_is_sent_in_parts_and_applied_whole() {
    let f = start_with(config());
    let title = "t".repeat(120);
    let sessions: Vec<_> = (0..2500).map(|i| session(&format!("s{i:04}"), &title, AgentStatus::Idle)).collect();
    let bytes: usize = sessions.iter().map(wire::json_len).sum();
    assert!(bytes > 2 * wire::MAX_FRAME_BYTES, "the fixture must not fit in a frame ({bytes} bytes)");
    f.projections.publish_sessions(sessions);
    populate_but_sessions(&f.projections);

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    // However the hub met this client (a snapshot, or events it lagged behind and a resync), the
    // board arrives as a snapshot in parts.
    let mut parts = 0;
    let deadline = Instant::now() + wait(Duration::from_secs(20));
    while !board_matches(&client.mirror, &f.projections) {
        assert!(Instant::now() < deadline, "the client did not catch up");
        let envelope = client.next().expect("connected");
        let before = client.mirror.sessions.clone();
        let applied = client.mirror.apply(&envelope).unwrap_or_else(|gap| panic!("gap {gap:?}"));
        if let Message::StateSnapshot { part: Some(part), .. } = &envelope.message {
            parts += 1;
            if part.index + 1 < part.count {
                assert_eq!(applied, Applied::Partial);
                assert_eq!(client.mirror.sessions, before, "the board changes only when the last part lands");
            }
        }
    }
    assert!(parts >= 3, "{parts} parts");
    assert_eq!(client.mirror.sessions.len(), 2500);

    // A client that asks for a resync gets parts again, and live events still follow them.
    client.mirror.sessions.clear();
    client.send(&Envelope::new(Message::Resync { reason: None, screens: Vec::new() }));
    client.sync_until(|m| board_matches(m, &f.projections));
    f.projections.publish_plans(vec![plan("p1", "after the parts")]);
    client.sync_until(|m| board_matches(m, &f.projections));
}

#[test]
fn snapshot_parts_respect_the_budget_and_one_oversize_item_is_an_error() {
    let p = Projections::default();
    populate(&p);
    p.publish_sessions((0..40).map(|i| session(&format!("s{i}"), &"x".repeat(60), AgentStatus::Idle)).collect());
    let mut hub = Hub::new("e1".into(), RingLimits::default(), 64, 1 << 20).with_frame_budget(2000);
    hub.poll(&p);
    let attached = hub.attach(None, None).unwrap();
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!() };
    assert!(frames.len() > 2, "{} parts", frames.len());
    let mut mirror = Mirror::default();
    let welcome = Message::Welcome {
        version: VERSION,
        capabilities: vec![],
        limits: wire::Limits::default(),
        role: wire::Role::Viewer,
        resume: attached.resume,
        may: vec![],
    };
    mirror.apply(&Envelope::host("e1", welcome)).unwrap();
    for (i, frame) in frames.iter().enumerate() {
        assert!(frame.text.len() <= 2000, "part {i} is {} bytes", frame.text.len());
        let envelope = wire::decode(&frame.text).unwrap();
        let Message::StateSnapshot { part: Some(part), .. } = &envelope.message else { panic!("part {i}") };
        assert_eq!((part.index as usize, part.count as usize), (i, frames.len()));
        let last = i + 1 == frames.len();
        assert_eq!(mirror.apply(&envelope), Ok(if last { Applied::Changed } else { Applied::Partial }));
    }
    assert!(board_matches(&mirror, &p));

    let mut tight = Hub::new("e1".into(), RingLimits::default(), 64, 1 << 20).with_frame_budget(600);
    p.publish_sessions(vec![session("big", &"y".repeat(5000), AgentStatus::Idle)]);
    tight.poll(&p);
    let error = tight.attach(None, None).err().expect("one session cannot be cut");
    assert_eq!(error.code, ErrorCode::Oversize);
}

#[test]
fn a_reorder_alone_sends_no_event_but_the_next_snapshot_shows_it() {
    let p = Projections::default();
    let mut hub = hub(64);
    p.publish_sessions(vec![session("a", "A", AgentStatus::Idle), session("b", "B", AgentStatus::Idle)]);
    hub.poll(&p);
    let head = hub.head();
    p.publish_sessions(vec![session("b", "B", AgentStatus::Idle), session("a", "A", AgentStatus::Idle)]);
    hub.poll(&p);
    assert_eq!(hub.head(), head, "a reorder is not an event");
    let attached = hub.attach(None, None).unwrap();
    let Taken::Frames { board: frames, .. } = attached.outbox.take() else { panic!() };
    let Message::StateSnapshot { sessions, revisions, .. } = wire::decode(&frames[0].text).unwrap().message else {
        panic!("expected a snapshot")
    };
    assert_eq!(sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    assert_eq!(revisions.sessions, Seq(2));
}

fn big_file() -> FileDiff {
    let mut file = file_diff();
    file.new_path = Some("big.rs".into());
    file.hunks = (0..3)
        .map(|h| Hunk {
            header: format!("@@ -{0},1500 +{0},1500 @@", h * 1500 + 1),
            old_start: h * 1500 + 1,
            new_start: h * 1500 + 1,
            lines: (0..1500)
                .map(|i| DiffLine {
                    kind: LineKind::Added,
                    old_no: None,
                    new_no: Some(h * 1500 + i + 1),
                    text: format!("line {} {}", h * 1500 + i, "z".repeat(90)),
                    emphasis: Vec::new(),
                })
                .collect(),
        })
        .collect();
    file
}

#[test]
fn a_diff_over_one_frame_comes_in_pages_that_add_up_to_the_file() {
    let f = start();
    let whole = big_file();
    assert!(wire::json_len(&whole) > 2 * wire::MAX_FRAME_BYTES);
    f.diffs.0.lock().unwrap().insert("/w/big".into(), (1, whole.clone()));
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);

    let get =
        |offset| Message::DiffGet { worktree: "/w/big".into(), path: "big.rs".into(), revision: Some(Seq(1)), offset };
    let (mut offset, mut pages, mut lines) = (None, 0, Vec::new());
    loop {
        client.send(&Envelope::new(get(offset)));
        let Message::DiffFile { body, truncated, next_offset } = client.step().unwrap().message else {
            panic!("expected a page")
        };
        pages += 1;
        assert_eq!(truncated, next_offset.is_some());
        for hunk in &body.file.hunks {
            assert!(whole.hunks.iter().any(|w| w.header == hunk.header), "a continuation keeps its header");
            assert_eq!(Some(hunk.new_start), hunk.lines[0].new_no, "and starts at its first line");
            lines.extend(hunk.lines.iter().cloned());
        }
        match next_offset {
            Some(next) => {
                assert_eq!(next as usize, lines.len(), "the next page starts where this one stopped");
                offset = Some(next);
            }
            None => break,
        }
    }
    assert!(pages >= 3, "{pages} pages");
    let all: Vec<_> = whole.hunks.iter().flat_map(|h| h.lines.iter().cloned()).collect();
    assert_eq!(lines, all, "the pages add up to the file, in order");

    // Past the end is an error, not an empty page.
    client.send(&Envelope::new(get(Some(4500))));
    let Message::Error { code, .. } = client.step().unwrap().message else { panic!("expected an error") };
    assert_eq!(code, ErrorCode::NotFound);
}

/// Read until the next board frame (an event, a `skipped` or a snapshot) and return it.
fn next_board(client: &mut TestClient) -> Message {
    loop {
        let message = client.step().expect("connected").message;
        if !matches!(message, Message::Ping { .. } | Message::Pong { .. }) {
            return message;
        }
    }
}

/// Wait until the host has handled everything sent before.
fn ping(client: &mut TestClient) {
    client.send(&Envelope::new(Message::Ping { nonce: Some("sync".into()) }));
    while !matches!(client.step().expect("connected").message, Message::Pong { .. }) {}
}

#[test]
fn subscribing_narrows_diff_and_plan_updates_and_widening_resyncs() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);

    // With no subscription the client gets everything, as before.
    f.projections.publish_diffs(vec![summary("/w/a", 1), summary("/w/b", 1)]);
    client.sync_until(|m| m.diffs.contains_key("/w/b"));

    // One diff topic: only that worktree's updates; the other's arrives as `skipped`, in order.
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Diff { worktree: "/w/a".into() }] }));
    ping(&mut client);
    f.projections.publish_diffs(vec![summary("/w/a", 2), summary("/w/b", 2)]);
    assert!(matches!(next_board(&mut client), Message::DiffSummary { summary, .. } if summary.worktree == "/w/a"));
    let Message::Skipped { cursor } = next_board(&mut client) else { panic!("the other worktree's update is skipped") };
    assert_eq!(cursor, Seq(client.mirror.cursor), "a skipped event still advances the cursor");
    assert_eq!((client.mirror.diffs["/w/a"].revision, client.mirror.diffs["/w/b"].revision), (2, 1));

    // Plans narrow separately; sessions and approvals never do.
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Plan { id: "p1".into() }] }));
    ping(&mut client);
    f.projections.publish_plans(vec![plan("p1", "changed"), plan("p2", "new")]);
    assert!(matches!(next_board(&mut client), Message::PlanUpsert { plan, .. } if plan.id == "p1"));
    assert!(matches!(next_board(&mut client), Message::Skipped { .. }), "p2 is not wanted");
    f.projections.publish_sessions(vec![session("a", "still delivered", AgentStatus::Done)]);
    client.sync_until(|m| m.sessions["a"].status == AgentStatus::Done);
    assert!(!client.mirror.plans.contains_key("p2"));

    // Widening the set means events were missed, so the host sends a fresh snapshot.
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Plan { id: "p2".into() }] }));
    assert!(matches!(next_board(&mut client), Message::StateSnapshot { .. }), "widening resyncs");
    assert!(board_matches(&client.mirror, &f.projections), "and the snapshot carries everything");

    // Dropping the last topic of a kind goes back to every item of it, which also resyncs.
    client.send(&Envelope::new(Message::Unsubscribe { topics: vec![wire::Topic::Diff { worktree: "/w/a".into() }] }));
    assert!(matches!(next_board(&mut client), Message::StateSnapshot { .. }));
    f.projections.publish_diffs(vec![summary("/w/a", 3), summary("/w/b", 3)]);
    client.sync_until(|m| board_matches(m, &f.projections));

    // Narrowing alone sends nothing extra: the client already has everything it keeps.
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Diff { worktree: "/w/b".into() }] }));
    ping(&mut client);
    f.projections.publish_diffs(vec![summary("/w/a", 4), summary("/w/b", 4)]);
    assert!(matches!(next_board(&mut client), Message::Skipped { .. }), "/w/a is left out now");
    assert!(matches!(next_board(&mut client), Message::DiffSummary { summary, .. } if summary.revision == 4));
}

#[test]
fn the_advertisement_a_stopped_server_removes_is_only_its_own() {
    let dir = crate::test_support::tempdir().unwrap();
    let path = dir.path().join("protocol.json");
    let mut f = start_with(ServerConfig { advertise: Some(path.clone()), ..config() });
    // Another instance replaced the file after this one wrote it.
    let theirs = atomics_core::advert::Advertisement::current("ws://127.0.0.1:9", "theirs");
    std::fs::write(&path, serde_json::to_vec(&theirs).unwrap()).unwrap();
    f.server.stop();
    assert!(path.exists(), "a file that is not this server's stays");
}

#[test]
fn trace_is_paged_live_and_only_sent_under_its_capability() {
    use atomics_core::trace::{TraceLog, TraceState};
    use atomics_core::trace_feed::TraceData;
    use atomics_core::{hook::HookEvent, tickets::Cleaner};
    use serde_json::json;
    let f = start_with(ServerConfig { frame_budget: 4096, ..config() });
    let mut log = TraceLog::default();
    for n in 0..100 {
        let hook = HookEvent::from_payload(&json!({
            "session_id": "chief", "hook_event_name": "PreToolUse", "tool_name": "mcp__example__lookup",
            "tool_use_id": format!("call-{n}"), "tool_input": {"query": "hello", "api_key": "do-not-send"}
        }))
        .unwrap();
        log.ingest(&hook, None, &Cleaner::default(), n).unwrap();
    }
    let data =
        TraceData { session: Some("chief".into()), entries: log.entries.into_iter().collect(), ..Default::default() };
    f.projections.trace.publish(data.clone());
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    assert!(matches!(client.next().unwrap().message, Message::Welcome { .. }));
    assert!(matches!(client.next().unwrap().message, Message::StateSnapshot { .. }));
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Trace] }));
    assert_eq!(client.next().unwrap().message, Message::TraceChanged { revision: Seq(1) });
    let mut offset = 0;
    let mut entries = Vec::new();
    loop {
        client.send(
            &Envelope::new(Message::TraceGet { revision: Seq(1), offset, limit: u32::MAX })
                .reply_to(Some("page".into())),
        );
        let envelope = loop {
            let envelope = client.next().unwrap();
            if !matches!(envelope.message, Message::TraceChanged { .. }) {
                break envelope;
            }
        };
        assert_eq!(envelope.request_id.as_deref(), Some("page"));
        assert!(wire::encode(&envelope).unwrap().len() <= 4096);
        let Message::TracePage(page) = envelope.message else { panic!("expected page") };
        if page.offset == 0 {
            let mut newer = data.clone();
            newer.entries[0].state = TraceState::Ok;
            f.projections.trace.publish(newer);
        }
        entries.extend(page.data.entries);
        match page.next_offset {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(entries, data.entries);
    assert!(!serde_json::to_string(&entries).unwrap().contains("do-not-send"));
    // A completion replaces a running call even though its sequence number did not change.
    let mut completed = data;
    completed.entries[1].state = TraceState::Ok;
    f.projections.trace.publish(completed);
    assert_eq!(client.next().unwrap().message, Message::TraceChanged { revision: Seq(3) });
    client.send(&Envelope::new(Message::TraceGet { revision: Seq(1), offset: 0, limit: 1 }));
    assert!(matches!(client.next().unwrap().message, Message::Error { code: ErrorCode::StaleRevision, .. }));
    client.send(&Envelope::new(Message::Unsubscribe { topics: vec![wire::Topic::Trace] }));
    client.send(&Envelope::new(Message::Ping { nonce: Some("unsubscribed".into()) }));
    assert!(matches!(client.next().unwrap().message, Message::Pong { .. }));
    f.projections.trace.publish(Default::default());
    client.send(&Envelope::new(Message::Ping { nonce: Some("quiet".into()) }));
    assert!(matches!(client.next().unwrap().message, Message::Pong { .. }));
    // Reconnect/resubscribe tells the latest revision even when the board cursor never moved.
    client.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Trace] }));
    assert_eq!(client.next().unwrap().message, Message::TraceChanged { revision: Seq(4) });

    let mut old = TestClient::connect(f.server.addr(), f.server.token());
    old.send(&Envelope::new(Message::Hello {
        client_id: "old".into(),
        versions: vec![VERSION],
        capabilities: vec!["board".into()],
        prior_epoch: None,
        prior_cursor: None,
    }));
    assert!(matches!(old.next().unwrap().message, Message::Welcome { .. }));
    assert!(matches!(old.next().unwrap().message, Message::StateSnapshot { .. }));
    old.send(&Envelope::new(Message::TraceGet { revision: Seq(4), offset: 0, limit: 1 }));
    assert!(matches!(old.next().unwrap().message, Message::Error { code: ErrorCode::Unauthorised, .. }));
    old.send(&Envelope::new(Message::Subscribe { topics: vec![wire::Topic::Trace] }));
    assert!(matches!(old.next().unwrap().message, Message::Error { code: ErrorCode::Unauthorised, .. }));
    old.send(&Envelope::new(Message::Ping { nonce: None }));
    assert!(matches!(old.next().unwrap().message, Message::Pong { .. }), "old clients receive no new message kinds");
}

#[path = "preview_tests.rs"]
mod preview;

#[path = "machine_tests.rs"]
mod machine;
