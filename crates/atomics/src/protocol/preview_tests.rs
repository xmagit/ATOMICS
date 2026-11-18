use super::*;
use atomics_core::preview::Preview;
use atomics_core::preview::PreviewSource;
use atomics_core::wire::{MAX_TUNNELS, TUNNEL_CHUNK, TUNNEL_WINDOW};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener};

fn next_matching(client: &mut TestClient, predicate: impl Fn(&Message) -> bool) -> Message {
    loop {
        let message = client.next().expect("protocol frame").message;
        if predicate(&message) {
            return message;
        }
    }
}

#[test]
fn previews_require_negotiation_and_a_native_controller() {
    let f = start_with(ServerConfig { allowed_origins: vec!["https://app.example".into()], ..config() });
    for (origin, capabilities) in
        [(None, vec!["board".into()]), (Some("https://app.example"), vec!["board".into(), wire::TUNNEL.into()])]
    {
        let mut client = TestClient::try_connect(f.server.addr(), Some(f.server.token()), origin, true).unwrap();
        client.send(&Envelope::new(Message::Hello {
            client_id: "test".into(),
            versions: vec![VERSION],
            capabilities,
            prior_epoch: None,
            prior_cursor: None,
        }));
        let welcome = next_matching(&mut client, |m| matches!(m, Message::Welcome { .. }));
        assert!(
            matches!(welcome, Message::Welcome { capabilities, .. } if !capabilities.iter().any(|c| c == wire::TUNNEL))
        );
        client.send(&Envelope::new(Message::TunnelOpen {
            source: PreviewSource::Session("a".into()),
            id: "t".into(),
            port: 5173,
        }));
        assert!(matches!(
            next_matching(&mut client, |m| matches!(m, Message::Error { .. })),
            Message::Error { code: ErrorCode::Unauthorised, .. }
        ));
    }
    let viewer = start_with(ServerConfig { role: wire::Role::Viewer, ..config() });
    let mut client = TestClient::connect(viewer.server.addr(), viewer.server.token());
    client.hello();
    assert!(
        matches!(next_matching(&mut client, |m| matches!(m, Message::Welcome { .. })), Message::Welcome { capabilities, .. } if !capabilities.iter().any(|c| c == wire::TUNNEL))
    );
}

#[test]
fn previews_refuse_undeclared_ports_non_loopback_targets_and_revoked_ports() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.send(&Envelope::new(Message::TunnelOpen {
        source: PreviewSource::Session("a".into()),
        id: "t".into(),
        port: 5173,
    }));
    assert!(
        matches!(next_matching(&mut client, |m| matches!(m, Message::TunnelRefused { .. })), Message::TunnelRefused { id, .. } if id == "t")
    );
    let error = crate::preview::relay::connect_loopback("192.0.2.1:5173".parse().unwrap()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut info = session("a", "web", AgentStatus::Working);
    info.previews.push(Preview { port, path: String::new(), label: String::new(), ..Default::default() });
    f.projections.publish_sessions(vec![info.clone()]);
    client.send(&Envelope::new(Message::TunnelOpen {
        source: PreviewSource::Session("a".into()),
        id: "allowed".into(),
        port,
    }));
    next_matching(&mut client, |m| matches!(m, Message::TunnelOpened { .. }));
    let (_socket, _) = listener.accept().unwrap();
    assert_eq!(f.projections.machine_watchers.load(std::sync::atomic::Ordering::Acquire), 1);
    info.id = "b".into();
    f.projections.publish_sessions(vec![info]);
    assert!(matches!(
        next_matching(&mut client, |m| matches!(m, Message::TunnelClose { .. })),
        Message::TunnelClose { abort: true, .. }
    ));
    client.send(&Envelope::new(Message::TunnelOpen {
        id: "stale".into(),
        port,
        source: PreviewSource::Session("a".into()),
    }));
    assert!(matches!(
        next_matching(&mut client, |m| matches!(m, Message::TunnelRefused { .. } | Message::TunnelOpened { .. })),
        Message::TunnelRefused { .. }
    ));
    assert_eq!(f.projections.machine_watchers.load(std::sync::atomic::Ordering::Acquire), 0);
}

#[test]
fn http_crosses_the_protocol_and_native_listener_in_both_directions_past_one_window() {
    let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = http.local_addr().unwrap().port();
    let body: Vec<u8> = (0..TUNNEL_WINDOW as usize * 3 + 97).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let service = std::thread::spawn(move || {
        let (mut socket, _) = http.accept().unwrap();
        socket.set_read_timeout(Some(wait(Duration::from_secs(10)))).unwrap();
        let mut request = Vec::new();
        socket.read_to_end(&mut request).unwrap(); // Client EOF must leave the response direction open.
        let split = request.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(request.starts_with(b"POST /demo HTTP/1.1\r\n"));
        assert_eq!(&request[split..], &body);
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(&body);
        socket.write_all(&response).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        response
    });
    let f = start();
    let mut info = session("web", "web", AgentStatus::Working);
    info.previews.push(Preview { port, path: "demo".into(), label: "Web".into(), ..Default::default() });
    f.projections.publish_sessions(vec![info.clone()]);
    let remote = atomics_web::remote::RemoteBackend::new("preview-test");
    remote.enable_native_previews();
    let mut protocol = TestClient::connect(f.server.addr(), f.server.token());
    protocol.send(&remote.hello("native"));
    while !remote.with_mirror(|m| m.sessions.contains_key("web")) {
        remote.receive(protocol.next().unwrap());
    }
    assert!(remote.tunnels());
    let mut native = crate::preview::client::Client::default();
    let url = native.open(PreviewSource::Session("web".into()), info.previews[0].clone()).unwrap();
    let local: u16 = url.strip_prefix("http://127.0.0.1:").unwrap().split('/').next().unwrap().parse().unwrap();
    assert_ne!(local, port, "the occupied port falls back to an ephemeral listener");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let browser = std::thread::spawn(move || {
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, local)).unwrap();
        socket.set_read_timeout(Some(wait(Duration::from_secs(10)))).unwrap();
        write!(socket, "POST /demo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n", expected.len()).unwrap();
        socket.write_all(&expected).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        let mut got = Vec::new();
        socket.read_to_end(&mut got).unwrap();
        tx.send(got).unwrap();
    });
    protocol.ws.get_mut().set_read_timeout(Some(Duration::from_millis(2))).unwrap();
    let deadline = Instant::now() + wait(Duration::from_secs(15));
    let mut sent = 0;
    let mut received = 0;
    let mut acks = (0, 0);
    let mut closes = (false, false);
    let mut got = None;
    while Instant::now() < deadline {
        native.pump(&remote, None);
        for envelope in remote.take_outgoing() {
            match &envelope.message {
                Message::TunnelData { bytes, .. } => sent += atomics_core::preview::decode(bytes).unwrap().len(),
                Message::TunnelAck { bytes, .. } => acks.0 += *bytes as usize,
                Message::TunnelClose { abort: false, .. } => closes.0 = true,
                _ => {}
            }
            protocol.send(&envelope);
        }
        match protocol.ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let envelope = wire::decode(&text).unwrap();
                match &envelope.message {
                    Message::TunnelData { bytes, .. } => {
                        received += atomics_core::preview::decode(bytes).unwrap().len()
                    }
                    Message::TunnelAck { bytes, .. } => acks.1 += *bytes as usize,
                    Message::TunnelClose { abort: false, .. } => closes.1 = true,
                    Message::TunnelRefused { reason, .. } => panic!("{reason}"),
                    _ => {}
                }
                remote.receive(envelope);
            }
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            other => panic!("{other:?}"),
        }
        if got.is_none() {
            got = rx.try_recv().ok();
        }
        if got.is_some() && acks == (received, sent) && closes == (true, true) {
            break;
        }
    }
    assert!(sent > TUNNEL_WINDOW as usize && received > TUNNEL_WINDOW as usize);
    assert_eq!(acks, (received, sent));
    assert_eq!(closes, (true, true));
    assert_eq!(got.expect("browser completed"), service.join().unwrap());
    browser.join().unwrap();
    atomics_ui::Backend::send(&remote, atomics_ui::Command::StopPreview { port });
    native.pump(&remote, None);
    assert!(!native.active());
    // Stopping the listener signals its worker; wait for that thread to release the socket.
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_ok() {
        assert!(Instant::now() < deadline, "stopped preview listener is still accepting connections");
        std::thread::sleep(Duration::from_millis(5));
    }
    let url = native.open(PreviewSource::Session("web".into()), info.previews[0].clone()).unwrap();
    assert_eq!(url, info.previews[0].url(port), "reuse the original port when free");
    protocol.ws.get_mut().set_read_timeout(Some(wait(Duration::from_secs(5)))).unwrap();
    let mut ended = info.clone();
    ended.status = AgentStatus::Exited;
    f.projections.publish_sessions(vec![ended]);
    loop {
        let envelope = protocol.next().unwrap();
        let exited = matches!(&envelope.message, Message::SessionUpsert { session, .. } if session.status == AgentStatus::Exited);
        remote.receive(envelope);
        if exited {
            break;
        }
    }
    native.pump(&remote, None);
    assert!(!native.active(), "session exit closes the listener");
    f.projections.publish_sessions(vec![info.clone()]);
    loop {
        let envelope = protocol.next().unwrap();
        let live = matches!(&envelope.message, Message::SessionUpsert { session, .. } if session.status != AgentStatus::Exited);
        remote.receive(envelope);
        if live {
            break;
        }
    }
    native.open(PreviewSource::Session("web".into()), info.previews[0].clone()).unwrap();
    remote.disconnected();
    native.pump(&remote, None);
    assert!(!native.active());
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_err(), "disconnect closed listener");
}

/// Link loss drops streams, but a fresh controller connection can reclaim the same browser origin.
#[test]
fn preview_recovery_requires_the_same_sessions_current_declaration() {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = service.local_addr().unwrap().port();
    let mut web = session("web", "web", AgentStatus::Working);
    web.previews.push(Preview { port, ..Default::default() });
    // Each fresh TCP connection receives a recognizable response. Echoing the first byte
    // also detects any browser request replayed from a previous stream.
    // Keep the listener available for ownership verification after the final accept.
    let (_hold, release) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        for stream in service.incoming().take(8) {
            let mut socket = stream.unwrap();
            std::thread::spawn(move || {
                let mut byte = [0];
                if socket.read_exact(&mut byte).is_ok() {
                    let _ = socket.write_all(if byte[0] == 0x16 { b"HTTP/" } else { &byte });
                    if byte[0] == b'a' {
                        // The service leaves this stream open until the original link dies.
                        socket.set_read_timeout(Some(wait(Duration::from_secs(8)))).unwrap();
                        assert_eq!(socket.read(&mut byte).unwrap_or(0), 0);
                    }
                }
            });
        }
        let _ = release.recv();
    });
    let dir = tempfile::tempdir().unwrap();
    let mut native = crate::preview::client::Client::default();
    native.restore(dir.path().into());
    let (opened, tabs) = std::sync::mpsc::channel();
    let capture_browser = |client: &mut crate::preview::client::Client| {
        let opened = opened.clone();
        client.browser(move |url| {
            opened.send(url.to_owned()).unwrap();
        });
    };
    capture_browser(&mut native);
    let remote = atomics_web::remote::RemoteBackend::new("restore-test");
    remote.enable_native_previews();
    let f = start();
    f.projections.publish_sessions(vec![web.clone()]);
    let (link, events) = live_link(&f, &remote, &native);
    atomics_ui::Backend::send(
        &remote,
        atomics_ui::Command::OpenPreview {
            source: PreviewSource::Session(web.id.clone()),
            preview: web.previews[0].clone(),
        },
    );
    wait_for_listener(&mut native, &remote, &link);
    let url = tabs.recv_timeout(wait(Duration::from_secs(2))).unwrap();
    let local = url::Url::parse(&url).unwrap().port().unwrap();
    let fresh_request = |c| {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, local)).unwrap();
        stream.set_read_timeout(Some(wait(Duration::from_secs(3)))).unwrap();
        stream.write_all(&[c]).unwrap();
        let mut received = [0];
        stream.read_exact(&mut received).unwrap();
        assert_eq!(received, [c]);
        stream
    };
    let mut old_stream = fresh_request(b'a');
    drop((link, events));
    remote.disconnected();
    native.pump(&remote, None);
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_err());
    match old_stream.read(&mut [0]) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("old stream survived disconnect: {other:?}"),
    }
    let (link, events) = live_link(&f, &remote, &native);
    wait_for_listener(&mut native, &remote, &link);
    drop(fresh_request(b'b'));
    assert!(tabs.try_recv().is_err(), "restoring must not open another tab");
    drop((link, events, f));
    remote.disconnected();
    native.pump(&remote, None);
    drop(native);
    let mut native = crate::preview::client::Client::default();
    native.restore(dir.path().into());
    capture_browser(&mut native);
    let f = start();
    let mut other = web.clone();
    other.id = "different-session".into();
    f.projections.publish_sessions(vec![other]);
    let (link, events) = live_link(&f, &remote, &native);
    native.pump(&remote, Some((7, link.frames())));
    assert!(!native.active(), "a new epoch cannot reuse the old board or another owner");
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_err());
    f.projections.publish_sessions(vec![web]);
    while !remote.offers_preview(&PreviewSource::Session("web".into()), port) {
        let event = events.recv_timeout(wait(Duration::from_secs(3))).unwrap();
        if let crate::client::link::Event::Data { bytes, .. } = event {
            remote.receive(wire::decode(std::str::from_utf8(&bytes).unwrap()).unwrap());
        }
    }
    wait_for_listener(&mut native, &remote, &link);
    drop(fresh_request(b'c'));
    drop((link, events, native));
    remote.disconnected();
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, local)).unwrap();
    let file = dir.path().join(format!("{port}.json"));
    let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    saved["waiting_since"] = serde_json::json!(atomics_core::store::now() - 600);
    std::fs::write(&file, serde_json::to_vec(&saved).unwrap()).unwrap();
    let mut native = crate::preview::client::Client::default();
    native.restore(dir.path().into());
    capture_browser(&mut native);
    let (link, _events) = live_link(&f, &remote, &native);
    native.pump(&remote, Some((7, link.frames())));
    assert!(!native.active() && !native.waiting(), "an expired wait stays paused even when its port returns");
    native.retry(port);
    wait_for_listener(&mut native, &remote, &link);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join(format!("{port}.json"))).unwrap()).unwrap();
    let replacement = u16::try_from(saved["local"].as_u64().unwrap()).unwrap();
    assert_ne!(replacement, occupied.local_addr().unwrap().port());
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, replacement)).unwrap();
    stream.set_read_timeout(Some(wait(Duration::from_secs(3)))).unwrap();
    stream.write_all(b"d").unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"d");
    assert!(tabs.try_recv().is_err(), "neither window restoration opened another tab");
}

#[test]
fn a_listener_revoked_while_connecting_is_refused_before_the_tunnel_opens() {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = service.local_addr().unwrap();
    let listener = atomics_core::machine::Listener::from(&atomics_core::machine::Port {
        port: address.port(),
        address: address.ip().to_string(),
        pid: Some(std::process::id()),
        ..Default::default()
    });
    let watchers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = watchers.clone();
    let mut relay = crate::preview::relay::Relay::default();
    relay
        .open("pending".into(), address.port(), PreviewSource::Machine(listener.clone()), move || {
            Ok((TcpStream::connect(address)?, crate::preview::relay::Binding::new(listener, 1, observed)))
        })
        .unwrap();
    let _peer = service.accept().unwrap();
    let deadline = Instant::now() + wait(Duration::from_secs(3));
    loop {
        if let Some(reply) = relay.poll(|_, _, binding| binding.is_none()).first() {
            assert!(matches!(reply, Message::TunnelRefused { .. }), "{reply:?}");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(watchers.load(std::sync::atomic::Ordering::Acquire), 0);
}

#[test]
fn relay_credit_and_connection_count_are_bounded() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, _) = listener.accept().unwrap();
    let mut relay = crate::preview::relay::Relay::default();
    relay.accept("t".into(), 5173, PreviewSource::Session("web".into()), stream).unwrap();
    relay.receive(Message::TunnelOpened { id: "t".into() }).unwrap();
    writer.write_all(&vec![7; TUNNEL_WINDOW as usize * 2]).unwrap();
    let mut bytes = 0;
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while bytes < TUNNEL_WINDOW as usize && Instant::now() < deadline {
        for message in relay.poll(|_, _, _| true) {
            if let Message::TunnelData { bytes: data, .. } = message {
                bytes += atomics_core::preview::decode(&data).unwrap().len();
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(relay.poll(|_, _, _| true).is_empty(), "no more reads without credit");
    assert_eq!(bytes, TUNNEL_WINDOW as usize, "no more reads without credit");
    assert!(relay.receive(Message::TunnelAck { id: "t".into(), bytes: TUNNEL_WINDOW + 1 }).is_err());
    let chunk = atomics_core::preview::encode(&vec![0; TUNNEL_CHUNK]);
    for _ in 0..TUNNEL_WINDOW as usize / TUNNEL_CHUNK {
        relay.receive(Message::TunnelData { id: "t".into(), bytes: chunk.clone() }).unwrap();
    }
    assert!(relay.receive(Message::TunnelData { id: "t".into(), bytes: chunk }).is_err());
    for n in 1..MAX_TUNNELS {
        relay
            .open(
                format!("t{n}"),
                listener.local_addr().unwrap().port(),
                PreviewSource::Session("web".into()),
                move || Err(std::io::Error::other("connect pending")),
            )
            .unwrap();
    }
    assert!(
        relay
            .open("overflow".into(), 5173, PreviewSource::Session("web".into()), move || Err(std::io::Error::other(
                "connect pending"
            )))
            .is_err()
    );
    assert!(
        relay
            .open("t".into(), 5173, PreviewSource::Session("web".into()), move || Err(std::io::Error::other(
                "connect pending"
            )))
            .is_err()
    );
}

/// Connect the production WebSocket and its off-window preview relay to an isolated protocol host.
fn live_link(
    f: &Fixture,
    remote: &atomics_web::remote::RemoteBackend,
    native: &crate::preview::client::Client,
) -> (crate::client::link::Link, std::sync::mpsc::Receiver<crate::client::link::Event>) {
    use crate::client::link::{Address, Endpoint, Event, Link};
    let (events_tx, events) = std::sync::mpsc::sync_channel(512);
    let endpoint = Endpoint {
        addr: Address::Tcp(f.server.addr()),
        host: f.server.addr().to_string(),
        token: zeroize::Zeroizing::new(f.server.token().to_string()),
    };
    let link = Link::open(7, Box::new(move || Ok(endpoint)), events_tx, Arc::new(|| {}), native.tap());
    // The window's turns until the board is in, as the driver takes them.
    let deadline = Instant::now() + wait(Duration::from_secs(10));
    while !(remote.tunnels() && remote.with_mirror(|m| m.resume_point().0.is_some())) {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("the board") {
            Event::Open { .. } => link.send(wire::encode(&remote.hello("native")).unwrap()),
            Event::Data { bytes, .. } => remote.receive(wire::decode(std::str::from_utf8(&bytes).unwrap()).unwrap()),
            Event::Closed { code, unreachable, .. } => panic!("the link closed: {code} {unreachable:?}"),
        }
    }
    (link, events)
}

fn wait_for_listener(
    native: &mut crate::preview::client::Client,
    remote: &atomics_web::remote::RemoteBackend,
    link: &crate::client::link::Link,
) {
    let deadline = Instant::now() + wait(Duration::from_secs(10));
    while !native.active() {
        assert!(Instant::now() < deadline, "the authorized service did not restore");
        native.pump(remote, Some((7, link.frames())));
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The window may not draw at all while a page loads (another app in front, the window hidden): once
/// the preview is open, requests cross the tunnel on the relay's own thread and the host's, a round trip
/// taking milliseconds, with no turn of the window's.
#[test]
fn a_preview_answers_at_once_while_the_window_draws_nothing() {
    const REQUESTS: usize = 40;
    let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = http.local_addr().unwrap().port();
    // A keep-alive server: one connection, a short answer to each request on it.
    std::thread::spawn(move || {
        let (mut socket, _) = http.accept().unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match socket.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => request.extend_from_slice(&buf[..n]),
            }
            while let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                request.drain(..end + 4);
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
            }
        }
    });
    let f = start();
    let mut info = session("web", "web", AgentStatus::Working);
    info.previews.push(Preview { port, path: String::new(), label: "Web".into(), ..Default::default() });
    f.projections.publish_sessions(vec![info.clone()]);

    let remote = atomics_web::remote::RemoteBackend::new("preview-test");
    remote.enable_native_previews();
    let mut native = crate::preview::client::Client::default();
    let (link, events) = live_link(&f, &remote, &native);
    let url = native.open(PreviewSource::Session("web".into()), info.previews[0].clone()).unwrap();
    native.pump(&remote, Some((7, link.frames())));
    let local: u16 = url.strip_prefix("http://127.0.0.1:").unwrap().split('/').next().unwrap().parse().unwrap();

    // From here on the window takes no turn and reads none of the link's events.
    let browser = std::thread::spawn(move || {
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, local)).unwrap();
        socket.set_read_timeout(Some(wait(Duration::from_secs(10)))).unwrap();
        let mut times = Vec::new();
        for _ in 0..REQUESTS {
            let started = Instant::now();
            socket.write_all(b"GET /module.js HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut response = Vec::new();
            let mut buf = [0u8; 256];
            while !response.ends_with(b"ok") {
                let n = socket.read(&mut buf).expect("an answer without a window turn");
                assert!(n > 0, "the tunnel closed");
                response.extend_from_slice(&buf[..n]);
            }
            times.push(started.elapsed());
        }
        times
    });
    let mut times = browser.join().unwrap();
    times.sort();
    let median = times[times.len() / 2];
    // A window turn every 100 ms took about that long per request; the relay takes a millisecond or two.
    assert!(median < wait(Duration::from_millis(20)), "median round trip {median:?}: {times:?}");
    drop(events);
}

struct PythonServer {
    child: std::process::Child,
    port: u16,
}

impl PythonServer {
    fn plain_http() -> Self {
        Self::start(
            r#"
from http.server import HTTPServer, SimpleHTTPRequestHandler
server = HTTPServer(("127.0.0.1", 0), SimpleHTTPRequestHandler)
print(server.server_port, flush=True)
server.serve_forever()
"#,
        )
    }

    fn start(script: &str) -> Self {
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        let mut child =
            Command::new("python3").args(["-c", script]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let mut port = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut port).unwrap();
        Self { child, port: port.trim().parse().unwrap() }
    }
}

impl Drop for PythonServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn plain_http_server_opens_directly() {
    let http = PythonServer::plain_http();
    let preview = Preview { port: http.port, ..Default::default() };
    let url = crate::preview::client::direct_url(&PreviewSource::Session("web".into()), &preview);
    assert_eq!(url, Some(preview.url(http.port)));
}

#[test]
fn plain_http_server_opens_through_a_tunnel() {
    let http = PythonServer::plain_http();
    let port = http.port;
    let f = start();
    let mut web = session("web", "web", AgentStatus::Working);
    let preview = Preview { port, ..Default::default() };
    web.previews.push(preview.clone());
    f.projections.publish_sessions(vec![web]);
    let remote = atomics_web::remote::RemoteBackend::new("plain-http");
    remote.enable_native_previews();
    let mut native = crate::preview::client::Client::default();
    let (link, _events) = live_link(&f, &remote, &native);
    let (opened, tabs) = std::sync::mpsc::channel();
    native.browser(move |url| opened.send(url.to_owned()).unwrap());
    atomics_ui::Backend::send(
        &remote,
        atomics_ui::Command::OpenPreview { source: PreviewSource::Session("web".into()), preview },
    );
    wait_for_listener(&mut native, &remote, &link);
    let url = tabs.recv_timeout(wait(Duration::from_secs(2))).unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
}

#[test]
fn a_silent_scheme_probe_keeps_saved_https_and_retries() {
    saved_https_retries(false);
}

#[test]
fn an_eof_scheme_probe_keeps_saved_https_and_retries() {
    saved_https_retries(true);
}

fn saved_https_retries(eof: bool) {
    let service = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = service.local_addr().unwrap().port();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(format!("{port}.json"));
    std::fs::write(
        &file,
        serde_json::to_vec(&serde_json::json!({
            "session":"web", "preview":{"port":port,"path":"/page"}, "local":port, "tls":true, "waiting_since":null
        }))
        .unwrap(),
    )
    .unwrap();
    let mut native = crate::preview::client::Client::default();
    native.restore(dir.path().into());
    let f = start();
    let mut info = session("web", "web", AgentStatus::Working);
    info.previews.push(Preview { port, ..Default::default() });
    f.projections.publish_sessions(vec![info]);
    let remote = atomics_web::remote::RemoteBackend::new("slow-https");
    remote.enable_native_previews();
    let (link, _events) = live_link(&f, &remote, &native);
    native.pump(&remote, Some((7, link.frames())));
    for prefix in [&[0x16, 0x03, 0x01][..], b"HEAD "] {
        let (mut silent, _) = service.accept().unwrap();
        silent.set_read_timeout(Some(wait(Duration::from_secs(10)))).unwrap();
        let mut request = vec![0; prefix.len()];
        silent.read_exact(&mut request).unwrap();
        assert_eq!(request, prefix);
        if eof {
            silent.shutdown(std::net::Shutdown::Write).unwrap();
        }
        silent.read_to_end(&mut Vec::new()).unwrap();
    }
    native.pump(&remote, Some((7, link.frames())));
    assert!(!native.active(), "the timeout must leave the scheme pending");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(saved["tls"], true);
    std::thread::spawn(move || {
        if let Ok((mut socket, _)) = service.accept() {
            let mut record = [0; 5];
            socket.read_exact(&mut record).unwrap();
            assert_eq!(record[0], 0x16, "a fresh TLS probe after the inconclusive pair");
            socket.write_all(&[0x16, 3, 3, 0, 42]).unwrap();
        }
    });
    wait_for_listener(&mut native, &remote, &link);
}

#[test]
fn https_preview_preserves_the_certificate_and_redirect_from_a_self_signed_server() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("openssl.cnf");
    std::fs::write(&config, "[req]\ndistinguished_name=dn\nx509_extensions=ext\nprompt=no\n[dn]\nCN=localhost\n[ext]\nsubjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n").unwrap();
    let key = dir.path().join("leaf.key");
    let cert = dir.path().join("leaf.der");
    let generated = std::process::Command::new("openssl")
        .args(["req", "-new", "-x509", "-nodes", "-newkey", "rsa:2048", "-days", "1", "-outform", "DER", "-config"])
        .arg(config)
        .arg("-keyout")
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .output()
        .unwrap();
    assert!(generated.status.success(), "{}", String::from_utf8_lossy(&generated.stderr));
    let certificate = CertificateDer::from(std::fs::read(cert).unwrap());
    let tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.clone()], PrivateKeyDer::from_pem_file(key).unwrap())
        .unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let reply = b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/login\r\nContent-Length: 0\r\n\r\n";
    let server = std::thread::spawn(move || {
        let tls = Arc::new(tls);
        // The scheme probe sends its ClientHello and leaves. The next connection is the browser's.
        for probe in [true, false] {
            let (socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(wait(Duration::from_secs(8)))).unwrap();
            let mut stream = rustls::StreamOwned::new(rustls::ServerConnection::new(tls.clone()).unwrap(), socket);
            if probe {
                let _ = stream.read(&mut [0]);
            } else {
                let mut request = [0; 41];
                let expected = b"GET /demo HTTP/1.1\r\nHost: localhost\r\n\r\n";
                stream.read_exact(&mut request[..expected.len()]).unwrap();
                assert_eq!(&request[..expected.len()], expected);
                stream.write_all(reply).unwrap();
                stream.flush().unwrap();
                stream.conn.send_close_notify();
                stream.flush().unwrap();
            }
        }
    });
    let f = start();
    let mut web = session("web", "web", AgentStatus::Working);
    let preview = Preview { port, path: "/demo".into(), label: "HTTPS".into(), ..Default::default() };
    web.previews.push(preview.clone());
    f.projections.publish_sessions(vec![web]);
    let remote = atomics_web::remote::RemoteBackend::new("https-test");
    remote.enable_native_previews();
    let mut native = crate::preview::client::Client::default();
    let (link, _events) = live_link(&f, &remote, &native);
    let (opened, urls) = std::sync::mpsc::channel();
    native.browser(move |url| {
        opened.send(url.to_owned()).unwrap();
    });
    atomics_ui::Backend::send(
        &remote,
        atomics_ui::Command::OpenPreview { source: PreviewSource::Session("web".into()), preview },
    );
    wait_for_listener(&mut native, &remote, &link);
    let url = urls.recv_timeout(wait(Duration::from_secs(3))).unwrap();
    let local = url::Url::parse(&url).unwrap().port().unwrap();
    assert_eq!(url, format!("https://localhost:{local}/demo"));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.clone()).unwrap();
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let socket = TcpStream::connect((std::net::Ipv6Addr::LOCALHOST, local)).unwrap();
    socket.set_read_timeout(Some(wait(Duration::from_secs(8)))).unwrap();
    let mut browser = rustls::StreamOwned::new(
        rustls::ClientConnection::new(Arc::new(config), "localhost".try_into().unwrap()).unwrap(),
        socket,
    );
    browser.write_all(b"GET /demo HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut page = Vec::new();
    browser.read_to_end(&mut page).unwrap();
    assert_eq!(browser.conn.peer_certificates().unwrap(), &[certificate]);
    assert_eq!(&page, reply);
    server.join().unwrap();
}

#[test]
fn a_port_only_declaration_requires_one_unambiguous_listener() {
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = v4.local_addr().unwrap().port();
    let _v6 = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
    let f = start();
    let mut web = session("web", "web", AgentStatus::Working);
    web.previews.push(Preview { port, ..Default::default() });
    f.projections.publish_sessions(vec![web]);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.send(&Envelope::new(Message::TunnelOpen {
        source: PreviewSource::Session("web".into()),
        id: "ambiguous".into(),
        port,
    }));
    assert!(matches!(
        next_matching(&mut client, |m| matches!(m, Message::TunnelOpened { .. } | Message::TunnelRefused { .. })),
        Message::TunnelRefused { .. }
    ));
}

#[test]
fn an_ipv6_only_wildcard_and_an_ipv4_listener_each_reach_their_own_service() {
    let v6 = PythonServer::start(
        r#"
import socket
with socket.create_server(("::", 0), family=socket.AF_INET6, dualstack_ipv6=False) as server:
    print(server.getsockname()[1], flush=True)
    connection, _ = server.accept()
    with connection:
        connection.sendall(b"6")
        connection.recv(1)
"#,
    );
    let port = v6.port;
    let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
    let f = start();
    let ports = crate::machine::listener_ports(&[], port, false);
    assert_eq!(ports.len(), 2);
    f.projections.machine.publish(atomics_core::machine::Machine { ports: ports.clone(), ..Default::default() });
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    for row in &ports {
        let source = PreviewSource::Machine(row.into());
        let id = row.address.clone();
        client.send(&Envelope::new(Message::TunnelOpen { source, id: id.clone(), port }));
        assert!(
            matches!(
                next_matching(&mut client, |m| matches!(
                    m,
                    Message::TunnelOpened { .. } | Message::TunnelRefused { .. }
                )),
                Message::TunnelOpened { .. }
            ),
            "{id}"
        );
        if row.address == "127.0.0.1" {
            v4.accept().unwrap().0.write_all(b"4").unwrap();
        }
        let Message::TunnelData { bytes, .. } = next_matching(&mut client, |m| matches!(m, Message::TunnelData { .. }))
        else {
            unreachable!()
        };
        assert_eq!(atomics_core::preview::decode(&bytes).unwrap(), if row.address == "::" { b"6" } else { b"4" });
        client.send(&Envelope::new(Message::TunnelClose { id, abort: true }));
    }
}

#[test]
fn a_detected_ipv6_listener_is_dialed_even_when_ipv4_has_the_same_port() {
    let ipv4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = ipv4.local_addr().unwrap().port();
    let ipv6 = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).unwrap();
    let roots = vec![("web".into(), Some(std::process::id()))];
    let owned = crate::machine::listener_ports(&roots, port, false).into_iter().find(|p| p.address == "::1").unwrap();
    assert_eq!(owned.session.as_deref(), Some("web"));
    let f = start();
    f.projections.preview_roots.publish(roots);
    let mut web = session("web", "web", AgentStatus::Working);
    web.previews.push(Preview { port, listener: Some((&owned).into()), ..Default::default() });
    f.projections.publish_sessions(vec![web]);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.send(&Envelope::new(Message::TunnelOpen {
        source: PreviewSource::Session("web".into()),
        id: "ipv6".into(),
        port,
    }));
    assert!(matches!(
        next_matching(&mut client, |m| matches!(m, Message::TunnelOpened { .. } | Message::TunnelRefused { .. })),
        Message::TunnelOpened { .. }
    ));
    ipv6.set_nonblocking(true).unwrap();
    ipv6.accept().expect("the IPv6 service received the connection");
    ipv4.set_nonblocking(true).unwrap();
    assert_eq!(ipv4.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
