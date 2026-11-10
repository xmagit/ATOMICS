//! The Phase F gate for `atomics host`, run against the real binary with an isolated home:
//! with only the headless host running, a shell session starts through ctl, a hook gets its
//! reply, a plan persists, a loopback protocol client sees the board and types into the shell
//! under its lease, SIGTERM stops it cleanly, and the home is locked against a second host and
//! the desktop.

#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomics_core::lease::Typist;
use atomics_core::mirror::Mirror;
use atomics_core::wire::{self, CAPABILITIES, Envelope, Message, Topic, VERSION};
use serde_json::{Value, json};
use test_support::{CHILD_TIMEOUT, TempDir, TestChild, scaled_timeout};
use tungstenite::client::IntoClientRequest;

struct Instance {
    dir: TempDir,
}

impl Instance {
    /// `remote` is the `[remote]` port to enable (0 picks a free one); `None` leaves remote off.
    fn new(remote: Option<u16>) -> Self {
        let dir = test_support::tempdir().unwrap();
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir.path().join("runtime")).unwrap();
        // Agents run bare here: the test never reaches a real sandbox.
        let mut config = atomics_core::config::DEFAULT_CONFIG.replace("mode = \"sandbox\"", "mode = \"bare\"");
        if let Some(port) = remote {
            assert_eq!(config.matches("\n[remote]\n").count(), 1);
            config = config.replace("\n[remote]\n", &format!("\n[remote]\nenabled = true\nport = {port}\n"));
        }
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Self { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("atomics.sock")
    }

    fn command<I: AsRef<std::ffi::OsStr>>(&self, args: impl IntoIterator<Item = I>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomics"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.dir.path())
            .env("ATOMICS_HOME", self.home())
            .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
            .env("ATOMICS_SOCKET", self.socket())
            .env("XDG_RUNTIME_DIR", self.dir.path().join("runtime"))
            .env("CODEX_HOME", self.dir.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"))
            // A login shell such as zsh would run its first-run wizard in the fresh HOME.
            .env("SHELL", "/bin/sh")
            .env_remove("ATOMICS_PANE")
            .env_remove("TERM_PROGRAM");
        command
    }

    /// No display either: the host needs no window system (the Linux QA image runs Xvfb).
    fn host(&self) -> TestChild {
        let mut command = self.command(["host"]);
        command.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").stdin(Stdio::null());
        TestChild::spawn(&mut command, CHILD_TIMEOUT).unwrap()
    }

    fn ctl(&self, request: Value) -> Value {
        let response = self.ctl_response(request.clone());
        assert_eq!(response["ok"], true, "{request} -> {response}");
        response["result"].clone()
    }

    /// The host's whole answer, which may be a refusal.
    fn ctl_response(&self, request: Value) -> Value {
        let output = test_support::output(&mut self.command(["ctl".to_string(), request.to_string()]));
        assert!(
            output.status.success(),
            "ctl {request} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Waits for the host to answer ctl, failing with its logs if it exits first.
    fn ready(&self, host: &mut TestChild) {
        let deadline = Instant::now() + scaled_timeout(Duration::from_secs(30));
        loop {
            if let Some(status) = host.child.try_wait().unwrap() {
                let output = host.wait_output().unwrap();
                panic!("host exited early with {status}: {}", String::from_utf8_lossy(&output.stderr));
            }
            let probe = test_support::output(&mut self.command(["ctl", r#"{"cmd":"list"}"#]));
            if probe.status.success() {
                return;
            }
            assert!(Instant::now() < deadline, "host never answered ctl");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// The id of the permission request pending in `session`, which `approve` names.
fn pending_id(instance: &Instance, session: &str) -> Value {
    let result = instance.ctl(json!({"cmd":"result","session":session}));
    assert!(result["approval"]["id"].is_string(), "no request is pending in {session}: {result}");
    result["approval"]["id"].clone()
}

fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

type Socket = tungstenite::WebSocket<TcpStream>;

/// A native `atomics.v1` client, as `examples/loopback_client.rs` connects: it reads
/// protocol.json, authenticates and says hello.
fn connect(home: &Path) -> Socket {
    let advert: Value = serde_json::from_str(&std::fs::read_to_string(home.join("protocol.json")).unwrap()).unwrap();
    let mut request = advert["url"].as_str().unwrap().into_client_request().unwrap();
    let token = advert["token"].as_str().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
    let stream = TcpStream::connect(request.uri().authority().unwrap().as_str()).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    stream.set_nodelay(true).unwrap();
    let (mut socket, _) = tungstenite::client(request, stream).unwrap();
    let hello = Envelope::new(Message::Hello {
        client_id: "headless-gate".into(),
        versions: vec![VERSION],
        capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        prior_epoch: None,
        prior_cursor: None,
    });
    send(&mut socket, hello);
    socket
}

/// The capabilities the host's `welcome` offers a client that asks for all of them.
fn welcome(home: &Path) -> Vec<String> {
    let mut socket = connect(home);
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    loop {
        assert!(Instant::now() < deadline, "no welcome");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Message::Welcome { capabilities, .. } = wire::decode(text.as_str()).unwrap().message {
                    return capabilities;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
}

fn send(socket: &mut Socket, envelope: Envelope) {
    socket.send(tungstenite::Message::text(serde_json::to_string(&envelope).unwrap())).unwrap();
}

/// Take `session`'s lease and type `text` into it, then let go.
fn type_into(home: &Path, session: &str, text: &str) {
    let mut socket = connect(home);
    let topics = vec![Topic::Screen { session: session.into(), focused: true }];
    send(&mut socket, Envelope::new(Message::Subscribe { topics }));
    let mut typist = Typist::new(session);
    send(&mut socket, Envelope::new(typist.acquire(false)));
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !typist.may_type() {
        assert!(Instant::now() < deadline, "the lease was never granted");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => match wire::decode(text.as_str()).unwrap().message {
                Message::LeaseState(state) => typist.apply(&state),
                Message::Error { code, message } => panic!("{code:?}: {message}"),
                _ => {}
            },
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
    for frame in typist.input(text) {
        send(&mut socket, Envelope::new(frame));
    }
    send(&mut socket, Envelope::new(typist.release().unwrap()));
    let _ = socket.close(None);
}

/// Mirror the board until `done` holds.
fn board(home: &Path, done: impl Fn(&Mirror) -> bool) -> Mirror {
    let mut socket = connect(home);
    let mut mirror = Mirror::default();
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !(mirror.epoch.is_some() && done(&mirror)) {
        assert!(Instant::now() < deadline, "the board never showed the expected state");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let envelope = wire::decode(text.as_str()).unwrap();
                if let Message::Ping { nonce } = &envelope.message {
                    send(&mut socket, Envelope::new(Message::Pong { nonce: nonce.clone() }));
                } else {
                    mirror.apply(&envelope).unwrap();
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
    let _ = socket.close(None);
    mirror
}

#[test]
fn control_starts_and_messages_the_chief_without_a_window() {
    let instance = Instance::new(None);
    // A local stand-in records the chief's launch arguments and echoes later input.
    let script = instance.dir.path().join("chief.sh");
    let arguments = instance.dir.path().join("chief-args");
    test_support::write_program(&script, "args=$1\nshift\nprintf '%s\\n' \"$@\" > \"$args\"\nexec cat\n");
    let config_path = instance.dir.path().join("config.toml");
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["chief"]["command"] = toml::Value::try_from(vec![
        "/bin/sh".to_string(),
        script.to_str().unwrap().to_string(),
        arguments.to_str().unwrap().to_string(),
    ])
    .unwrap();
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    assert_eq!(instance.ctl(json!({"cmd":"list"}))["sessions"], json!([]));

    // Raw keys never start an agent. A normal send starts the chief with exactly one first prompt.
    assert_eq!(instance.ctl_response(json!({"cmd":"send", "session":"chief", "text":"x", "raw":true}))["ok"], false);
    let first = "Plan the next task from the headless host";
    assert_eq!(instance.ctl(json!({"cmd":"send", "session":"chief", "text":first}))["sent"], true);
    eventually("the chief's first prompt", || {
        std::fs::read_to_string(&arguments).is_ok_and(|args| args.lines().filter(|line| *line == first).count() == 1)
    });
    let list = instance.ctl(json!({"cmd":"list"}));
    assert!(list["window"].is_null(), "{list}");
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["chief"], true);
    let id = sessions[0]["id"].clone();

    // Explicit start is idempotent; a follow-up send reaches the existing terminal.
    assert_eq!(instance.ctl(json!({"cmd":"start_chief"}))["id"], id);
    assert_eq!(instance.ctl(json!({"cmd":"send", "session":"chief", "text":"headless-followup"}))["sent"], true);
    eventually("the chief's follow-up input", || {
        instance.ctl(json!({"cmd":"read", "session":id}))["screen"]
            .as_str()
            .is_some_and(|screen| screen.contains("headless-followup"))
    });
    assert_eq!(instance.ctl(json!({"cmd":"list"}))["sessions"].as_array().unwrap().len(), 1);

    // With none running, explicit start also works without a prompt or a window.
    instance.ctl(json!({"cmd":"close", "session":id}));
    let started = instance.ctl(json!({"cmd":"start_chief"}));
    assert_eq!(started["chief"], true);
    assert_ne!(started["id"], id);
    assert!(instance.ctl(json!({"cmd":"list"}))["window"].is_null());

    // SAFETY: this is the host child owned by this test.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

#[test]
fn host_window_commands_explain_they_are_not_forwarded_to_connected_clients() {
    let instance = Instance::new(Some(0));
    let mut host = instance.host();
    instance.ready(&mut host);
    let mut client = connect(&instance.home());
    // Keep an authenticated client attached while the local control commands run.
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    loop {
        assert!(Instant::now() < deadline, "no welcome");
        match client.read() {
            Ok(tungstenite::Message::Text(text))
                if matches!(wire::decode(text.as_str()).unwrap().message, Message::Welcome { .. }) =>
            {
                break;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
    let screenshot = instance.dir.path().join("never.png");
    for request in [
        json!({"cmd":"screenshot", "path":screenshot}),
        json!({"cmd":"resize", "width":800, "height":600}),
        json!({"cmd":"show_settings", "open":true}),
    ] {
        let response = instance.ctl_response(request);
        assert_eq!(response["ok"], false, "{response}");
        let error = response["error"].as_str().unwrap();
        assert!(error.contains("no local window") && error.contains("not forwarded to connected clients"), "{error}");
    }
    assert!(!screenshot.exists());
    let _ = client.close(None);
    // SAFETY: this is the host child owned by this test.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

/// A headless host, with no session or desktop selecting the worktree, still serves a folder feed.
#[test]
fn headless_explorer_lists_idle_worktrees_and_completes_plans() {
    let instance = Instance::new(Some(0));
    let repo = instance.dir.path().join("repo");
    let tree = instance.dir.path().join("topic");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(repo.join("src")).unwrap();
    for i in 0..260 {
        std::fs::write(repo.join(format!("src/file{i:03}.rs")), "pub fn example() {}\n").unwrap();
    }
    std::fs::write(repo.join("README.md"), "A project readme\n").unwrap();
    std::fs::write(instance.dir.path().join("private.txt"), "not a worktree file").unwrap();
    std::os::unix::fs::symlink(instance.dir.path().join("private.txt"), repo.join("link")).unwrap();
    let git = |args: &[&str]| {
        let output = test_support::output(
            Command::new("git")
                .current_dir(&repo)
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args),
        );
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "Example files"]);
    git(&["worktree", "add", "-q", "-b", "topic", tree.to_str().unwrap()]);
    // Git and the host both use canonical host paths (including on macOS's temporary directory).
    let repo = repo.canonicalize().unwrap();
    let tree = tree.canonicalize().unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    instance.ctl(json!({"cmd":"open_project", "path":repo}));
    board(&instance.home(), |m| {
        m.host
            .projects
            .iter()
            .any(|p| p.root == repo.to_str().unwrap() && p.worktrees.iter().any(|w| w.path == tree.to_str().unwrap()))
    });
    let mut socket = connect(&instance.home());
    let next = |socket: &mut Socket, predicate: &dyn Fn(&Message) -> bool| {
        let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
        loop {
            assert!(Instant::now() < deadline, "host never answered explorer request");
            match socket.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let envelope = wire::decode(&text).unwrap();
                    if predicate(&envelope.message) {
                        break envelope.message;
                    }
                    if let Message::Ping { nonce } = envelope.message {
                        send(socket, Envelope::new(Message::Pong { nonce }));
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(error) => panic!("explorer connection failed: {error}"),
            }
        }
    };
    next(&mut socket, &|m| matches!(m, Message::StateSnapshot { .. }));
    send(
        &mut socket,
        Envelope::new(Message::Subscribe {
            topics: vec![Topic::Folder { worktree: tree.to_string_lossy().into(), path: "src".into() }],
        }),
    );
    let Message::FolderPage(page) = next(&mut socket, &|m| matches!(m, Message::FolderPage(_))) else { unreachable!() };
    assert_eq!((page.entries.len(), page.next_offset), (128, Some(128)));
    send(
        &mut socket,
        Envelope::new(Message::FolderGet {
            worktree: page.worktree.clone(),
            path: page.path.clone(),
            revision: page.revision,
            offset: 128,
        }),
    );
    let Message::FolderPage(second) = next(&mut socket, &|m| matches!(m, Message::FolderPage(_))) else {
        unreachable!()
    };
    assert_eq!(second.offset, 128);
    assert!(page.entries.last().unwrap().path < second.entries[0].path);
    send(&mut socket, Envelope::new(Message::FileGet { worktree: page.worktree.clone(), path: "README.md".into() }));
    assert!(
        matches!(next(&mut socket, &|m| matches!(m, Message::FileContent { .. })), Message::FileContent { content, truncated: false, .. } if content == "A project readme\n")
    );
    send(&mut socket, Envelope::new(Message::FileGet { worktree: page.worktree, path: "link".into() }));
    assert!(matches!(
        next(&mut socket, &|m| matches!(m, Message::Error { .. })),
        Message::Error { code: wire::ErrorCode::Rejected, .. }
    ));
    let plan = instance.ctl(json!({"cmd":"plan_set", "goal":"Example plan", "tasks":[]}));
    let id = plan["plan"].as_str().unwrap();
    send(
        &mut socket,
        Envelope::new(Message::PlanCommand(atomics_core::ops::PlanCommand {
            op: "complete-example".into(),
            plan: Some(id.into()),
            digest: None,
            action: atomics_core::ops::PlanAction::Complete,
        })),
    );
    assert!(
        matches!(next(&mut socket, &|m| matches!(m, Message::CommandResult(r) if r.outcome.is_final())), Message::CommandResult(r) if matches!(r.outcome, atomics_core::ops::Outcome::Done { .. }))
    );
    let saved: Value = serde_json::from_str(
        &std::fs::read_to_string(instance.home().join("plans").join(format!("{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["status"], "completed");
    let _ = socket.close(None);
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

#[test]
fn account_usage_and_governor_state_are_published_without_a_window() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    let instance = Instance::new(Some(0));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let sessions = instance.dir.path().join("codex/sessions/2026/01/01");
    std::fs::create_dir_all(&sessions).unwrap();
    let resets_at = now + 86400;
    let event = json!({
        "timestamp": "2026-10-28T15:00:00Z",
        "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
            "primary": {"used_percent": 25, "window_minutes": 300, "resets_at": resets_at},
            "secondary": {"used_percent": 40, "window_minutes": 10080, "resets_at": resets_at}
        }}
    });
    std::fs::write(sessions.join("rollout-test.jsonl"), event.to_string()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    let initial = board(&instance.home(), |m| m.host.resources.as_ref().is_some_and(|r| r.codex.session.is_some()));
    let resources = initial.host.resources.unwrap();
    assert_eq!(resources.codex.session.unwrap().left_percent, 75.0);
    assert_eq!(resources.codex.weekly.unwrap().left_percent, 60.0);
    assert!(resources.cores > 0 && resources.test.slots > 0 && resources.qa.slots == 1);
    assert!(resources.sampled_at >= now);

    // The same status-line IPC as a provider sends; the client never reads account files.
    let mut statusline = UnixStream::connect(instance.socket()).unwrap();
    statusline.set_read_timeout(Some(scaled_timeout(Duration::from_secs(5)))).unwrap();
    writeln!(
        statusline,
        "{}",
        json!({"statusline": {"rate_limits": {
            "five_hour": {"used_percentage": 22, "resets_at": resets_at},
            "seven_day": {"used_percentage": 12, "resets_at": resets_at}
        }}})
    )
    .unwrap();
    BufReader::new(&statusline).read_line(&mut String::new()).unwrap();
    let updated = board(&instance.home(), |m| m.host.resources.as_ref().is_some_and(|r| r.claude.session.is_some()));
    let resources = updated.host.resources.unwrap();
    assert_eq!(resources.claude.session.unwrap().left_percent, 78.0);
    assert_eq!(resources.claude.weekly.unwrap().left_percent, 88.0);

    let queue = || {
        let mut stream = UnixStream::connect(instance.socket()).unwrap();
        stream.set_read_timeout(Some(scaled_timeout(Duration::from_secs(5)))).unwrap();
        writeln!(stream, "{}", json!({"cmd": "queue", "class": "qa", "label": "parity check"})).unwrap();
        stream
    };
    let running = queue();
    let mut line = String::new();
    BufReader::new(&running).read_line(&mut line).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["granted"], true);
    let waiting = queue();
    board(&instance.home(), |m| m.host.resources.as_ref().is_some_and(|r| r.qa.running == 1 && r.qa.waiting == 1));
    drop(waiting);
    drop(running);
    board(&instance.home(), |m| {
        m.host
            .resources
            .as_ref()
            .is_some_and(|r| r.qa.running == 0 && r.qa.waiting == 0 && r.sampled_at > resources.sampled_at)
    });
    // SAFETY: this is the host child owned by this test.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

#[test]
fn headless_host_serves_ctl_hooks_plans_and_the_protocol_then_stops_on_sigterm() {
    let instance = Instance::new(Some(0));
    let mut host = instance.host();
    instance.ready(&mut host);

    // protocol.json is private and carries the token the logs never show.
    use std::os::unix::fs::PermissionsExt;
    let advert = instance.home().join("protocol.json");
    assert_eq!(std::fs::metadata(&advert).unwrap().permissions().mode() & 0o777, 0o600);
    let token = serde_json::from_str::<Value>(&std::fs::read_to_string(&advert).unwrap()).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);

    // A window-only request fails clearly instead of waiting for a desktop.
    let screenshot = test_support::output(
        &mut instance.command(["ctl", &json!({"cmd":"screenshot","path":"/tmp/never.png"}).to_string()]),
    );
    assert!(String::from_utf8_lossy(&screenshot.stdout).contains("no local window"), "{screenshot:?}");
    let list = instance.ctl(json!({"cmd":"list"}));
    assert!(list["window"].is_null(), "{list}");
    // With no window whose panes would size its terminals, a client's panes may.
    assert!(welcome(&instance.home()).iter().any(|c| c == wire::FIT));

    // A shell session starts through ctl and runs in a PTY with no client attached.
    let marker = instance.dir.path().join("shell.pid");
    let started = instance.ctl(json!({
        "cmd":"start", "agent":"shell", "isolation":"bare",
        "prompt": format!("echo $$ > '{}'; echo headless-ready", marker.display()),
    }));
    let session = started["id"].as_str().unwrap().to_string();
    eventually("the shell's output", || {
        let read = instance.ctl(json!({"cmd":"read","session":session,"lines":40}));
        read.to_string().contains("headless-ready") && marker.exists()
    });
    let shell: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    assert!(alive(shell));

    // A hook gets its reply: a permission request waits for an approval given through ctl.
    let mut hook = TestChild::spawn(
        instance.command(["hook", "--socket"]).arg(instance.socket()).args(["--pane", &session]).stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    let request = json!({
        "hook_event_name":"PermissionRequest", "session_id":"headless-gate", "cwd":instance.dir.path(),
        "tool_name":"Bash", "tool_input":{"command":"touch approved-by-ctl"}
    });
    let mut stdin = hook.child.stdin.take().unwrap();
    stdin.write_all(request.to_string().as_bytes()).unwrap();
    drop(stdin);
    eventually("the pending approval", || {
        let list = instance.ctl(json!({"cmd":"list"}));
        list["sessions"].as_array().unwrap().iter().any(|s| s["id"] == session && !s["approval"].is_null())
    });
    let id = pending_id(&instance, &session);
    instance.ctl(json!({"cmd":"approve","session":session,"request":id,"decision":"allow"}));
    let reply = hook.wait_output().unwrap();
    assert!(reply.status.success());
    let reply: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["decision"]["behavior"], "allow", "{reply}");

    // Dropping an entry of the stash every worktree shares waits, and its approval says which entry
    // that is, to ctl and to protocol clients alike.
    let repo = instance.dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = test_support::output(
            Command::new("git")
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(&repo),
        );
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "base"]);
    std::fs::write(repo.join("a.txt"), "work in progress\n").unwrap();
    git(&["stash", "push", "-q", "-m", "ENG-123"]);
    let mut hook = TestChild::spawn(
        instance.command(["hook", "--socket"]).arg(instance.socket()).args(["--pane", &session]).stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    let request = json!({
        "hook_event_name":"PermissionRequest", "session_id":"headless-gate", "cwd":repo,
        "tool_name":"Bash", "tool_input":{"command":"git stash drop stash@{0}"}
    });
    let mut stdin = hook.child.stdin.take().unwrap();
    stdin.write_all(request.to_string().as_bytes()).unwrap();
    drop(stdin);
    let entry = "git stash drop drops stash@{0} (On main: ENG-123)";
    eventually("the stash warning", || {
        let list = instance.ctl(json!({"cmd":"list"}));
        list["sessions"].as_array().unwrap().iter().any(|s| {
            s["id"] == session && s["approval"]["warning"].as_str().is_some_and(|warning| warning.contains(entry))
        })
    });
    let mirror = board(&instance.home(), |mirror| {
        mirror.approvals.values().any(|approval| approval.warning.as_deref().is_some_and(|w| w.contains(entry)))
    });
    assert!(mirror.approvals.values().all(|approval| approval.session == session), "{:?}", mirror.approvals);
    let id = pending_id(&instance, &session);
    instance.ctl(json!({"cmd":"approve","session":session,"request":id,"decision":"deny"}));
    let reply: Value = serde_json::from_slice(&hook.wait_output().unwrap().stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["decision"]["behavior"], "deny", "{reply}");

    // A plan persists on disk.
    let plan = instance.ctl(json!({
        "cmd":"plan_set", "goal":"Headless gate",
        "tasks":[{"id":"t1","title":"First task","brief":"Nothing to do"}]
    }));
    let id = plan["plan"].as_str().unwrap().to_string();
    let saved = instance.home().join("plans").join(format!("{id}.json"));
    eventually("the saved plan", || std::fs::read_to_string(&saved).is_ok_and(|text| text.contains("Headless gate")));
    // A plan whose tasks could never start is refused, naming why, and not saved.
    let refused = instance.ctl_response(json!({
        "cmd":"plan_set", "goal":"Never starts",
        "tasks":[{"id":"t1","title":"Follow-up","brief":"After the review","depends_on":["review"]}]
    }));
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(refused["error"].as_str().unwrap().contains("Dependency cycle t1 → review → t1"), "{refused}");
    let open = instance.ctl(json!({"cmd":"plan_get"}));
    assert!(open["plans"].as_array().unwrap().iter().all(|plan| plan["goal"] != "Never starts"), "{open}");

    // A loopback protocol client connects and sees the board: the session and the plan.
    let mirror = board(&instance.home(), |mirror| {
        mirror.sessions.contains_key(&session) && mirror.plans.values().any(|p| p.goal == "Headless gate")
    });
    assert!(mirror.plans.values().any(|p| p.goal == "Headless gate" && p.id == id));

    // It types into the shell under the terminal's lease, and the command runs.
    type_into(&instance.home(), &session, "echo typed-$((40+2))\r");
    eventually("the typed command's output", || {
        instance.ctl(json!({"cmd":"read","session":session,"lines":40})).to_string().contains("typed-42")
    });

    // The home belongs to this host: a second host and the desktop are both refused. Should the
    // desktop ever start anyway, its screenshot request closes it at once.
    let never = instance.dir.path().join("never.png");
    let desktop = ["--size", "480x320", "--screenshot", never.to_str().unwrap(), "--screenshot-delay", "0"];
    for args in [&["host"][..], &desktop] {
        let refused = test_support::output(&mut instance.command(args));
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(1), "atomics {args:?}: {stderr}");
        assert!(stderr.contains(&format!("headless ATOMICS host (pid {})", host.child.id())), "{stderr}");
    }

    // SIGTERM stops it cleanly: exit 0, its agents gone, protocol.json and the socket removed.
    // SAFETY: the pid is our own child's.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}: {stderr}", output.status);
    assert!(stderr.contains("SIGTERM received; stopping") && stderr.contains("atomics host: stopped"), "{stderr}");
    assert!(stderr.contains("atomics.v1 protocol on ws://127.0.0.1:"), "{stderr}");
    for log in [&output.stdout[..], &output.stderr] {
        assert!(!String::from_utf8_lossy(log).contains(&token), "the token reached a log");
    }
    assert!(!advert.exists(), "protocol.json outlived the host");
    assert!(!instance.socket().exists(), "the socket outlived the host");
    eventually("the shell to end", || !alive(shell));
    let crash_log = std::fs::read_to_string(instance.home().join("crash.log")).unwrap_or_default();
    assert!(crash_log.contains(&format!("exit {}", host.child.id())), "{crash_log}");

    // The lock went with the process: the home can be hosted again.
    let mut again = instance.host();
    instance.ready(&mut again);
    assert_eq!(unsafe { libc::kill(again.child.id() as i32, libc::SIGINT) }, 0);
    let output = again.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// The chief's two tools as `atomics mcp` serves them, one call each, in order.
fn mcp_calls(instance: &Instance, calls: &[(&str, Value)]) -> Vec<Value> {
    let mut mcp = TestChild::spawn(instance.command(["mcp"]).stdin(Stdio::piped()), CHILD_TIMEOUT).unwrap();
    let mut stdin = mcp.child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0", "id":0, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}})
    )
    .unwrap();
    for (id, (name, arguments)) in calls.iter().enumerate() {
        let call =
            json!({"jsonrpc":"2.0", "id":id + 1, "method":"tools/call", "params":{"name":name, "arguments":arguments}});
        writeln!(stdin, "{call}").unwrap();
    }
    drop(stdin);
    let output = mcp.wait_output().unwrap();
    assert!(output.status.success());
    let replies: Vec<Value> = output
        .stdout
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    (1..=calls.len()).map(|id| replies.iter().find(|reply| reply["id"] == id).unwrap()["result"].clone()).collect()
}

#[test]
fn an_agent_without_hooks_has_its_own_approval_menu_answered_through_the_host() {
    let instance = Instance::new(Some(0));
    // An agent with no hooks that stops on Codex's recorded command menu.
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approval-menu");
    let screen =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../atomics-core/tests/fixtures/codex-0.165-approval-screen.txt");
    let mut config = std::fs::OpenOptions::new().append(true).open(instance.dir.path().join("config.toml")).unwrap();
    let agent = json!([script, screen]);
    write!(config, "\n[[agents]]\nname = \"menu\"\ncommand = {agent}\nkind = \"generic\"\n").unwrap();
    drop(config);
    let mut host = instance.host();
    instance.ready(&mut host);
    let started = instance.ctl(json!({"cmd":"start", "agent":"menu", "isolation":"bare"}));
    let session = started["id"].as_str().unwrap().to_string();
    let command = "atomics queue --class test -- ./check.sh";

    // ctl and the board both hold the menu as the session's one pending approval.
    eventually("the native approval", || {
        let list = instance.ctl(json!({"cmd":"list"}));
        list["sessions"][0]["approval_menu"] == "codex" && list["sessions"][0]["approval"]["command"] == command
    });
    let mirror = board(&instance.home(), |mirror| mirror.approvals.values().any(|a| a.session == session));
    let approval = mirror.approvals.values().find(|a| a.session == session).unwrap();
    assert_eq!(approval.command.as_deref(), Some(command));
    assert_eq!(mirror.sessions[&session].pending_approvals, 1);

    // The chief's tools agree: raw keys wait for atomics_approve, which answers the menu it names and echoes
    // it. A decision that names no request answers nothing, and says which one is pending.
    let id = pending_id(&instance, &session);
    let results = mcp_calls(
        &instance,
        &[
            ("atomics_send", json!({"session":session, "text":"y", "raw":true})),
            ("atomics_approve", json!({"session":session, "decision":"allow"})),
            ("atomics_approve", json!({"session":session, "request":id, "decision":"allow"})),
        ],
    );
    assert_eq!(results[0]["isError"], true, "{}", results[0]);
    assert!(results[0]["content"][0]["text"].as_str().unwrap().contains("use atomics_approve"), "{}", results[0]);
    assert_eq!(results[1]["isError"], true, "{}", results[1]);
    assert!(results[1]["content"][0]["text"].as_str().unwrap().contains(id.as_str().unwrap()), "{}", results[1]);
    assert_eq!(results[2]["isError"], false, "{}", results[2]);
    let answered = results[2]["content"][0]["text"].as_str().unwrap();
    assert!(answered.contains(command) && answered.contains("codex"), "{answered}");
    eventually("the menu's keys", || {
        instance.ctl(json!({"cmd":"read", "session":session, "lines":40}))["screen"]
            .as_str()
            .is_some_and(|screen| screen.contains("Keys: 31 0d"))
    });
    board(&instance.home(), |mirror| {
        mirror.approvals.is_empty() && mirror.sessions.get(&session).is_some_and(|s| s.pending_approvals == 0)
    });

    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// The chief's `atomics_ticket_search` through `atomics mcp --chief`: whether it failed, and its text.
fn chief_ticket_search(instance: &Instance) -> (bool, String) {
    let mut mcp = TestChild::spawn(instance.command(["mcp", "--chief"]).stdin(Stdio::piped()), CHILD_TIMEOUT).unwrap();
    let mut stdin = mcp.child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call",
               "params":{"name":"atomics_ticket_search", "arguments":{"query":"sign-in"}}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = mcp.wait_output().unwrap();
    assert!(output.status.success());
    let reply = output
        .stdout
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .expect("an answer to the tool call");
    let text = reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    (reply["result"]["isError"] == true, text)
}

/// Send one `host_command` and wait for its final outcome.
fn host_command(home: &Path, action: atomics_core::ops::HostAction) -> atomics_core::ops::Outcome {
    let mut socket = connect(home);
    // Each operation has an ID of its own: the journal answers a reused one with its first outcome.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let op = format!("gate-op-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    let command = atomics_core::ops::HostCommand { op, action };
    send(&mut socket, Envelope::new(Message::HostCommand(command)));
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    loop {
        assert!(Instant::now() < deadline, "the host_command was never answered");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => match wire::decode(text.as_str()).unwrap().message {
                Message::CommandResult(result) if result.outcome.is_final() => {
                    let _ = socket.close(None);
                    return result.outcome;
                }
                Message::Ping { nonce } => send(&mut socket, Envelope::new(Message::Pong { nonce })),
                Message::Error { code, message } => {
                    panic!("the host_command was refused as a frame: {code:?}: {message}")
                }
                _ => {}
            },
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
}

#[test]
fn a_start_without_a_workspace_opens_the_last_one_and_the_ticket_tools_say_why_when_none_is_open() {
    let instance = Instance::new(Some(0));
    // Saved workspaces, as flat files that load whatever this machine is called; one has [tickets].
    let saved = instance.dir.path().join("workspaces");
    std::fs::create_dir_all(&saved).unwrap();
    std::fs::write(
        saved.join("team.toml"),
        "projects = []\n[tickets]\nsystem = \"tracker\"\n[tickets.server]\ncommand = [\"/bin/false\"]\n\
         [tickets.operations.search]\ntool = \"search\"\nitems = \"tickets\"\nresult = { id = \"id\" }\n",
    )
    .unwrap();
    std::fs::write(saved.join("daily.toml"), "projects = []\n").unwrap();

    // Started with no workspace and none open last time: the chief's ticket tools name the cause.
    let mut host = instance.host();
    instance.ready(&mut host);
    let (failed, text) = chief_ticket_search(&instance);
    assert!(failed, "{text}");
    assert!(text.contains("no workspace is open; saved: daily, team (has [tickets])"), "{text}");
    // Clients see that no workspace is open, and which are saved.
    let mirror = board(&instance.home(), |m| m.host.workspaces.len() == 2);
    assert_eq!(
        (mirror.host.workspace.clone(), mirror.host.workspaces.clone()),
        (None, vec!["daily".into(), "team".into()])
    );

    // A client opens one from its picker; every client sees it, and the ticket tools have a system.
    let opened = host_command(&instance.home(), atomics_core::ops::HostAction::OpenWorkspace { name: "team".into() });
    assert!(matches!(opened, atomics_core::ops::Outcome::Done { .. }), "{opened:?}");
    board(&instance.home(), |m| m.host.workspace.as_deref() == Some("team"));
    // Ticket requests run without a window, and an asynchronous worker failure reaches the client.
    let searched = host_command(
        &instance.home(),
        atomics_core::ops::HostAction::Tickets {
            request: "headless-ticket-search".into(),
            ticket: Box::new(atomics_core::ticket_ui::TicketAction::Search {
                query: "parser".into(),
                keyword: true,
                view: 0,
                filters: vec![],
                cursor: None,
            }),
        },
    );
    assert!(matches!(searched, atomics_core::ops::Outcome::Done { .. }), "{searched:?}");
    let mirror = board(&instance.home(), |m| {
        m.host.tickets.as_ref().is_some_and(|info| {
            info.replies.iter().any(|reply| reply.request == "headless-ticket-search" && reply.error.is_some())
        })
    });
    assert!(mirror.host.tickets.unwrap().search);
    let (_, text) = chief_ticket_search(&instance);
    assert!(!text.contains("No ticket system is configured"), "{text}");
    // A workspace that is not saved is refused, and nothing changes.
    let refused = host_command(&instance.home(), atomics_core::ops::HostAction::OpenWorkspace { name: "gone".into() });
    assert!(matches!(&refused, atomics_core::ops::Outcome::Refused { message, .. } if message.contains("gone")));
    // No OAuth server in this profile: a sign-in is refused, saying why.
    let refused = host_command(&instance.home(), atomics_core::ops::HostAction::SignIn);
    assert!(
        matches!(&refused, atomics_core::ops::Outcome::Refused { message, .. } if message.contains("oauth = true")),
        "{refused:?}"
    );

    // The next start names no workspace either, as from the Dock: the one open last opens again.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
    let mut again = instance.host();
    instance.ready(&mut again);
    board(&instance.home(), |m| m.host.workspace.as_deref() == Some("team"));
    let (_, text) = chief_ticket_search(&instance);
    assert!(!text.contains("No ticket system is configured"), "{text}");
    assert_eq!(unsafe { libc::kill(again.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(again.wait_output().unwrap().status.success());
}

#[test]
fn without_remote_there_is_no_protocol_server_and_mcp_still_reaches_the_host() {
    let instance = Instance::new(None);
    // An advert from an earlier run with remote on names a server that is gone.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(instance.home()).unwrap();
    let advert = instance.home().join("protocol.json");
    std::fs::write(&advert, json!({"url":"ws://127.0.0.1:1","token":"stale","pid":1}).to_string()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    assert!(!advert.exists(), "a stale protocol.json outlived the start of a host without remote");
    let brief = format!("Brief start {}", "context ".repeat(5000));
    let plan =
        instance.ctl(json!({"cmd":"plan_set","goal":"Local only","tasks":[{"id":"a","title":"First","brief":brief}]}));
    let id = plan["plan"].as_str().unwrap().to_string();

    // `atomics mcp` reaches the host through the local socket, with no client attached: a plan
    // reads compactly, and one task in full.
    let mut mcp = TestChild::spawn(instance.command(["mcp"]).stdin(Stdio::piped()), CHILD_TIMEOUT).unwrap();
    let mut stdin = mcp.child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"atomics_plan_get", "arguments":{"plan":id}}}),
        json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{"name":"atomics_plan_get", "arguments":{"plan":id, "task":"a"}}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = mcp.wait_output().unwrap();
    assert!(output.status.success());
    let replies: Vec<Value> = output
        .stdout
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let got = replies.iter().find(|reply| reply["id"] == 2).expect("an answer to the tool call");
    assert_eq!(got["result"]["isError"], false, "{got}");
    let compact = got["result"]["content"][0]["text"].as_str().unwrap();
    assert!(compact.contains("Local only") && compact.contains("\"First\""), "{got}");
    assert!(!compact.contains("Brief start"), "a plan reads without its briefs: {got}");
    let task = replies.iter().find(|reply| reply["id"] == 3).expect("an answer to the task call");
    assert_eq!(task["result"]["isError"], false, "{task}");
    let full: Value = serde_json::from_str(task["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(full["task"]["brief"], brief, "one task in full");

    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("protocol server off"), "{stderr}");
}

#[test]
fn with_remote_on_a_taken_port_the_host_fails_loudly_and_cleans_up() {
    // Something else already listens on the configured port.
    let taken = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let instance = Instance::new(Some(port));
    // An advert an earlier run left behind must not outlive this one either.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(instance.home()).unwrap();
    let advert = instance.home().join("protocol.json");
    let stale = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
    std::fs::write(&advert, json!({"url":"ws://127.0.0.1:1","token":stale,"pid":1}).to_string()).unwrap();

    let output = instance.host().wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "a half-reachable host must fail: {stderr}");
    let expected = format!("[remote] is enabled but the protocol server cannot start (port {port}): ");
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(!stderr.contains("protocol server off"), "the log must not claim remote is off: {stderr}");
    assert!(stderr.contains("atomics host: stopped"), "it stops as cleanly as on SIGTERM: {stderr}");
    for log in [&output.stdout[..], &output.stderr] {
        assert!(!String::from_utf8_lossy(log).contains(stale), "a token reached a log");
    }
    assert!(!advert.exists(), "clients must not find an advert for a server that is not running");
    assert!(!instance.socket().exists(), "the socket outlived the host");

    // Once the port is free the same config serves on it, and the failed run left the home unlocked.
    drop(taken);
    let mut host = instance.host();
    instance.ready(&mut host);
    let url = serde_json::from_str::<Value>(&std::fs::read_to_string(&advert).unwrap()).unwrap()["url"].clone();
    assert_eq!(url, format!("ws://127.0.0.1:{port}"));
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

/// The NixOS VM's service: `atomics host --remote-port` serves `atomics.v1` on that loopback port with
/// `[remote]` off in the config, and a client paired through ctl gets in with its own token, which the
/// host keeps only as a digest, until it is unpaired.
#[test]
fn a_service_port_and_a_paired_client_reach_the_headless_host() {
    let instance = Instance::new(None);
    let free = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let mut command = instance.command(["host", "--remote-port", &port.to_string()]);
    command.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").stdin(Stdio::null());
    let mut host = TestChild::spawn(&mut command, CHILD_TIMEOUT).unwrap();
    instance.ready(&mut host);
    assert_eq!(instance.ctl(json!({"cmd":"remote"}))["protocol"]["url"], format!("ws://127.0.0.1:{port}"));

    let paired = instance.ctl(json!({"cmd":"pair_client","name":"ATOMICS on a Mac"}));
    let token = paired["token"].as_str().unwrap().to_string();
    let upgrade = |token: &str| {
        // As through a forward on another port: the Host names the loopback, any port.
        let mut request = format!("ws://127.0.0.1:{port}").into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert("Host", "127.0.0.1:1".parse().unwrap());
        headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        tungstenite::client(request, stream).is_ok()
    };
    assert!(upgrade(&token), "the paired token gets in");
    assert!(!upgrade(&token[1..]), "a wrong one does not");
    let clients = instance.ctl(json!({"cmd":"remote"}))["clients"].clone();
    assert_eq!(clients[0]["name"], "ATOMICS on a Mac");
    let kept = std::fs::read_to_string(instance.home().join("paired-clients.json")).unwrap();
    assert!(!kept.contains(&token), "only a digest is kept");
    instance.ctl(json!({"cmd":"unpair_client","name":"ATOMICS on a Mac"}));
    assert!(!upgrade(&token), "an unpaired token stops working");

    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains(&token), "the token reached the log");
}

#[test]
fn the_host_start_drops_cleanup_failures_it_no_longer_finds_and_keeps_alerts() {
    let instance = Instance::new(None);
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(instance.home()).unwrap();
    // An earlier version left the ledger noting a git record whose file and sandbox are gone, so
    // every start and plan cleanup failed on it, and kept those failures forever.
    let record = instance.home().join("git-pins/atomics-gone.json");
    std::fs::write(
        instance.home().join("sandbox-ownership.json"),
        json!({"atomics-gone": {"created": 0, "pins": instance.home().join("git-pins")}}).to_string(),
    )
    .unwrap();
    let missing = format!("reading {}: No such file or directory (os error 2)", record.display());
    let alert = "Final ref guard check FAILED: refs/heads/main moved";
    let notes = [
        format!("Startup cleanup skipped: {missing}"),
        format!("Plan cleanup skipped: {missing}"),
        "Worktrees /home/user/deleted-scratch: git worktree list --porcelain failed: fatal: cannot change to".into(),
        alert.to_string(),
    ];
    std::fs::write(instance.home().join("cleanup-leftovers.json"), json!(notes).to_string()).unwrap();

    let mut host = instance.host();
    instance.ready(&mut host);
    let kept = || {
        std::fs::read(instance.home().join("cleanup-leftovers.json"))
            .map(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).unwrap())
            .unwrap_or_default()
    };
    eventually("the startup sweep to drop what it no longer finds", || kept() == [alert]);
    assert!(!instance.home().join("cleanup-sweep.json").exists(), "the sweep found nothing to report");
    let ledger: Value =
        serde_json::from_slice(&std::fs::read(instance.home().join("sandbox-ownership.json")).unwrap()).unwrap();
    assert!(ledger.get("atomics-gone").is_none(), "{ledger}");

    let doctor = test_support::output(&mut instance.command(["doctor"]));
    let text = String::from_utf8_lossy(&doctor.stdout);
    assert_eq!(text.matches("cleanup leftover").count(), 1, "{text}");
    assert!(text.contains(&format!("✗ cleanup leftover (warning): {alert}")), "{text}");

    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn settings_proposals_and_host_folder_browsing_work_without_a_window() {
    use atomics_core::ops::{HostAction, Outcome};
    let instance = Instance::new(Some(0));
    let mut host = instance.host();
    instance.ready(&mut host);
    let config = instance.dir.path().join("config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    let proposed = host_command(
        &instance.home(),
        HostAction::ConfigPropose {
            scope: atomics_core::selfconfig::Scope::Main,
            toml: "[layout]\nmin_tile_width = 401.0".into(),
            remove: vec![],
            theme: None,
            reason: Some("Review a layout change".into()),
        },
    );
    let Outcome::Done { result } = proposed else { panic!("{proposed:?}") };
    let id = result["proposal"].as_str().unwrap().to_string();
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    let mirror =
        board(&instance.home(), |m| m.host.management.as_ref().is_some_and(|i| i.proposals.iter().any(|p| p.id == id)));
    let info = mirror.host.management.unwrap();
    assert!(info.proposals[0].diff.contains("401"));
    assert_ne!(info.config["values"]["layout.min_tile_width"]["value"], 401.0);
    let applied = host_command(&instance.home(), HostAction::ConfigDecide { id, apply: true, note: "Reviewed".into() });
    assert!(matches!(applied, Outcome::Done { .. }), "{applied:?}");
    board(&instance.home(), |m| {
        m.host
            .management
            .as_ref()
            .is_some_and(|i| i.proposals.is_empty() && i.config["values"]["layout.min_tile_width"]["value"] == 401.0)
    });
    let folder = instance.dir.path().join("folders");
    std::fs::create_dir_all(folder.join("project")).unwrap();
    let browsed =
        host_command(&instance.home(), HostAction::BrowseFolder { path: folder.to_string_lossy().into_owned() });
    assert!(matches!(browsed, Outcome::Done { .. }), "{browsed:?}");
    let canonical = folder.canonicalize().unwrap().to_string_lossy().into_owned();
    board(&instance.home(), |m| {
        m.host.management.as_ref().is_some_and(|i| {
            i.folders.iter().any(|f| f.page.worktree == canonical && f.page.entries.iter().any(|c| c.path == "project"))
        })
    });
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

#[test]
fn headless_chief_trace_streams_cleaned_running_and_completed_calls_without_a_window() {
    let instance = Instance::new(Some(0));
    let config_path = instance.dir.path().join("config.toml");
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    // Accept the chief's launch arguments while keeping a harmless local PTY alive.
    config["chief"]["command"] = toml::Value::try_from(vec!["/bin/sh", "-c", "exec cat", "trace-chief"]).unwrap();
    config["chief"]["args"] = toml::Value::Array(Vec::new());
    config["chief"].as_table_mut().unwrap().insert("autostart".into(), toml::Value::Boolean(true));
    std::fs::write(config_path, toml::to_string(&config).unwrap()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    let mirror = board(&instance.home(), |m| m.sessions.values().any(|s| s.chief));
    let chief = mirror.sessions.values().find(|s| s.chief).unwrap().id.clone();
    let list = instance.ctl(json!({"cmd":"list"}));
    let conversation =
        list["sessions"].as_array().unwrap().iter().find(|s| s["id"] == chief).unwrap()["claude_session_id"]
            .as_str()
            .unwrap()
            .to_string();
    let mut socket = connect(&instance.home());
    send(&mut socket, Envelope::new(Message::Subscribe { topics: vec![Topic::Trace] }));
    let capture = |name: &str| {
        let mut hook = TestChild::spawn(
            instance
                .command(["hook", "--socket"])
                .arg(instance.socket())
                .args(["--pane", &chief])
                .stdin(Stdio::piped()),
            CHILD_TIMEOUT,
        )
        .unwrap();
        let event = json!({
            "hook_event_name":name, "session_id":conversation, "tool_name":"mcp__example__lookup",
            "tool_use_id":"call-1", "tool_input":{"query":"hello", "api_key":"never-publish-this"},
            "tool_response":{"text":"found"}
        });
        let mut stdin = hook.child.stdin.take().unwrap();
        write!(stdin, "{event}").unwrap();
        drop(stdin);
        assert!(hook.wait_output().unwrap().status.success());
    };
    let mut read_call = |state: atomics_core::trace::TraceState| {
        let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
        loop {
            assert!(Instant::now() < deadline, "trace was not published without a window");
            match socket.read() {
                Ok(tungstenite::Message::Text(text)) => match wire::decode(text.as_str()).unwrap().message {
                    Message::TraceChanged { revision } => {
                        send(&mut socket, Envelope::new(Message::TraceGet { revision, offset: 0, limit: 32 }))
                    }
                    Message::TracePage(page) if page.data.entries.first().is_some_and(|e| e.state == state) => {
                        assert_eq!(page.data.session.as_deref(), Some(chief.as_str()));
                        assert_eq!(page.data.entries.len(), 1);
                        assert!(!serde_json::to_string(&page).unwrap().contains("never-publish-this"));
                        return page.data.entries[0].seq;
                    }
                    Message::Ping { nonce } => send(&mut socket, Envelope::new(Message::Pong { nonce })),
                    _ => {}
                },
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => panic!("trace connection failed: {e}"),
            }
        }
    };
    capture("PreToolUse");
    let seq = read_call(atomics_core::trace::TraceState::Running);
    capture("PostToolUse");
    assert_eq!(read_call(atomics_core::trace::TraceState::Ok), seq);
}

#[test]
fn a_headless_editor_streams_and_accepts_leased_input_after_the_client_reconnects() {
    use atomics_core::ops::{HostAction, Outcome};
    use std::os::unix::fs::PermissionsExt;
    let instance = Instance::new(Some(0));
    let script = instance.dir.path().join("editor");
    let received = instance.dir.path().join("editor-input");
    std::fs::write(&script, format!("#!/bin/sh\nstty -echo\nprintf 'editor-ready\\r\\n'\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> '{}'; done\n", received.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = instance.dir.path().join("config.toml");
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["editor"]["command"] = toml::Value::Array(vec![toml::Value::String(script.to_string_lossy().into_owned())]);
    config["editor"]["startup_keys"] = toml::Value::Array(vec![]);
    config["editor"]["open_keys"] = toml::Value::Array(vec![]);
    std::fs::write(config_path, toml::to_string(&config).unwrap()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    let started = instance.ctl(json!({"cmd":"start", "agent":"shell", "isolation":"bare"}));
    let session = started["id"].as_str().unwrap();
    let mirror = board(&instance.home(), |m| m.sessions.contains_key(session));
    let row = &mirror.sessions[session];
    let root = row.worktree.as_ref().unwrap_or(&row.cwd).clone();
    let outcome = host_command(
        &instance.home(),
        HostAction::Edit { root: root.clone(), path: Some("sample.rs".into()), line: Some(3) },
    );
    assert!(matches!(outcome, Outcome::Done { .. }), "{outcome:?}");
    let mirror = board(&instance.home(), |m| m.host.editors.iter().any(|e| e.root == root && e.running));
    let editor = mirror.host.editors.iter().find(|e| e.root == root).unwrap();
    assert_eq!(editor.file.as_deref(), Some("sample.rs"));
    let terminal = editor.terminal.clone();
    assert!(!mirror.sessions.contains_key(&terminal), "an editor is not an agent session");
    // host_command closed its socket. A fresh client follows the still-running editor and types under its lease.
    type_into(&instance.home(), &terminal, "editor-input\n");
    eventually("leased editor input", || std::fs::read_to_string(&received).is_ok_and(|s| s.contains("editor-input")));
    let forbidden =
        host_command(&instance.home(), HostAction::Edit { root, path: Some("../outside".into()), line: None });
    assert!(matches!(forbidden, Outcome::Refused { .. }));
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}
