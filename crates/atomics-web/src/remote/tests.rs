//! The remote Backend's reads and writes, fed frames directly.

use atomics_core::approvals::Decision;
use atomics_core::ops::{CommandResult, HostAction, Outcome};
use atomics_core::projection::{HostInfo, SignInState};
use atomics_core::steer::ReviewComment;
use atomics_core::wire::{Limits, ResetReason, Resume, Revisions, WireError};

use super::*;
use crate::fake_host::{self, WORKTREE};

const EPOCH: &str = "e1";

#[test]
fn task_cards_keep_report_evidence_and_reviewer_reason_from_the_host() {
    let mut info = fake_host::board().plans[0].tasks[0].clone();
    info.reviewer_reason = Some("Selected by [plan] review_agent override.".into());
    let check = atomics_core::plan::Check {
        check: "Tests pass".into(),
        command: "cargo test".into(),
        exit: Some(0),
        output_tail: "passed".into(),
    };
    info.report = Some(atomics_core::projection::ReportInfo {
        status: "done".into(),
        summary: "Checked".into(),
        at: 1,
        checks: vec![check.clone()],
    });
    let card = task(&info);
    assert_eq!(card.reviewer_reason, info.reviewer_reason);
    assert_eq!(card.report.unwrap().checks, [check]);
}

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
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            host: board.host,
            part: None,
        },
    )
}

fn connected(role: Role) -> RemoteBackend {
    let remote = RemoteBackend::new("op");
    remote.set_now(10_000);
    remote.receive(welcome(role));
    remote.receive(snapshot());
    remote.take_outgoing();
    remote
}

#[test]
fn folder_subscriptions_page_reset_unsubscribe_and_reconnect() {
    let remote = connected(Role::Viewer);
    let root = Path::new(WORKTREE);
    let paths: Vec<String> = (0..260).map(|i| format!("file{i:03}.rs")).collect();
    remote.take_outgoing();
    assert!(remote.folder(root, "").loading);
    assert!(
        matches!(&remote.take_outgoing()[0].message, Message::Subscribe { topics } if matches!(&topics[0], wire::Topic::Folder { worktree, path } if worktree == WORKTREE && path.is_empty()))
    );
    let page = |revision, offset| {
        atomics_core::folder::page(
            paths.iter().map(String::as_str),
            WORKTREE,
            "",
            revision,
            offset,
            wire::MAX_FRAME_BYTES,
        )
        .unwrap()
    };
    remote.receive(Envelope::host(EPOCH, Message::FolderPage(page(4, 0))));
    assert_eq!(remote.folder(root, "").entries.len(), 128);
    remote.more_folder(root, "");
    assert!(matches!(remote.take_outgoing()[0].message, Message::FolderGet { offset: 128, revision: Seq(4), .. }));
    remote.receive(Envelope::host(EPOCH, Message::FolderPage(page(5, 0))));
    remote.receive(Envelope::host(EPOCH, Message::FolderPage(page(4, 128))));
    assert_eq!(remote.folder(root, "").entries.len(), 128, "an old page cannot join the new listing");
    remote.more_folder(root, "");
    remote.take_outgoing();
    remote.receive(Envelope::host(EPOCH, Message::FolderPage(page(5, 128))));
    assert_eq!(remote.folder(root, "").entries.len(), 128, "a page replaces the last to bound the client's file cache");
    assert!(!remote.folder(root, "").first);
    remote.end_frame();
    remote.end_frame();
    assert!(remote.take_outgoing().iter().any(|e| matches!(e.message, Message::Unsubscribe { .. })));
    remote.receive(Envelope::host(EPOCH, Message::FolderPage(page(5, 0))));
    assert!(remote.state.borrow().folders.is_empty(), "a late page does not restore a closed folder");
    remote.folder(root, "");
    remote.disconnected();
    remote.receive(welcome(Role::Viewer));
    remote.folder(root, "");
    assert!(
        remote.take_outgoing().iter().any(|e| matches!(e.message, Message::Subscribe { .. })),
        "reconnect follows the folder again"
    );
}

#[test]
fn completing_a_plan_requires_the_hosts_capability() {
    let remote = connected(Role::Controller);
    let complete = Command::ClosePlan { plan: 0, status: PlanStatus::Completed };
    assert!(remote.available(&complete));
    remote.state.borrow_mut().welcome.as_mut().unwrap().capabilities.retain(|c| c != wire::PLAN_COMPLETE);
    assert!(!remote.available(&complete));
    assert!(remote.available(&Command::ClosePlan { plan: 0, status: PlanStatus::Archived }));
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
fn an_approvals_warning_reaches_its_banner() {
    let remote = connected(Role::Controller);
    let mut approval = fake_host::board().approvals[0].clone();
    approval.command = Some("git stash drop stash@{1}".into());
    approval.warning = Some("stash@{1} (On atomics/t2: t2), which belongs to task t2 (session s2)".into());
    remote.receive(Envelope::host(EPOCH, Message::ApprovalPending { cursor: Seq(4), approval: approval.clone() }));
    let row = remote.session_approval(1).expect("s1's approval");
    assert_eq!(row.request.warning, approval.warning);
    assert!(!row.request.may_allow_always(), "a stash drop is never allowed always");
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
    assert!(again.iter().any(|m| matches!(m, Message::PlanCommand(c) if c.op == "op-1")), "the same op: {again:?}");

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
fn a_board_maps_the_ticket_keys_the_projection_carries_and_nothing_else() {
    let remote = connected(Role::Controller);
    let board = remote.board(0).unwrap();
    let keys = |ticket: &Option<TicketLink>| ticket.as_ref().map(|link| link.label().to_string());
    assert_eq!(keys(&board.plan.ticket).as_deref(), Some("ENG-40"));
    assert_eq!(keys(&board.plan.tasks[0].ticket).as_deref(), Some("ENG-41"));
    assert_eq!(board.plan.tasks[1].ticket, None);
    let link = board.plan.ticket.unwrap();
    assert_eq!(link, TicketLink { id: "ENG-40".into(), ..TicketLink::default() }, "no address and no system");

    let read = Command::OpenTicket(link);
    assert_eq!(read.remote(&remote, &mut String::new), atomics_ui::Remote::Client, "nothing for atomics.v1");
    assert!(!remote.available(&read), "a page has no reader");
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
        Command::ArchiveFinished(0),
        Command::ReopenPlan(0),
        Command::CleanUpPlan(0),
        Command::ResumeSession("s1".into()),
        Command::EditBrief { plan: 0, task: "t2".into(), brief: "b".into() },
        Command::SyncTickets(0),
        Command::DismissMcp("s1".into()),
        Command::PickProject,
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
    for command in [steer, open, Command::NewSession, Command::SetView(None)] {
        assert!(controller.available(&command), "{command:?} on a phone");
    }
    for command in [Command::ToggleChief, Command::HideChief] {
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
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            host: board.host,
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
        (Command::ClosePlan { plan: 0, status: PlanStatus::Completed }, PlanAction::Complete),
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
fn a_steer_missed_by_a_session_gone_from_the_mirror_names_its_title() {
    let steer = |sessions: &[&str]| Command::Steer {
        sessions: sessions.iter().map(|s| s.to_string()).collect(),
        text: "go on".into(),
    };
    let no = || Outcome::refused(WireError::new(ErrorCode::Rejected, "session exited"));
    let yes = || Outcome::done(serde_json::Value::Null);
    let missed = "Missed by Fix the parser. The host refused: session exited";
    let remote = connected(Role::Controller);

    // The board comes back without s1 while the steer is on its way, as after a reset.
    remote.send(steer(&["chief", "s1"]));
    let mut board = fake_host::board();
    board.sessions.retain(|s| s.chief);
    remote.receive(Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            host: board.host,
            part: None,
        },
    ));
    assert_eq!(remote.session_list().len(), 1, "s1 has left the mirror");
    result(&remote, "op-1", yes());
    result(&remote, "op-2", no());
    assert_eq!(remote.notice().as_deref(), Some(missed));
}

#[test]
fn an_expired_steer_after_a_reset_names_its_target_before_the_snapshot() {
    let remote = connected(Role::Controller);
    remote.send(Command::Steer { sessions: vec!["chief".into(), "s1".into()], text: "go on".into() });
    result(&remote, "op-1", Outcome::done(serde_json::Value::Null));
    remote.disconnected();
    remote.set_now(10_000 + u64::from(ops::OP_TTL_SECS) * 1000);
    remote.receive(welcome(Role::Controller));
    assert!(remote.session_list().is_empty(), "reset cleared the mirror");
    assert_eq!(
        remote.notice().as_deref(),
        Some(
            "Missed by Fix the parser. Some earlier actions may not have reached the host. Check the board before trying again."
        )
    );
    assert_eq!(remote.take_unsent(), None, "the chief already received it");
    remote.receive(snapshot());
    assert!(remote.session_list().iter().any(|s| s.id == "s1"), "the agent is still live");
}

#[test]
fn a_steer_result_before_the_snapshot_names_its_target() {
    let remote = connected(Role::Controller);
    remote.send(Command::Steer { sessions: vec!["chief".into(), "s1".into()], text: "go on".into() });
    result(&remote, "op-1", Outcome::done(serde_json::Value::Null));
    remote.disconnected();
    remote.receive(welcome(Role::Controller));
    assert!(remote.session_list().is_empty(), "reset cleared the mirror");
    result(&remote, "op-2", Outcome::refused(WireError::new(ErrorCode::Rejected, "too busy")));
    assert_eq!(remote.notice().as_deref(), Some("Missed by Fix the parser. The host refused: too busy"));
    assert_eq!(remote.take_unsent(), None, "the chief already received it");
    remote.receive(snapshot());
    assert!(remote.session_list().iter().any(|s| s.id == "s1"), "the agent is still live");
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
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            host: board.host,
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
    host_upsert(&remote, 4, fake_host::management().board.host);
    remote.set_view(ViewHints { diff_open: true, ..default_hints() });
    let text = frame_text(&remote);
    let has = |label: &str| text.iter().any(|t| t.starts_with(label));
    for shown in [
        "ATOMICS",
        "CHIEF",
        "Ungrouped · 0",
        "Parse faster",
        "Diff",
        "Board 0/2",
        "AGENTS",
        "PLAN",
        "Approve plan",
        "Settings",
        "Remote",
        "Project",
    ] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    assert!(has("USAGE") && has("Session  76%") && has("Weekly  24%") && has("load 4.2"), "{text:?}");
    for shown in ["All ·", "Repos", "Steer", "Send", "+ New"] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    assert!(has("Close plan…"), "its archive goes through: {text:?}");
    wide(&remote);
    let text = frame_text(&remote);
    let has = |label: &str| text.iter().any(|t| t.starts_with(label));
    for shown in ["Steer", "Send", "+ New"] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }

    remote.state.borrow_mut().trace = fake_host::trace_data();
    remote.set_chief_window(ChiefWindow { open: true, dock: ChiefDock::Docked, width: None }, true);
    assert!(frame_text(&remote).iter().any(|text| text.starts_with("TRACE")));

    let viewer = connected(Role::Viewer);
    viewer.set_view(ViewHints { diff_open: true, ..default_hints() });
    let text = frame_text(&viewer);
    assert!(text.iter().any(|t| t == "PLAN"), "a viewer sees the board: {text:?}");
    for action in ["Approve plan", "Allow", "×", "Close plan…", "Dispatch", "Send", "+ New", "Start", "Accept"] {
        assert!(!text.iter().any(|t| t.starts_with(action)), "{action}: a viewer cannot change it: {text:?}");
    }
    assert!(!text.iter().any(|t| t.starts_with("Steer")), "{text:?}");
}

/// What a browser ends on after signing in, with a code a notice must never repeat.
const PASTE: &str = "http://127.0.0.1:53682/callback?code=c0de-from-the-browser&state=s1";

/// The host's own state changes, as its `host_upsert` says.
fn host_upsert(remote: &RemoteBackend, cursor: u64, host: HostInfo) {
    remote.receive(Envelope::host(EPOCH, Message::HostUpsert { cursor: Seq(cursor), host }));
}

#[test]
fn the_hosts_own_state_is_the_sign_in_and_the_project_menu() {
    let remote = connected(Role::Controller);
    assert_eq!(remote.sign_in(), None, "a host that says nothing of a sign-in");
    assert_eq!(remote.project_menu(), ProjectMenu::default());
    let host = fake_host::tickets(SignInState::Expired).board.host;
    host_upsert(&remote, 4, host.clone());
    assert_eq!(remote.with_mirror(|m| m.cursor), 4);
    // A page never has the host open a browser: its link opens here, and how long it waits is the host's.
    let view = remote.sign_in().expect("the sign-in");
    assert_eq!(view, SignInView { info: host.sign_in.clone().unwrap(), opens_browser: false, seconds_left: None });
    let menu = remote.project_menu();
    let expected = ProjectMenu {
        recent: Vec::new(),
        workspaces: vec!["daily".into(), "team".into()],
        current: Some("team".into()),
        can_save: false,
    };
    assert_eq!(menu, expected, "the host's workspaces, none of its folders, and no saving");
    // Offline it reads as the host last said.
    remote.disconnected();
    assert_eq!(remote.sign_in(), Some(view));
}

#[test]
fn host_commands_go_where_welcome_grants_host_and_no_other_project_command_does() {
    assert!(CAPABILITIES.contains(&wire::HOST), "the browser asks for the host's own state");
    let host_commands = [
        Command::SignInTickets,
        Command::PasteSignIn(String::new()),
        Command::CancelSignIn,
        Command::OpenWorkspace(String::new()),
    ];
    let controller = connected(Role::Controller);
    let viewer = connected(Role::Viewer);
    let offline = connected(Role::Controller);
    offline.disconnected();
    // A host from before `host`: its welcome lacks it, so it grants no `host_command`, and the rest as before.
    let older = RemoteBackend::new("op");
    let capabilities: Vec<String> = CAPABILITIES.iter().filter(|c| **c != wire::HOST).map(|c| c.to_string()).collect();
    let may = Role::Controller.may(&capabilities);
    let resume = Resume::Reset { cursor: Seq(3), reason: ResetReason::NewClient };
    let role = Role::Controller;
    older.receive(Envelope::host(
        EPOCH,
        Message::Welcome { version: VERSION, capabilities, limits: Limits::default(), role, resume, may },
    ));
    older.receive(snapshot());
    assert!(older.may(Grant::Approve) && !older.may(Grant::Host));
    for command in &host_commands {
        assert!(controller.available(command), "a controller may {command:?}");
        assert!(!viewer.available(command), "a viewer may not {command:?}");
        assert!(!offline.available(command), "nothing goes offline: {command:?}");
        assert!(!older.available(command), "an older host has no {command:?}");
    }
    // A host predating shared management cannot offer the new project operations.
    for command in [
        Command::PickProject,
        Command::SaveWorkspace("team".into()),
        Command::OpenProject("/w/repo".into()),
        Command::CloseProject("/w/repo".into()),
    ] {
        assert!(!controller.available(&command), "{command:?}");
    }

    controller.take_outgoing();
    for (command, action) in [
        (Command::SignInTickets, HostAction::SignIn),
        (Command::PasteSignIn(PASTE.into()), HostAction::SignInPaste { text: PASTE.into() }),
        (Command::CancelSignIn, HostAction::SignInCancel),
        (Command::OpenWorkspace("daily".into()), HostAction::OpenWorkspace { name: "daily".into() }),
    ] {
        controller.send(command.clone());
        let [Message::HostCommand(sent)] = one(&controller) else { panic!("{command:?}") };
        assert_eq!(sent.action, action, "{command:?}");
    }
}

#[test]
fn a_paste_that_does_not_go_through_says_why_and_never_repeats_the_address() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::PasteSignIn(PASTE.into()));
    let [Message::HostCommand(sent)] = one(&remote) else { panic!("a host command") };
    let refused = Outcome::refused(WireError::new(ErrorCode::Rejected, "that address is from another sign-in"));
    result(&remote, &sent.op, refused);
    assert_eq!(remote.notice().as_deref(), Some("The host refused: that address is from another sign-in"));
    assert!(remote.state.borrow().pending.is_empty(), "nothing keeps it once the host has answered");

    let mut said = Vec::new();
    remote.dismiss_notice();
    remote.send(Command::PasteSignIn(PASTE.into()));
    result(&remote, "op-2", Outcome::Uncertain { message: "the host restarted".into() });
    said.push(remote.notice().unwrap());
    assert!(said[0].contains("could not confirm"), "{said:?}");
    // Over the host's limit: not sent, and nothing waits.
    remote.dismiss_notice();
    remote.take_outgoing();
    remote.send(Command::PasteSignIn(format!("{PASTE}{}", "x".repeat(ops::MAX_TEXT_BYTES))));
    assert!(remote.take_outgoing().is_empty() && remote.state.borrow().pending.is_empty());
    said.push(remote.notice().unwrap());
    assert!(said[1].starts_with("That was not sent: text is over"), "{said:?}");

    // A viewer and a page offline send nothing and say why.
    let viewer = connected(Role::Viewer);
    viewer.take_outgoing();
    viewer.send(Command::PasteSignIn(PASTE.into()));
    assert!(viewer.take_outgoing().is_empty());
    said.push(viewer.notice().unwrap());
    assert!(said[2].contains("may only view"), "{said:?}");
    let offline = connected(Role::Controller);
    offline.disconnected();
    offline.send(Command::PasteSignIn(PASTE.into()));
    said.push(offline.notice().unwrap());
    assert!(said[3].contains("Not connected"), "{said:?}");
    offline.receive(welcome(Role::Controller));
    assert!(offline.take_outgoing().iter().all(|e| !matches!(e.message, Message::HostCommand(_))), "nor later");
    assert!(said.iter().all(|notice| !notice.contains("c0de") && !notice.contains("127.0.0.1")), "{said:?}");
}

#[test]
fn an_empty_host_offers_its_configured_agents_and_open_project_folders() {
    let remote = RemoteBackend::new("op");
    remote.receive(welcome(Role::Controller));
    let mut state = snapshot();
    let Message::StateSnapshot { sessions, plans, host, .. } = &mut state.message else { panic!() };
    sessions.clear();
    plans.clear();
    *host = fake_host::management().board.host;
    let info = host.management.as_mut().unwrap();
    info.config["values"]["agents"] = serde_json::json!({"value":[{"name":"codex"}],"source":"main"});
    host.projects.push(atomics_core::projection::ProjectInfo { root: "/home/user/repo".into(), worktrees: Vec::new() });
    remote.receive(state);
    wide(&remote);
    assert_eq!(remote.ui_config().agents, ["codex"]);
    assert!(remote.folders().contains(&PathBuf::from("/home/user/repo")));
    assert!(remote.available(&Command::NewSession));
}

#[test]
fn trace_pages_replace_atomically_follow_live_revisions_and_resubscribe() {
    use atomics_core::trace::TraceState;
    let remote = connected(Role::Viewer);
    let data = fake_host::trace_data();
    let deliver = |revision, offset, limit, request: &Envelope, source: &atomics_core::trace_feed::TraceData| {
        let page = source.page(revision, offset, limit, 64000).unwrap();
        remote.receive(Envelope::host(EPOCH, Message::TracePage(page)).reply_to(request.request_id.clone()));
    };
    remote.take_outgoing();
    remote.receive(Envelope::host(EPOCH, Message::TraceChanged { revision: Seq(1) }));
    let first = remote.take_outgoing().pop().unwrap();
    deliver(1, 0, 1, &first, &data);
    assert!(remote.trace().entries.is_empty(), "partial pages never replace the visible lane");
    let second = remote.take_outgoing().pop().unwrap();
    deliver(1, 1, 1, &second, &data);
    assert_eq!(remote.trace(), data);
    let mut completed = data.clone();
    completed.entries[1].state = TraceState::Ok;
    remote.receive(Envelope::host(EPOCH, Message::TraceChanged { revision: Seq(2) }));
    let second_revision = remote.take_outgoing().pop().unwrap();
    remote.receive(Envelope::host(EPOCH, Message::TraceChanged { revision: Seq(3) }));
    assert!(remote.take_outgoing().is_empty(), "finish the pinned revision before following a newer one");
    deliver(1, 1, 1, &second, &data);
    assert_eq!(remote.trace(), data, "old page replies cannot change the lane");
    deliver(2, 0, 1, &second_revision, &completed);
    let next_page = remote.take_outgoing().pop().unwrap();
    assert!(matches!(next_page.message, Message::TraceGet { revision: Seq(2), offset: 1, .. }));
    deliver(2, 1, 1, &next_page, &completed);
    assert_eq!(remote.trace(), completed, "busy streams still make visible progress");
    let current = remote.take_outgoing().pop().unwrap();
    assert!(matches!(current.message, Message::TraceGet { revision: Seq(3), offset: 0, .. }));
    deliver(3, 0, 32, &current, &completed);
    assert_eq!(remote.trace(), completed);
    remote.disconnected();
    remote.receive(welcome(Role::Viewer));
    assert!(
        remote
            .take_outgoing()
            .iter()
            .any(|e| matches!(&e.message, Message::Subscribe { topics } if topics.contains(&wire::Topic::Trace)))
    );
    remote.receive(Envelope::host(EPOCH, Message::TraceChanged { revision: Seq(0) }));
    let current = remote.take_outgoing().pop().unwrap();
    deliver(0, 0, 32, &current, &Default::default());
    assert!(remote.trace().entries.is_empty(), "a restarted host clears the former chief's trace");
}

#[test]
fn resource_snapshots_and_upserts_are_shared_by_controllers_and_viewers() {
    for role in [Role::Controller, Role::Viewer] {
        let remote = connected(role);
        assert_eq!(remote.resources(), Some(fake_host::resources()));
        let mut host = fake_host::board().host;
        host.resources.as_mut().unwrap().load = Some(6.1);
        host_upsert(&remote, 4, host.clone());
        assert_eq!(remote.resources(), host.resources);
        host.resources = None;
        host_upsert(&remote, 5, host);
        assert_eq!(remote.resources(), None);
    }
}

#[test]
fn panel_commands_need_the_capability_and_a_control_grant() {
    let edit = Command::Edit { root: WORKTREE.into(), path: Some("src/a.rs".into()), line: Some(12) };
    let commands =
        [edit, Command::EditorHints(WORKTREE.into()), Command::LoadPullRequest("pull:example/repo#7".into())];
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    for command in &commands {
        assert!(remote.available(command));
        remote.send(command.clone());
    }
    let sent = remote.take_outgoing();
    assert!(
        matches!(&sent[0].message, Message::HostCommand(c) if matches!(&c.action, HostAction::Edit { path, line: Some(12), .. } if path.as_deref() == Some("src/a.rs")))
    );
    assert!(matches!(&sent[1].message, Message::HostCommand(c) if matches!(c.action, HostAction::EditorHints { .. })));
    assert!(
        matches!(&sent[2].message, Message::HostCommand(c) if matches!(c.action, HostAction::PullRequestDiff { .. }))
    );
    for remote in [connected(Role::Viewer), RemoteBackend::new("offline")] {
        for command in &commands {
            assert!(!remote.available(command));
        }
    }
    let old_host = connected(Role::Controller);
    let mut old = welcome(Role::Controller);
    if let Message::Welcome { capabilities, .. } = &mut old.message {
        capabilities.retain(|c| c != wire::PANELS);
    }
    old_host.receive(old);
    for command in &commands {
        assert!(!old_host.available(command), "{command:?} on an older host");
    }
}

#[test]
fn tickets_use_the_host_grant_and_a_refusal_ends_the_clients_request() {
    use atomics_core::ticket_ui::TicketAction;
    let remote = connected(Role::Controller);
    let mut host = HostInfo::default();
    host.tickets = fake_host::ticket_reader().board.host.tickets;
    remote.receive(Envelope::host(EPOCH, Message::HostUpsert { cursor: Seq(4), host: host.clone() }));
    assert!(remote.available(&Command::OpenTicket(Default::default())));
    assert!(remote.available(&Command::SetPanel(PanelMode::Tickets)));
    let action =
        TicketAction::Search { query: "parser".into(), keyword: false, view: 0, filters: vec![], cursor: None };
    let command = Command::Ticket { request: "page-request".into(), action: action.clone() };
    assert!(remote.available(&command));
    remote.send(command);
    let sent = remote
        .take_outgoing()
        .into_iter()
        .find_map(|envelope| match envelope.message {
            Message::HostCommand(command) => Some(command),
            _ => None,
        })
        .unwrap();
    assert_eq!(sent.action, HostAction::Tickets { request: "page-request".into(), ticket: Box::new(action) });
    remote.receive(Envelope::host(
        EPOCH,
        Message::CommandResult(CommandResult {
            op: sent.op,
            outcome: Outcome::refused(WireError::new(ErrorCode::Rejected, "ticket server unavailable")),
        }),
    ));
    let reply = remote.tickets().unwrap().replies.pop().unwrap();
    assert_eq!(reply.request, "page-request");
    assert!(reply.error.unwrap().contains("ticket server unavailable"));

    let viewer = connected(Role::Viewer);
    viewer.receive(Envelope::host(EPOCH, Message::HostUpsert { cursor: Seq(4), host }));
    assert!(!viewer.available(&Command::Ticket {
        request: "read".into(),
        action: TicketAction::Read { ticket: Default::default() }
    }));
    let old = connected(Role::Controller);
    assert!(!old.available(&Command::OpenTicket(Default::default())));
    assert!(!old.available(&Command::SetPanel(PanelMode::Tickets)));
}

#[test]
fn preview_actions_need_native_negotiation_and_never_replay() {
    let remote = connected(Role::Controller);
    assert!(
        matches!(remote.hello("browser").message, Message::Hello { capabilities, .. } if !capabilities.iter().any(|c| c == wire::TUNNEL))
    );
    let preview = atomics_core::preview::Preview { port: 5173, path: "/demo".into(), label: "Web".into() };
    let open = Command::OpenPreview { session: Some("s1".into()), preview };
    remote.send(open.clone());
    assert!(remote.take_preview_actions().is_empty());
    remote.enable_native_previews();
    assert!(
        matches!(remote.hello("native").message, Message::Hello { capabilities, .. } if capabilities.iter().any(|c| c == wire::TUNNEL))
    );
    assert!(!remote.can_preview(), "older host did not negotiate it");
    let mut hello = welcome(Role::Controller);
    if let Message::Welcome { capabilities, .. } = &mut hello.message {
        capabilities.push(wire::TUNNEL.into());
    }
    remote.receive(hello);
    assert!(remote.can_preview());
    remote.send(open.clone());
    assert_eq!(remote.take_preview_actions(), vec![open.clone()]);
    remote.send(open);
    remote.set_active_previews(vec![5173]);
    assert!(remote.preview_active(5173));
    remote.disconnected();
    assert!(!remote.preview_active(5173));
    assert!(remote.take_preview_actions().is_empty());
    assert!(remote.take_tunnel_messages().is_empty());
}

#[test]
fn machine_follows_the_open_panel_and_resubscribes_after_reconnect() {
    let remote = connected(Role::Controller);
    assert!(remote.machine_available());
    assert!(remote.machine().is_none());
    let snapshot = atomics_core::machine::Machine { sampled_at: 1, cores: 4, ..Default::default() };
    remote.receive(Envelope::host(EPOCH, Message::Machine { snapshot: snapshot.clone() }));
    assert!(remote.machine().is_none(), "ignore unsolicited data");
    remote.watch_machine(true);
    remote.watch_machine(true);
    assert_eq!(
        remote
            .take_outgoing()
            .iter()
            .filter(|e| matches!(&e.message, Message::Subscribe { topics } if topics == &[wire::Topic::Machine]))
            .count(),
        1
    );
    remote.receive(Envelope::host(EPOCH, Message::Machine { snapshot: snapshot.clone() }));
    assert_eq!(remote.machine(), Some(snapshot));
    remote.disconnected();
    assert!(remote.machine().is_none());
    remote.receive(welcome(Role::Controller));
    assert!(
        remote
            .take_outgoing()
            .iter()
            .any(|e| matches!(&e.message, Message::Subscribe { topics } if topics == &[wire::Topic::Machine]))
    );
    remote.watch_machine(false);
    assert!(
        remote
            .take_outgoing()
            .iter()
            .any(|e| matches!(&e.message, Message::Unsubscribe { topics } if topics == &[wire::Topic::Machine]))
    );
    let viewer = connected(Role::Viewer);
    viewer.watch_machine(true);
    assert!(!viewer.machine_available());
    assert!(viewer.take_outgoing().is_empty());
}

/// The worktree's diff moves on to `revision`, with `files` in it.
fn new_revision(remote: &RemoteBackend, cursor: u64, revision: u64, files: &[&str]) {
    let mut summary = fake_host::board().diffs.remove(0);
    let file = summary.files[0].clone();
    summary.revision = revision;
    summary.files = files
        .iter()
        .map(|path| DiffFileSummary {
            old_path: Some(path.to_string()),
            new_path: Some(path.to_string()),
            ..file.clone()
        })
        .collect();
    remote.receive(Envelope::host(EPOCH, Message::DiffSummary { cursor: Seq(cursor), summary }));
}

/// The diff requests `remote` has for the host: each one's request, path, revision and offset.
fn diff_asks(remote: &RemoteBackend) -> Vec<(Option<String>, String, u64, Option<u32>)> {
    let sent = remote.take_outgoing();
    sent.into_iter()
        .filter_map(|envelope| match envelope.message {
            Message::DiffGet { path, revision, offset, .. } => {
                Some((envelope.request_id, path, revision.unwrap().0, offset))
            }
            _ => None,
        })
        .collect()
}

/// The host answers a request with `lines` lines of `path`, with more to come from `next_offset`.
fn answer(
    remote: &RemoteBackend,
    request: &Option<String>,
    path: &str,
    revision: u64,
    lines: u32,
    next_offset: Option<u32>,
) {
    let body = DiffFileBody { worktree: WORKTREE.into(), revision, file: fake_host::file_diff(path, lines) };
    let message = Message::DiffFile { body, truncated: next_offset.is_some(), next_offset };
    remote.receive(Envelope::host(EPOCH, message).reply_to(request.clone()));
}

/// The lines the diff shows of `path`, and whether it shows that file at all.
fn lines_shown(remote: &RemoteBackend, path: &str) -> Option<usize> {
    let snapshot = remote.diff(Path::new(WORKTREE)).expect("the diff");
    let prepared = snapshot.prepared.expect("the diff's rows");
    let file = prepared.diff.files.iter().find(|file| file.path() == path)?;
    Some(file.hunks.iter().map(|hunk| hunk.lines.len()).sum())
}

/// The fake board's worktree with its files read: a.rs of 6 lines and a context line, b.rs of 40.
fn diff_read() -> RemoteBackend {
    let remote = connected(Role::Controller);
    remote.want_diff(Path::new(WORKTREE));
    for (request, path, revision, _) in diff_asks(&remote) {
        answer(&remote, &request, &path, revision, if path == "src/a.rs" { 6 } else { 40 }, None);
    }
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7));
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(41));
    remote.take_outgoing();
    remote
}

#[test]
fn a_new_revision_keeps_each_file_on_screen_until_its_new_body_is_whole() {
    let remote = diff_read();
    new_revision(&remote, 4, 5, &["src/a.rs", "src/b.rs"]);
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7), "the diff does not empty");
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(41));
    let asks = diff_asks(&remote);
    assert_eq!(
        asks.iter().map(|(_, path, revision, _)| (path.as_str(), *revision)).collect::<Vec<_>>(),
        [("src/a.rs", 5), ("src/b.rs", 5)]
    );

    // One file swaps, the other still shows what it did.
    answer(&remote, &asks[0].0, "src/a.rs", 5, 9, None);
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(10));
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(41));

    // Page by page, the old body stays whole until the last page is in.
    answer(&remote, &asks[1].0, "src/b.rs", 5, 3, Some(4));
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(41), "not the first page as if it were the file");
    let next = diff_asks(&remote);
    assert_eq!(next.len(), 1);
    assert_eq!((next[0].1.as_str(), next[0].3), ("src/b.rs", Some(4)));
    answer(&remote, &next[0].0, "src/b.rs", 5, 3, None);
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(8), "both pages, one hunk");
}

#[test]
fn a_new_revision_drops_files_it_no_longer_lists() {
    let remote = diff_read();
    new_revision(&remote, 4, 5, &["src/a.rs"]);
    assert_eq!(lines_shown(&remote, "src/b.rs"), None);
    let asks = diff_asks(&remote);
    assert_eq!(asks.len(), 1);
    answer(&remote, &asks[0].0, "src/a.rs", 5, 6, None);
    new_revision(&remote, 5, 6, &["src/a.rs", "src/b.rs"]);
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(0), "back, with nothing old to show");
}

#[test]
fn a_refresh_reads_a_few_files_at_a_time_the_empty_ones_first() {
    let remote = diff_read();
    let many: Vec<String> = (0..MAX_IN_FLIGHT + 2).map(|i| format!("src/n{i}.rs")).collect();
    let mut files = vec!["src/a.rs", "src/b.rs"];
    files.extend(many.iter().map(String::as_str));
    new_revision(&remote, 4, 5, &files);
    let asks = diff_asks(&remote);
    assert_eq!(asks.len(), MAX_IN_FLIGHT);
    assert!(asks.iter().all(|(_, path, _, _)| path.starts_with("src/n")), "{asks:?}");
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7), "an old body waits its turn on screen");
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(41));
}

#[test]
fn a_body_that_did_not_change_leaves_the_diff_as_it_is() {
    let remote = diff_read();
    new_revision(&remote, 4, 5, &["src/a.rs", "src/b.rs"]);
    let asks = diff_asks(&remote);
    let before = remote.diff(Path::new(WORKTREE)).unwrap().prepared.unwrap();
    answer(&remote, &asks[0].0, "src/a.rs", 5, 6, None);
    answer(&remote, &asks[1].0, "src/b.rs", 5, 40, None);
    let after = remote.diff(Path::new(WORKTREE)).unwrap();
    assert!(!after.loading);
    assert!(Arc::ptr_eq(&before, &after.prepared.unwrap()), "nothing to redraw");
}

#[test]
fn a_refresh_that_fails_keeps_the_body_and_says_so() {
    let remote = diff_read();
    remote.send(Command::RefreshDiff(WORKTREE.into()));
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7), "the refresh button does not empty the diff");
    let asks = diff_asks(&remote);
    let refusal = WireError::new(ErrorCode::Internal, "the disk is gone").into_envelope(asks[0].0.clone(), None);
    remote.receive(refusal);
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7));
    assert!(remote.diff(Path::new(WORKTREE)).unwrap().error.is_some());
}

#[test]
fn answers_to_an_earlier_read_do_not_touch_the_new_one() {
    let remote = diff_read();
    new_revision(&remote, 4, 5, &["src/a.rs", "src/b.rs"]);
    let old = diff_asks(&remote);
    new_revision(&remote, 5, 6, &["src/a.rs", "src/b.rs"]);
    let new = diff_asks(&remote);
    assert_eq!(new.len(), 2);

    // The host refuses the old revision's request: the new read goes on, and nothing is asked twice.
    let stale = WireError::new(ErrorCode::StaleRevision, "the diff moved on").into_envelope(old[1].0.clone(), None);
    remote.receive(stale);
    assert!(diff_asks(&remote).is_empty(), "b.rs is already being read");
    answer(&remote, &new[1].0, "src/b.rs", 6, 3, Some(4));
    let next = diff_asks(&remote);
    answer(&remote, &next[0].0, "src/b.rs", 6, 3, None);
    assert_eq!(lines_shown(&remote, "src/b.rs"), Some(8));

    // Refresh forgets what was in flight: a late page of the read it replaced is not one of the new read's.
    remote.send(Command::RefreshDiff(WORKTREE.into()));
    let refreshed = diff_asks(&remote);
    answer(&remote, &new[0].0, "src/a.rs", 6, 2, None);
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(7), "the late answer is dropped");
    answer(&remote, &refreshed[0].0, "src/a.rs", 6, 9, None);
    assert_eq!(lines_shown(&remote, "src/a.rs"), Some(10));
}
