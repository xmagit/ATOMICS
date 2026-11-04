//! The remote Backend's reads and writes, fed frames directly.

use atomics_core::approvals::Decision;
use atomics_core::ops::{CommandResult, Outcome};
use atomics_core::steer::ReviewComment;
use atomics_core::wire::{Limits, ResetReason, Resume, Revisions, WireError};

use super::*;
use crate::fake_host::{self, WORKTREE};

const EPOCH: &str = "e1";

fn welcome(role: Role) -> Envelope {
    let capabilities: Vec<String> = CAPABILITIES.iter().map(|c| c.to_string()).collect();
    let may = role.may(&capabilities);
    let resume = Resume::Reset { cursor: Seq(3), reason: ResetReason::NewClient };
    Envelope::host(
        EPOCH,
        Message::Welcome { version: VERSION, capabilities, limits: Limits::default(), role, resume, may },
    )
}

fn snapshot() -> Envelope {
    let board = fake_host::board();
    Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            part: None,
        },
    )
}

fn connected(role: Role) -> RemoteBackend {
    let remote = RemoteBackend::new("op");
    remote.set_now(10_000);
    remote.receive(welcome(role));
    remote.receive(snapshot());
    remote
}

#[test]
fn the_board_reads_as_the_desktop_draws_it() {
    let remote = connected(Role::Controller);
    let plans = remote.plan_list();
    assert_eq!(plans.len(), 1);
    let row = &plans[0];
    assert_eq!((row.id.as_str(), row.tab_name.as_str(), row.done, row.total), ("p1", "Parse faster", 0, 2));
    assert_eq!(row.state, PlanState::NeedsYou, "its agent waits on an approval");
    assert_eq!(remote.plan_row(0), Some(row.clone()));
    assert_eq!(remote.plan_digest(0).as_deref(), Some("digest-1"));

    let board = remote.board(0).expect("a board");
    assert_eq!(board.plan.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
    assert!(board.ready.is_empty(), "t2 waits for t1");
    assert_eq!(board.sessions.get("s1").map(|(title, _)| title.as_str()), Some("Fix the parser"));
    assert_eq!(board.now, 10, "the board's clock is this client's");

    let sessions = remote.session_list();
    assert_eq!(sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["chief", "s1"]);
    let s1 = &sessions[1];
    assert_eq!((s1.root.to_str(), s1.ungrouped, s1.changes), (Some(WORKTREE), false, Some((12, 4))));
    assert!(sessions[0].chief && remote.session_approval(0).is_none());
    let approval = remote.session_approval(1).expect("s1's approval");
    assert_eq!((approval.session.as_str(), approval.id.as_str(), approval.request.tool.as_str()), ("s1", "a1", "Bash"));

    assert_eq!(remote.diff_roots(), [PathBuf::from(WORKTREE)]);
    let heading = remote.diff_heading(Path::new(WORKTREE));
    assert_eq!(
        (heading.title.as_str(), heading.branch.as_deref(), heading.can_send),
        ("Fix the parser", Some("atomics/t1"), true)
    );
    assert!(!remote.diff_heading(Path::new("/w/other")).can_send, "no agent works there");
    assert_eq!(remote.diff_heading(Path::new("/w/other")).title, "other");
    // The tab's sessions, never the chief: the plan's tab its task's agent, the Ungrouped tab none here.
    assert_eq!(remote.visible_sessions(), [1]);
    remote.set_view(ViewHints { plan_view: false, ungrouped: true, ..default_hints() });
    assert!(remote.visible_sessions().is_empty());
    remote.set_view(ViewHints { plan_view: false, ..default_hints() });
    assert_eq!(remote.visible_sessions(), [1]);
}

#[test]
fn commands_become_the_mutations_welcome_allows() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Deny });
    remote.send(Command::ApprovePlan(0));
    let sent: Vec<Message> = remote.take_outgoing().into_iter().map(|e| e.message).collect();
    let [Message::ApprovalDecide(decide), Message::PlanCommand(approve)] = sent.as_slice() else { panic!("{sent:?}") };
    assert_eq!((decide.op.as_str(), decide.approval.as_str(), decide.decision), ("op-1", "a1", Decision::Deny));
    assert_eq!(
        (approve.op.as_str(), approve.plan.as_deref(), approve.digest.as_deref()),
        ("op-2", Some("p1"), Some("digest-1"))
    );

    // The host refused the decision: the banner may offer it again, and the notice says why.
    let refused = Outcome::refused(WireError::new(ErrorCode::AlreadyResolved, "allowed on the desktop"));
    remote
        .receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-1".into(), outcome: refused })));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())]);
    assert!(remote.notice().unwrap().contains("already answered"));
    let done = Outcome::done(serde_json::Value::Null);
    remote.receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-2".into(), outcome: done })));
    assert!(remote.take_refused().is_empty());
}

#[test]
fn nothing_is_sent_offline_or_without_a_grant() {
    let offline = RemoteBackend::new("op");
    offline.send(Command::ApprovePlan(0));
    assert!(offline.take_outgoing().is_empty());
    assert!(offline.notice().unwrap().contains("Not connected"));

    let viewer = connected(Role::Viewer);
    viewer.take_outgoing();
    viewer.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    assert!(viewer.take_outgoing().is_empty());
    assert_eq!(viewer.take_refused(), [("s1".to_string(), "a1".to_string())]);
    assert!(!viewer.may(Grant::Approve));
}

#[test]
fn operations_waiting_for_the_host_are_bounded() {
    let remote = connected(Role::Controller);
    for _ in 0..wire::MAX_PENDING_OPS {
        remote.send(Command::ApprovePlan(0));
    }
    assert_eq!(remote.take_outgoing().len(), wire::MAX_PENDING_OPS);
    assert_eq!(remote.notice(), None);
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    assert!(remote.take_outgoing().is_empty(), "no more than the host takes at once");
    assert!(remote.notice().unwrap().contains("Too many changes"));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())], "the banner may offer it again");
    // An answer makes room.
    let done = Outcome::done(serde_json::Value::Null);
    remote.receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-1".into(), outcome: done })));
    remote.send(Command::ApprovePlan(0));
    assert_eq!(remote.take_outgoing().len(), 1);
}

#[test]
fn operations_in_flight_are_sent_again_after_a_reconnect_while_young() {
    let remote = connected(Role::Controller);
    remote.send(Command::ApprovePlan(0));
    remote.disconnected();
    assert!(remote.take_outgoing().is_empty(), "nothing waits to go out on a closed channel");
    let hello = remote.hello("atomics-web");
    let Message::Hello { prior_epoch, prior_cursor, .. } = hello.message else { panic!("hello") };
    assert_eq!((prior_epoch.as_deref(), prior_cursor), (Some(EPOCH), Some(Seq(3))));

    remote.receive(welcome(Role::Controller));
    let again: Vec<Message> = remote.take_outgoing().into_iter().map(|e| e.message).collect();
    assert!(matches!(again.as_slice(), [Message::PlanCommand(c), ..] if c.op == "op-1"), "the same op: {again:?}");

    remote.receive(snapshot());

    // One too old to send again is the user's to check.
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    remote.disconnected();
    remote.set_now(10_000 + u64::from(ops::OP_TTL_SECS) * 1000);
    remote.receive(welcome(Role::Controller));
    assert!(remote.take_outgoing().iter().all(|e| !matches!(e.message, Message::ApprovalDecide(_))));
    assert!(remote.notice().unwrap().contains("may not have reached"));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())]);
}

#[test]
fn a_file_the_host_cannot_read_shows_as_an_error() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.want_diff(Path::new(WORKTREE));
    let asked: Vec<Envelope> = remote.take_outgoing();
    assert_eq!(asked.len(), 2);
    let id = asked[0].request_id.clone();
    remote.receive(WireError::new(ErrorCode::NotFound, "gone").into_envelope(id, Some(EPOCH.into())));
    let diff = remote.diff(Path::new(WORKTREE)).unwrap();
    assert!(diff.loading, "the other file is still on its way");
    assert_eq!(diff.error.as_deref(), Some("1 file(s) could not be read from the host"));
    assert_eq!(remote.notice(), None, "a diff error is the diff's, not a notice");
}

#[test]
fn refresh_fetches_again_and_what_names_nothing_says_so() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::RefreshDiff(WORKTREE.into()));
    assert_eq!(remote.take_outgoing().len(), 2, "refresh reads the files again");
    for gone in [Command::IntegrateAccepted(0), Command::DispatchReady(9), Command::ArchivePlan(9)] {
        remote.dismiss_notice();
        remote.send(gone.clone());
        assert!(remote.take_outgoing().is_empty(), "{gone:?}");
        assert!(remote.notice().unwrap().contains("no longer on the host"), "{gone:?}");
    }
}

#[test]
fn archive_finished_is_not_sent_as_unconditional_archive() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    assert_eq!(Command::ArchiveFinished(0).remote(&remote, &mut || "op".into()), atomics_ui::Remote::Unsupported);
    remote.send(Command::ArchiveFinished(0));
    assert!(remote.take_outgoing().is_empty());
    assert!(remote.notice().unwrap().contains("not available"));
}

/// What `Command::remote` maps, each as the views ask whether to draw it: positions into the plan and session
/// lists, and a steer with no target or a comment not written yet.
fn mapped() -> Vec<Command> {
    let root = PathBuf::from(WORKTREE);
    vec![
        Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow },
        Command::ApprovePlan(0),
        Command::StartTask { plan: 0, task: String::new() },
        Command::DispatchReady(0),
        Command::SetTaskStatus { plan: 0, task: String::new(), status: TaskStatus::Done },
        Command::IntegrateTask { plan: 0, task: String::new() },
        Command::IntegrateAccepted(0),
        Command::ArchivePlan(0),
        Command::ClosePlan { plan: 0, status: PlanStatus::Archived },
        Command::EditComments { root: root.clone(), before: Vec::new(), after: Vec::new() },
        Command::SendReview(root),
        Command::Steer { sessions: Vec::new(), text: String::new() },
        Command::StartAgent { agent: String::new(), folder: PathBuf::new() },
        Command::Start { agent: String::new(), folder: None },
        Command::CloseSession(String::new()),
    ]
}

fn wide(remote: &RemoteBackend) {
    let window = crate::desk::ChiefWindow { open: true, dock: atomics_core::config::ChiefDock::Docked, width: None };
    remote.set_chief_window(window, true);
}

#[test]
fn a_controller_may_do_what_atomics_v1_carries_and_a_viewer_or_a_page_offline_nothing() {
    let controller = connected(Role::Controller);
    wide(&controller);
    let viewer = connected(Role::Viewer);
    wide(&viewer);
    let offline = connected(Role::Controller);
    wide(&offline);
    offline.disconnected();
    for command in mapped() {
        assert!(controller.available(&command), "a controller may {command:?}");
        assert!(!viewer.available(&command), "a viewer may not {command:?}");
        assert!(!offline.available(&command), "nothing goes offline: {command:?}");
    }
    for command in [Command::NewSession, Command::ClosePlanTab(0)] {
        assert!(controller.available(&command), "{command:?}");
        assert!(!viewer.available(&command), "{command:?}");
        assert!(!offline.available(&command), "{command:?}");
    }
}

#[test]
fn what_atomics_v1_has_no_action_for_stays_hidden_and_a_voice_with_it() {
    let controller = connected(Role::Controller);
    wide(&controller);
    for command in [
        Command::VoiceToSession { session: 1, text: "go on".into() },
        Command::ClosePlan { plan: 0, status: PlanStatus::Completed },
        Command::ArchiveFinished(0),
        Command::ReopenPlan(0),
        Command::CleanUpPlan(0),
        Command::ResumeSession("s1".into()),
        Command::EditBrief { plan: 0, task: "t2".into(), brief: "b".into() },
        Command::SyncTickets(0),
        Command::Edit { root: WORKTREE.into(), path: None, line: None },
        Command::DismissMcp("s1".into()),
        Command::PickProject,
        Command::SetView(None),
        Command::Rename("s1".into()),
    ] {
        assert!(!controller.available(&command), "{command:?}");
    }
    controller.take_outgoing();
    controller.send(Command::VoiceToSession { session: 1, text: "go on".into() });
    assert!(controller.take_outgoing().is_empty(), "a browser has no voice to send");
    assert!(controller.notice().unwrap().contains("not available"));
}

#[test]
fn the_views_own_commands_stay_and_what_lives_in_one_layout_goes_with_it() {
    let offline = RemoteBackend::new("op");
    assert!(offline.available(&Command::ShowPlan(0)), "the frame's own commands stay");
    let controller = connected(Role::Controller);
    assert!(controller.available(&Command::RefreshDiff(WORKTREE.into())));
    for command in [
        Command::Activate { session: "s1".into(), open_diff: false },
        Command::ToggleDiff,
        Command::ShowUngrouped,
        Command::Toast("hi".into()),
    ] {
        assert!(controller.available(&command), "the frame carries out {command:?}");
    }
    // The chief's window, the steer bar, the changed files' column and the picker are the wide layout's.
    let steer = Command::Steer { sessions: Vec::new(), text: String::new() };
    let open = Command::OpenFile { root: WORKTREE.into(), path: String::new() };
    for command in [Command::ToggleChief, Command::HideChief, steer, open, Command::NewSession] {
        assert!(!controller.available(&command), "{command:?} on a phone");
        wide(&controller);
        assert!(controller.available(&command), "{command:?} in the wide layout");
        let window =
            crate::desk::ChiefWindow { open: true, dock: atomics_core::config::ChiefDock::Docked, width: None };
        controller.set_chief_window(window, false);
    }
    let viewer = connected(Role::Viewer);
    assert!(viewer.available(&Command::ShowPlan(0)), "a viewer looks around");
}

#[test]
fn a_finished_plans_tab_has_no_close_and_what_names_nothing_is_not_offered() {
    let remote = RemoteBackend::new("op");
    remote.set_now(10_000);
    remote.receive(welcome(Role::Controller));
    let mut board = fake_host::board();
    board.plans.push(fake_host::closed_plan("p2", PlanStatus::Completed, "Done"));
    remote.receive(Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            part: None,
        },
    ));
    assert!(remote.available(&Command::ClosePlanTab(0)), "the open plan");
    assert!(!remote.available(&Command::ClosePlanTab(1)), "archiving it needs a guard the protocol lacks");
    assert!(remote.available(&Command::ArchivePlan(1)), "its board's own Archive stays");
    assert!(!remote.available(&Command::DispatchReady(9)), "no such plan");
}

#[test]
fn a_page_starts_only_the_agents_it_can_name() {
    let empty = RemoteBackend::new("op");
    empty.receive(welcome(Role::Controller));
    wide(&empty);
    let start = Command::Start { agent: "claude".into(), folder: None };
    assert!(!empty.available(&start) && !empty.available(&Command::NewSession), "the mirror names no agent");
    assert!(empty.ui_config().agents.is_empty());

    let remote = connected(Role::Controller);
    wide(&remote);
    assert!(remote.available(&start));
    assert_eq!(remote.ui_config().agents, ["claude"]);
    assert_eq!(remote.folders(), [PathBuf::from("/w/repo"), PathBuf::from(WORKTREE)]);
}

/// The frame the last command sent.
fn one(remote: &RemoteBackend) -> [Message; 1] {
    let sent: Vec<Message> = remote.take_outgoing().into_iter().map(|e| e.message).collect();
    sent.try_into().expect("one frame")
}

#[test]
fn each_command_sends_its_mutation() {
    use atomics_core::ops::{ReviewAction, SessionAction};

    let remote = connected(Role::Controller);
    remote.take_outgoing();
    let root = PathBuf::from(WORKTREE);
    let comment = ReviewComment {
        path: "src/a.rs".into(),
        line: Some(2),
        removed: false,
        excerpt: "x".into(),
        body: "why?".into(),
    };
    for (command, sent) in [
        (Command::StartTask { plan: 0, task: "t2".into() }, PlanAction::Dispatch { task: Some("t2".into()) }),
        (Command::DispatchReady(0), PlanAction::Dispatch { task: None }),
        (
            Command::SetTaskStatus { plan: 0, task: "t1".into(), status: TaskStatus::Done },
            PlanAction::Update { task: "t1".into(), status: Some(TaskStatus::Done), note: None },
        ),
        (Command::IntegrateTask { plan: 0, task: "t1".into() }, PlanAction::Integrate { task: "t1".into() }),
        (Command::ArchivePlan(0), PlanAction::Archive),
        (Command::ClosePlan { plan: 0, status: PlanStatus::Archived }, PlanAction::Archive),
    ] {
        remote.send(command.clone());
        let [Message::PlanCommand(plan)] = one(&remote) else { panic!("{command:?}") };
        assert_eq!((plan.plan.as_deref(), plan.action), (Some("p1"), sent), "{command:?}");
    }
    let edit = Command::EditComments { root: root.clone(), before: Vec::new(), after: vec![comment.clone()] };
    remote.send(edit);
    let [Message::ReviewCommand(added)] = one(&remote) else { panic!("a review command") };
    assert_eq!(added.worktree, WORKTREE);
    assert!(matches!(added.action, ReviewAction::Add { revision: Seq(4), line: Some(2), .. }), "{:?}", added.action);
    remote.send(Command::EditComments { root: root.clone(), before: vec![comment], after: Vec::new() });
    let [Message::ReviewCommand(removed)] = one(&remote) else { panic!("a review command") };
    assert!(matches!(removed.action, ReviewAction::Remove { .. }), "{:?}", removed.action);
    remote.send(Command::SendReview(root));
    let [Message::ReviewCommand(sent)] = one(&remote) else { panic!("a review command") };
    assert_eq!(sent.action, ReviewAction::Send);

    let session = |message: &Message| match message {
        Message::SessionCommand(command) => command.action.clone(),
        other => panic!("{other:?}"),
    };
    remote.send(Command::Steer { sessions: vec!["s1".into(), "chief".into()], text: "go on".into() });
    let sent: Vec<SessionAction> = remote.take_outgoing().iter().map(|e| session(&e.message)).collect();
    assert_eq!(
        sent,
        [
            SessionAction::Steer { session: "s1".into(), text: "go on".into() },
            SessionAction::Steer { session: "chief".into(), text: "go on".into() }
        ],
        "one steer each"
    );
    remote.send(Command::Start { agent: "claude".into(), folder: None });
    remote.send(Command::StartAgent { agent: "claude".into(), folder: "/w/repo".into() });
    remote.send(Command::CloseSession("s1".into()));
    let sent: Vec<SessionAction> = remote.take_outgoing().iter().map(|e| session(&e.message)).collect();
    let start = |folder: Option<&str>| SessionAction::Start {
        agent: "claude".into(),
        folder: folder.map(str::to_string),
        repo: None,
        branch: None,
        prompt: None,
    };
    assert_eq!(sent, [start(None), start(Some("/w/repo")), SessionAction::Close { session: "s1".into() }]);
}

#[test]
fn a_viewer_and_a_page_offline_send_nothing_and_say_why() {
    let root = PathBuf::from(WORKTREE);
    let commands = [
        Command::DispatchReady(0),
        Command::ArchivePlan(0),
        Command::SendReview(root),
        Command::Steer { sessions: vec!["s1".into()], text: "go on".into() },
        Command::CloseSession("s1".into()),
        Command::Start { agent: "claude".into(), folder: None },
    ];
    let viewer = connected(Role::Viewer);
    viewer.take_outgoing();
    for command in commands.clone() {
        viewer.dismiss_notice();
        viewer.send(command.clone());
        assert!(viewer.take_outgoing().is_empty(), "{command:?}");
        assert!(viewer.notice().unwrap().contains("may only view"), "{command:?}");
    }
    let offline = connected(Role::Controller);
    offline.take_outgoing();
    offline.disconnected();
    for command in commands {
        offline.dismiss_notice();
        offline.send(command.clone());
        assert!(offline.take_outgoing().is_empty(), "{command:?}");
        assert!(offline.notice().unwrap().contains("Not connected"), "{command:?}");
    }
}

fn result(remote: &RemoteBackend, op: &str, outcome: Outcome) {
    remote.receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: op.into(), outcome })));
}

#[test]
fn a_steer_that_does_not_go_through_is_the_users_again() {
    let steer = Command::Steer { sessions: vec!["s1".into()], text: "go on".into() };
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(steer.clone());
    assert_eq!(remote.take_unsent(), None, "while it is in flight");
    result(&remote, "op-1", Outcome::refused(WireError::new(ErrorCode::Rejected, "session exited")));
    assert_eq!(remote.take_unsent().as_deref(), Some("go on"));
    assert_eq!(remote.notice().as_deref(), Some("The host refused: session exited"));

    remote.dismiss_notice();
    remote.send(steer.clone());
    result(&remote, "op-2", Outcome::Uncertain { message: "the host restarted".into() });
    assert_eq!(remote.take_unsent().as_deref(), Some("go on"));
    assert!(remote.notice().unwrap().contains("could not confirm"));

    remote.dismiss_notice();
    remote.send(steer.clone());
    result(&remote, "op-3", Outcome::done(serde_json::json!({ "sent": true, "queued_until_next_hook": false })));
    assert_eq!(remote.take_unsent(), None, "it went through");
    assert_eq!(remote.notice(), None, "what it did shows");

    // Too old to send again after a reconnect.
    remote.send(steer.clone());
    remote.disconnected();
    remote.set_now(10_000 + u64::from(ops::OP_TTL_SECS) * 1000);
    remote.receive(welcome(Role::Controller));
    assert_eq!(remote.take_unsent().as_deref(), Some("go on"));

    // Over the host's limit: it says so here, and nothing waits.
    remote.dismiss_notice();
    remote.take_outgoing();
    remote.send(Command::Steer { sessions: vec!["s1".into()], text: "x".repeat(ops::MAX_TEXT_BYTES + 1) });
    assert!(remote.take_outgoing().is_empty());
    assert!(remote.notice().unwrap().contains("not sent"), "{:?}", remote.notice());
    assert!(remote.take_unsent().is_some());
}

#[test]
fn a_steer_some_targets_missed_names_them_and_only_one_all_missed_comes_back() {
    let steer = || Command::Steer { sessions: vec!["chief".into(), "s1".into()], text: "go on".into() };
    let no = || Outcome::refused(WireError::new(ErrorCode::Rejected, "session exited"));
    let yes = || Outcome::done(serde_json::Value::Null);
    let remote = connected(Role::Controller);
    let agent = remote.session_list().into_iter().find(|s| s.id == "s1").unwrap().title;

    // Whichever answers first, the text stays out of the bar and the note names who missed it.
    remote.send(steer());
    result(&remote, "op-1", yes());
    assert_eq!(remote.notice(), None, "while the other is on its way");
    result(&remote, "op-2", no());
    assert_eq!(remote.take_unsent(), None);
    assert_eq!(remote.notice(), Some(format!("Missed by {agent}. The host refused: session exited")));

    remote.dismiss_notice();
    remote.send(steer());
    result(&remote, "op-3", no());
    assert_eq!(remote.take_unsent(), None, "the other may still get it");
    assert_eq!(remote.notice().as_deref(), Some("The host refused: session exited"));
    result(&remote, "op-4", yes());
    assert_eq!(remote.take_unsent(), None);
    assert!(remote.notice().unwrap().starts_with("Missed by "), "{:?}", remote.notice());

    // Missed by all: it comes back, as a steer to one does.
    remote.dismiss_notice();
    remote.send(steer());
    result(&remote, "op-5", no());
    result(&remote, "op-6", no());
    assert_eq!(remote.take_unsent().as_deref(), Some("go on"));
    assert_eq!(remote.notice().as_deref(), Some("The host refused: session exited"));

    // Too many waiting: the one that went has it, and the other is named.
    remote.dismiss_notice();
    for _ in 0..wire::MAX_PENDING_OPS - 1 {
        remote.send(Command::ApprovePlan(0));
    }
    remote.send(steer());
    assert_eq!(remote.take_unsent(), None, "the first went");
    let last = format!("op-{}", 6 + wire::MAX_PENDING_OPS);
    result(&remote, &last, yes());
    assert_eq!(remote.take_unsent(), None);
    assert!(remote.notice().unwrap().starts_with("Missed by "), "{:?}", remote.notice());
    assert!(remote.notice().unwrap().contains("Too many changes"), "{:?}", remote.notice());
}

#[test]
fn a_steer_missed_by_a_session_gone_from_the_mirror_says_closed_and_not_its_id() {
    let steer = |sessions: &[&str]| Command::Steer {
        sessions: sessions.iter().map(|s| s.to_string()).collect(),
        text: "go on".into(),
    };
    let no = || Outcome::refused(WireError::new(ErrorCode::Rejected, "session exited"));
    let yes = || Outcome::done(serde_json::Value::Null);
    let closed = "Missed by a closed session. The host refused: session exited";
    let remote = connected(Role::Controller);

    // The board comes back without s1 while the steer is on its way, as after a reset.
    remote.send(steer(&["chief", "s1"]));
    let mut board = fake_host::board();
    board.sessions.retain(|s| s.chief);
    remote.receive(Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            part: None,
        },
    ));
    assert_eq!(remote.session_list().len(), 1, "s1 has left the mirror");
    result(&remote, "op-1", yes());
    result(&remote, "op-2", no());
    assert_eq!(remote.notice().as_deref(), Some(closed));

    // Two of them read as one.
    remote.dismiss_notice();
    remote.send(steer(&["chief", "s1", "s2"]));
    result(&remote, "op-3", yes());
    result(&remote, "op-4", no());
    result(&remote, "op-5", no());
    assert_eq!(remote.notice().as_deref(), Some(closed));
}

#[test]
fn a_comment_that_does_not_go_through_is_the_users_again() {
    let comment = ReviewComment {
        path: "src/a.rs".into(),
        line: Some(2),
        removed: false,
        excerpt: String::new(),
        body: "Why?".into(),
    };
    let add = || Command::EditComments { root: WORKTREE.into(), before: Vec::new(), after: vec![comment.clone()] };
    let back = [(PathBuf::from(WORKTREE), comment.clone())];

    let remote = connected(Role::Controller);
    remote.send(add());
    assert!(remote.take_unsent_comments().is_empty(), "while it is in flight");
    result(&remote, "op-1", Outcome::refused(WireError::new(ErrorCode::StaleRevision, "the diff moved on")));
    assert_eq!(remote.take_unsent_comments(), back);
    assert_eq!(remote.notice().as_deref(), Some("The host refused: the diff moved on"));

    remote.send(add());
    result(&remote, "op-2", Outcome::done(serde_json::Value::Null));
    assert!(remote.take_unsent_comments().is_empty(), "it went through");

    // A comment that could not be removed stays on the host and on screen.
    remote.send(Command::EditComments { root: WORKTREE.into(), before: vec![comment.clone()], after: Vec::new() });
    result(&remote, "op-3", Outcome::refused(WireError::new(ErrorCode::Rejected, "no")));
    assert!(remote.take_unsent_comments().is_empty());

    // Offline nothing is sent, and it comes back all the same.
    remote.disconnected();
    remote.send(add());
    assert_eq!(remote.take_unsent_comments(), back);
}

#[test]
fn a_note_about_the_connection_goes_when_it_is_back_and_no_other_does() {
    let remote = connected(Role::Controller);
    remote.disconnected();
    remote.send(Command::ApprovePlan(0));
    assert_eq!(remote.notice().as_deref(), Some(NOT_CONNECTED));
    remote.receive(welcome(Role::Controller));
    assert_eq!(remote.notice(), None);

    remote.disconnected();
    remote.say("That was refused.");
    remote.receive(welcome(Role::Controller));
    assert_eq!(remote.notice().as_deref(), Some("That was refused."));
}

#[test]
fn a_session_counts_as_started_once_the_host_has_started_it() {
    let start = || Command::Start { agent: "claude".into(), folder: None };
    let remote = connected(Role::Controller);
    remote.send(start());
    assert!(!remote.take_started(), "while it is on its way");
    result(&remote, "op-1", Outcome::refused(WireError::new(ErrorCode::NotFound, "no agent claude is configured")));
    assert!(!remote.take_started());
    remote.send(start());
    result(&remote, "op-2", Outcome::done(serde_json::Value::Null));
    assert!(remote.take_started());
    assert!(!remote.take_started(), "once");
}

#[test]
fn a_delivery_that_did_not_happen_yet_says_so_without_hiding_a_refusal() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    let steer = |remote: &RemoteBackend| remote.send(Command::Steer { sessions: vec!["s1".into()], text: "t".into() });
    steer(&remote);
    result(&remote, "op-1", Outcome::done(serde_json::json!({ "sent": false, "held": true })));
    assert!(remote.notice().unwrap().contains("Held"));
    remote.dismiss_notice();
    steer(&remote);
    result(&remote, "op-2", Outcome::done(serde_json::json!({ "sent": true, "queued_until_next_hook": true })));
    assert!(remote.notice().unwrap().contains("Queued"));

    remote.dismiss_notice();
    steer(&remote);
    steer(&remote);
    result(&remote, "op-3", Outcome::refused(WireError::new(ErrorCode::Rejected, "session exited")));
    result(&remote, "op-4", Outcome::done(serde_json::json!({ "sent": false, "held": true })));
    assert!(remote.notice().unwrap().contains("session exited"), "{:?}", remote.notice());
}

#[test]
fn a_task_shows_as_integrating_until_its_outcome() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    let integrate = Command::IntegrateTask { plan: 0, task: "t1".into() };
    remote.send(integrate.clone());
    assert!(remote.board(0).unwrap().integrating.contains("t1"));
    result(&remote, "op-1", Outcome::refused(WireError::new(ErrorCode::Rejected, "merge conflict in a.rs")));
    assert!(remote.board(0).unwrap().integrating.is_empty(), "nothing stays stale");
    assert!(remote.notice().unwrap().contains("merge conflict"));

    remote.send(integrate);
    assert!(remote.board(0).unwrap().integrating.contains("t1"));
    result(&remote, "op-2", Outcome::Accepted);
    assert!(remote.board(0).unwrap().integrating.contains("t1"), "under way");
    result(&remote, "op-2", Outcome::done(serde_json::Value::Null));
    assert!(remote.board(0).unwrap().integrating.is_empty());
}

#[test]
fn an_integrated_task_is_not_offered_again_and_no_steer_goes_to_every_agent() {
    let remote = RemoteBackend::new("op");
    remote.receive(welcome(Role::Controller));
    let mut board = fake_host::board();
    let plan = &mut board.plans[0];
    plan.approved = true;
    plan.tasks[0].status = TaskStatus::Done;
    plan.tasks[0].integrated = Some("atomics/plan-p1".into());
    plan.tasks[1].status = TaskStatus::Done;
    plan.tasks[1].branch = Some("atomics/t2".into());
    let mut shell = board.sessions[1].clone();
    (shell.id, shell.agent, shell.worktree) = ("sh".into(), "shell".into(), Some("/w/shell".into()));
    board.sessions.push(shell);
    board.diffs[0].worktree = "/w/shell".into();
    remote.receive(Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            part: None,
        },
    ));
    let plan = remote.board(0).unwrap().plan;
    let pending: Vec<&str> = plan.pending_integrations().iter().map(|t| t.id.as_str()).collect();
    assert_eq!(pending, ["t2"], "t1 is in the integration branch");

    remote.set_view(ViewHints { active: Some("s1".into()), ..default_hints() });
    // The mirror has no agent kinds: typed into a shell under another name, it would run as a command.
    assert!(!remote.tells_agents_from_shells());
    let mut client = atomics_ui::Client { steer_target: atomics_ui::chief::SteerTarget::Agents, ..Default::default() };
    client.steer_text = "go on".into();
    atomics_ui::chief::send_steer(&remote, &mut client);
    assert_eq!(client.commands, [Command::Toast("No session to send to".into())]);
    assert_eq!(client.steer_text, "go on");
    assert!(!remote.diff_heading(Path::new("/w/shell")).can_send, "a shell takes no review");
}

/// The window's frame over this backend, with no host items and no terminals.
struct Frame<'a> {
    remote: &'a RemoteBackend,
    client: atomics_ui::Client,
}

struct NoTerminals;

impl atomics_ui::terminal_surface::TerminalSurface for NoTerminals {
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        _index: usize,
        _session: &str,
        rect: egui::Rect,
        id: egui::Id,
        _metrics: &atomics_ui::terminal::Metrics,
        _palette: &atomics_ui::theme::Palette,
    ) -> egui::Response {
        ui.interact(rect, id, egui::Sense::click())
    }
}

impl atomics_ui::shell::Platform for Frame<'_> {
    fn draw<R>(
        &mut self,
        draw: impl FnOnce(
            &dyn Backend,
            &mut atomics_ui::Client,
            &mut dyn atomics_ui::terminal_surface::TerminalSurface,
        ) -> R,
    ) -> R {
        draw(self.remote, &mut self.client, &mut NoTerminals)
    }
}

/// The text the frame draws over `remote` in a wide window, once it has settled.
fn frame_text(remote: &RemoteBackend) -> Vec<String> {
    let ctx = egui::Context::default();
    let mut frame = Frame { remote, client: atomics_ui::Client::default() };
    let mut text = Vec::new();
    for time in [0.0, 0.1] {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))),
            time: Some(time),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| atomics_ui::shell::frame(ui, &mut frame));
        text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        assert!(frame.client.commands.is_empty(), "drawing alone asks for nothing");
    }
    text
}

#[test]
fn the_frame_leaves_out_the_host_items_and_what_the_browser_cannot_do() {
    let remote = connected(Role::Controller);
    remote.set_view(ViewHints { diff_open: true, ..default_hints() });
    let text = frame_text(&remote);
    let has = |label: &str| text.iter().any(|t| t.starts_with(label));
    for shown in [
        "ATOMICS",
        "CHIEF",
        "Ungrouped · 0",
        "◆ Parse faster",
        "Hide diff",
        "Plan 0/2",
        "AGENTS",
        "PLAN",
        "Approve plan",
    ] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    for host_only in ["USAGE", "TICKETS", "Settings", "Tickets", "load ", "Remote", "Project"] {
        assert!(!has(host_only), "{host_only} is the host's: {text:?}");
    }
    // Its steer bar is the wide layout's, and this frame's page has not said it is wide.
    for not_drawn in ["All ·", "Repos", "EXPLORER", "Steer", "Send", "+ New", "Complete plan"] {
        assert!(!has(not_drawn), "{not_drawn} has nothing to do here: {text:?}");
    }
    assert!(has("Close plan…"), "its archive goes through: {text:?}");
    wide(&remote);
    let text = frame_text(&remote);
    let has = |label: &str| text.iter().any(|t| t.starts_with(label));
    for shown in ["Steer", "Send", "+ New"] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }

    let viewer = connected(Role::Viewer);
    viewer.set_view(ViewHints { diff_open: true, ..default_hints() });
    let text = frame_text(&viewer);
    assert!(text.iter().any(|t| t == "PLAN"), "a viewer sees the board: {text:?}");
    for action in ["Approve plan", "Allow", "×", "Close plan…", "Dispatch", "Send", "+ New", "start", "accept"] {
        assert!(!text.iter().any(|t| t.starts_with(action)), "{action}: a viewer cannot change it: {text:?}");
    }
    assert!(!text.iter().any(|t| t.starts_with("Steer")), "{text:?}");
}
