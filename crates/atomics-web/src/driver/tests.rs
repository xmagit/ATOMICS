//! The frame's whole life against a fake host: boot, pairing, the board, commands, reconnecting and the
//! ways it stops. The "network" is synchronous: a message sent is received before the next is sent.

use atomics_core::approvals::Decision;
use atomics_core::noise::{Access, Secret};
use atomics_core::session::AgentStatus;
use atomics_core::wire::{Message, Seq};
use atomics_ui::{Backend, Command};

use super::*;
use crate::fake_host::{self, HOST_ID, WORKTREE};
use crate::keep::Failure;
use crate::world::{BROKER, PARENT, World};

#[test]
fn pairing_shows_the_hosts_code_then_opens_the_board() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    assert_eq!(world.posted, [boot::READY], "the frame asks for its boot at once");
    assert_eq!(world.driver.status(), &Status::Waiting);

    world.boot(Some(&pairing));
    assert_eq!(world.posted, [boot::READY, boot::BOOTED]);
    let Status::Pairing { code: Some(code) } = world.driver.status().clone() else {
        panic!("the code shows: {:?}", world.driver.status())
    };
    assert_eq!(Some(&code), world.host.code.as_ref(), "both screens show the same six digits");
    assert_eq!(world.host.named.as_deref(), Some("Safari on iPhone"), "the host's prompt names the browser");
    assert!(world.store.is_empty(), "nothing is kept before the host accepts the device");
    assert!(world.driver.remote.plan_list().is_empty());

    let answer = world.host.confirm();
    world.deliver(answer);
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.posted.last(), Some(&boot::PAIRED));
    let device = world.store.get(HOST_ID).expect("the device key is kept");
    assert_eq!(device.host_key, world.host.public());
    assert_eq!(world.host.allowed.iter().map(|(key, _)| *key).collect::<Vec<_>>(), [device.secret.public()]);

    // The channel goes on as the session: hello, then the board.
    let Message::Hello { client_id, capabilities, prior_epoch, .. } = &world.sent()[0] else { panic!("hello first") };
    assert_eq!((client_id.as_str(), prior_epoch), ("atomics-web", &None));
    for wanted in ["mutations", "terminals", "input", "host"] {
        assert!(capabilities.iter().any(|c| c == wanted), "{wanted}: {capabilities:?}");
    }
    let plans = world.driver.remote.plan_list();
    assert_eq!(plans.iter().map(|p| p.goal.as_str()).collect::<Vec<_>>(), ["Parse faster"]);
    let welcome = world.driver.remote.welcome().expect("welcomed");
    assert!(welcome.may.contains(&atomics_core::ops::Grant::Approve), "a controller may approve");
    assert!(welcome.may.contains(&atomics_core::ops::Grant::Host), "and sign the host in to its tickets");
}

#[test]
fn the_ticket_travels_only_in_the_subprotocol() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.dials, [vec!["atomics.rendezvous.v1".to_string(), "atomics.ticket.ticket-1".to_string()]]);
    assert_eq!(world.driver.status(), &Status::Live);
    let shown = format!(
        "{:?}",
        Output::Dial { socket: 1, url: BROKER.into(), protocols: vec![Zeroizing::new("atomics.ticket.secret".into())] }
    );
    assert!(!shown.contains("secret"), "{shown}");
    assert!(!format!("{:?}", world.driver.boot()).contains("ticket-1"));
}

#[test]
fn messages_from_elsewhere_are_refused_unread() {
    let mut world = World::paired(fake_host::host());
    let data = world.boot_data(None);
    assert_eq!(world.driver.message("https://evil.example", true, &data), Err(Refused::Origin));
    assert_eq!(world.driver.message(PARENT, false, &data), Err(Refused::Window));
    world.pump();
    assert_eq!(world.driver.status(), &Status::Waiting);
    assert!(world.dials.is_empty());
    // With no parent origin configured, nothing boots the frame and it says nothing.
    let mut orphan = Driver::new(None, "op", "x");
    assert!(orphan.take_outputs().is_empty());
    assert_eq!(orphan.message(PARENT, true, &data), Err(Refused::Origin));
    assert_eq!(orphan.status(), &Status::Stopped(Stop::NoParent));
}

#[test]
fn a_paired_browser_reconnects_with_a_fresh_ticket_and_resumes() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let cursor = world.driver.remote.with_mirror(|m| m.cursor);

    world.drop_socket(close::ABNORMAL);
    assert!(world.driver.remote.welcome().is_none(), "offline: nothing may be sent");
    assert_eq!(world.driver.remote.plan_list().len(), 1, "the board stays on screen while away");
    // The host goes on meanwhile.
    let mut session = fake_host::board().sessions[1].clone();
    session.status = AgentStatus::Working;
    world.host.event(|cursor| Message::SessionUpsert { cursor, session });
    world.host.received.clear();

    // The retry asks the hosting page for a fresh ticket; nothing dials without one.
    world.advance(world.driver.wake_in().expect("a retry is due"));
    assert_eq!(world.posted.last(), Some(&boot::WANTS_TICKET));
    assert_eq!(world.dials.len(), 1);
    world.ticket("ticket-2");
    assert_eq!(world.dials.len(), 2);
    assert_eq!(world.dials[1][1], "atomics.ticket.ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    let Message::Hello { prior_epoch, prior_cursor, .. } = &world.sent()[0] else { panic!("hello") };
    assert_eq!((prior_epoch.as_deref(), *prior_cursor), (Some("epoch-1"), Some(Seq(cursor))), "it resumes");
    assert!(
        !world.sent().iter().any(|m| matches!(m, Message::Resync { .. })),
        "the replay follows on: no snapshot needed"
    );
    let s1 = world.driver.remote.session_list().into_iter().find(|s| s.id == "s1").unwrap();
    assert_eq!(s1.status, AgentStatus::Working, "what happened while away was replayed");
}

#[test]
fn a_host_that_restarted_sends_a_snapshot() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::GOING_AWAY);
    world.host.epoch = "epoch-2".into();
    world.host.board.plans[0].goal = "Parse much faster".into();
    world.advance(world.driver.wake_in().unwrap());
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.driver.remote.with_mirror(|m| m.epoch.clone()).as_deref(), Some("epoch-2"));
    assert_eq!(world.driver.remote.plan_list()[0].goal, "Parse much faster");
}

#[test]
fn a_steady_channel_starts_the_waits_over_and_quota_waits_longer() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::ABNORMAL);
    let first = world.driver.wake_in().unwrap();
    assert!((FIRST_RETRY_MS / 2..=FIRST_RETRY_MS).contains(&first), "{first}");
    world.advance(first);
    world.ticket("ticket-2");
    world.drop_socket(close::ABNORMAL);
    let second = world.driver.wake_in().unwrap();
    assert!((FIRST_RETRY_MS..=2 * FIRST_RETRY_MS).contains(&second), "the wait doubles: {second}");
    world.advance(second);
    world.ticket("ticket-3");
    world.advance(STEADY_MS);
    world.drop_socket(close::SLOW);
    assert!(world.driver.wake_in().unwrap() <= FIRST_RETRY_MS, "after a steady channel the waits start over");
    world.advance(world.driver.wake_in().unwrap());
    world.ticket("ticket-4");
    world.drop_socket(close::QUOTA);
    assert!(world.driver.wake_in().unwrap() >= QUOTA_RETRY_MS / 2);
    // An unanswered ticket request is asked again.
    world.advance(world.driver.wake_in().unwrap());
    let asked = world.posted.iter().filter(|p| **p == boot::WANTS_TICKET).count();
    world.advance(TICKET_WAIT_MS);
    assert_eq!(world.posted.iter().filter(|p| **p == boot::WANTS_TICKET).count(), asked + 1);
}

#[test]
fn approving_goes_out_where_the_welcome_grants_it() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let remote = &world.driver.remote;
    let index = remote.session_list().iter().position(|s| s.id == "s1").unwrap();
    let approval = remote.session_approval(index).expect("s1 waits on a1");
    assert_eq!((approval.id.as_str(), approval.request.command.as_deref()), ("a1", Some("cargo test")));

    world.send(Command::Decide { session: index, approval: "a1".into(), decision: Decision::Allow });
    let Some(Message::ApprovalDecide(decide)) = world.sent().last().copied() else { panic!("approval_decide") };
    assert_eq!((decide.approval.as_str(), decide.decision), ("a1", Decision::Allow));
    assert!(decide.op.starts_with("op-"));
    assert!(world.driver.remote.session_approval(index).is_none(), "the host's approval_resolved cleared it");

    world.send(Command::ApprovePlan(0));
    let Some(Message::PlanCommand(command)) = world.sent().last().copied() else { panic!("plan_command") };
    assert_eq!((command.plan.as_deref(), command.digest.as_deref()), (Some("p1"), Some("digest-1")));
    assert!(world.driver.remote.board(0).unwrap().plan.approved);
    assert_eq!(world.driver.remote.notice(), None);
}

#[test]
fn a_view_only_device_sends_no_mutation() {
    let mut host = fake_host::host();
    host.answer = Some(Access::View);
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::Live);
    assert!(world.driver.remote.welcome().unwrap().may.is_empty());
    let index = world.driver.remote.session_list().iter().position(|s| s.id == "s1").unwrap();
    let before = world.host.received.len();
    world.send(Command::Decide { session: index, approval: "a1".into(), decision: Decision::Deny });
    world.send(Command::ApprovePlan(0));
    assert_eq!(world.host.received.len(), before, "nothing reaches the host");
    assert!(world.driver.remote.notice().unwrap().contains("only view"));
    assert_eq!(
        world.driver.remote.take_refused(),
        [("s1".to_string(), "a1".to_string())],
        "the banner may offer it again"
    );
}

#[test]
fn what_atomics_v1_has_no_action_for_says_so() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let before = world.host.received.len();
    for command in [
        Command::ReopenPlan(0),
        Command::ResumeSession("s1".into()),
        Command::VoiceToSession { session: 1, text: "go on".into() },
    ] {
        assert!(!world.driver.remote.available(&command), "the views do not draw {command:?}");
        world.driver.remote.dismiss_notice();
        world.send(command);
        assert!(world.driver.remote.notice().unwrap().contains("not available in the browser"));
    }
    assert_eq!(world.host.received.len(), before);
}

/// The fake host's plan task `id`.
fn task<'a>(world: &'a World, id: &str) -> &'a atomics_core::projection::TaskInfo {
    world.host.board.plans[0].tasks.iter().find(|t| t.id == id).expect("a task")
}

#[test]
fn each_command_changes_the_hosts_board_and_the_board_on_screen_follows() {
    use atomics_core::plan::{PlanStatus, TaskStatus};

    let mut world = World::paired(fake_host::working());
    world.boot(None);
    assert_eq!(world.driver.remote.board(0).unwrap().ready, ["t3"]);
    let quiet = |world: &World| assert_eq!(world.driver.remote.notice(), None);

    world.send(Command::StartTask { plan: 0, task: "t3".into() });
    assert_eq!(task(&world, "t3").status, TaskStatus::Running);
    assert!(world.driver.remote.board(0).unwrap().ready.is_empty(), "the mirror followed");
    quiet(&world);

    world.send(Command::SetTaskStatus { plan: 0, task: "t4".into(), status: TaskStatus::Done });
    assert_eq!(task(&world, "t4").status, TaskStatus::Done, "accepted");
    world.send(Command::SetTaskStatus { plan: 0, task: "t6".into(), status: TaskStatus::Running });
    assert_eq!(task(&world, "t6").status, TaskStatus::Running, "unblocked");

    world.send(Command::IntegrateTask { plan: 0, task: "t4".into() });
    assert_eq!(task(&world, "t4").integrated.as_deref(), Some("atomics/plan-p1"));
    let pending = |world: &World| {
        let plan = world.driver.remote.board(0).unwrap().plan;
        plan.pending_integrations().iter().map(|t| t.id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(pending(&world), ["t5"], "t4 is in; t5 waits");
    world.send(Command::IntegrateAccepted(0));
    assert!(pending(&world).is_empty(), "every accepted task went in");

    let another = atomics_core::projection::TaskInfo {
        id: "t7".into(),
        status: TaskStatus::Planned,
        ..task(&world, "t5").clone()
    };
    world.host.board.plans[0].tasks.push(another);
    let plan = world.host.board.plans[0].clone();
    let frames = world.host.event(|cursor| Message::PlanUpsert { cursor, plan });
    world.deliver(frames);
    world.send(Command::DispatchReady(0));
    assert_eq!(task(&world, "t7").status, TaskStatus::Running);
    quiet(&world);

    world.send(Command::Steer { sessions: vec!["s1".into()], text: "go on".into(), now: false });
    assert_eq!(world.host.steered, [("s1".to_string(), "go on".to_string())]);
    assert!(world.host.terminals["s1"].typed.is_empty(), "a steer is no one's typing");

    world.send(Command::Start { agent: "claude".into(), folder: None });
    let started = world.host.board.sessions.iter().find(|s| s.id.starts_with("new-")).expect("a new session");
    assert_eq!((started.agent.as_str(), started.cwd.as_str()), ("claude", "/scratch/new"));
    let id = started.id.clone();
    assert!(world.driver.remote.session_list().iter().any(|s| s.id == id), "the mirror followed");

    world.send(Command::CloseSession(id.clone()));
    assert!(!world.host.board.sessions.iter().any(|s| s.id == id));
    assert!(!world.driver.remote.session_list().iter().any(|s| s.id == id), "the mirror followed");
    quiet(&world);

    world.send(Command::ClosePlan { plan: 0, status: PlanStatus::Archived });
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Archived);
    assert_eq!(world.driver.remote.plan_row(0).unwrap().status, PlanStatus::Archived);
    quiet(&world);
}

#[test]
fn comments_and_a_review_go_through_the_host_and_their_refusals_say_why() {
    use atomics_core::steer::ReviewComment;

    let mut world = World::paired(fake_host::working());
    world.boot(None);
    let root = std::path::PathBuf::from(WORKTREE);
    let comment = ReviewComment {
        path: "src/a.rs".into(),
        line: Some(2),
        removed: false,
        excerpt: "x".into(),
        body: "why?".into(),
    };
    let comments = |world: &World| world.driver.remote.diff(&root).unwrap().comments.len();
    world.send(Command::EditComments { root: root.clone(), before: Vec::new(), after: vec![comment.clone()] });
    assert_eq!(comments(&world), 1, "on the board the page reads");
    assert_eq!(world.host.board.diffs[0].comments[0].body, "why?");

    world.send(Command::SendReview(root.clone()));
    assert_eq!(world.host.reviews.len(), 1);
    assert_eq!(world.host.reviews[0].1[0].body, "why?");
    assert_eq!(comments(&world), 0, "sent ones leave the diff");
    assert_eq!(world.driver.remote.notice(), None);

    // Nothing left to send: the host says so, and the page does.
    world.send(Command::SendReview(root.clone()));
    assert!(world.driver.remote.notice().unwrap().contains("no pending comments"));
    world.driver.remote.dismiss_notice();

    // The diff moved on under the comment: it is not added, and the page says why.
    world.host.board.diffs[0].revision = 5;
    world.send(Command::EditComments { root: root.clone(), before: Vec::new(), after: vec![comment] });
    assert_eq!(comments(&world), 0);
    assert!(world.driver.remote.notice().unwrap().contains("anchor the comment again"));
}

#[test]
fn a_refusal_changes_nothing_on_the_host_and_says_why() {
    use atomics_core::plan::{PlanStatus, TaskStatus};
    use atomics_core::wire::{ErrorCode, WireError};

    let refusal = || Some(WireError::new(ErrorCode::Rejected, "the merge has conflicts"));
    let mut world = World::paired(fake_host::working());
    world.boot(None);
    for (command, unchanged) in [
        (
            Command::SetTaskStatus { plan: 0, task: "t4".into(), status: TaskStatus::Done },
            (&|w: &World| task(w, "t4").status == TaskStatus::Review) as &dyn Fn(&World) -> bool,
        ),
        (Command::StartTask { plan: 0, task: "t3".into() }, &|w: &World| task(w, "t3").status == TaskStatus::Planned),
        (Command::IntegrateTask { plan: 0, task: "t5".into() }, &|w: &World| task(w, "t5").integrated.is_none()),
        (Command::ArchivePlan(0), &|w: &World| w.host.board.plans[0].status == PlanStatus::Open),
        (Command::CloseSession("s1".into()), &|w: &World| w.host.board.sessions.iter().any(|s| s.id == "s1")),
        (Command::Steer { sessions: vec!["s1".into()], text: "go".into(), now: false }, &|w: &World| {
            w.host.steered.is_empty()
        }),
        (Command::SendReview(WORKTREE.into()), &|w: &World| w.host.reviews.is_empty()),
    ] {
        world.driver.remote.dismiss_notice();
        world.host.refuse_next = refusal();
        world.send(command.clone());
        assert!(unchanged(&world), "{command:?}");
        assert_eq!(
            world.driver.remote.notice().as_deref(),
            Some("The host refused: the merge has conflicts"),
            "{command:?}"
        );
        assert!(world.driver.remote.board(0).unwrap().integrating.is_empty(), "{command:?}");
    }
}

#[test]
fn diffs_are_fetched_page_by_page_while_on_screen() {
    let mut host = fake_host::host();
    host.page_budget = 1024;
    let mut world = World::paired(host);
    world.boot(None);
    let root = std::path::Path::new(WORKTREE);
    let before = world.driver.remote.diff(root).unwrap();
    assert_eq!(before.revision, 4);
    assert!(!before.loading, "nothing is fetched until the diff is on screen");
    let files = &before.prepared.as_ref().unwrap().diff.files;
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.hunks.is_empty()));

    world.driver.remote.want_diff(root);
    world.driver.flush();
    world.pump();
    let gets = world.sent().iter().filter(|m| matches!(m, Message::DiffGet { .. })).count();
    assert!(gets > 2, "the long file came in several pages ({gets} requests)");
    let after = world.driver.remote.diff(root).unwrap();
    assert!(!after.loading && after.error.is_none());
    let prepared = after.prepared.unwrap();
    let expected = [fake_host::file_diff("src/a.rs", 6), fake_host::file_diff("src/b.rs", 40)];
    for (got, want) in prepared.diff.files.iter().zip(&expected) {
        let lines = |f: &atomics_core::diff::FileDiff| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
        assert_eq!(lines(got), lines(want), "{}: every line arrived", want.path());
        assert_eq!(got.hunks.len(), 1, "a hunk cut between pages is put back together");
    }
    assert_eq!((prepared.additions, prepared.deletions), (12, 4), "the counts are the summary's");

    // A new revision fetches again.
    let mut summary = world.host.board.diffs[0].clone();
    summary.revision = 5;
    world.host.received.clear();
    let messages = world.host.event(|cursor| Message::DiffSummary { cursor, summary });
    world.deliver(messages);
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { revision: Some(Seq(5)), .. })));
}

#[test]
fn a_gap_asks_for_one_resync() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.host.received.clear();
    let head = world.host.head();
    let session = fake_host::board().sessions[0].clone();
    let skipped = world.host.stray(Message::SessionUpsert { cursor: Seq(head + 5), session: session.clone() });
    let again = world.host.stray(Message::SessionUpsert { cursor: Seq(head + 6), session });
    world.deliver(skipped.into_iter().chain(again).collect());
    let resyncs = world.sent().iter().filter(|m| matches!(m, Message::Resync { .. })).count();
    assert_eq!(resyncs, 1, "one resync for a run of events after a gap");
    assert_eq!(world.driver.remote.with_mirror(|m| m.cursor), head, "the snapshot answered it");
}

#[test]
fn pings_are_answered() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.host.received.clear();
    let ping = world.host.stray(Message::Ping { nonce: Some("n1".into()) });
    world.deliver(ping);
    assert_eq!(world.sent(), [&Message::Pong { nonce: Some("n1".into()) }]);
}

#[test]
fn it_stops_where_trying_again_cannot_help() {
    // Never paired in this browser.
    let mut world = World::new(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotHere));
    assert!(world.dials.is_empty());

    // The host forgot this browser: its key goes too.
    let mut world = World::paired(fake_host::host());
    world.host.allowed.clear();
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPaired));
    assert!(world.store.is_empty(), "the stored key is forgotten");

    // The host's user refused the device.
    let mut host = fake_host::host();
    host.answer = None;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::PairRefused));
    assert!(world.store.is_empty());

    // A link whose psk the host does not hold.
    let mut host = fake_host::host();
    let stale = host.open_pairing();
    host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&stale));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::PairFailed));
}

#[test]
fn only_the_hosts_sealed_refusal_forgets_the_key() {
    // The host refuses inside the channel: the key goes.
    let mut world = World::paired(fake_host::host());
    world.host.allowed.clear();
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPaired));
    assert!(world.store.is_empty());

    // Revoked during a session: the same sealed refusal.
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let refusal = world.host.stray(wire::not_paired().message);
    world.deliver(refusal);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPaired));
    assert!(world.store.is_empty());

    // A bare `not_paired` close, which anything on the way could send: it stops, and the key stays.
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::NOT_PAIRED);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPairedUnconfirmed));
    assert!(world.store.contains_key(HOST_ID), "the stored key is kept");
    assert_eq!(world.driver.wake_in(), None, "and nothing is tried again");
}

#[test]
fn a_broken_message_reconnects_and_only_a_bad_answer_is_a_key_mismatch() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let mut ping = world.host.stray(Message::Ping { nonce: Some("n1".into()) });
    ping[0][3] ^= 1;
    world.deliver(ping);
    assert_eq!(world.driver.status(), &Status::Reconnecting { attempt: 1 });
    assert_eq!(world.closed, [close::REFUSED], "the channel ends");
    assert!(world.store.contains_key(HOST_ID));
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live, "a new channel works");

    // The answer to the first frame does not authenticate: not the host this browser paired with.
    let mut world = World::paired(fake_host::host());
    world.host.corrupt_answer = true;
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::KeyMismatch));
    assert!(world.store.contains_key(HOST_ID), "kept: pairing again replaces it");
}

#[test]
fn a_used_or_expired_link_stops_at_once_with_its_own_message() {
    let mut host = fake_host::host();
    let used = host.open_pairing();
    host.pairing = None;
    let mut world = World::new(host);
    world.boot(Some(&used));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NoPairing));
    assert!(!world.driver.pairing_busy());
    assert_eq!(world.dials.len(), 1, "asked once");
    assert_eq!(world.driver.wake_in(), None, "and never again");
    assert_eq!(
        Stop::NoPairing.message(),
        "This pairing link was used or has expired. Show a new code on the computer."
    );
}

#[test]
fn a_busy_host_is_tried_again_until_its_link_comes_free() {
    let mut host = fake_host::host();
    let link = host.open_pairing();
    host.busy = true;
    let mut world = World::new(host);
    world.boot(Some(&link));
    assert_eq!(world.driver.status(), &Status::Pairing { code: None });
    assert!(world.driver.pairing_busy());
    world.host.busy = false;
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert!(world.host.code.is_some(), "the pairing went through");
    assert_eq!(world.driver.status(), &Status::Live);
    assert!(!world.driver.pairing_busy());
    assert!(world.store.contains_key(HOST_ID));

    // Busy, then used up by the other device: it stops at once.
    let mut host = fake_host::host();
    let link = host.open_pairing();
    host.busy = true;
    let mut world = World::new(host);
    world.boot(Some(&link));
    assert!(world.driver.pairing_busy());
    (world.host.busy, world.host.pairing) = (false, None);
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NoPairing));
}

#[test]
fn the_busy_window_runs_from_when_the_page_got_the_link() {
    // Answer each retry with a fresh ticket until the frame stops: how long that took.
    let stops_after = |world: &mut World| {
        let started = world.now;
        let mut tickets = 1;
        while matches!(world.driver.status(), Status::Pairing { .. }) {
            assert!(tickets < 60, "it never gives up");
            world.advance(world.driver.wake_in().expect("a retry is due"));
            tickets += 1;
            world.ticket(&format!("ticket-{tickets}"));
        }
        assert_eq!(world.driver.status(), &Status::Stopped(Stop::NoPairing), "the link has surely expired");
        world.now - started
    };
    let lifetime = PAIRING_LIFETIME_SECS * 1000;
    let busy = || {
        let mut host = fake_host::host();
        let link = host.open_pairing();
        host.busy = true;
        (World::new(host), link)
    };

    // The page says nothing: the link's five minutes run from the boot.
    let (mut world, link) = busy();
    world.boot(Some(&link));
    let took = stops_after(&mut world);
    assert!((lifetime..lifetime + MAX_RETRY_MS).contains(&took), "{took}");

    // The page got the link four minutes before the boot: one minute is left.
    let (mut world, link) = busy();
    let mut data = world.boot_data(Some(&link));
    data["pair"]["age_ms"] = serde_json::json!(4 * 60_000);
    world.driver.message(PARENT, true, &data).unwrap();
    world.pump();
    assert!(world.driver.pairing_busy());
    let took = stops_after(&mut world);
    assert!((60_000..60_000 + MAX_RETRY_MS).contains(&took), "{took}");

    // Older than the link's lifetime: busy is not tried again at all.
    let (mut world, link) = busy();
    let mut data = world.boot_data(Some(&link));
    data["pair"]["age_ms"] = serde_json::json!(lifetime);
    world.driver.message(PARENT, true, &data).unwrap();
    world.pump();
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NoPairing));
}

#[test]
fn pairing_again_under_another_host_key_asks_first() {
    let replaced = |world: &mut World| {
        let old = Secret::generate().unwrap().public();
        world.store.get_mut(HOST_ID).unwrap().host_key = old;
        old
    };
    // Go on: the new key is kept once the host accepts the device.
    let mut world = World::paired(fake_host::host());
    let old = replaced(&mut world);
    let pairing = world.host.open_pairing();
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::ConfirmRepair);
    assert!(world.dials.is_empty(), "nothing is dialed before the answer");
    assert_eq!(world.store[HOST_ID].host_key, old);
    world.driver.repair(true);
    world.pump();
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.store[HOST_ID].host_key, world.host.public());

    // Keep the current pairing: no pairing, and the key this browser holds is used.
    let mut world = World::paired(fake_host::host());
    let old = replaced(&mut world);
    let pairing = world.host.open_pairing();
    world.boot(Some(&pairing));
    world.driver.repair(false);
    world.pump();
    assert_eq!(world.dials.len(), 1);
    assert!(world.host.code.is_none(), "no pairing handshake");
    assert_eq!(world.store[HOST_ID].host_key, old, "nothing replaced");

    // The same host key: only that host can answer the link, so nothing is asked.
    let mut world = World::paired(fake_host::host());
    let pairing = world.host.open_pairing();
    let key = world.store[HOST_ID].secret.public();
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.store[HOST_ID].secret.public(), key, "pairing again reuses the stored device key");
    assert!(world.host.allowed.iter().all(|(allowed, _)| *allowed == key), "no extra key was registered");
}

/// A native client reaches its host directly: it dials at once with no boot or ticket, says hello as soon
/// as the upgrade is through, dials again after a drop with the usual waits, and stops for good on a
/// refused token or another major version.
#[test]
fn a_direct_client_dials_at_once_and_stops_only_when_waiting_cannot_help() {
    let dial = |driver: &mut Driver| match driver.take_outputs().as_slice() {
        [Output::DialDirect { socket }] => *socket,
        other => panic!("one direct dial: {other:?}"),
    };
    let mut driver = Driver::direct("t");
    assert!(driver.is_direct());
    assert_eq!(driver.status(), &Status::Connecting);
    let first = dial(&mut driver);
    driver.socket_open(first, wire::SUBPROTOCOL);
    assert_eq!(driver.status(), &Status::Live);
    let sent = driver.take_outputs();
    let [Output::Send { socket, message }] = sent.as_slice() else { panic!("the hello: {sent:?}") };
    assert_eq!(*socket, first);
    let hello = wire::decode(std::str::from_utf8(message).unwrap()).unwrap();
    assert!(matches!(hello.message, Message::Hello { .. }), "plain text frames on a direct link");

    // A drop: the usual waits, then a fresh dial (the glue finds the address and token again).
    driver.socket_closed(first, close::ABNORMAL);
    assert!(matches!(driver.status(), Status::Reconnecting { attempt: 1 }));
    assert!(driver.take_outputs().is_empty());
    driver.tick(MAX_RETRY_MS);
    let second = dial(&mut driver);
    assert_ne!(first, second);

    // A refused token stops it; retry_now is the user's own "try again".
    driver.socket_closed(second, close::UNAUTHORISED);
    assert_eq!(driver.status(), &Status::Stopped(Stop::Unauthorised));
    driver.tick(10 * MAX_RETRY_MS);
    assert!(driver.take_outputs().is_empty(), "no dial by itself");
    driver.retry_now();
    let third = dial(&mut driver);

    // A subprotocol other than atomics.v1 is not this host's server.
    driver.socket_open(third, tunnel::SUBPROTOCOL);
    assert!(matches!(driver.take_outputs().as_slice(), [Output::Close { .. }]));
    driver.tick(20 * MAX_RETRY_MS);
    let fourth = dial(&mut driver);
    driver.socket_open(fourth, wire::SUBPROTOCOL);
    driver.take_outputs();
    driver.socket_message(fourth, br#"{"v":{"major":2,"minor":0},"kind":"ping"}"#);
    assert_eq!(driver.status(), &Status::Stopped(Stop::Version));
}

#[test]
fn a_failed_device_read_never_pairs_or_replaces_a_key_and_can_be_retried() {
    for pair in [false, true] {
        let mut world = World::paired(fake_host::host());
        let key = world.store[HOST_ID].secret.public();
        let pairing = world.host.open_pairing();
        let data = world.boot_data(pair.then_some(&pairing));
        world.driver.message(PARENT, true, &data).unwrap();
        let outputs = world.driver.take_outputs();
        assert!(outputs.iter().any(|o| matches!(o, Output::LoadDevice(_))));
        world.driver.device_loaded(Err(DeviceReadError));
        assert_eq!(world.driver.status(), &Status::Stopped(Stop::DeviceReadFailed));
        assert_eq!(world.driver.pairing_reason(), Some("Reason: could not read the stored key."));
        assert!(world.driver.device.is_none(), "no device key is created before a successful read");
        world.driver.repair(true);
        world.driver.resumed(LIVE_SOCKET_STALE_AFTER_MS);
        assert!(world.driver.take_outputs().is_empty(), "no dial, save, forget or pairing on read failure");
        assert_eq!(world.store[HOST_ID].secret.public(), key);

        world.driver.retry_now();
        assert!(world.driver.loading);
        world.pump();
        assert_eq!(world.driver.status(), &Status::Live);
        assert_eq!(world.store[HOST_ID].secret.public(), key);
    }
}

#[test]
fn a_short_tab_switch_keeps_the_live_socket_screen_and_typing_lease() {
    for hidden_ms in [0, LIVE_SOCKET_STALE_AFTER_MS - 1] {
        let mut world = World::paired(fake_host::host());
        world.boot(None);
        world.driver.remote.show_screen("s1", true);
        world.driver.flush();
        world.pump();
        world.driver.remote.acquire("s1", false);
        world.driver.flush();
        world.pump();
        let lease = world.driver.remote.lease("s1");
        assert!(lease.yours, "the browser holds the typing lease before switching tabs");
        let socket = world.socket;
        let posted = world.posted.clone();
        assert_eq!(world.driver.remote.with_screen("s1", |s| s.map(|(_, stale)| stale)), Some(false));

        world.driver.resumed(hidden_ms);
        assert!(world.driver.take_outputs().is_empty(), "no close, ticket request or redial after {hidden_ms} ms");
        assert_eq!(world.driver.status(), &Status::Live);
        assert_eq!(world.socket, socket);
        assert_eq!(world.posted, posted);
        assert_eq!(world.driver.remote.lease("s1"), lease, "the typing lease is kept");
        assert_eq!(world.driver.remote.with_screen("s1", |s| s.map(|(_, stale)| stale)), Some(false));
    }
}

#[test]
fn waking_after_thirty_seconds_reconnects_with_the_same_device() {
    let mut world = World::paired(fake_host::host());
    let key = world.store[HOST_ID].secret.public();
    world.boot(None);
    let first = world.socket;
    world.driver.resumed(LIVE_SOCKET_STALE_AFTER_MS);
    world.pump();
    assert_eq!(world.socket, None, "even a socket that looked live was closed");
    assert_eq!(world.closed, [close::NORMAL]);
    assert_eq!(world.posted.last(), Some(&boot::WANTS_TICKET));
    world.ticket("wake-ticket");
    assert_eq!(world.driver.status(), &Status::Live);
    assert_ne!(world.socket, first);
    assert_eq!(world.store[HOST_ID].secret.public(), key);

    world.drop_socket(close::ABNORMAL);
    world.driver.resumed(0);
    world.pump();
    assert_eq!(world.driver.retry.due_ms, None, "wake skips the reconnect wait");
    world.ticket("wake-again");
    assert_eq!(world.driver.status(), &Status::Live);
}

#[test]
fn expired_sign_in_keeps_the_pairing_and_asks_to_sign_in() {
    let mut world = World::paired(fake_host::host());
    let key = world.store[HOST_ID].secret.public();
    world.boot(None);
    world.drop_socket(close::UNAUTHORISED);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::SignIn));
    assert!(Stop::SignIn.message().contains("Sign in again"));
    assert_eq!(world.store[HOST_ID].secret.public(), key);
    world.driver.retry_now();
    world.pump();
    assert_eq!(world.posted.last(), Some(&boot::WANTS_TICKET));
    world.ticket("signed-in-again");
    assert_eq!(world.driver.status(), &Status::Live);
}

#[test]
fn waking_does_not_restart_a_pairing_or_a_security_refusal() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    let status = world.driver.status().clone();
    assert_eq!(world.driver.pairing_reason(), Some("Reason: no key stored for this host."));
    world.driver.resumed(LIVE_SOCKET_STALE_AFTER_MS);
    assert_eq!(world.driver.status(), &status);
    assert!(world.driver.take_outputs().is_empty());

    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::NOT_PAIRED);
    world.driver.resumed(LIVE_SOCKET_STALE_AFTER_MS);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPairedUnconfirmed));
    assert!(world.driver.take_outputs().is_empty());
    assert!(world.store.contains_key(HOST_ID));
}

#[test]
fn device_dates_include_pairing_and_work_with_older_hosts() {
    use atomics_core::projection::DeviceInfo;
    let mut info: DeviceInfo = serde_json::from_value(serde_json::json!({
        "id": "opaque-id", "name": "A browser", "controls": false, "connected": false,
        "seen_ms": 1_793_836_800_000u64,
    }))
    .unwrap();
    assert_eq!(info.dates(), ("unknown".into(), "2026-11-05 00:00 UTC".into()));
    info.paired_ms = 1_793_750_400_000;
    assert_eq!(info.dates().0, "2026-11-04 00:00 UTC");
}

#[test]
fn a_missing_key_says_what_storage_held_on_screen_and_to_the_host() {
    let facts = serde_json::json!({
        "open": "existing", "seal": "present", "records": 0, "read": "none", "save": "not-run",
        "mark": "saved 2026-11-07T10:18:08.123Z", "persisted": false, "access": true,
    });
    let mut world = World::new(fake_host::host());
    world.driver.storage_noted(Storage::read(&facts));
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotHere));
    let line = world.driver.storage_reason().expect("under the reason");
    assert!(line.starts_with("Storage: database existing, sealing key present, 0 saved; read none;"), "{line}");

    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.driver.storage_noted(Storage::read(&facts));
    world.boot(Some(&pairing));
    assert_eq!(
        world.host.named.as_deref(),
        Some("Safari on iPhone [kept s1 k0 saved 11-07 10:18Z p0 c- a?]"),
        "the host's device list says what this pairing found"
    );
    let answer = world.host.confirm();
    world.deliver(answer);
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.driver.storage_reason(), None, "a live board shows no storage line");
}

fn safari() -> Passkeys {
    Passkeys::read(&serde_json::json!({ "available": true, "per_session": true, "create_in_frame": false }))
}

/// What a passkey seals, as `web/passkey.js` would hand it back: the device's host, and ciphertext.
fn sealed_for(device: &Device) -> Sealed {
    Sealed {
        host_id: device.host_id.clone(),
        host_key: device.host_key,
        credential: "Q3JlZGVudGlhbC1pZC0x".into(),
        iv: "A".repeat(16),
        sealed: "B".repeat(64),
        paired_at: 1_762_510_688_000,
    }
}

#[test]
fn where_no_copy_can_help_a_pairing_is_reported_once_saved_and_nothing_asks() {
    // A page that keeps nothing, as before; and one that keeps copies, in a browser that keeps the frame's storage.
    for (keeps, chrome) in [(false, false), (true, true)] {
        let mut host = fake_host::host();
        host.hold = true;
        let pairing = host.open_pairing();
        let mut world = World::new(host);
        world.keeps = keeps;
        if chrome {
            world
                .driver
                .passkeys_noted(Passkeys::read(&serde_json::json!({ "available": true, "create_in_frame": true })));
        }
        world.boot(Some(&pairing));
        let answer = world.host.confirm();
        world.deliver(answer);
        assert_eq!(world.driver.status(), &Status::Live);
        assert_eq!((world.posted.last(), world.kept.as_slice()), (Some(&boot::PAIRED), &[false][..]));
        assert!(world.ask.is_none() && world.driver.keep_prompt().is_none(), "no passkey is asked for");
        let why = if keeps { "this browser keeps the frame's storage" } else { "the page keeps no copies" };
        assert!(world.driver.key_line().contains(why), "{}", world.driver.key_line());
    }
}

#[test]
fn a_pairing_in_safari_is_kept_by_the_page_before_it_is_reported() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.keeps = true;
    world.driver.passkeys_noted(safari());
    world.boot(Some(&pairing));
    let answer = world.host.confirm();
    world.deliver(answer);
    assert_eq!(world.driver.status(), &Status::Live, "the session goes on meanwhile");
    assert!(!world.posted.contains(&boot::PAIRED), "paired waits for the copy");
    assert_eq!(
        world.driver.keep_prompt(),
        Some(KeepPrompt::Ask { first: true, failed: None, another: false }),
        "the first tap makes the passkey"
    );
    let Some(Ask::Keep { credential: None, made: false, create_in_frame: false, .. }) = &world.ask else {
        panic!("a keep that makes the passkey in its own window: {:?}", world.ask)
    };

    // The window opens, and closes without a word: whichever passkey the browser offers will do.
    world.driver.passkey_answered(Answer::Enrolling);
    assert_eq!(world.driver.keep_prompt(), Some(KeepPrompt::Enrolling));
    world.driver.enrolled_elsewhere(true);
    world.pump();
    let Some(Ask::Keep { made: true, .. }) = &world.ask else { panic!("a keep with any passkey: {:?}", world.ask) };

    // A cancelled tap says why and asks again.
    let cancelled = Failure { step: "get".into(), name: "NotAllowedError".into(), detail: String::new() };
    world.driver.passkey_answered(Answer::Failed(cancelled));
    let Some(KeepPrompt::Ask { failed: Some(why), .. }) = world.driver.keep_prompt() else { panic!("asks again") };
    assert!(why.starts_with("cancelled"), "{why}");

    let device = world.store.get(HOST_ID).cloned().expect("the frame's store has it");
    world.driver.passkey_answered(Answer::Sealed(sealed_for(&device)));
    world.pump();
    assert_eq!(world.kept, [true], "reported once the page keeps the copy");
    assert_eq!(world.page.get(HOST_ID).map(|s| s.host_key), Some(device.host_key));
    assert!(world.ask.is_none() && world.driver.keep_prompt().is_none(), "the button goes");
    assert!(
        world.driver.key_line().starts_with("Key: paired, sealed copy kept by the page"),
        "{}",
        world.driver.key_line()
    );
}

#[test]
fn a_page_that_does_not_keep_the_copy_leaves_the_pairing_reported_and_working() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.keeps = true;
    world.page_answers = Some(false);
    world.driver.passkeys_noted(safari());
    world.boot(Some(&pairing));
    let answer = world.host.confirm();
    world.deliver(answer);
    let device = world.store.get(HOST_ID).cloned().unwrap();
    world.driver.passkey_answered(Answer::Sealed(sealed_for(&device)));
    world.pump();
    let Some(KeepPrompt::Ask { failed: Some(why), another: true, .. }) = world.driver.keep_prompt() else {
        panic!("asks again: {:?}", world.driver.keep_prompt())
    };
    assert!(why.contains("could not keep"), "{why}");

    // Silent this time: the wait ends, and the pairing is reported as not kept.
    world.page_answers = None;
    world.driver.passkey_answered(Answer::Sealed(sealed_for(&device)));
    world.pump();
    assert_eq!(world.driver.keep_prompt(), Some(KeepPrompt::Handing));
    world.advance(KEEP_WAIT_MS);
    assert_eq!(world.kept, [false]);
    assert!(world.driver.key_line().contains("the page did not answer"), "{}", world.driver.key_line());
    assert_eq!(world.driver.status(), &Status::Live);

    // "Not now" on another pairing reports it at once.
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.keeps = true;
    world.driver.passkeys_noted(safari());
    world.boot(Some(&pairing));
    let answer = world.host.confirm();
    world.deliver(answer);
    world.driver.keep_declined();
    world.pump();
    assert_eq!((world.kept.as_slice(), world.ask.is_none()), (&[false][..], true));
}

#[test]
fn a_cold_start_in_safari_unlocks_the_pages_copy_and_dials_with_a_fresh_ticket() {
    let mut world = World::paired(fake_host::host());
    let device = world.store.remove(HOST_ID).expect("paired earlier");
    world.keeps = true;
    world.page.insert(HOST_ID.into(), sealed_for(&device));
    world.driver.passkeys_noted(safari());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::Locked));
    assert!(matches!(&world.ask, Some(Ask::Unlock(sealed)) if sealed.host_key == device.host_key));
    assert!(world.dials.is_empty(), "nothing dials before the unlock");
    let line = world.driver.storage_reason().expect("the locked screen says why");
    assert!(
        line.contains("Key: sealed copy, waiting for the passkey")
            && line.contains("page keeps a sealed copy from 2025-"),
        "{line}"
    );

    // A failed unlock forgets nothing.
    let open = Failure { step: "open".into(), name: "OperationError".into(), detail: String::new() };
    world.driver.passkey_answered(Answer::Failed(open));
    world.pump();
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::Locked));
    assert!(world.driver.unlock_failed().is_some_and(|why| why.contains("did not open")));
    assert!(world.page.contains_key(HOST_ID) && world.ask.is_some(), "the copy and the button stay");

    world.driver.passkey_answered(Answer::Unlocked(Secret::from_bytes(device.secret.as_bytes())));
    world.pump();
    assert_eq!(world.posted.last(), Some(&boot::WANTS_TICKET), "the boot's ticket may have expired");
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.dials.last().map(|d| d[1].as_str()), Some("atomics.ticket.ticket-2"));
    assert!(world.store.contains_key(HOST_ID), "back in the frame's store for this browser's run");
    assert!(world.ask.is_none());
    assert!(world.driver.key_line().starts_with("Key: unsealed with the passkey"));
}

#[test]
fn without_a_passkey_a_kept_copy_changes_nothing() {
    let mut world = World::paired(fake_host::host());
    let device = world.store.remove(HOST_ID).unwrap();
    world.keeps = true;
    world.page.insert(HOST_ID.into(), sealed_for(&device));
    world.driver.passkeys_noted(Passkeys::read(&serde_json::json!({ "available": false, "per_session": true })));
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotHere));
    assert!(world.ask.is_none());
    assert!(world.driver.key_line().contains("no passkey (no passkey with user verification here)"));

    // Locked first, then the browser says it has none: as before, with the reason.
    let mut world = World::paired(fake_host::host());
    let device = world.store.remove(HOST_ID).unwrap();
    world.keeps = true;
    world.page.insert(HOST_ID.into(), sealed_for(&device));
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::Locked));
    world.driver.passkeys_noted(Passkeys::read(&serde_json::json!({ "available": true, "prf": false })));
    world.pump();
    assert_eq!((world.driver.status(), world.ask.is_none()), (&Status::Stopped(Stop::NotHere), true));
}

#[test]
fn the_hosts_sealed_refusal_forgets_both_copies() {
    let mut world = World::paired(fake_host::host());
    let device = world.store.get(HOST_ID).cloned().unwrap();
    world.keeps = true;
    world.page.insert(HOST_ID.into(), sealed_for(&device));
    world.host.allowed.clear();
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPaired));
    assert!(world.store.is_empty() && world.page.is_empty());
}

#[test]
fn f2_a_connection_refusal_stops_retrying_and_keeps_the_reason() {
    let mut driver = Driver::direct("refusal");
    let [Output::DialDirect { socket }] = driver.take_outputs()[..] else { panic!("dial") };
    driver.socket_open(socket, wire::SUBPROTOCOL);
    driver.take_outputs();
    let refusal = wire::WireError::new(ErrorCode::Oversize, "plan release is too large to show")
        .into_envelope(None, Some("e".into()));
    driver.socket_message(socket, wire::encode(&refusal).unwrap().as_bytes());
    assert!(matches!(driver.status(), Status::Stopped(_)));
    assert!(driver.remote.notice().unwrap().contains("plan release is too large"));
    driver.socket_closed(socket, close::REFUSED);
    driver.tick(10 * MAX_RETRY_MS);
    assert!(!driver.take_outputs().iter().any(|o| matches!(o, Output::DialDirect { .. })));
}
