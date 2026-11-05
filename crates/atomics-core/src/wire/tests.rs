use super::*;
use crate::diff::{FileDiff, FileStatus};
use crate::plan::{PlanStatus, TaskStatus};
use crate::projection::{DiffFileSummary, Projections, TaskInfo, TaskLink};
use crate::session::AgentStatus;

fn session() -> SessionInfo {
    SessionInfo {
        previews: Vec::new(),
        id: "s1".into(),
        agent: "claude".into(),
        title: "fix".into(),
        cwd: "/w/a".into(),
        worktree: Some("/w/a".into()),
        branch: Some("fix/a".into()),
        status: AgentStatus::NeedsInput,
        chief: false,
        task: Some(TaskLink { plan: "p".into(), task: "t".into() }),
        pending_approvals: 1,
    }
}

fn host() -> crate::projection::HostInfo {
    use crate::projection::{HostInfo, SignInInfo, SignInState};
    HostInfo {
        projects: Vec::new(),
        resources: Some(crate::projection::ResourceInfo {
            claude: crate::usage::Usage {
                observed_at: 100,
                session: Some(crate::usage::Window { remaining: 76.5, resets_at: 200 }),
                ..Default::default()
            },
            load: Some(4.2),
            cores: 8,
            ..Default::default()
        }),
        workspace: Some("team".into()),
        tickets: None,
        workspaces: vec!["daily".into(), "team".into()],
        sign_in: Some(SignInInfo {
            system: "tracker".into(),
            server: "tickets.example.com".into(),
            state: SignInState::SigningIn,
            reason: Some("the server refused the saved sign-in".into()),
            url: Some("https://auth.example.com/authorize?state=s".into()),
            rejected: None,
            browser_failed: true,
        }),
        ..HostInfo::default()
    }
}

fn plan() -> PlanInfo {
    PlanInfo {
        ticket_tree: None,
        id: "p".into(),
        status: PlanStatus::Open,
        goal: "ship".into(),
        project: Some("/w".into()),
        approved: true,
        integration: None,
        tasks: vec![TaskInfo {
            id: "t".into(),
            title: "do".into(),
            status: TaskStatus::Review,
            agent: "claude".into(),
            depends_on: vec![],
            session: Some("s1".into()),
            branch: None,
            integrated: None,
            additions: 3,
            deletions: 1,
            report: Some(report()),
            ticket: Some("ENG-7".into()),
        }],
        digest: "cedf306324d4".into(),
        ticket: Some("ENG-1".into()),
    }
}

fn report() -> ReportInfo {
    ReportInfo { status: "done".into(), summary: "ok".into(), at: 9 }
}

fn approval() -> ApprovalInfo {
    ApprovalInfo {
        id: "a1".into(),
        session: "s1".into(),
        tool: "Bash".into(),
        command: Some("ls".into()),
        cwd: "/w/a".into(),
        reason: None,
        warning: Some("stash@{0} (On atomics/t2: t2), which belongs to task t2".into()),
    }
}

#[test]
fn an_approval_without_a_warning_reads_as_before() {
    let mut info = approval();
    info.warning = None;
    let json = serde_json::to_value(&info).unwrap();
    assert!(json.get("warning").is_none(), "{json}");
    assert_eq!(serde_json::from_value::<ApprovalInfo>(json).unwrap(), info);
}

fn summary() -> DiffSummary {
    DiffSummary {
        worktree: "/w/a".into(),
        revision: 4,
        base_ref: "main".into(),
        base: "abc".into(),
        partial: false,
        files: vec![DiffFileSummary {
            old_path: None,
            new_path: Some("x.rs".into()),
            status: FileStatus::Added,
            binary: false,
            additions: 2,
            deletions: 0,
            omitted: 0,
        }],
        comments: vec![crate::steer::ReviewComment {
            path: "x.rs".into(),
            line: Some(1),
            removed: false,
            excerpt: "fn a() {}".into(),
            body: "name it".into(),
        }],
    }
}

fn file_body() -> DiffFileBody {
    DiffFileBody {
        worktree: "/w/a".into(),
        revision: 4,
        file: FileDiff {
            old_path: None,
            new_path: Some("x.rs".into()),
            status: FileStatus::Added,
            binary: false,
            hunks: vec![],
            omitted: 0,
        },
    }
}

fn screen_snapshot() -> crate::screen::ScreenSnapshot {
    use crate::screen::{Cell, Screen};
    let mut screen = Screen::blank(6, 2);
    screen.lines[0].cells[0] = Cell { c: '界', combining: None, width: 2, style: Default::default() };
    screen.lines[0].cells[1].width = 0;
    screen.lines[1].cells[0].combining = Some(vec!['\u{301}'].into_boxed_slice());
    screen.title = Some("vim".into());
    screen.snapshot("s1", 2, 3, Some).unwrap().remove(0)
}

fn screen_delta() -> crate::screen::ScreenDelta {
    use crate::screen::Screen;
    let old = Screen::blank(6, 2);
    let mut new = old.clone();
    new.lines[1].cells[2].c = 'x';
    new.first_line = 1;
    new.history = 1;
    new.modes.bracketed_paste = true;
    old.delta(&new, "s1", 2, 3, 4).unwrap()
}

fn every_message() -> Vec<Message> {
    let topics = vec![
        Topic::Machine,
        Topic::Diff { worktree: "/w/a".into() },
        Topic::Plan { id: "p".into() },
        Topic::Screen { session: "s1".into(), focused: true },
    ];
    vec![
        Message::Machine { snapshot: crate::machine::Machine { sampled_at: 1, cores: 4, ..Default::default() } },
        Message::TunnelOpen { id: "t1".into(), port: 5173 },
        Message::TunnelOpened { id: "t1".into() },
        Message::TunnelRefused { id: "t1".into(), reason: "not previewable".into() },
        Message::TunnelData { id: "t1".into(), bytes: crate::preview::encode(&[0, 128, 255]) },
        Message::TunnelAck { id: "t1".into(), bytes: 3 },
        Message::TunnelClose { id: "t1".into(), abort: false },
        Message::Hello {
            client_id: "c".into(),
            versions: vec![VERSION],
            capabilities: vec!["board".into()],
            prior_epoch: Some("e".into()),
            prior_cursor: Some(Seq(u64::MAX)),
        },
        Message::Welcome {
            version: VERSION,
            capabilities: vec!["board".into()],
            limits: Limits::default(),
            role: Role::Controller,
            resume: Resume::Reset { cursor: Seq(7), reason: ResetReason::NewClient },
            may: vec![crate::ops::Grant::Type, crate::ops::Grant::Approve],
        },
        Message::Welcome {
            version: VERSION,
            capabilities: vec![],
            limits: Limits::default(),
            role: Role::Viewer,
            resume: Resume::Replay { after: Seq(5) },
            may: vec![],
        },
        Message::Subscribe { topics: topics.clone() },
        Message::FolderGet { worktree: "/w/a".into(), path: "src".into(), revision: Seq(1), offset: 128 },
        Message::FolderPage(crate::folder::Page {
            worktree: "/w/a".into(),
            path: "src".into(),
            revision: Seq(1),
            offset: 0,
            entries: vec![crate::folder::Entry { path: "src/a.rs".into(), directory: false }],
            next_offset: None,
        }),
        Message::Unsubscribe { topics },
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(2), approvals: Seq(3), diffs: Seq(4), host: Seq(5) },
            sessions: vec![session()],
            plans: vec![plan()],
            approvals: vec![approval()],
            diffs: vec![summary()],
            host: host(),
            part: Some(Part { index: 1, count: 3 }),
        },
        Message::HostUpsert { cursor: Seq(13), host: host() },
        Message::HostCommand(crate::ops::HostCommand { op: "o9".into(), action: crate::ops::HostAction::SignIn }),
        Message::HostCommand(crate::ops::HostCommand {
            op: "o10".into(),
            action: crate::ops::HostAction::SignInPaste { text: "http://127.0.0.1:1/callback?code=c&state=s".into() },
        }),
        Message::HostCommand(crate::ops::HostCommand {
            op: "o11".into(),
            action: crate::ops::HostAction::OpenWorkspace { name: "team".into() },
        }),
        Message::Skipped { cursor: Seq(12) },
        Message::SessionUpsert { cursor: Seq(4), session: session() },
        Message::SessionRemoved { cursor: Seq(5), id: "s1".into() },
        Message::PlanUpsert { cursor: Seq(6), plan: plan() },
        Message::PlanRemoved { cursor: Seq(7), id: "p".into() },
        Message::TaskReport { cursor: Seq(8), plan: "p".into(), task: "t".into(), report: report() },
        Message::ApprovalPending { cursor: Seq(9), approval: approval() },
        Message::ApprovalResolved { cursor: Seq(10), id: "a1".into(), disposition: Disposition::AllowedAlways },
        Message::DiffSummary { cursor: Seq(11), summary: summary() },
        Message::DiffGet { worktree: "/w/a".into(), path: "x.rs".into(), revision: Some(Seq(4)), offset: Some(120) },
        Message::DiffFile { body: file_body(), truncated: true, next_offset: Some(240) },
        Message::FileGet { worktree: "/w/a".into(), path: "x.rs".into() },
        Message::FileContent {
            worktree: "/w/a".into(),
            path: "x.rs".into(),
            content: "fn a() {}\n".into(),
            truncated: false,
        },
        Message::ScreenSnapshot(screen_snapshot()),
        Message::ScreenDelta(screen_delta()),
        Message::HistoryGet(crate::screen::HistoryGet {
            session: "s1".into(),
            generation: Seq(2),
            before: Seq(40),
            count: 100,
        }),
        Message::HistoryChunk(crate::screen::HistoryChunk {
            session: "s1".into(),
            generation: Seq(2),
            cols: 4,
            first: Seq(30),
            oldest: Seq(30),
            evicted: true,
            styles: vec![crate::screen::Style::default()],
            lines: vec![crate::screen::WireLine {
                row: 0,
                runs: vec![crate::screen::Run { s: 0, t: "ok".into(), w: None }],
                wrapped: true,
            }],
        }),
        Message::LeaseAcquire { session: "s1".into(), takeover: true },
        Message::LeaseRenew { session: "s1".into(), lease: Seq(3) },
        Message::LeaseRelease { session: "s1".into(), lease: Seq(3) },
        Message::LeaseState(crate::lease::LeaseState {
            session: "s1".into(),
            holder: Some("phone".into()),
            yours: true,
            lease: Some(Seq(3)),
            next: Some(Seq(1)),
            ttl_ms: Some(30_000),
            resized: true,
            reason: Some(crate::lease::LeaseReason::TakenOver),
        }),
        Message::TerminalInput { session: "s1".into(), lease: Seq(3), seq: Seq(1), data: "ls\r\u{1b}[A".into() },
        Message::TerminalResize {
            session: "s1".into(),
            lease: Seq(3),
            size: Some(crate::lease::GridSize { cols: 40, rows: 20, cell_width: Some(9), cell_height: None }),
        },
        Message::TerminalResize { session: "s1".into(), lease: Seq(3), size: None },
        Message::TerminalFit {
            session: "s1".into(),
            size: crate::lease::GridSize { cols: 120, rows: 40, cell_width: Some(8), cell_height: Some(17) },
        },
        Message::ApprovalDecide(crate::ops::ApprovalDecide {
            op: "o1".into(),
            approval: "a1".into(),
            decision: crate::approvals::Decision::Deny,
            message: Some("not on main".into()),
        }),
        Message::PlanCommand(crate::ops::PlanCommand {
            op: "o2".into(),
            plan: Some("p".into()),
            digest: Some("cedf306324d4".into()),
            action: crate::ops::PlanAction::Update {
                task: "t1".into(),
                status: Some(crate::plan::TaskStatus::Done),
                note: Some("checked".into()),
            },
        }),
        Message::PlanCommand(crate::ops::PlanCommand {
            op: "o3".into(),
            plan: None,
            digest: None,
            action: crate::ops::PlanAction::Set {
                goal: "g".into(),
                tasks: vec![crate::plan::Task { id: "t1".into(), title: "do".into(), ..Default::default() }],
                integration: Some("atomics/plan-g".into()),
            },
        }),
        Message::ReviewCommand(crate::ops::ReviewCommand {
            op: "o4".into(),
            worktree: "/w/a".into(),
            action: crate::ops::ReviewAction::Add {
                revision: Seq(4),
                path: "x.rs".into(),
                line: Some(3),
                removed: true,
                text: "why?".into(),
            },
        }),
        Message::ReviewCommand(crate::ops::ReviewCommand {
            op: "o5".into(),
            worktree: "/w/a".into(),
            action: crate::ops::ReviewAction::Send,
        }),
        Message::SessionCommand(crate::ops::SessionCommand {
            op: "o6".into(),
            action: crate::ops::SessionAction::Start {
                agent: "claude".into(),
                folder: None,
                repo: Some("/w".into()),
                branch: Some("fix/a".into()),
                prompt: Some("go".into()),
            },
        }),
        Message::CommandResult(crate::ops::CommandResult { op: "o1".into(), outcome: crate::ops::Outcome::Accepted }),
        Message::CommandResult(crate::ops::CommandResult {
            op: "o6".into(),
            outcome: crate::ops::Outcome::done(serde_json::json!({"sent": true})),
        }),
        Message::CommandResult(crate::ops::CommandResult {
            op: "o7".into(),
            outcome: crate::ops::Outcome::Uncertain { message: "the host stopped".into() },
        }),
        Message::TraceChanged { revision: Seq(1) },
        Message::TraceGet { revision: Seq(1), offset: 0, limit: 32 },
        Message::TracePage(crate::trace_feed::TracePage {
            revision: Seq(1),
            offset: 0,
            data: Default::default(),
            next_offset: None,
        }),
        Message::Ack { cursor: Seq(11) },
        Message::Resync { reason: Some("gap".into()), screens: vec![] },
        Message::Resync { reason: None, screens: vec!["s1".into()] },
        Message::Ping { nonce: Some("n".into()) },
        Message::Pong { nonce: None },
        Message::Error { code: ErrorCode::StaleRevision, message: "stale".into() },
    ]
}

#[test]
fn every_message_round_trips_through_its_frame() {
    let messages = every_message();
    let kinds: std::collections::BTreeSet<_> =
        messages.iter().map(|m| serde_json::to_value(m).unwrap()["kind"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds.len(), KINDS.len(), "a kind has no round-trip case");
    assert!(KINDS.iter().all(|k| kinds.contains(*k)));
    for message in messages {
        let envelope = Envelope { v: VERSION, epoch: Some("e1".into()), request_id: Some("r1".into()), message };
        let frame = encode(&envelope).unwrap();
        assert_eq!(decode(&frame).unwrap(), envelope, "{frame}");
    }
}

#[test]
fn cursors_travel_as_strings_and_numbers_are_refused() {
    let frame = encode(&Envelope::new(Message::Ack { cursor: Seq(u64::MAX) })).unwrap();
    assert!(frame.contains(r#""cursor":"18446744073709551615""#), "{frame}");
    for bad in [
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":5}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":""}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"-1"}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"1.5"}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"18446744073709551616"}"#,
    ] {
        assert_eq!(decode(bad).unwrap_err().code, ErrorCode::Malformed, "{bad}");
    }
}

#[test]
fn malformed_frames_are_rejected_with_a_typed_error() {
    let v = r#""v":{"major":1,"minor":0}"#;
    let cases = [
        "".to_string(),
        "not json".into(),
        "[]".into(),
        "null".into(),
        "42".into(),
        "{".into(),
        format!("{{{v}}}"),
        format!("{{{v},\"kind\":7}}"),
        r#"{"kind":"ping"}"#.into(),
        r#"{"v":"1.0","kind":"ping"}"#.into(),
        r#"{"v":{"major":-1,"minor":0},"kind":"ping"}"#.into(),
        // Right kind, wrong or missing fields.
        format!("{{{v},\"kind\":\"ack\"}}"),
        format!("{{{v},\"kind\":\"session_removed\",\"cursor\":\"1\",\"id\":5}}"),
        format!("{{{v},\"kind\":\"subscribe\",\"topics\":[{{\"topic\":\"nope\"}}]}}"),
        format!("{{{v},\"kind\":\"hello\",\"client_id\":\"c\",\"versions\":[]}}"),
        // An unknown code is read as `unknown` (a newer peer); a code that is not a string is not.
        format!("{{{v},\"kind\":\"error\",\"code\":7,\"message\":\"x\"}}"),
    ];
    for case in cases {
        let error = decode(&case).unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed, "{case}: {error}");
    }
}

#[test]
fn deeply_nested_input_is_an_error_not_a_stack_overflow() {
    let frame = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::Malformed);
}

#[test]
fn truncating_any_valid_frame_never_panics_and_never_decodes() {
    for message in every_message() {
        let frame = encode(&Envelope::new(message)).unwrap();
        for end in (0..frame.len()).filter(|i| frame.is_char_boundary(*i)) {
            assert!(decode(&frame[..end]).is_err(), "{}", &frame[..end]);
        }
    }
}

#[test]
fn an_unsupported_major_is_rejected_before_the_body_is_read() {
    // The body here would also be malformed; the version is what is reported.
    let frame = r#"{"v":{"major":2,"minor":0},"kind":"ack","cursor":"not a cursor"}"#;
    assert_eq!(decode(frame).unwrap_err().code, ErrorCode::UnsupportedVersion);
    let frame = r#"{"v":{"major":0,"minor":9},"kind":"ping"}"#;
    assert_eq!(decode(frame).unwrap_err().code, ErrorCode::UnsupportedVersion);
}

#[test]
fn a_newer_minor_decodes_and_unknown_fields_are_ignored() {
    let frame = r#"{"v":{"major":1,"minor":7,"extra":1},"kind":"ping","nonce":"n","future":{"a":[1]}}"#;
    let envelope = decode(frame).unwrap();
    assert_eq!(envelope.message, Message::Ping { nonce: Some("n".into()) });
    assert_eq!(envelope.v, Version { major: 1, minor: 7 });
}

#[test]
fn host_state_needs_its_capability_and_an_older_snapshot_has_none() {
    let caps = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let may = Role::Controller.may(&caps(&["board", MUTATIONS]));
    assert!(!may.contains(&Grant::Host), "host_command needs the host capability too: {may:?}");
    assert!(Role::Controller.may(&caps(&[MUTATIONS, HOST])).contains(&Grant::Host));
    assert!(Role::Controller.may(&caps(&[HOST])).is_empty(), "nothing changes without mutations");
    assert!(Role::Viewer.may(&caps(&[MUTATIONS, HOST])).is_empty());
    // A snapshot from a host before `host` decodes with an empty host state.
    let frame = r#"{"v":{"major":1,"minor":0},"epoch":"e","kind":"state_snapshot","cursor":"1","revisions":{"sessions":"1","plans":"0","approvals":"0","diffs":"0"},"sessions":[],"plans":[],"approvals":[],"diffs":[]}"#;
    let Message::StateSnapshot { host, revisions, .. } = decode(frame).unwrap().message else { panic!("snapshot") };
    assert_eq!((host, revisions.host), (Default::default(), Seq(0)));
    // A sign-in state from a newer host is read as unknown, never refused.
    let frame = r#"{"v":{"major":1,"minor":2},"epoch":"e","kind":"host_upsert","cursor":"2","host":{"sign_in":{"system":"tracker","server":"tickets.example.com","state":"locked"}}}"#;
    let Message::HostUpsert { host, .. } = decode(frame).unwrap().message else { panic!("host_upsert") };
    assert_eq!(host.sign_in.map(|s| s.state), Some(crate::projection::SignInState::Unknown));
    // A host command keeps its bounds: a workspace needs a name, a paste its size.
    let command = |action| Message::HostCommand(crate::ops::HostCommand { op: "o".into(), action });
    let blank = command(crate::ops::HostAction::OpenWorkspace { name: " ".into() });
    assert_eq!(blank.validate().unwrap_err().code, ErrorCode::Malformed);
    let long = command(crate::ops::HostAction::SignInPaste { text: "x".repeat(crate::ops::MAX_TEXT_BYTES + 1) });
    assert_eq!(long.validate().unwrap_err().code, ErrorCode::Oversize);
}

#[test]
fn an_error_code_from_a_newer_peer_decodes_as_unknown() {
    let frame = r#"{"v":{"major":1,"minor":3},"kind":"error","code":"quota_exceeded","message":"later"}"#;
    let envelope = decode(frame).unwrap();
    assert_eq!(envelope.message, Message::Error { code: ErrorCode::Unknown, message: "later".into() });
}

#[test]
fn the_hosts_refusal_is_an_error_frame_of_this_major() {
    let frame = encode(&not_paired()).unwrap();
    assert!(frame.starts_with(r#"{"v":{"major":1,"minor":0},"kind":"error","code":"not_paired""#), "{frame}");
    let envelope = decode(&frame).unwrap();
    assert!(is_not_paired(&envelope));
    assert_eq!((envelope.epoch, envelope.request_id), (None, None), "it answers no request and needs no hello");
    // Another error, or the code a client before this one reads it as, is no refusal.
    assert!(!is_not_paired(&Envelope::new(Message::Error { code: ErrorCode::Unknown, message: "x".into() })));
    assert!(!is_not_paired(&Envelope::new(Message::Error { code: ErrorCode::Unauthorised, message: "x".into() })));
}

#[test]
fn an_unknown_kind_fails_explicitly() {
    let frame = r#"{"v":{"major":1,"minor":0},"kind":"file_put","path":"a.rs"}"#;
    let error = decode(frame).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnknownKind);
    assert!(error.message.contains("file_put"));
    // A known mutation missing what it needs is malformed, never ignored.
    let frame = r#"{"v":{"major":1,"minor":0},"kind":"approval_decide","approval":"a1"}"#;
    assert_eq!(decode(frame).unwrap_err().code, ErrorCode::Malformed);
}

#[test]
fn hello_with_another_major_decodes_so_it_can_be_negotiated() {
    let frame = r#"{"v":{"major":2,"minor":0},"kind":"hello","client_id":"c","versions":[{"major":2,"minor":0},{"major":1,"minor":3}]}"#;
    let Message::Hello { versions, .. } = decode(frame).unwrap().message else { panic!("hello") };
    assert_eq!(negotiate(&versions, &[VERSION]).unwrap(), VERSION, "minor is clamped to the host's");
    let only_future = [Version { major: 2, minor: 0 }];
    let error = negotiate(&only_future, &[VERSION]).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert!(error.message.contains("1.0"));
    // The refusal is itself a frame the client can read.
    let reply = encode(&error.into_envelope(Some("r".into()), Some("e".into()))).unwrap();
    assert_eq!(decode(&reply).unwrap().request_id.as_deref(), Some("r"));
}

#[test]
fn negotiation_takes_the_highest_shared_major_and_the_lower_minor() {
    let host = [Version { major: 1, minor: 4 }, Version { major: 2, minor: 1 }];
    let pick = |c: &[Version]| negotiate(c, &host).unwrap();
    assert_eq!(pick(&[Version { major: 1, minor: 9 }]), Version { major: 1, minor: 4 });
    assert_eq!(pick(&[Version { major: 1, minor: 2 }]), Version { major: 1, minor: 2 });
    assert_eq!(pick(&[Version { major: 1, minor: 2 }, Version { major: 2, minor: 0 }]), Version { major: 2, minor: 0 });
    assert_eq!(common_capabilities(&["x".into(), "diffs".into(), "board".into()], CAPABILITIES), ["board", "diffs"]);
}

#[test]
fn oversize_frames_are_rejected_before_parsing_and_never_encoded() {
    let big = "x".repeat(MAX_FRAME_BYTES);
    let frame = format!(r#"{{"v":{{"major":1,"minor":0}},"kind":"ping","nonce":"{big}"}}"#);
    assert!(frame.len() > MAX_FRAME_BYTES);
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::Oversize);
    // Oversize wins even over invalid JSON: nothing is parsed.
    assert_eq!(decode(&"{".repeat(MAX_FRAME_BYTES + 1)).unwrap_err().code, ErrorCode::Oversize);
    let message = Message::FileContent { worktree: "w".into(), path: "p".into(), content: big, truncated: false };
    assert_eq!(encode(&Envelope::new(message)).unwrap_err().code, ErrorCode::Oversize);
    // A frame exactly at the limit is allowed.
    let head = r#"{"v":{"major":1,"minor":0},"kind":"ping","nonce":""#;
    let exact = format!("{head}{}\"}}", "y".repeat(MAX_FRAME_BYTES - head.len() - 2));
    assert_eq!(exact.len(), MAX_FRAME_BYTES);
    assert!(decode(&exact).is_ok());
}

#[test]
fn collection_and_identifier_bounds_are_enforced() {
    let many = vec![Topic::Plan { id: "p".into() }; MAX_SUBSCRIPTIONS + 1];
    let frame = serde_json::to_string(&Envelope::new(Message::Subscribe { topics: many })).unwrap();
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::TooMany);
    let ok = vec![Topic::Plan { id: "p".into() }; MAX_SUBSCRIPTIONS];
    assert!(decode(&serde_json::to_string(&Envelope::new(Message::Subscribe { topics: ok })).unwrap()).is_ok());
    let long =
        Message::DiffGet { worktree: "w".into(), path: "p".repeat(MAX_ID_BYTES + 1), revision: None, offset: None };
    assert_eq!(decode(&serde_json::to_string(&Envelope::new(long)).unwrap()).unwrap_err().code, ErrorCode::Oversize);
    let mut envelope = Envelope::new(Message::Ping { nonce: None });
    envelope.request_id = Some("r".repeat(MAX_ID_BYTES + 1));
    assert_eq!(decode(&serde_json::to_string(&envelope).unwrap()).unwrap_err().code, ErrorCode::Oversize);
    let hello = Message::Hello {
        client_id: "c".into(),
        versions: vec![VERSION; MAX_VERSIONS + 1],
        capabilities: vec![],
        prior_epoch: None,
        prior_cursor: None,
    };
    assert_eq!(decode(&serde_json::to_string(&Envelope::new(hello)).unwrap()).unwrap_err().code, ErrorCode::TooMany);
}

#[test]
fn resume_replays_inside_the_ring_and_resets_otherwise() {
    let (e, head, oldest) = ("e2", Seq(100), Seq(60));
    let decide = |epoch: Option<&str>, cursor: Option<u64>| decide_resume(e, head, oldest, epoch, cursor.map(Seq));
    assert_eq!(decide(Some("e2"), Some(100)), Resume::Replay { after: Seq(100) }, "caught up");
    assert_eq!(decide(Some("e2"), Some(60)), Resume::Replay { after: Seq(60) });
    assert_eq!(decide(Some("e2"), Some(59)), Resume::Replay { after: Seq(59) }, "event 60 is the next one needed");
    let reset = |reason| Resume::Reset { cursor: head, reason };
    assert_eq!(decide(Some("e2"), Some(58)), reset(ResetReason::CursorTooOld));
    assert_eq!(decide(Some("e2"), Some(101)), reset(ResetReason::CursorAhead));
    assert_eq!(decide(Some("e1"), Some(90)), reset(ResetReason::EpochChanged), "host restarted");
    assert_eq!(decide(None, None), reset(ResetReason::NewClient));
    assert_eq!(decide(Some("e2"), None), reset(ResetReason::NewClient));
    // An empty ring (oldest = head + 1) replays only for a client that is caught up.
    assert_eq!(decide_resume(e, Seq(5), Seq(6), Some(e), Some(Seq(5))), Resume::Replay { after: Seq(5) });
    assert!(matches!(decide_resume(e, Seq(5), Seq(6), Some(e), Some(Seq(3))), Resume::Reset { .. }));
}

#[test]
fn a_state_snapshot_carries_the_cursor_and_revisions_it_was_loaded_at() {
    let p = Projections::default();
    p.publish_sessions(vec![session()]);
    p.publish_plans(vec![plan()]);
    p.publish_approvals(vec![approval()]);
    p.publish_diffs(vec![summary()]);
    p.publish_sessions(vec![]);
    let envelope = Envelope::snapshot("boot", &p.load());
    let decoded = decode(&encode(&envelope).unwrap()).unwrap();
    assert_eq!(decoded.epoch.as_deref(), Some("boot"));
    let Message::StateSnapshot { cursor, revisions, sessions, plans, approvals, diffs, host: none, part } =
        decoded.message
    else {
        panic!("snapshot")
    };
    assert_eq!(part, None, "a board that fits is one frame");
    assert_eq!(cursor, Seq(5));
    assert_eq!(
        revisions,
        Revisions { sessions: Seq(2), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) }
    );
    assert_eq!((sessions.len(), plans.len(), approvals.len(), diffs.len()), (0, 1, 1, 1));
    assert_eq!(none, Default::default());
    assert!(!encode(&envelope).unwrap().contains("\"host\":{"), "an empty host is left out");

    // The host's own state moves the cursor and its revision, and travels with the snapshot.
    assert!(p.publish_host(host()));
    assert!(!p.publish_host(host()), "the same host state is not a change");
    let Message::StateSnapshot { cursor, revisions, host: carried, .. } =
        decode(&encode(&Envelope::snapshot("boot", &p.load())).unwrap()).unwrap().message
    else {
        panic!("snapshot")
    };
    assert_eq!((cursor, revisions.host, carried), (Seq(6), Seq(1), host()));
}

#[test]
fn errors_are_frames_that_echo_the_request_id() {
    let error = decode("{").unwrap_err();
    let frame = encode(&error.into_envelope(Some("r9".into()), None)).unwrap();
    let envelope = decode(&frame).unwrap();
    assert_eq!(envelope.request_id.as_deref(), Some("r9"));
    assert!(matches!(envelope.message, Message::Error { code: ErrorCode::Malformed, .. }));
}

/// A stand-in for the fuzz target (`fuzz/fuzz_targets/envelope_decode.rs`) that runs with the suite:
/// mutate valid frames at random and require that decoding never panics and that whatever it
/// accepts survives a round trip.
#[test]
fn mutated_frames_never_panic_and_accepted_ones_round_trip() {
    let p = Projections::default();
    p.publish_sessions(vec![session()]);
    p.publish_plans(vec![plan()]);
    let seeds: Vec<String> = [
        Envelope::snapshot("boot", &p.load()),
        Envelope::new(Message::Hello {
            client_id: "c".into(),
            versions: vec![VERSION],
            capabilities: vec!["board".into()],
            prior_epoch: Some("boot".into()),
            prior_cursor: Some(Seq(3)),
        }),
        Envelope::new(Message::DiffGet {
            worktree: "/w".into(),
            path: "a.rs".into(),
            revision: Some(Seq(2)),
            offset: None,
        }),
        Envelope::new(Message::Subscribe {
            topics: vec![Topic::Diff { worktree: "/w".into() }, Topic::Plan { id: "p".into() }],
        }),
        Envelope::new(Message::Ping { nonce: Some("n".into()) }).reply_to(Some("r".into())),
    ]
    .iter()
    .map(|e| encode(e).unwrap())
    .collect();

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let mut accepted = 0;
    for round in 0..4000 {
        let mut bytes = seeds[round % seeds.len()].clone().into_bytes();
        for _ in 0..=next(4) {
            match next(5) {
                0 if !bytes.is_empty() => {
                    let i = next(bytes.len());
                    bytes[i] ^= 1 << next(8);
                }
                1 if !bytes.is_empty() => bytes.truncate(next(bytes.len())),
                2 if !bytes.is_empty() => {
                    let (a, b) = (next(bytes.len()), next(bytes.len()));
                    let chunk = bytes[a.min(b)..a.max(b)].to_vec();
                    bytes.splice(a.min(b)..a.min(b), chunk);
                }
                3 => bytes.insert(next(bytes.len() + 1), b"{}[]\":,\\0123456789"[next(18)]),
                _ => {}
            }
        }
        let frame = String::from_utf8_lossy(&bytes);
        if let Ok(envelope) = decode(&frame) {
            accepted += 1;
            if let Ok(again) = encode(&envelope) {
                assert_eq!(decode(&again).as_ref(), Ok(&envelope), "{frame}");
            }
        }
    }
    assert!(accepted > 0, "some mutations stay valid");
}

#[test]
fn screen_frames_keep_their_bounds() {
    use crate::screen::{HistoryGet, MAX_HISTORY_LINES, MAX_ROWS, Part, Rgb};
    let refused = |message: Message| decode(&encode(&Envelope::new(message)).unwrap()).unwrap_err().code;
    let ok = |message: Message| decode(&encode(&Envelope::new(message)).unwrap()).map(|_| ());

    let snapshot = screen_snapshot();
    assert_eq!(ok(Message::ScreenSnapshot(snapshot.clone())), Ok(()));
    for broken in [
        crate::screen::ScreenSnapshot { rows: MAX_ROWS + 1, ..snapshot.clone() },
        crate::screen::ScreenSnapshot { cols: 0, ..snapshot.clone() },
        crate::screen::ScreenSnapshot { palette: vec![Rgb::default(); 3], ..snapshot.clone() },
        crate::screen::ScreenSnapshot { part: Some(Part { index: 2, count: 2 }), ..snapshot.clone() },
        crate::screen::ScreenSnapshot { lines: vec![Default::default(); 3], ..snapshot.clone() },
    ] {
        assert_eq!(refused(Message::ScreenSnapshot(broken)), ErrorCode::Malformed);
    }
    let long = "s".repeat(MAX_ID_BYTES + 1);
    assert_eq!(
        refused(Message::ScreenSnapshot(crate::screen::ScreenSnapshot { session: long.clone(), ..snapshot })),
        ErrorCode::Oversize
    );

    let delta = screen_delta();
    assert_eq!(ok(Message::ScreenDelta(delta.clone())), Ok(()));
    let backwards = crate::screen::ScreenDelta { revision: delta.base, ..delta.clone() };
    assert_eq!(refused(Message::ScreenDelta(backwards)), ErrorCode::Malformed);
    let palette = crate::screen::ScreenDelta { palette: Some(vec![Rgb::default()]), ..delta };
    assert_eq!(refused(Message::ScreenDelta(palette)), ErrorCode::Malformed);

    let get = HistoryGet { session: "s".into(), generation: Seq(1), before: Seq(9), count: MAX_HISTORY_LINES };
    assert_eq!(ok(Message::HistoryGet(get.clone())), Ok(()));
    assert_eq!(refused(Message::HistoryGet(HistoryGet { count: MAX_HISTORY_LINES + 1, ..get })), ErrorCode::TooMany);
    let topics = vec![Topic::Screen { session: long.clone(), focused: false }];
    assert_eq!(refused(Message::Subscribe { topics }), ErrorCode::Oversize);
    let resync = Message::Resync { reason: None, screens: vec![long] };
    assert_eq!(refused(resync), ErrorCode::Oversize);
    // A colour is "#rrggbb" and nothing else.
    for bad in [r#""ff0000""#, r##""#ff00""##, r##""#gg0000""##, "16711680"] {
        assert!(serde_json::from_str::<Rgb>(bad).is_err(), "{bad}");
    }
}

#[test]
fn input_frames_keep_their_bounds() {
    use crate::lease::{GridSize, MAX_INPUT_BYTES};
    let refused = |message: Message| decode(&encode(&Envelope::new(message)).unwrap()).unwrap_err().code;
    let ok = |message: Message| decode(&encode(&Envelope::new(message)).unwrap()).map(|_| ());
    let input = |data: String| Message::TerminalInput { session: "s".into(), lease: Seq(1), seq: Seq(1), data };
    assert_eq!(ok(input("a".repeat(MAX_INPUT_BYTES))), Ok(()));
    assert_eq!(refused(input("a".repeat(MAX_INPUT_BYTES + 1))), ErrorCode::Oversize);
    assert_eq!(refused(input(String::new())), ErrorCode::Malformed);
    let resize = |size| Message::TerminalResize { session: "s".into(), lease: Seq(1), size: Some(size) };
    assert_eq!(ok(resize(GridSize::new(80, 24))), Ok(()));
    assert_eq!(refused(resize(GridSize::new(0, 24))), ErrorCode::Malformed);
    let fit = |size| Message::TerminalFit { session: "s".into(), size };
    assert_eq!(ok(fit(GridSize::new(120, 40))), Ok(()));
    assert_eq!(refused(fit(GridSize::new(80, 0))), ErrorCode::Malformed);
    let long = "s".repeat(MAX_ID_BYTES + 1);
    assert_eq!(refused(Message::LeaseAcquire { session: long.clone(), takeover: false }), ErrorCode::Oversize);
    assert_eq!(refused(Message::LeaseRelease { session: long, lease: Seq(1) }), ErrorCode::Oversize);
    // Tokens and sequence numbers are decimal strings, like cursors.
    for frame in [
        r#"{"v":{"major":1,"minor":0},"kind":"terminal_input","session":"s","lease":1,"seq":"1","data":"a"}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"terminal_input","session":"s","lease":"1","seq":"-1","data":"a"}"#,
    ] {
        assert_eq!(decode(frame).unwrap_err().code, ErrorCode::Malformed, "{frame}");
    }
    // Acquiring without `takeover` is the default: opening a screen never steals control.
    let frame = r#"{"v":{"major":1,"minor":0},"kind":"lease_acquire","session":"s"}"#;
    assert_eq!(decode(frame).unwrap().message, Message::LeaseAcquire { session: "s".into(), takeover: false });
}

/// A file of added lines numbered from 1 across `hunks` hunks of the given lengths.
fn numbered_file(hunks: &[usize]) -> FileDiff {
    use crate::diff::{DiffLine, LineKind};
    let mut next = 1;
    FileDiff {
        old_path: None,
        new_path: Some("n.rs".into()),
        status: FileStatus::Added,
        binary: false,
        hunks: hunks
            .iter()
            .enumerate()
            .map(|(h, count)| Hunk {
                header: format!("@@ hunk {h} @@"),
                old_start: 0,
                new_start: next,
                lines: (0..*count)
                    .map(|_| {
                        next += 1;
                        DiffLine {
                            kind: LineKind::Added,
                            old_no: None,
                            new_no: Some(next - 1),
                            text: format!("line {}", next - 1),
                            emphasis: vec![],
                        }
                    })
                    .collect(),
            })
            .collect(),
        omitted: 0,
    }
}

#[test]
fn diff_pages_cover_the_file_exactly_and_a_continuation_keeps_its_header() {
    let file = numbered_file(&[7, 0, 9]);
    assert_eq!(diff_page(&file, 0, usize::MAX).unwrap(), (file.clone(), None), "a roomy budget is one page");

    let (mut offset, mut pages, mut seen, mut empty_hunks) = (0, 0, Vec::new(), 0);
    loop {
        let (page, next) = diff_page(&file, offset, 420).unwrap();
        pages += 1;
        let lines: usize = page.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(json_len(&page) <= 420 || lines == 1, "page {pages} is {} bytes", json_len(&page));
        for hunk in &page.hunks {
            let Some(first) = hunk.lines.first() else {
                empty_hunks += 1;
                continue;
            };
            assert_eq!(Some(hunk.new_start), first.new_no, "a hunk starts at its first line");
            assert!(file.hunks.iter().any(|h| h.header == hunk.header), "a continuation keeps its header");
            seen.extend(hunk.lines.iter().cloned());
        }
        match next {
            Some(n) => {
                assert!(n > offset, "every page makes progress");
                assert_eq!(n, seen.len(), "the next page starts where this one stopped");
                offset = n;
            }
            None => break,
        }
    }
    assert!(pages > 2, "{pages} pages");
    let all: Vec<_> = file.hunks.iter().flat_map(|h| h.lines.iter().cloned()).collect();
    assert_eq!(seen, all, "the pages add up to the file, in order");
    assert_eq!(empty_hunks, 1, "an empty hunk is on exactly one page");

    // A continuation starts at its first line's numbers and keeps the original header.
    let (page, _) = diff_page(&file, 3, usize::MAX).unwrap();
    assert_eq!(
        (page.hunks[0].header.as_str(), page.hunks[0].new_start, page.hunks[0].lines.len()),
        ("@@ hunk 0 @@", 4, 4)
    );
    assert_eq!(diff_page(&file, 16, usize::MAX).unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(diff_page(&numbered_file(&[]), 0, 10).unwrap().1, None, "an empty file is one empty page");
}

#[test]
fn a_board_that_fits_is_one_snapshot_frame() {
    let loaded = Projections::default().load();
    let frames = snapshot_frames("e", &loaded, 4096).unwrap();
    assert_eq!(frames.len(), 1);
    let Message::StateSnapshot { part, .. } = decode(&frames[0]).unwrap().message else { panic!("a snapshot") };
    assert_eq!(part, None);
}

#[test]
fn a_board_over_the_budget_is_cut_into_numbered_parts_of_one_cursor() {
    let p = Projections::default();
    p.publish_sessions(
        (0..40).map(|i| SessionInfo { id: format!("s{i}"), title: "t".repeat(60), ..session() }).collect(),
    );
    p.publish_plans(vec![plan()]);
    p.publish_approvals(vec![approval()]);
    p.publish_diffs(vec![summary()]);
    let loaded = p.load();
    let frames = snapshot_frames("e", &loaded, 2000).unwrap();
    assert!(frames.len() > 2, "{} parts", frames.len());
    let mut ids = Vec::new();
    let mut rest = (0, 0, 0);
    for (i, frame) in frames.iter().enumerate() {
        assert!(frame.len() <= 2000, "part {i} is {} bytes", frame.len());
        let Message::StateSnapshot { cursor, revisions, sessions, plans, approvals, diffs, part, .. } =
            decode(frame).unwrap().message
        else {
            panic!("a snapshot part")
        };
        assert_eq!(part, Some(Part { index: i as u16, count: frames.len() as u16 }));
        assert_eq!(cursor, Seq(loaded.cursor));
        assert_eq!(revisions.sessions, Seq(loaded.sessions.revision));
        ids.extend(sessions.into_iter().map(|s| s.id));
        rest = (rest.0 + plans.len(), rest.1 + approvals.len(), rest.2 + diffs.len());
    }
    let all: Vec<_> = loaded.sessions.data.iter().map(|s| s.id.clone()).collect();
    assert_eq!(ids, all, "the parts hold every session once, in order");
    assert_eq!(rest, (1, 1, 1));

    // One item that cannot fit any part is refused, not cut.
    p.publish_sessions(vec![SessionInfo { title: "y".repeat(5000), ..session() }]);
    assert_eq!(snapshot_frames("e", &p.load(), 2000).unwrap_err().code, ErrorCode::Oversize);
}

#[test]
fn a_snapshot_part_out_of_range_is_malformed() {
    for part in [Part { index: 0, count: 0 }, Part { index: 2, count: 2 }] {
        let mut envelope = Envelope::snapshot("e", &Projections::default().load());
        let Message::StateSnapshot { part: slot, .. } = &mut envelope.message else { unreachable!() };
        *slot = Some(part);
        let frame = serde_json::to_string(&envelope).unwrap();
        assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::Malformed, "{part:?}");
    }
}

#[test]
fn tunnel_frames_keep_their_bounds() {
    let valid = Message::TunnelData { id: "t".into(), bytes: crate::preview::encode(&vec![255; TUNNEL_CHUNK]) };
    assert!(decode(&encode(&Envelope::new(valid)).unwrap()).is_ok());
    for invalid in [
        Message::TunnelOpen { id: "t".into(), port: 0 },
        Message::TunnelData { id: "t".into(), bytes: "!".into() },
        Message::TunnelData { id: "t".into(), bytes: crate::preview::encode(&vec![0; TUNNEL_CHUNK + 1]) },
        Message::TunnelData { id: "t".into(), bytes: String::new() },
        Message::TunnelAck { id: "t".into(), bytes: 0 },
        Message::TunnelAck { id: "t".into(), bytes: TUNNEL_WINDOW + 1 },
        Message::TunnelClose { id: "x".repeat(MAX_ID_BYTES + 1), abort: true },
    ] {
        assert!(invalid.validate().is_err(), "{invalid:?}");
        assert!(decode(&serde_json::to_string(&Envelope::new(invalid)).unwrap()).is_err());
    }
}

#[test]
fn machine_snapshot_rows_fit_a_small_frame_budget_without_losing_resource_readings() {
    use crate::machine::{Container, Disk, Machine, Port};
    let snapshot = Machine {
        sampled_at: 1,
        cores: 8,
        load: Some([1.0, 2.0, 3.0]),
        ports: (6000..6200)
            .map(|port| Port { address: "127.0.0.1".into(), port, previewable: true, ..Default::default() })
            .collect(),
        containers: Some(
            (0..100)
                .map(|i| Container {
                    name: format!("web-{i}"),
                    image: "app.example/web:1".into(),
                    ..Default::default()
                })
                .collect(),
        ),
        disks: vec![Disk { path: "/srv/work".into(), total: 100, free: 50 }],
        ..Default::default()
    };
    let frame = machine_frame("epoch", &snapshot, 4096).unwrap();
    assert!(frame.len() <= 4096);
    let Message::Machine { snapshot: bounded } = decode(&frame).unwrap().message else { unreachable!() };
    assert_eq!(bounded.cores, snapshot.cores);
    assert_eq!(bounded.load, snapshot.load);
    assert!(bounded.truncated);
    assert!(bounded.ports.len() < snapshot.ports.len());
    assert!(machine_frame("epoch", &snapshot, 1).is_err());
}
