use super::*;
use atomics_core::machine::{Machine, Port};
use atomics_core::ops::Grant;
use atomics_core::wire::{Role, Topic};
use std::net::{Ipv4Addr, TcpListener};

fn next_matching(client: &mut TestClient, predicate: impl Fn(&Message) -> bool) -> Message {
    loop {
        let message = client.next().expect("protocol frame").message;
        if predicate(&message) {
            return message;
        }
    }
}

fn snapshot(port: u16) -> Machine {
    Machine {
        sampled_at: 1,
        cores: 4,
        ports: vec![Port { address: "127.0.0.1".into(), port, previewable: true, ..Default::default() }],
        ..Default::default()
    }
}

fn quiet(client: &mut TestClient) {
    client.send(&Envelope::new(Message::Ping { nonce: Some("barrier".into()) }));
    loop {
        let message = client.next().unwrap().message;
        assert!(!matches!(message, Message::Machine { .. }), "unsolicited machine snapshot");
        if matches!(message, Message::Pong { .. }) {
            break;
        }
    }
}

/// A controller with a distinct paired credential and an explicit desktop grant.
fn machine_host(role: Role) -> (Fixture, String, tempfile::TempDir) {
    machine_host_with(role, Arc::new(crate::protocol::NoCommands))
}

fn machine_host_with(role: Role, commands: Arc<dyn crate::protocol::Commands>) -> (Fixture, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let clients = Arc::new(crate::protocol::clients::PairedClients::open(dir.path().join("clients.json")).unwrap());
    let (_, paired) = clients.pair("Machine panel").unwrap();
    assert!(clients.grant_host(&clients.find(&paired).unwrap().id).unwrap());
    let fixture = start_with_commands(
        ServerConfig { role, paired: Some(clients), allowed_origins: vec!["https://app.example".into()], ..config() },
        commands,
    );
    let token = if role == Role::Viewer { fixture.server.token().to_owned() } else { paired };
    (fixture, token, dir)
}

#[test]
fn machine_is_only_offered_to_host_managing_controllers_including_browsers() {
    for (role, missing, browser, allowed) in [
        (Role::Viewer, None, false, false),
        (Role::Controller, Some(wire::HOST), false, false),
        (Role::Controller, Some(wire::MUTATIONS), false, false),
        (Role::Controller, Some(wire::MACHINE), false, false),
        (Role::Controller, None, false, true),
        (Role::Controller, None, true, true),
    ] {
        let (f, token, _dir) = machine_host(role);
        f.projections.machine.publish(snapshot(8080));
        let mut client =
            TestClient::try_connect(f.server.addr(), Some(&token), browser.then_some("https://app.example"), true)
                .unwrap();
        client.send(&Envelope::new(Message::Hello {
            client_id: "machine-test".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().filter(|c| Some(**c) != missing).map(|c| c.to_string()).collect(),
            prior_epoch: None,
            prior_cursor: None,
        }));
        let Message::Welcome { capabilities, may, .. } =
            next_matching(&mut client, |m| matches!(m, Message::Welcome { .. }))
        else {
            unreachable!()
        };
        assert_eq!(capabilities.iter().any(|c| c == wire::MACHINE), allowed);
        assert_eq!(
            may.contains(&Grant::Host),
            role == Role::Controller && missing != Some(wire::HOST) && missing != Some(wire::MUTATIONS)
        );
        if browser {
            assert!(!capabilities.iter().any(|c| c == wire::TUNNEL));
        }
        quiet(&mut client);
        client.send(&Envelope::new(Message::Subscribe { topics: vec![Topic::Machine] }));
        if allowed {
            assert!(
                matches!(next_matching(&mut client, |m| matches!(m, Message::Machine { .. })), Message::Machine { snapshot } if snapshot.cores == 4)
            );
            assert_eq!(f.projections.machine_watchers.load(Ordering::Acquire), 1);
        } else {
            assert!(matches!(
                next_matching(&mut client, |m| matches!(m, Message::Error { .. })),
                Message::Error { code: ErrorCode::Unauthorised, .. }
            ));
            assert_eq!(f.projections.machine_watchers.load(Ordering::Acquire), 0);
        }
    }
}

#[test]
fn machine_snapshots_only_flow_while_subscribed_and_disconnect_releases_interest() {
    let (f, token, _dir) = machine_host(Role::Controller);
    let mut data = snapshot(8080);
    f.projections.machine.publish(data.clone());
    let mut client = TestClient::connect(f.server.addr(), &token);
    client.hello();
    quiet(&mut client);
    data.sampled_at = 2;
    f.projections.machine.publish(data.clone());
    quiet(&mut client);
    client.send(&Envelope::new(Message::Subscribe { topics: vec![Topic::Machine, Topic::Machine] }));
    assert_eq!(
        next_matching(&mut client, |m| matches!(m, Message::Machine { .. })),
        Message::Machine { snapshot: data.clone() }
    );
    client.send(&Envelope::new(Message::Subscribe { topics: vec![Topic::Machine] }));
    quiet(&mut client);
    assert_eq!(f.projections.machine_watchers.load(Ordering::Acquire), 1);
    data.sampled_at = 3;
    f.projections.machine.publish(data.clone());
    assert_eq!(
        next_matching(&mut client, |m| matches!(m, Message::Machine { .. })),
        Message::Machine { snapshot: data.clone() }
    );
    client.send(&Envelope::new(Message::Unsubscribe { topics: vec![Topic::Machine, Topic::Machine] }));
    quiet(&mut client);
    assert_eq!(f.projections.machine_watchers.load(Ordering::Acquire), 0);
    data.sampled_at = 4;
    f.projections.machine.publish(data.clone());
    quiet(&mut client);
    client.send(&Envelope::new(Message::Subscribe { topics: vec![Topic::Machine] }));
    assert_eq!(
        next_matching(&mut client, |m| matches!(m, Message::Machine { .. })),
        Message::Machine { snapshot: data }
    );
    drop(client);
    let started = Instant::now();
    while f.projections.machine_watchers.load(Ordering::Acquire) != 0 {
        assert!(started.elapsed() < wait(Duration::from_secs(3)));
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn machine_only_tunnel_refuses_without_a_desktop_decision() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (f, token, _dir) = machine_host(Role::Controller);
    f.projections.machine.publish(snapshot(port));
    let mut client = TestClient::connect(f.server.addr(), &token);
    client.hello();
    client.send(&Envelope::new(Message::TunnelOpen { id: "machine".into(), port }));
    let result =
        next_matching(&mut client, |m| matches!(m, Message::TunnelOpened { .. } | Message::TunnelRefused { .. }));
    assert!(matches!(result, Message::TunnelRefused { .. }), "{result:?}");
    listener.set_nonblocking(true).unwrap();
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn machine_tunnels_require_both_capabilities_and_a_current_loopback_listener() {
    let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = http.local_addr().unwrap().port();
    let (commands, requests) = crate::host::Wake::default().channel::<crate::protocol::OpRequest>();
    let (f, token, _dir) = machine_host_with(Role::Controller, Arc::new(commands));
    let mut data = snapshot(port);
    data.ports.push(Port { address: "192.0.2.1".into(), port: 5173, previewable: true, ..Default::default() });
    f.projections.machine.publish(data);
    for missing in [Some(wire::MACHINE), Some(wire::TUNNEL), None] {
        let mut client = TestClient::connect(f.server.addr(), &token);
        client.send(&Envelope::new(Message::Hello {
            client_id: "machine-test".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().filter(|c| Some(**c) != missing).map(|c| c.to_string()).collect(),
            prior_epoch: None,
            prior_cursor: None,
        }));
        next_matching(&mut client, |m| matches!(m, Message::Welcome { .. }));
        client.send(&Envelope::new(Message::TunnelOpen { id: "listed".into(), port }));
        if missing.is_none() {
            let request = requests.recv_timeout(wait(Duration::from_secs(3))).unwrap();
            assert!(matches!(&request.command, atomics_core::ops::Command::HostCommand(host)
                if host.action == atomics_core::ops::HostAction::OpenMachinePort { port }));
            request.reply.send(request.command.op(), atomics_core::ops::Outcome::done(serde_json::Value::Null));
        }
        let message = next_matching(&mut client, |m| {
            matches!(m, Message::TunnelOpened { .. } | Message::TunnelRefused { .. } | Message::Error { .. })
        });
        match missing {
            Some(wire::MACHINE) => assert!(matches!(message, Message::TunnelRefused { .. })),
            Some(wire::TUNNEL) => assert!(matches!(message, Message::Error { code: ErrorCode::Unauthorised, .. })),
            None => {
                assert!(matches!(message, Message::TunnelOpened { .. }));
                let (_socket, _) = http.accept().unwrap();
                for (id, port) in [("unlisted", 5174), ("non-loopback", 5173)] {
                    client.send(&Envelope::new(Message::TunnelOpen { id: id.into(), port }));
                    assert!(
                        matches!(next_matching(&mut client, |m| matches!(m, Message::TunnelRefused { .. })), Message::TunnelRefused { id: refused, .. } if refused == id)
                    );
                }
                f.projections.machine.publish(Machine::default());
                assert!(matches!(
                    next_matching(&mut client, |m| matches!(m, Message::TunnelClose { .. })),
                    Message::TunnelClose { abort: true, .. }
                ));
                client.send(&Envelope::new(Message::TunnelOpen { id: "removed".into(), port }));
                assert!(
                    matches!(next_matching(&mut client, |m| matches!(m, Message::TunnelRefused { .. })), Message::TunnelRefused { id, .. } if id == "removed")
                );
            }
            _ => unreachable!(),
        }
    }
    let error = crate::preview::relay::connect_loopback("192.0.2.1:5173".parse().unwrap()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn native_dashboard_forwards_survive_without_an_agent_and_stop_when_the_listener_disappears() {
    use atomics_ui::Backend;
    let http = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = http.local_addr().unwrap().port();
    let (f, token, _dir) = machine_host(Role::Controller);
    f.projections.machine.publish(snapshot(port));
    let remote = atomics_web::remote::RemoteBackend::new("machine-test");
    remote.enable_native_previews();
    let mut protocol = TestClient::connect(f.server.addr(), &token);
    protocol.send(&remote.hello("native"));
    while !remote.live() {
        remote.receive(protocol.next().unwrap());
    }
    remote.watch_machine(true);
    for envelope in remote.take_outgoing() {
        protocol.send(&envelope);
    }
    while remote.machine().is_none() {
        remote.receive(protocol.next().unwrap());
    }
    let mut native = crate::preview::client::Client::default();
    native.open(None, atomics_core::preview::Preview { port, ..Default::default() }).unwrap();
    native.pump(&remote, None);
    assert!(native.active(), "a machine forward needs no owning agent");
    f.projections.machine.publish(Machine::default());
    while remote.machine().is_some_and(|m| m.previewable(port)) {
        remote.receive(protocol.next().unwrap());
    }
    native.pump(&remote, None);
    assert!(!native.active());
}
