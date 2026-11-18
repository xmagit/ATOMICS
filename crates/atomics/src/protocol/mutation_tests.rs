//! Mutations at the protocol's edge, over real loopback sockets, with a fake behind it that
//! records what reaches it: the gates (capability and grants), the routing of outcomes back to
//! the request that sent them, repeats while an operation runs, and the bound on operations under
//! way. The journal and the runtime's side (one winner per approval, stale approvals, retries,
//! `uncertain` after a crash) are tested in `journal.rs` and `app::host_tests`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomics_core::approvals::Decision;
use atomics_core::ops::{
    ApprovalDecide, Command, Grant, Outcome, PlanAction, PlanCommand, ReviewAction, ReviewCommand, SessionAction,
    SessionCommand,
};
use atomics_core::projection::Projections;
use atomics_core::wire::{CAPABILITIES, Envelope, ErrorCode, MAX_PENDING_OPS, MUTATIONS, Message, Role, VERSION};

use super::tests::{TestClient, wait};
use super::{Commands, OpRequest, Server, ServerConfig, SessionConfig, Terminals};

struct NoDiffs;

impl super::DiffSource for NoDiffs {
    fn file(&self, _: &str, _: &str) -> Option<(u64, atomics_core::diff::FileDiff)> {
        None
    }
}

/// Keeps what it is handed; the test answers on each request's reply.
#[derive(Default)]
struct Recorder(Mutex<Vec<OpRequest>>);

impl Commands for Recorder {
    fn submit(&self, request: OpRequest) -> bool {
        self.0.lock().unwrap().push(request);
        true
    }
}

impl Recorder {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Wait for the `n`th request (from 0) to arrive and answer it.
    fn answer(&self, n: usize, outcome: Outcome) -> String {
        let deadline = std::time::Instant::now() + wait(Duration::from_secs(5));
        loop {
            if let Some(request) = self.0.lock().unwrap().get(n) {
                request.reply.send(request.command.op(), outcome);
                return request.client.clone();
            }
            assert!(std::time::Instant::now() < deadline, "request {n} never arrived");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn start(role: Role) -> (Server, Arc<Recorder>) {
    start_with(role, Arc::new(Projections::default()))
}

fn start_with(role: Role, projections: Arc<Projections>) -> (Server, Arc<Recorder>) {
    let config = ServerConfig {
        poll_interval: Duration::from_millis(5),
        session: SessionConfig { tick: Duration::from_millis(5), ..SessionConfig::default() },
        role,
        ..ServerConfig::default()
    };
    let recorder = Arc::new(Recorder::default());
    let server =
        Server::start(config, projections, Arc::new(NoDiffs), Arc::new(Terminals::default()), recorder.clone())
            .unwrap();
    (server, recorder)
}

/// Connect, say hello with `capabilities`, and return the client and its welcome's `may`.
fn connect(server: &Server, capabilities: &[&str]) -> (TestClient, Vec<Grant>) {
    let mut client = TestClient::connect(server.addr(), server.token());
    client.send(&Envelope::new(Message::Hello {
        client_id: "phone".into(),
        versions: vec![VERSION],
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        prior_epoch: None,
        prior_cursor: None,
    }));
    loop {
        if let Message::Welcome { may, .. } = client.step().expect("welcome").message {
            return (client, may);
        }
    }
}

fn every_command(op: &str) -> Vec<Command> {
    vec![
        Command::ApprovalDecide(ApprovalDecide {
            digest: None,
            op: format!("{op}-a"),
            approval: "x1".into(),
            decision: Decision::Allow,
            message: None,
        }),
        Command::PlanCommand(PlanCommand {
            op: format!("{op}-p"),
            plan: Some("p1".into()),
            digest: Some("abc".into()),
            action: PlanAction::Approve,
        }),
        Command::ReviewCommand(ReviewCommand {
            op: format!("{op}-r"),
            worktree: "/w/a".into(),
            action: ReviewAction::Send,
        }),
        Command::SessionCommand(SessionCommand {
            op: format!("{op}-s"),
            action: SessionAction::Steer { session: "a".into(), text: "go on".into(), now: false },
        }),
    ]
}

#[test]
fn completing_a_plan_is_capability_gated_and_reaches_the_journal() {
    let (server, recorder) = start(Role::Controller);
    let caps: Vec<_> = CAPABILITIES.iter().copied().filter(|cap| *cap != atomics_core::wire::PLAN_COMPLETE).collect();
    let (mut old, _) = connect(&server, &caps);
    let complete = |op: &str| {
        Message::PlanCommand(PlanCommand {
            op: op.into(),
            plan: Some("p1".into()),
            digest: None,
            action: PlanAction::Complete,
        })
    };
    old.send(&Envelope::new(complete("old")));
    assert!(matches!(result(&mut old).2, Outcome::Refused { code: ErrorCode::Unauthorised, .. }));
    assert_eq!(recorder.count(), 0);
    let (mut client, _) = connect(&server, CAPABILITIES);
    client.send(&Envelope::new(complete("new")));
    recorder.answer(0, Outcome::done(serde_json::Value::Null));
    assert!(matches!(result(&mut client).2, Outcome::Done { .. }));
    assert!(
        matches!(&recorder.0.lock().unwrap()[0].command, Command::PlanCommand(c) if c.action == PlanAction::Complete)
    );
}

/// The next `command_result`, with the request it answers.
fn result(client: &mut TestClient) -> (Option<String>, String, Outcome) {
    loop {
        let envelope = client.next().expect("connection open");
        if let Message::CommandResult(result) = envelope.message {
            return (envelope.request_id, result.op, result.outcome);
        }
    }
}

fn send(client: &mut TestClient, command: Command, request: &str) {
    client.send(&Envelope::new(command.into_message()).reply_to(Some(request.into())));
}

#[test]
fn a_view_only_client_is_told_it_may_change_nothing_and_every_mutation_is_refused() {
    let (server, recorder) = start(Role::Viewer);
    let (mut client, may) = connect(&server, CAPABILITIES);
    assert!(may.is_empty(), "the welcome says so up front: {may:?}");
    for (i, command) in every_command("v").into_iter().enumerate() {
        let op = command.op().to_string();
        send(&mut client, command, &format!("r{i}"));
        let (request, answered, outcome) = result(&mut client);
        assert_eq!((request.as_deref(), answered.as_str()), (Some(format!("r{i}").as_str()), op.as_str()));
        let Outcome::Refused { code, message } = outcome else { panic!("{op}: {outcome:?}") };
        assert_eq!(code, ErrorCode::Unauthorised, "{op}");
        assert!(message.contains("view-only"), "{message}");
    }
    assert_eq!(recorder.count(), 0, "nothing reached the host's state");
}

#[test]
fn mutations_need_the_capability_and_a_controller_is_told_what_it_may_do() {
    let (server, recorder) = start(Role::Controller);
    let (_, may) = connect(&server, CAPABILITIES);
    assert_eq!(may, vec![Grant::Type, Grant::Approve, Grant::Session, Grant::Plan, Grant::Review]);

    let without: Vec<&str> = CAPABILITIES.iter().copied().filter(|c| *c != MUTATIONS).collect();
    let (mut client, may) = connect(&server, &without);
    assert_eq!(may, vec![Grant::Type], "typing only: it did not negotiate mutations");
    for command in every_command("c") {
        send(&mut client, command, "r");
        let (_, _, outcome) = result(&mut client);
        assert!(
            matches!(&outcome, Outcome::Refused { code: ErrorCode::Unauthorised, message } if message.contains("capability")),
            "{outcome:?}"
        );
    }
    assert_eq!(recorder.count(), 0);
}

#[test]
fn outcomes_answer_the_request_that_sent_the_operation_and_a_repeat_is_not_sent_twice() {
    let (server, recorder) = start(Role::Controller);
    let (mut client, _) = connect(&server, CAPABILITIES);
    let steer = every_command("o").pop().unwrap();
    send(&mut client, steer.clone(), "first");
    assert_eq!(recorder.answer(0, Outcome::Accepted), "phone", "the journal names it by the client's ID");
    assert_eq!(result(&mut client), (Some("first".into()), "o-s".into(), Outcome::Accepted));
    // Sent again while it runs: answered at once, not handed over a second time.
    send(&mut client, steer, "again");
    assert_eq!(result(&mut client), (Some("again".into()), "o-s".into(), Outcome::Accepted));
    let done = Outcome::done(serde_json::json!({ "sent": true }));
    recorder.answer(0, done.clone());
    assert_eq!(result(&mut client), (Some("again".into()), "o-s".into(), done), "the latest request hears the end");
    assert_eq!(recorder.count(), 1);
}

#[test]
fn a_repeat_with_a_different_body_conflicts_without_replacing_the_waiter() {
    let (server, recorder) = start(Role::Controller);
    let (mut client, _) = connect(&server, CAPABILITIES);
    let original = every_command("o").pop().unwrap();
    send(&mut client, original, "first");
    recorder.answer(0, Outcome::Accepted);
    assert_eq!(result(&mut client).2, Outcome::Accepted);
    let changed = Command::SessionCommand(SessionCommand {
        op: "o-s".into(),
        action: SessionAction::Close { session: "a".into() },
    });
    send(&mut client, changed, "conflict");
    let (request, _, outcome) = result(&mut client);
    assert_eq!(request.as_deref(), Some("conflict"));
    assert!(matches!(outcome, Outcome::Refused { code: ErrorCode::Conflict, .. }));
    let done = Outcome::done(serde_json::Value::Null);
    recorder.answer(0, done.clone());
    assert_eq!(result(&mut client), (Some("first".into()), "o-s".into(), done));
    assert_eq!(recorder.count(), 1);
}

#[test]
fn operations_under_way_are_bounded() {
    let (server, recorder) = start(Role::Controller);
    let (mut client, _) = connect(&server, CAPABILITIES);
    for i in 0..MAX_PENDING_OPS {
        let command = Command::SessionCommand(SessionCommand {
            op: format!("op{i}"),
            action: SessionAction::Close { session: "a".into() },
        });
        client.send(&Envelope::new(command.into_message()));
    }
    let deadline = std::time::Instant::now() + wait(Duration::from_secs(5));
    while recorder.count() < MAX_PENDING_OPS {
        assert!(std::time::Instant::now() < deadline, "only {} arrived", recorder.count());
        std::thread::sleep(Duration::from_millis(2));
    }
    let extra = Command::SessionCommand(SessionCommand {
        op: "one too many".into(),
        action: SessionAction::Close { session: "a".into() },
    });
    send(&mut client, extra, "r");
    let (request, op, outcome) = result(&mut client);
    assert_eq!((request.as_deref(), op.as_str()), (Some("r"), "one too many"));
    assert!(matches!(outcome, Outcome::Refused { code: ErrorCode::TooMany, .. }), "{outcome:?}");
    // Once one ends there is room again.
    recorder.answer(0, Outcome::done(serde_json::Value::Null));
    assert_eq!(result(&mut client).1, "op0");
    let room = Command::SessionCommand(SessionCommand {
        op: "room".into(),
        action: SessionAction::Close { session: "a".into() },
    });
    send(&mut client, room, "r2");
    let deadline = std::time::Instant::now() + wait(Duration::from_secs(5));
    while recorder.count() < MAX_PENDING_OPS + 1 {
        assert!(std::time::Instant::now() < deadline, "the next one was not handed over");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn settings_pairing_and_projects_require_the_host_grant() {
    use atomics_core::ops::{HostAction, HostCommand, SettingsTool};
    let actions = [
        HostAction::BrowseFolder { path: "/home/user".into() },
        HostAction::OpenProject { path: "/home/user/repo".into() },
        HostAction::CloseProject { path: "/home/user/repo".into() },
        HostAction::SaveWorkspace { name: "team".into() },
        HostAction::ConfigPropose {
            scope: atomics_core::selfconfig::Scope::Main,
            toml: "[font]\nsize = 14.0".into(),
            remove: vec![],
            theme: None,
            reason: None,
        },
        HostAction::ConfigDecide { id: "config-1".into(), apply: true, note: String::new() },
        HostAction::PairDevice,
        HostAction::CancelPairing,
        HostAction::DecidePairing { code: "123456".into(), controls: Some(true) },
        HostAction::RevokeDevice { id: "device".into() },
        HostAction::SettingsTool {
            tool: SettingsTool::PutCredential { kind: "openai".into(), value: "hidden".into(), host: None },
        },
    ];
    for (role, capabilities) in [
        (Role::Viewer, CAPABILITIES.to_vec()),
        (Role::Controller, CAPABILITIES.to_vec()),
        (Role::Controller, CAPABILITIES.iter().copied().filter(|c| *c != atomics_core::wire::HOST).collect()),
    ] {
        let (server, recorder) = start(role);
        // Stay below the protocol's intentional per-connection refusal limit.
        for chunk in actions.chunks(4) {
            let (mut client, may) = connect(&server, &capabilities);
            assert!(!may.contains(&Grant::Host));
            for (index, action) in chunk.iter().enumerate() {
                send(
                    &mut client,
                    Command::HostCommand(HostCommand { op: format!("management-{index}"), action: action.clone() }),
                    "request",
                );
                assert!(matches!(result(&mut client).2, Outcome::Refused { code: ErrorCode::Unauthorised, .. }));
            }
        }
        assert_eq!(recorder.count(), 0);
    }
}

#[test]
fn pairing_links_are_private_in_snapshots_and_host_updates() {
    use atomics_core::projection::{HostInfo, ManagementInfo, PairingInfo, RemoteInfo};
    let projections = Arc::new(Projections::default());
    let host = |link: &str| HostInfo {
        management: Some(ManagementInfo {
            remote: RemoteInfo {
                pairing: Some(PairingInfo::Open { link: Some(link.into()), seconds_left: 300, busy: false }),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    projections.publish_host(host("https://example.com/pair#first-secret"));
    let (viewer_server, _) = start_with(Role::Viewer, projections.clone());
    let (control_server, _) = start_with(Role::Controller, projections.clone());
    let (mut viewer, _) = connect(&viewer_server, CAPABILITIES);
    let (mut control, _) = connect(&control_server, CAPABILITIES);
    viewer.sync_until(|m| m.host.management.is_some());
    control.sync_until(|m| m.host.management.is_some());
    let link = |client: &TestClient| match &client.mirror.host.management.as_ref().unwrap().remote.pairing {
        Some(PairingInfo::Open { link, .. }) => link.clone(),
        _ => panic!("pairing"),
    };
    assert_eq!(link(&viewer), None);
    assert_eq!(link(&control), None);
    projections.publish_host(host("https://example.com/pair#next-secret"));
    let previous = control.mirror.cursor;
    control.sync_until(|m| m.cursor > previous);
    assert_eq!(link(&control), None);
    viewer.sync_until(|m| m.cursor >= control.mirror.cursor);
    assert_eq!(link(&viewer), None, "host_upsert must not reveal the link either");
}

#[test]
fn panel_operations_require_the_panels_capability_and_a_controller() {
    use atomics_core::ops::{HostAction, HostCommand};
    for (role, panels) in [(Role::Controller, false), (Role::Viewer, true)] {
        let (server, recorder) = start(role);
        let caps: Vec<_> =
            CAPABILITIES.iter().copied().filter(|c| panels || *c != atomics_core::wire::PANELS).collect();
        let (mut client, _) = connect(&server, &caps);
        let command = Command::HostCommand(HostCommand {
            op: "editor".into(),
            action: HostAction::Edit { root: "/w/repo".into(), path: None, line: None },
        });
        send(&mut client, command, "editor-request");
        assert!(matches!(result(&mut client).2, Outcome::Refused { code: ErrorCode::Unauthorised, .. }));
        assert_eq!(recorder.count(), 0);
    }
}
