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
            sessions: Vec::new(),
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
        let mut output = ctx.run_ui(input, |ui| assert!(app.vm_screen(ui)));
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
    let shown = frame();
    assert!(shown.iter().any(|text| text == "The VM atomics is stopped."), "{shown:?}");
    assert!(shown.iter().any(|text| text == "Start atomics"), "{shown:?}");
    gate.set_test_status(crate::vm::Status::Running);
    let shown = frame();
    assert!(shown.iter().any(|text| text == "Starting the VM atomics…"), "{shown:?}");
    assert!(!shown.iter().any(|text| text == "Start atomics"), "an already running VM must wait for its host");
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
