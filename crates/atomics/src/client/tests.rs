//! The native client against servers that misbehave: malformed frames are skipped, a host of another
//! major version stops it with a message rather than retrying forever, and a refused token stops it too.
//! The real host's gates are in `app::client_tests`.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use atomics_core::wire::{self, Envelope, Message};
use atomics_web::driver::{Status, Stop};
use tungstenite::handshake::server::{Request, Response};
use zeroize::Zeroizing;

use super::ClientApp;
use super::link::{Address, Endpoint};
use super::vm;

const HELD_SCREENS: [(&str, &str); 7] = [
    ("codex-0.170-folder-restricted.txt", "Open restricted"),
    ("codex-0.170-folder-trust.txt", "Trust and continue"),
    ("codex-0.170-permission-request.txt", "Would you like to grant these permissions?"),
    ("codex-0.165-approval-screen.txt", "Would you like to run the following command?"),
    ("codex-0.170-model-picker.txt", "Select Model and Effort"),
    ("codex-0.170-reasoning-picker.txt", "Select Reasoning Level"),
    ("codex-0.170-permissions-picker.txt", "Update Model Permissions"),
];

#[test]
fn a_paired_native_window_can_answer_held_agent_and_chief_screens() {
    use std::{path::Path, process::Command};

    use atomics_core::control::ControlRequest;
    use serde_json::json;

    use crate::test_support::{self, CHILD_TIMEOUT, TestChild};

    const PROCESS: &str = "ATOMICS_HELD_SCREEN_TEST_PROCESS";
    match std::env::var(PROCESS).as_deref() {
        Ok("host") => std::process::exit(crate::headless::run(crate::app::Options::default())),
        Ok("client") => {
            let url = std::env::var("ATOMICS_HELD_SCREEN_TEST_URL").unwrap();
            enter_on_held_screens(super::target::Target::parse(&url).unwrap());
            return;
        }
        _ => {}
    }

    let dir = test_support::tempdir().unwrap();
    let root = dir.path();
    let worker = root.join("worker");
    let chief = root.join("chief");
    let home = root.join("state");
    for folder in [&worker, &chief, &home] {
        std::fs::create_dir(folder).unwrap();
    }
    let program = root.join("held-screens");
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../atomics-core/tests/fixtures");
    test_support::write_program(
        &program,
        format!(
            r#"#!/usr/bin/env python3
import os, pathlib, time, tty
tty.setraw(0)
fixtures = pathlib.Path({})
for index, filename in enumerate({}):
    screen = (fixtures / filename).read_text()
    print('\033[2J\033[H' + screen.replace('\n', '\r\n'), end='', flush=True)
    key = os.read(0, 1)
    print('\033[2J\033[HKeys: ' + key.hex() + '\r\n\r\n› ', end='', flush=True)
    while not pathlib.Path(f'next-{{index}}').exists():
        time.sleep(0.02)
while os.read(0, 4096):
    pass
"#,
            json!(fixtures),
            json!(HELD_SCREENS.map(|(file, _)| file))
        ),
    );
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[remote]\nenabled = true\nport = 0\nhost_input_wrappers = [{program:?}]\n[isolation]\nmode = 'bare'\n[chief]\ncommand = [{program:?}]\ncwd = {chief:?}\n[[agents]]\nname = 'fixture'\ncommand = [{program:?}]\n"
        ),
    )
    .unwrap();
    let paired = crate::protocol::clients::PairedClients::open(home.join("paired-clients.json")).unwrap();
    let (_, token) = paired.pair("Fixture desktop").unwrap();
    assert!(paired.grant_host(&paired.find(&token).unwrap().id).unwrap());
    let socket = root.join("atomics.sock");
    let child = |role: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "client::tests::a_paired_native_window_can_answer_held_agent_and_chief_screens"])
            .current_dir(root)
            .env("HOME", root)
            .env("ATOMICS_HOME", if role == "host" { &home } else { root })
            .env("ATOMICS_CONFIG", &config)
            .env("ATOMICS_SOCKET", &socket)
            .env("ATOMICS_CREDENTIALS_DIR", root.join("credentials"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env(PROCESS, role);
        command
    };
    let mut host = TestChild::spawn(&mut child("host"), CHILD_TIMEOUT).unwrap();
    let deadline = Instant::now() + test_support::scaled_timeout(Duration::from_secs(15));
    let advert = loop {
        if let Ok(advert) = atomics_core::advert::read(&home.join("protocol.json")) {
            break advert;
        }
        assert!(host.child.try_wait().unwrap().is_none(), "the fixture host exited");
        assert!(Instant::now() < deadline, "the fixture host did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    for request in [json!({"cmd":"start", "agent":"fixture", "folder":worker}), json!({"cmd":"start_chief"})] {
        let request: ControlRequest = serde_json::from_value(request).unwrap();
        let reply = crate::ipc::request_at(&socket, &request).unwrap();
        assert!(reply.ok, "{:?}", reply.error);
    }
    crate::relay::Keychain::in_folder(&root.join("credentials"))
        .write_item(super::target::PAIRED_SERVICE, &advert.url, "Fixture desktop", &token)
        .unwrap();
    let mut client =
        TestChild::spawn(child("client").env("ATOMICS_HELD_SCREEN_TEST_URL", &advert.url), CHILD_TIMEOUT).unwrap();
    let output = client.wait_output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    assert!(host.wait_output().unwrap().status.success());
}

fn enter_on_held_screens(target: super::target::Target) {
    use atomics_core::{config::UiMode, ops::Grant};
    use eframe::App as _;
    use egui::{Event, Key, Modifiers, PointerButton, RawInput, Rect, pos2, vec2};

    let ctx = egui::Context::default();
    let mut app = ClientApp::new(&ctx, target, None, super::Launch::default());
    app.client.mode = Some(UiMode::Full);
    let frame = |app: &mut ClientApp, events| {
        let mut raw = RawInput {
            events,
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1600.0, 960.0))),
            ..RawInput::default()
        };
        app.raw_input_hook(&ctx, &mut raw);
        app.pump();
        let mut output = ctx.run_ui(raw, |ui| app.draw(ui));
        output.textures_delta.clear();
        output.shapes
    };
    let wait = |app: &mut ClientApp, what: &str, done: &dyn Fn(&ClientApp) -> bool| {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        while !done(app) {
            frame(app, Vec::new());
            assert!(
                Instant::now() < deadline,
                "{what}: {:?}, {:?}, keyboard={}, pane={}, widget={:?}",
                app.driver.status(),
                app.driver.remote.notice(),
                app.keyboard.focused,
                app.view.term.typing(),
                ctx.memory(|m| m.focused())
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut app, "the native desktop sees the worker and chief", &|app| {
        app.driver.remote.may(Grant::Host) && app.driver.remote.with_mirror(|m| m.sessions.len() == 2)
    });
    app.view.desk.diff_open = false;
    app.view.desk.chief.open = false;
    for chief in [false, true] {
        let (id, cwd, title) = app.driver.remote.with_mirror(|m| {
            let session = m.sessions.values().find(|s| s.chief == chief).unwrap();
            (session.id.clone(), std::path::PathBuf::from(&session.cwd), session.title.clone())
        });
        if chief {
            app.view.open(atomics_web::views::Tab::Chief);
            app.view.desk.typing_in = Some(id.clone());
        }
        let showing = |app: &ClientApp, text: &str| {
            app.driver
                .remote
                .with_screen(&id, |screen| screen.is_some_and(|(screen, stale)| !stale && screen.text().contains(text)))
        };
        wait(&mut app, "the terminal is drawn", &|app| {
            app.view.term.session() == Some(&id)
                && app.view.term.overlay.rect.is_some()
                && showing(app, "Open restricted")
        });
        let pos = if chief {
            app.view.term.overlay.rect.unwrap().center()
        } else {
            frame(&mut app, Vec::new())
                .into_iter()
                .filter_map(|shape| match shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == title => {
                        Some(text.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .find(|pos| pos.y < app.view.term.overlay.rect.unwrap().top())
                .expect("the agent's title")
        };
        for pressed in [true, false] {
            frame(
                &mut app,
                vec![
                    Event::PointerMoved(pos),
                    Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE },
                ],
            );
        }
        if !chief {
            assert!(app.view.desk.diff_open, "selecting the title opens the diff");
        }
        for (index, (_, title)) in HELD_SCREENS.iter().enumerate() {
            wait(&mut app, title, &|app| showing(app, title));
            frame(
                &mut app,
                vec![Event::Key {
                    key: Key::Enter,
                    physical_key: Some(Key::Enter),
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
            );
            wait(&mut app, &format!("Enter reaches {title} (chief={chief})"), &|app| showing(app, "Keys: 0d"));
            std::fs::write(cwd.join(format!("next-{index}")), "").unwrap();
        }
    }
}

/// What the fake server does after it accepts the upgrade.
type Script = fn(&mut tungstenite::WebSocket<std::net::TcpStream>);

/// A server that accepts one connection: refuses its upgrade with `refuse`, or runs `script`.
fn server(refuse: Option<u16>, script: Script) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else { return };
        #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
        let callback = |request: &Request, mut response: Response| {
            assert_eq!(
                request.headers().get("Authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer the-token"),
                "the token goes in the upgrade"
            );
            if let Some(status) = refuse {
                return Err(tungstenite::http::Response::builder().status(status).body(None).unwrap());
            }
            response.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
            Ok(response)
        };
        if let Ok(mut ws) = tungstenite::accept_hdr(stream, callback) {
            script(&mut ws);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    addr
}

fn client(addr: SocketAddr) -> ClientApp {
    let ctx = egui::Context::default();
    ClientApp::fixed(&ctx, move || {
        Ok(Endpoint {
            addr: Address::Tcp(addr),
            host: format!("127.0.0.1:{}", addr.port()),
            token: Zeroizing::new("the-token".into()),
        })
    })
}

fn until(app: &mut ClientApp, what: &str, done: impl Fn(&Status) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    while !done(app.driver().status()) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", app.driver().status());
        app.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The client's hello, then the host's welcome: what every script starts with.
fn greet(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) {
    let hello = ws.read().unwrap();
    let hello = wire::decode(hello.to_text().unwrap()).unwrap();
    assert!(matches!(hello.message, Message::Hello { .. }), "{hello:?}");
}

#[test]
fn malformed_frames_are_skipped_and_another_major_version_stops_the_client() {
    let addr = server(None, |ws| {
        greet(ws);
        for junk in ["{not json", "[1,2]", r#"{"v":{"major":1,"minor":0},"kind":"teleport"}"#] {
            ws.send(tungstenite::Message::text(junk)).unwrap();
        }
        let ping = Envelope::new(Message::Ping { nonce: Some("still-here".into()) });
        ws.send(tungstenite::Message::text(serde_json::to_string(&ping).unwrap())).unwrap();
        // The client answers the ping after the junk: it skipped it and is still there.
        loop {
            let frame = ws.read().unwrap();
            let Ok(envelope) = wire::decode(frame.to_text().unwrap()) else { continue };
            if matches!(envelope.message, Message::Pong { .. }) {
                break;
            }
        }
        ws.send(tungstenite::Message::text(r#"{"v":{"major":2,"minor":0},"kind":"ping"}"#)).unwrap();
    });
    let mut app = client(addr);
    until(&mut app, "the version stop", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Version));
    assert!(Stop::Version.message().contains("version"));
}

#[test]
fn a_host_of_another_major_version_refusing_the_hello_stops_the_client() {
    let addr = server(None, |ws| {
        greet(ws);
        let refusal = Envelope::new(Message::Error {
            code: wire::ErrorCode::UnsupportedVersion,
            message: "this host speaks 2.0".into(),
        });
        ws.send(tungstenite::Message::text(serde_json::to_string(&refusal).unwrap())).unwrap();
        let _ = ws.close(None);
    });
    let mut app = client(addr);
    until(&mut app, "the version stop", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Version));
}

#[test]
fn a_refused_token_stops_the_client() {
    let addr = server(Some(401), |_| {});
    let mut app = client(addr);
    until(&mut app, "the refusal", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Unauthorised));
}

#[test]
fn a_host_that_is_not_there_is_tried_again_and_the_window_says_why() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut app = client(addr);
    until(&mut app, "a reconnect", |status| matches!(status, Status::Reconnecting { .. }));
    let why = app.unreachable().unwrap_or_default();
    assert!(why.starts_with(&format!("cannot connect to {addr}: ")), "{why}");
}

/// What finding the host failed on (the VM's forward, say) is what the window shows.
#[test]
fn a_host_that_cannot_be_found_says_why() {
    let ctx = egui::Context::default();
    let mut app = ClientApp::fixed(&ctx, || anyhow::bail!("the VM atomics is stopped"));
    until(&mut app, "a reconnect", |status| matches!(status, Status::Reconnecting { .. }));
    assert_eq!(app.unreachable(), Some("the VM atomics is stopped"));
}

#[test]
fn saved_machine_previews_are_drawn_only_in_the_machine_window() {
    let ctx = egui::Context::default();
    let mut app = ClientApp::fixed(&ctx, || anyhow::bail!("host offline"));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("5173.json"),
        br#"{"source":null,"preview":{"port":5173},"local":5173,"tls":false,"waiting_since":null}"#,
    )
    .unwrap();
    app.previews.restore(dir.path().into());
    app.pump_previews();
    let frame = |app: &mut ClientApp, width| {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 800.0))),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.draw(ui));
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for width in [1400.0, 490.0] {
        for mode in [atomics_core::config::UiMode::Full, atomics_core::config::UiMode::Simple] {
            app.client.mode = Some(mode);
            app.saved_mode = Some(mode);
            app.client.machine_open = false;
            let shown = frame(&mut app, width);
            assert!(!shown.iter().any(|text| text.contains("5173")), "{shown:?}");
            assert!(shown.iter().any(|text| text == "Machine"), "{shown:?}");

            app.client.machine_open = true;
            frame(&mut app, width);
            let shown = frame(&mut app, width);
            assert!(shown.iter().any(|text| text == "Port 5173"), "{shown:?}");
            assert!(shown.iter().any(|text| text == "Stop"), "{shown:?}");
        }
    }
}

#[test]
fn a_mirrored_board_does_not_hide_the_stopped_vm_or_its_start_button() {
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().unwrap();
    let gate = vm::Gate::with(None, dir.path().into(), crate::relay::Keychain::in_folder(dir.path()));
    gate.set_test_status(crate::vm::Status::Stopped);
    let mut app = ClientApp::fixed_vm(&ctx, gate.clone(), || anyhow::bail!("host gone"));
    app.driver.remote.receive(Envelope::host(
        "old-board",
        Message::StateSnapshot {
            cursor: wire::Seq(1),
            revisions: wire::Revisions {
                sessions: wire::Seq(1),
                plans: wire::Seq(1),
                approvals: wire::Seq(1),
                diffs: wire::Seq(1),
                host: wire::Seq(1),
            },
            sessions: vec![atomics_core::projection::SessionInfo {
                id: "mirrored".into(),
                agent: "shell".into(),
                title: "Mirrored agent".into(),
                cwd: "/project".into(),
                worktree: None,
                branch: None,
                status: atomics_core::session::AgentStatus::Idle,
                chief: false,
                task: None,
                pending_approvals: 0,
                status_hint: None,
                previews: Vec::new(),
                activity: None,
            }],
            plans: Vec::new(),
            approvals: Vec::new(),
            diffs: Vec::new(),
            host: Default::default(),
            part: None,
        },
    ));
    assert!(app.driver.remote.with_mirror(|mirror| mirror.epoch.is_some()));
    let mut frame = || {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 700.0))),
            ..Default::default()
        };
        let mut covered = false;
        let mut output = ctx.run_ui(input, |ui| covered = app.vm_screen(ui));
        output.textures_delta.clear();
        let text = output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        (covered, text)
    };
    let (covered, shown) = frame();
    assert!(covered, "a stopped VM replaces the stale board");
    assert!(shown.iter().any(|text| text == "The VM atomics is stopped."), "{shown:?}");
    assert!(shown.iter().any(|text| text == "Start atomics"), "{shown:?}");
    gate.set_test_status(crate::vm::Status::Running);
    let (covered, shown) = frame();
    assert!(!covered, "an up VM keeps the board while its host reconnects");
    assert!(!shown.iter().any(|text| text == "Starting the VM atomics…"), "{shown:?}");
    assert!(!shown.iter().any(|text| text == "Start atomics"), "an already running VM must wait for its host");
    let socket = app
        .driver
        .take_outputs()
        .into_iter()
        .find_map(|output| match output {
            super::Output::DialDirect { socket } => Some(socket),
            _ => None,
        })
        .unwrap();
    app.driver.socket_closed(socket, 1006);
    assert!(matches!(app.driver.status(), Status::Reconnecting { .. }));
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| assert!(!app.vm_screen(ui)));
    output.textures_delta.clear();
    app.client.machine_open = true;
    for _ in 0..3 {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.draw(ui));
        output.textures_delta.clear();
    }
    let mut shown = ctx.run_ui(egui::RawInput::default(), |ui| app.draw(ui));
    shown.textures_delta.clear();
    let shown: Vec<_> = shown
        .shapes
        .into_iter()
        .filter_map(|shape| match shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect();
    for control in ["VM", "Stop", "Check", "Update"] {
        assert!(shown.iter().any(|text| text == control), "Machine is missing {control}: {shown:?}");
    }
    assert!(shown.iter().any(|text| text == "VM atomics running, host not connected"), "{shown:?}");
    assert!(shown.iter().any(|text| text.contains("Mirrored agent")), "the mirrored agent stays visible: {shown:?}");
}

#[test]
fn vm_host_disconnects_refusals_and_stops_have_their_own_text() {
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().unwrap();
    let gate = vm::Gate::with(None, dir.path().into(), crate::relay::Keychain::in_folder(dir.path()));
    gate.set_test_status(crate::vm::Status::Running);
    let mut app = ClientApp::fixed_vm(&ctx, gate.clone(), || anyhow::bail!("host gone"));
    let socket = app
        .driver
        .take_outputs()
        .into_iter()
        .find_map(|output| match output {
            super::Output::DialDirect { socket } => Some(socket),
            _ => None,
        })
        .unwrap();
    let frame = |app: &mut ClientApp| {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| assert!(app.vm_screen(ui)));
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    for reconnecting in [false, true] {
        if reconnecting {
            app.driver.socket_closed(socket, 1006);
        }
        let shown = frame(&mut app);
        assert!(shown.iter().any(|text| text == "VM atomics running, host not connected"), "{shown:?}");
        assert!(!shown.iter().any(|text| text.contains("Starting the VM")), "{shown:?}");
    }
    // A new dial carries the refusal, just as a real reconnect would.
    app.driver = atomics_web::driver::Driver::direct("refused");
    let socket = app
        .driver
        .take_outputs()
        .into_iter()
        .find_map(|output| match output {
            super::Output::DialDirect { socket } => Some(socket),
            _ => None,
        })
        .unwrap();
    app.driver.socket_closed(socket, atomics_core::tunnel::close::UNAUTHORISED);
    let shown = frame(&mut app);
    assert!(shown.iter().any(|text| text == "VM atomics running, host refused this Mac's token"), "{shown:?}");
    gate.set_test_busy(Some("stop"));
    let shown = frame(&mut app);
    assert!(shown.iter().any(|text| text == "Stopping the VM atomics…"), "{shown:?}");
    assert!(!shown.iter().any(|text| text == "Start atomics"), "{shown:?}");
}

/// A `connect` value ATOMICS cannot open names the setting, the file that sets it, and the way out.
#[test]
fn a_bad_connect_value_says_where_it_is_set() {
    let error = super::target::Target::parse("vm2").unwrap_err();
    let file = std::path::Path::new("/home/user/.config/atomics/config.d/mac.toml");
    let message = super::bad_connect_message("vm2", false, Some(file), &error);
    assert!(message.starts_with("[client] connect = \"vm2\" in /home/user/.config/atomics/config.d/mac.toml: "));
    assert!(message.contains("connect to `local`, `vm` or ws://127.0.0.1:PORT"), "{message}");
    assert!(message.contains("remove it to run agents on this Mac"), "{message}");
    let message = super::bad_connect_message("vm2", true, None, &error);
    assert!(message.starts_with("--connect \"vm2\": connect to"), "{message}");
}

/// The VM's host refusing this Mac's token stops the window and asks: the token stays in the keychain,
/// and nothing dials or pairs again until the user says "Pair again".
#[test]
fn a_token_the_vms_host_refuses_is_kept_until_the_user_pairs_again() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let dials = Arc::new(AtomicUsize::new(0));
    {
        let dials = dials.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                dials.fetch_add(1, Ordering::SeqCst);
                #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
                let refuse = |_: &Request, _: Response| {
                    Err(tungstenite::http::Response::builder().status(401).body(None).unwrap())
                };
                let _ = tungstenite::accept_hdr(stream, refuse);
            }
        });
    }
    let dir = crate::test_support::tempdir().unwrap();
    let info = crate::vm::BundleInfo {
        version: 1,
        instance: "atomics-test".into(),
        user: "atomics".into(),
        home: "/home/atomics".into(),
        mac_user: "user".into(),
        install_dir: dir.path().display().to_string(),
        host: Some(crate::vm::HostInfo { port: addr.port() }),
        admin: None,
        build: None,
        flavor: crate::vm::Flavor::Standard,
    };
    let keychain = crate::relay::Keychain::in_folder(&dir.path().join("credentials"));
    keychain.write_item(vm::KEYCHAIN_SERVICE, &info.instance, "test", "the-token").unwrap();
    let kept = || keychain.read_item(vm::KEYCHAIN_SERVICE, "atomics-test").unwrap();
    let gate = vm::Gate::with(Some(info), dir.path().to_path_buf(), keychain.clone());
    let ctx = egui::Context::default();
    let mut app = ClientApp::fixed_vm(&ctx, gate, move || {
        Ok(Endpoint { addr: Address::Tcp(addr), host: addr.to_string(), token: Zeroizing::new("the-token".into()) })
    });
    until(&mut app, "the refusal", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Unauthorised));
    assert!(app.asks_to_pair_again());
    // Well past the driver's first wait: nothing dials, and the token is not forgotten.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        app.pump();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(dials.load(Ordering::SeqCst), 1, "the window dialled again on its own");
    assert_eq!(kept().as_deref(), Some("the-token"), "the refused token was forgotten without asking");
    assert!(app.asks_to_pair_again());
    // "Pair again": the token goes, and the window dials (and would pair) at once.
    app.pair_again();
    assert_eq!(kept(), None);
    assert!(!app.asks_to_pair_again());
    assert_eq!(app.unreachable(), None, "the refusal is not news while it pairs again");
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    while dials.load(Ordering::SeqCst) < 2 {
        assert!(Instant::now() < deadline, "no dial after Pair again");
        app.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A host that refuses every connection keeps the window off its board: "Restart host" is there all the same,
/// and a restart the host refuses for busy work lists that work and asks before going on.
#[test]
fn a_vm_host_the_window_cannot_reach_can_be_restarted_and_busy_work_is_asked_about() {
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir().unwrap();
    let info: crate::vm::BundleInfo = serde_json::from_value(serde_json::json!({
        "version": 1, "instance": "atomics", "user": "atomics", "home": "/home/atomics", "macUser": "alice",
        "installDir": "/Users/alice/.local/share/atomics-vm", "host": {"port": 47800}, "admin": "admin"
    }))
    .unwrap();
    let gate = vm::Gate::with(Some(info), dir.path().into(), crate::relay::Keychain::in_folder(dir.path()));
    gate.set_test_status(crate::vm::Status::Running);
    let mut app = ClientApp::fixed_vm(&ctx, gate.clone(), || anyhow::bail!("refused: frame limit"));
    let frame = |app: &mut ClientApp| {
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| assert!(app.vm_screen(ui)));
        output.textures_delta.clear();
        output
            .shapes
            .into_iter()
            .filter_map(|shape| match shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let shown = frame(&mut app);
    assert!(shown.iter().any(|text| text == "VM atomics running, host not connected"), "{shown:?}");
    assert!(shown.iter().any(|text| text == "Restart host"), "{shown:?}");

    // `atomics vm restart-host` stops for busy work with its own exit code; the window asks.
    let refusing = dir.path().join("atomics");
    crate::test_support::write_program(
        &refusing,
        "#!/bin/sh\nprintf 'atomics vm: work in the VM is busy:\\n  agent parser (s1) is working\\nnot restarted\\n' >&2\nexit 3\n",
    );
    gate.run_test_command(refusing, "restart-host");
    let shown = frame(&mut app);
    assert!(shown.iter().any(|text| text.contains("agent parser (s1) is working")), "{shown:?}");
    assert!(shown.iter().any(|text| text == "Restart anyway") && shown.iter().any(|text| text == "Cancel"));
}

#[test]
fn a_connected_windows_own_layout_is_kept_in_a_file_of_its_own() {
    use atomics_core::config::UiMode;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state").join("client-mode");
    assert_eq!(super::load_mode(&file), None, "nothing kept: the host's mode");
    super::save_mode(&file, Some(UiMode::Simple)).unwrap();
    assert_eq!(super::load_mode(&file), Some(UiMode::Simple));
    super::save_mode(&file, Some(UiMode::Full)).unwrap();
    assert_eq!(super::load_mode(&file), Some(UiMode::Full));
    super::save_mode(&file, None).unwrap();
    assert!(!file.exists());
    super::save_mode(&file, None).unwrap();
    assert_eq!(super::mode_file().file_name().unwrap(), "client-mode");
    assert_ne!(super::mode_file(), atomics_core::paths::config_file());
}
