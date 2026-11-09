use super::*;
use atomics_core::preview::Preview;
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
        client.send(&Envelope::new(Message::TunnelOpen { id: "t".into(), port: 5173 }));
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
    client.send(&Envelope::new(Message::TunnelOpen { id: "t".into(), port: 5173 }));
    assert!(
        matches!(next_matching(&mut client, |m| matches!(m, Message::TunnelRefused { .. })), Message::TunnelRefused { id, .. } if id == "t")
    );
    let error = crate::preview::relay::connect_loopback("192.0.2.1:5173".parse().unwrap()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut info = session("a", "web", AgentStatus::Working);
    info.previews.push(Preview { port, path: String::new(), label: String::new() });
    f.projections.publish_sessions(vec![info]);
    client.send(&Envelope::new(Message::TunnelOpen { id: "allowed".into(), port }));
    next_matching(&mut client, |m| matches!(m, Message::TunnelOpened { .. }));
    let (_socket, _) = listener.accept().unwrap();
    f.projections.publish_sessions(Vec::new());
    assert!(matches!(
        next_matching(&mut client, |m| matches!(m, Message::TunnelClose { .. })),
        Message::TunnelClose { abort: true, .. }
    ));
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
    info.previews.push(Preview { port, path: "demo".into(), label: "Web".into() });
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
    let url = native.open(Some("web".into()), info.previews[0].clone()).unwrap();
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
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_err());
    let url = native.open(None, info.previews[0].clone()).unwrap();
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
    native.open(Some("web".into()), info.previews[0].clone()).unwrap();
    remote.disconnected();
    native.pump(&remote, None);
    assert!(!native.active());
    assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, local)).is_err(), "disconnect closed listener");
}

#[test]
fn relay_credit_and_connection_count_are_bounded() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (stream, _) = listener.accept().unwrap();
    let mut relay = crate::preview::relay::Relay::default();
    relay.accept("t".into(), 5173, stream).unwrap();
    relay.receive(Message::TunnelOpened { id: "t".into() }).unwrap();
    writer.write_all(&vec![7; TUNNEL_WINDOW as usize * 2]).unwrap();
    let mut bytes = 0;
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while bytes < TUNNEL_WINDOW as usize && Instant::now() < deadline {
        for message in relay.poll(|_| true) {
            if let Message::TunnelData { bytes: data, .. } = message {
                bytes += atomics_core::preview::decode(&data).unwrap().len();
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(relay.poll(|_| true).is_empty(), "no more reads without credit");
    assert_eq!(bytes, TUNNEL_WINDOW as usize, "no more reads without credit");
    assert!(relay.receive(Message::TunnelAck { id: "t".into(), bytes: TUNNEL_WINDOW + 1 }).is_err());
    let chunk = atomics_core::preview::encode(&vec![0; TUNNEL_CHUNK]);
    for _ in 0..TUNNEL_WINDOW as usize / TUNNEL_CHUNK {
        relay.receive(Message::TunnelData { id: "t".into(), bytes: chunk.clone() }).unwrap();
    }
    assert!(relay.receive(Message::TunnelData { id: "t".into(), bytes: chunk }).is_err());
    for n in 1..MAX_TUNNELS {
        relay.open(format!("t{n}"), listener.local_addr().unwrap().port()).unwrap();
    }
    assert!(relay.open("overflow".into(), 5173).is_err());
    assert!(relay.open("t".into(), 5173).is_err());
}

/// The window may not draw at all while a page loads (another app in front, the window hidden): once
/// the preview is open, requests cross the tunnel on the relay's own thread and the host's, a round trip
/// taking milliseconds, with no turn of the window's.
#[test]
fn a_preview_answers_at_once_while_the_window_draws_nothing() {
    use crate::client::link::{Address, Endpoint, Event, Link};
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
    info.previews.push(Preview { port, path: String::new(), label: "Web".into() });
    f.projections.publish_sessions(vec![info.clone()]);

    let remote = atomics_web::remote::RemoteBackend::new("preview-test");
    remote.enable_native_previews();
    let mut native = crate::preview::client::Client::default();
    let (events_tx, events) = std::sync::mpsc::sync_channel(512);
    let endpoint = Endpoint {
        addr: Address::Tcp(f.server.addr()),
        host: f.server.addr().to_string(),
        token: zeroize::Zeroizing::new(f.server.token().to_string()),
    };
    let link = Link::open(7, Box::new(move || Ok(endpoint)), events_tx, Arc::new(|| {}), native.tap());
    // The window's turns until the board is in, as the driver takes them.
    let deadline = Instant::now() + wait(Duration::from_secs(10));
    while !(remote.tunnels() && remote.with_mirror(|m| m.sessions.contains_key("web"))) {
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("the board") {
            Event::Open { .. } => link.send(wire::encode(&remote.hello("native")).unwrap()),
            Event::Data { bytes, .. } => remote.receive(wire::decode(std::str::from_utf8(&bytes).unwrap()).unwrap()),
            Event::Closed { code, unreachable, .. } => panic!("the link closed: {code} {unreachable:?}"),
        }
    }
    let url = native.open(Some("web".into()), info.previews[0].clone()).unwrap();
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
