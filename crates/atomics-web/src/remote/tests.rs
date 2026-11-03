//! The remote Backend's reads and writes, fed frames directly.

use atomics_core::approvals::Decision;
use atomics_core::ops::{CommandResult, Outcome};
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
        ("Fix the parser", Some("atomics/t1"), false)
    );
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
fn refresh_fetches_again_and_other_commands_are_declined() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::RefreshDiff(WORKTREE.into()));
    assert_eq!(remote.take_outgoing().len(), 2, "refresh reads the files again");
    remote.send(Command::IntegrateAccepted(0));
    assert!(remote.take_outgoing().is_empty());
    assert!(remote.notice().unwrap().contains("not available"));
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

#[test]
fn only_what_the_browser_can_send_is_available() {
    let decide = Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow };
    let offline = RemoteBackend::new("op");
    assert!(!offline.available(&Command::ApprovePlan(0)), "offline nothing can be sent");
    assert!(offline.available(&Command::ShowPlan(0)), "the frame's own commands stay");

    let controller = connected(Role::Controller);
    for command in [decide.clone(), Command::ApprovePlan(0), Command::RefreshDiff(WORKTREE.into())] {
        assert!(controller.available(&command), "{command:?}");
    }
    for command in [
        Command::Activate { session: "s1".into(), open_diff: false },
        Command::ToggleDiff,
        Command::ShowUngrouped,
        Command::Toast("hi".into()),
    ] {
        assert!(controller.available(&command), "the frame carries out {command:?}");
    }
    for command in [
        Command::DispatchReady(0),
        Command::IntegrateAccepted(0),
        Command::ArchiveFinished(0),
        Command::ClosePlan { plan: 0, status: PlanStatus::Completed },
        Command::SendReview(WORKTREE.into()),
        Command::Steer { sessions: vec!["s1".into()], text: "go on".into() },
        Command::CloseSession("s1".into()),
        Command::Start { agent: "claude".into(), folder: None },
        Command::NewSession,
        Command::PickProject,
        Command::SetView(None),
        Command::Rename("s1".into()),
    ] {
        assert!(!controller.available(&command), "{command:?}");
    }
    // The chief's window is the page's, in the wide layout only.
    for command in [Command::ToggleChief, Command::HideChief] {
        assert!(!controller.available(&command), "{command:?} on a phone");
        let window =
            crate::desk::ChiefWindow { open: true, dock: atomics_core::config::ChiefDock::Docked, width: None };
        controller.set_chief_window(window, true);
        assert!(controller.available(&command), "{command:?} in the wide layout");
        controller.set_chief_window(window, false);
    }

    let viewer = connected(Role::Viewer);
    assert!(!viewer.available(&decide), "a viewer may not decide");
    assert!(!viewer.available(&Command::ApprovePlan(0)));
    assert!(viewer.available(&Command::ShowPlan(0)));
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
    for unavailable in ["+ New", "All ·", "Repos", "Close plan…", "Steer", "Send", "EXPLORER"] {
        assert!(!has(unavailable), "{unavailable} cannot be sent from the browser: {text:?}");
    }

    let viewer = connected(Role::Viewer);
    viewer.set_view(ViewHints { diff_open: true, ..default_hints() });
    let text = frame_text(&viewer);
    assert!(text.iter().any(|t| t == "PLAN"), "a viewer sees the board: {text:?}");
    assert!(!text.iter().any(|t| t == "Approve plan" || t == "Allow"), "but cannot change it: {text:?}");
}
