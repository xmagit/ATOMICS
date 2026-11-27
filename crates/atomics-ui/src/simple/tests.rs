use super::*;

/// On Linux and Windows the mode's chord is the steer bar's with Alt, taken exactly; a Mac's is Command+Shift+L.
#[test]
fn the_mode_chord_is_its_own_on_every_system() {
    use egui::os::OperatingSystem::{Mac, Nix, Windows};
    let steer = |os| crate::keys::shortcut_for_os("cmd+l", os).unwrap();
    for os in [Nix, Windows] {
        let modifiers = chord(os).modifiers;
        assert!(modifiers.ctrl && modifiers.alt && modifiers.shift, "{os:?}: {modifiers:?}");
        assert_ne!(chord(os), steer(os), "{os:?}");
    }
    assert!(chord(Mac).modifiers.mac_cmd && chord(Mac).modifiers.shift && !chord(Mac).modifiers.alt);
    assert_ne!(chord(Mac), steer(Mac));
}
use atomics_core::hook::HookEvent;
use atomics_core::tickets::Cleaner;
use atomics_core::trace::TraceLog;
use atomics_core::trace_feed::TraceData;

#[test]
fn folder_trust_groups_agents_of_one_board_into_one_card() {
    let sessions: Vec<_> = ["p1", "p1", "p2", "p1", "p1"]
        .iter()
        .enumerate()
        .map(|(i, plan)| SessionRow {
            id: format!("agent-{i}"),
            title: format!("Task {i}"),
            agent: "claude".into(),
            kind: atomics_core::config::AgentKind::Claude,
            status: AgentStatus::NeedsInput,
            status_hint: None,
            chief: false,
            thread: None,
            context: None,
            worktree: None,
            branch: None,
            root: format!("/work/{i}").into(),
            project: None,
            ungrouped: false,
            task: Some(crate::TaskLink { plan: (*plan).into(), task: format!("t{i}") }),
            scratch: false,
            sandbox: None,
            outside: false,
            can_receive: true,
            pending_approvals: usize::from(i == 4),
            held_messages: 0,
            changes: None,
            activity: None,
        })
        .collect();
    let folder_trust = |i: usize| {
        (i != 3).then(|| FolderTrustRow {
            session: sessions[i].id.clone(),
            request: format!("menu-{i}"),
            folder: sessions[i].root.clone(),
            trust: FolderAccess::Trust,
            refusal: FolderAccess::Exit,
            answering: false,
        })
    };
    let needs = agent_needs(&sessions, "p1", folder_trust, |_| None);
    let [Need::FolderTrust(rows), Need::Question(3, None)] = needs.as_slice() else { panic!("{needs:?}") };
    assert_eq!(rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1]);
    assert_eq!(rows[0].1.folder, std::path::Path::new("/work/0"));
    let single = agent_needs(&sessions, "p2", folder_trust, |_| None);
    assert!(matches!(single.as_slice(), [Need::FolderTrust(rows)] if rows.len() == 1 && rows[0].0 == 2));
}

fn said(name: &str, field: &str, text: &str, session: &str) -> HookEvent {
    HookEvent::from_payload(&serde_json::json!({"hook_event_name": name, "session_id": session, field: text})).unwrap()
}

#[test]
fn the_outbox_claims_each_new_user_turn_once() {
    let trace = |messages: &[&str]| {
        let mut log = TraceLog::default();
        for text in messages {
            log.ingest(&said("UserPromptSubmit", "prompt", text, "provider"), None, &Cleaner::default(), 1);
        }
        TraceData { session: Some("chief".into()), entries: log.entries.into(), ..Default::default() }
    };
    let old = trace(&["Keep going"]);
    let chief = composer_chief(AgentStatus::Working);
    let mut state = State::default();
    for now in [false, true] {
        let mut out = Outgoing::new(String::new(), "Keep going".into(), None, now);
        assert_eq!(
            out.submit(Some(&chief), &old),
            Command::Steer { sessions: vec!["chief".into()], text: "Keep going".into(), now }
        );
        state.outbox.push(out);
    }
    reconcile_outbox(&mut state, &old);
    assert_eq!(state.outbox.len(), 2);
    let once = trace(&["Keep going", "Keep going"]);
    reconcile_outbox(&mut state, &once);
    reconcile_outbox(&mut state, &once);
    assert_eq!(state.outbox.len(), 1);
    reconcile_outbox(&mut state, &trace(&["Keep going", "Keep going", "Keep going"]));
    assert!(state.outbox.is_empty());
}

fn composer_chief(status: AgentStatus) -> SessionRow {
    SessionRow {
        id: "chief".into(),
        title: "chief".into(),
        agent: "claude".into(),
        status,
        chief: true,
        kind: atomics_core::config::AgentKind::Claude,
        thread: Some(String::new()),
        context: None,
        status_hint: None,
        worktree: None,
        branch: None,
        root: "/work".into(),
        project: None,
        ungrouped: false,
        task: None,
        scratch: false,
        sandbox: None,
        outside: false,
        can_receive: true,
        pending_approvals: 0,
        held_messages: 0,
        changes: None,
        activity: None,
    }
}

#[test]
fn stop_targets_only_the_working_chief_of_the_conversation() {
    let state = State::default();
    let chief = composer_chief(AgentStatus::Working);
    assert_eq!(stop_chief(&chief, Some(""), &state), Some(Command::StopChief("chief".into())));
    assert_eq!(stop_chief(&chief, Some("another-thread"), &state), None);
    assert_eq!(stop_chief(&composer_chief(AgentStatus::Idle), Some(""), &state), None);
}

#[test]
fn open_repositories_do_not_count_as_a_project_pick() {
    let repos = ["/work/ATOMICS".into(), "/work/RELAY".into()];
    let state = State::default();
    assert_eq!(picked_project(&repos, &state), None);
    let state = State { project: Some(repos[1].clone()), ..state };
    assert_eq!(picked_project(&repos, &state), Some(repos[1].clone()));
    assert_eq!(picked_project(&repos[..1], &state), None);
}

#[test]
fn the_conversation_is_the_chiefs_turns_and_plans_of_its_current_conversation() {
    let cleaner = Cleaner::default();
    let mut log = TraceLog::default();
    // An earlier conversation, before a /clear: not this one.
    log.ingest(&said("UserPromptSubmit", "prompt", "old question", "before"), None, &cleaner, 1);
    log.ingest(&said("UserPromptSubmit", "prompt", "Plan the week", "now"), None, &cleaner, 2);
    log.ingest(&said("UserPromptSubmit", "prompt", "ATOMICS wake: 2 new events", "now"), None, &cleaner, 3);
    let ended = "<task-notification>\n<status>completed</status>\n</task-notification>";
    log.ingest(&said("UserPromptSubmit", "prompt", ended, "now"), None, &cleaner, 3);
    let planned = HookEvent::from_payload(&serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "now", "tool_name": "mcp__atomics__atomics_plan_set",
        "tool_use_id": "p", "tool_input": {"goal": "Week", "tasks": [{"id": "t1"}, {"id": "t2"}]},
        "tool_response": {"content": [{"type": "text", "text": "ok"}]}
    }))
    .unwrap();
    log.ingest_for_plan(&planned, None, &cleaner, 4, Some("p1"));
    // Other calls are the trace lane's, not the conversation's.
    let lookup = HookEvent::from_payload(&serde_json::json!({
        "hook_event_name": "PostToolUse", "session_id": "now", "tool_name": "mcp__atomics__atomics_list",
        "tool_use_id": "l", "tool_input": {}, "tool_response": {}
    }))
    .unwrap();
    log.ingest(&lookup, None, &cleaner, 5);
    log.ingest(&said("Stop", "last_assistant_message", "On it.", "now"), None, &cleaner, 6);
    let data = TraceData { entries: log.entries.into_iter().collect(), ..TraceData::default() };
    assert_eq!(
        conversation(&data),
        [
            Said::User("Plan the week".into()),
            Said::Atomics("ATOMICS wake: 2 new events".into()),
            Said::Atomics(ended.into()),
            Said::Planned(Some("p1".into()), 2),
            Said::Chief("On it.".into()),
        ]
    );
    assert!(conversation(&TraceData::default()).is_empty());
}

#[test]
fn a_boards_decision_belongs_under_the_latest_change_the_chief_made_to_it() {
    let action = |title, plan: Option<&str>, failed| Said::Action {
        title,
        request: "{}".into(),
        error: None,
        failed,
        plan: plan.map(str::to_owned),
    };
    let said = [
        (1, Said::User("Plan the week".into())),
        (2, Said::Planned(Some("p1".into()), 2)),
        (3, Said::Planned(Some("p2".into()), 1)),
        (4, Said::Chief("Approve when ready.".into())),
        (5, action("Add a task", Some("p1"), false)),
        (6, action("Add a task", Some("p2"), true)),
        (7, action("Send a message", Some("p1"), false)),
        (8, action("Edit a task", None, false)),
        (9, Said::Chief("I added the retry task.".into())),
    ];
    let changes = latest_changes(&said);
    // The task added to p1 moved its decision down; a failed edit, a message and an edit of no board did not.
    assert_eq!(changes.get("p1"), Some(&4));
    assert_eq!(changes.get("p2"), Some(&2));
    assert_eq!(changes.len(), 2);
    assert!(latest_changes(&said[..2]).get("p1") == Some(&1), "before the change, its first card asks");
}

#[test]
fn notes_nobody_typed_and_the_same_answer_to_them_again_are_left_out() {
    let wake = |at| (at, Said::Atomics("ATOMICS wake: nothing new".into()));
    let chief = |at, text: &str| (at, Said::Chief(text.into()));
    let user = |at, text: &str| (at, Said::User(text.into()));
    assert_eq!(
        spoken(vec![
            wake(1),
            chief(2, "Clean."),
            wake(3),
            chief(4, "Clean."),
            chief(5, "t1 is done."),
            chief(6, "Clean.")
        ]),
        [chief(4, "Clean."), chief(5, "t1 is done."), chief(6, "Clean.")]
    );
    let asked = vec![user(1, "go on"), chief(2, "Clean."), user(3, "go on"), chief(4, "Clean.")];
    assert_eq!(spoken(asked.clone()), asked, "an answer to the user is its own");
}

#[test]
fn a_clients_own_choice_is_kept_by_name_and_anything_else_follows_the_host() {
    for mode in [Some(UiMode::Simple), Some(UiMode::Full), None] {
        assert_eq!(named_mode(mode_name(mode)), mode);
    }
    assert_eq!(named_mode(" simple\n"), Some(UiMode::Simple));
    assert_eq!(named_mode("fancy"), None);
}

#[test]
fn a_providers_quota_keeps_the_last_reading_when_its_windows_expire() {
    use atomics_core::usage::{Usage, Window};
    let now = 10_000;
    let window = |left, resets_at| Some(Window { left_percent: left, resets_at });
    assert_eq!(quota(&Usage::default(), now), Quota::Unknown, "never read: unknown, not 0 and not full");
    let fresh = Usage {
        session: window(62.0, now + 600),
        weekly: window(40.0, now + 9000),
        observed_at: now - 60,
        ..Usage::default()
    };
    assert_eq!(quota(&fresh, now), Quota::Left(40.0));
    assert!(!low(quota(&fresh, now)));
    let old = Usage { observed_at: now - USAGE_FRESH - 1, ..fresh.clone() };
    assert_eq!(quota(&old, now), Quota::Stale { left: 40.0, observed_at: old.observed_at, expired: false });
    let reset = Usage { session: window(0.0, now - 1), observed_at: now - 120, ..Usage::default() };
    let Quota::Stale { left, observed_at, expired: true } = quota(&reset, now) else {
        panic!("expected the expired reading")
    };
    assert_eq!(crate::resources::last_reading(left, observed_at, now), "0% · 2m ago");
    assert!(!low(quota(&reset, now)));
    let renewed = Usage { session: window(100.0, now + 600), observed_at: now, ..Usage::default() };
    assert_eq!(quota(&renewed, now), Quota::Left(100.0));
    let none = Usage { observed_at: now - 60, ..Usage::default() };
    assert_eq!(quota(&none, now), Quota::Unknown);
    let running_low = Usage { session: window(12.0, now + 600), observed_at: now - 60, ..Usage::default() };
    assert!(low(quota(&running_low, now)));
}

#[test]
fn a_legacy_brief_pointer_uses_the_saved_opening_line() {
    let pointer = "Your task brief is in ***. Read it first and follow it exactly.";
    assert_eq!(history::opening_text(pointer, Some("Ship the release")), "Ship the release");
    assert_eq!(history::opening_text(pointer, Some(pointer)), "Opening message unavailable");
    assert_eq!(
        history::opening_text("Your task brief is in the repository.", None),
        "Your task brief is in the repository."
    );
}

#[test]
fn a_message_time_that_cannot_be_right_is_not_shown() {
    let now = 1_800_000_000_000;
    assert_eq!(ago(now - 10_000, now).as_deref(), Some("just now"));
    assert_eq!(ago(now - 5 * 60_000, now).as_deref(), Some("5 min"));
    assert_eq!(ago(now - 3 * 86_400_000, now).as_deref(), Some("3 d"));
    assert_eq!(ago(0, now), None, "a time of zero");
    assert_eq!(ago(900_000, now), None, "seconds or minutes taken for milliseconds");
    assert_eq!(ago(now + 3_600_000, now), None, "ahead of now");
    assert_eq!(ago(now - 400 * 86_400_000, now), None, "more than a year back");
    assert_eq!(ago(now - 10_000, 0), None, "no clock");
}

#[test]
fn where_the_agents_run_says_the_machine_its_hardening_and_the_link_in_a_few_words() {
    use atomics_core::projection::{AgentMachine, AgentsInfo};
    let vm = AgentsInfo {
        machine: AgentMachine::Vm,
        name: Some("atomics-vm".into()),
        isolation: "sandbox".into(),
        hardened: Some(true),
    };
    let (short, long) = place_words(Some(&vm), Some(true), None);
    assert_eq!(short, "In the VM · hardened");
    assert!(long.contains("ATOMICS's VM (atomics-vm): hardened") && long.contains("sandbox"), "{long}");
    assert!(long.contains("connected"), "{long}");
    let soft = AgentsInfo { hardened: Some(false), ..vm.clone() };
    let (short, long) = place_words(Some(&soft), Some(false), None);
    assert_eq!(short, "In the VM · reconnecting");
    assert!(long.contains("may become root") && long.contains("reconnecting"), "{long}");
    // The Mac: this window is on it, or another client reaches it.
    let mac = AgentsInfo { isolation: "bare".into(), ..AgentsInfo::default() };
    assert_eq!(place_words(Some(&mac), None, None).0, "This Mac");
    assert_eq!(place_words(Some(&mac), Some(true), None).0, "On the Mac");
    assert!(place_words(Some(&mac), None, None).1.contains("bare"));
    let other = AgentsInfo { machine: AgentMachine::Other, name: Some("builder".into()), ..mac };
    assert_eq!(place_words(Some(&other), Some(true), None).0, "On builder");
    // An older host says nothing: the client's own word, else a plain one.
    assert_eq!(place_words(None, Some(true), Some("Agents in VM atomics")).0, "Agents in VM atomics");
    assert_eq!(place_words(None, Some(true), None).0, "On the host");
}

#[test]
fn transcript_heights_survive_eviction_but_follow_edited_text_and_width() {
    let layout = (egui::Id::NULL.with("transcript"), 550.0_f32.to_bits(), 1.0_f32.to_bits());
    let mut transcript = TranscriptRows::default();
    let original = [
        (1, Said::User("Earlier".into())),
        (2, Said::Chief("Wrapped answer".into())),
        (3, Said::Chief("In progress".into())),
    ];
    transcript.sync(layout, &original);
    transcript.rows[0].1 = Some(28.0);
    transcript.rows[1].1 = Some(56.0);
    transcript.rows[2].1 = Some(26.0);
    let new = [original[1].clone(), (3, Said::Chief("In progress, with more detail".into()))];
    transcript.sync(layout, &new);
    assert_eq!(transcript.rows.iter().map(|(_, height)| *height).collect::<Vec<_>>(), [Some(56.0), None]);
    transcript.sync((layout.0, 400.0_f32.to_bits(), layout.2), &new);
    assert!(transcript.rows.iter().all(|(_, height)| height.is_none()));
}

#[test]
fn transcript_rows_keep_their_measurements_while_the_scroll_view_width_wobbles() {
    let ctx = egui::Context::default();
    let mut transcript = TranscriptRows::default();
    let said: Vec<_> = (0..96).map(|n| (n, Said::Chief(format!("A completed step {n}")))).collect();
    for (frame, (viewport, column)) in
        [(584.0, 570.0), (576.0, 570.0), (581.0, 570.0), (591.0, 570.0), (616.0, 602.0), (612.0, 602.0)]
            .into_iter()
            .enumerate()
    {
        ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.scope_builder(
                egui::UiBuilder::new().max_rect(ui.max_rect().with_max_x(ui.min_rect().left() + viewport)),
                |ui| {
                    ui.set_width(viewport);
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        // A wide sibling can expand the scroll area's content UI.
                        ui.allocate_space(vec2(viewport + 5.0, 8.0));
                        with_transcript_width(ui, column, |ui| {
                            assert_eq!(ui.available_width(), column);
                            transcript.sync((ui.scope_id(), column.to_bits(), 1.0_f32.to_bits()), &said);
                            let cached = if frame == 0 || frame == 4 { None } else { Some(32.0) };
                            assert!(transcript.rows.iter().all(|(_, height)| *height == cached));
                            for (_, height) in &mut transcript.rows {
                                *height = Some(32.0);
                            }
                        });
                    });
                },
            );
        })
        .drop_without_applying_deltas();
    }
}

#[test]
fn composer_reserves_room_for_newlines_and_wrapping_up_to_its_scroll_limit() {
    let ctx = egui::Context::default();
    ctx.run_ui(egui::RawInput::default(), |ui| {
        let one_line = composer_height(ui, 400.0, 600.0, "One line");
        assert_eq!(one_line, COMPOSER_MIN_HEIGHT);
        assert!(composer_height(ui, 400.0, 600.0, "One line\n") > one_line);
        assert!(composer_height(ui, 140.0, 600.0, "These words wrap onto another line") > one_line);
        assert!(composer_height(ui, 400.0, 300.0, &"more\n".repeat(30)) <= 300.0 * 0.35);
    })
    .drop_without_applying_deltas();
}

/// A conversation held at its bottom shows what came in, or what went, in the frame it changes: egui moves a stuck
/// scroll only after painting, so without settling the frame showed the column off by the change, then jumped.
#[test]
fn a_conversation_held_at_its_bottom_moves_with_its_content_in_the_frame_it_changes() {
    let ctx = egui::Context::default();
    // Where the last row ends against the bottom of the view, as the frame that is shown draws it.
    let frame = |rows: usize, settle: bool| {
        let mut seen = (0.0, 0.0);
        ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(400.0, 300.0))),
                ..Default::default()
            },
            |ui| {
                let scrolled = egui::ScrollArea::vertical()
                    .id_salt(("held", settle))
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for row in 0..rows {
                            ui.add_sized(vec2(100.0, 20.0), egui::Label::new(format!("row {row}")));
                        }
                        ui.min_rect().bottom()
                    });
                if settle {
                    settle_at_bottom(ui, &scrolled);
                }
                seen = (scrolled.inner, scrolled.inner_rect.bottom());
            },
        )
        .drop_without_applying_deltas();
        seen
    };
    for settle in [false, true] {
        for _ in 0..3 {
            frame(30, settle);
        }
        let (last, bottom) = frame(30, settle);
        assert!((last - bottom).abs() < 0.5, "held at the bottom: {last} {bottom}");
        for rows in [32, 31, 34] {
            let (last, bottom) = frame(rows, settle);
            let steady = (last - bottom).abs() < 0.5;
            assert_eq!(
                steady, settle,
                "{rows} rows, settled {settle}: the last row ends at {last}, the view at {bottom}"
            );
            let (last, bottom) = frame(rows, settle);
            assert!((last - bottom).abs() < 0.5, "the next frame holds the bottom either way");
        }
    }
}

#[test]
fn a_chat_is_titled_without_the_scope_atomics_put_before_its_prompt() {
    assert_eq!(history::title("[Plan plan-1]\nIn relay: use a Sol agent"), "In relay: use a Sol agent");
    assert_eq!(history::title("[Plan plan-1] Time to clean up"), "Time to clean up");
    assert_eq!(history::title("[Chat chat-1]"), "");
    assert_eq!(history::title("Plan the week"), "Plan the week");
    // A brief handed over as a file names nothing: the chat takes its board's name, else "Chief".
    assert_eq!(history::title("[Plan plan-1]\nYour task brief is in ***. Read it first and follow it exactly."), "");
}

#[test]
fn the_rail_lists_a_chat_once_and_never_beside_its_board() {
    use atomics_core::projection::ChiefConversation;
    let chat = |n: char| format!("chat-{}", n.to_string().repeat(32));
    let (a, b, c, d, e, f) = (chat('a'), chat('b'), chat('c'), chat('d'), chat('e'), chat('f'));
    let conversation = |id: &str, plan: Option<&str>| ChiefConversation {
        title: String::new(),
        plan: plan.map(str::to_string),
        thread: None,
        agent: atomics_core::config::ChiefAgent::Claude,
        conversation: id.into(),
        turns: Vec::new(),
        output: Vec::new(),
    };
    // `a` acquired the open board p1; `b` is the thread the open board p2 was planned in; `d` was closed here;
    // `e` and `f` made boards that are over, and only `e`'s chief still runs.
    let history = [
        conversation(&a, Some("p1")),
        conversation(&b, None),
        conversation(&c, None),
        conversation(&e, Some("p3")),
        conversation(&f, Some("p4")),
    ];
    let opened = [c.clone(), d.clone(), b.clone()];
    let dismissed = std::collections::HashSet::from([d.clone()]);
    let boards = [("p1", ""), ("p2", b.as_str())];
    let rows = chat_rows(&history, &opened, &boards, &dismissed, |thread| thread == e);
    assert_eq!(rows, [c, e]);
}

#[test]
fn the_chief_at_work_shows_its_step_and_each_subagent_at_work() {
    use atomics_core::projection::{ActivityInfo, ActivityStep, ThreadInfo};
    assert!(now_lines(None).is_empty());
    let thread = |kind: &str, running: bool, steps: u32, tool: Option<&str>| ThreadInfo {
        id: kind.into(),
        kind: kind.into(),
        running,
        steps,
        tool: tool.map(str::to_string),
    };
    let activity = ActivityInfo {
        recent_tools: vec![ActivityStep::from_tool("mcp__relay__get_ticket"), ActivityStep::from_tool("Bash")],
        threads: vec![
            thread("Explore", true, 12, Some("Grep")),
            thread("general-purpose", false, 40, Some("Read")),
            thread("", true, 1, None),
        ],
        ..Default::default()
    };
    assert_eq!(
        now_lines(Some(&activity)),
        ["Using get_ticket", "Explore agent · 12 steps · Grep…", "A subagent · 1 step"]
    );
    let reading = ActivityInfo { recent_tools: vec![ActivityStep::from_tool("Read")], ..Default::default() };
    assert_eq!(now_lines(Some(&reading)), ["Reading"]);
}

#[test]
fn a_message_to_a_board_in_a_thread_of_its_own_is_shown_once_when_delivered() {
    let thread = format!("chat-{}", "a".repeat(32));
    let out = |text: &str, provider_chat: &str| Outgoing::new(provider_chat.into(), text.into(), None, false);
    let entry = |text: &str, chat: Option<&str>, plan: Option<&str>| {
        let mut log = TraceLog::default();
        let mut entry = log
            .ingest_for_plan(&said("UserPromptSubmit", "prompt", text, "provider"), None, &Cleaner::default(), 1, plan)
            .unwrap()
            .clone();
        entry.chat_id = chat.map(str::to_owned);
        entry
    };
    let text = "[Plan plan-1]\nIn relay-backend: close the ticket";
    // The board's chief runs in its own thread: its turns carry that thread as their chat.
    let user = entry(text, Some(thread.as_str()), Some("plan-1"));
    assert!(delivered(&out(text, &thread), &user));
    // The workspace chief's turns carry no chat, and a named chat's carry its own.
    assert!(delivered(&out(text, ""), &entry(text, None, Some("plan-1"))));
    let chat = format!("chat-{}", "b".repeat(32));
    let named = format!("[Chat {chat}]\nHello");
    assert!(delivered(&out(&named, ""), &entry("Hello", Some(chat.as_str()), None)));
    // Not yet in the conversation, or only in another thread's: still on its way.
    assert!(!delivered(&out(text, &thread), &entry("other", None, Some("plan-1"))));
    assert!(!delivered(&out(text, ""), &user));
}

#[test]
fn the_chief_waits_for_the_heartbeat_between_turns_of_open_work_and_in_atomics_wait() {
    use atomics_core::projection::{ActivityInfo, ActivityStep};
    let thread = format!("chat-{}", "a".repeat(32));
    let chief = |status: AgentStatus, tool: Option<&str>| SessionRow {
        context: None,
        thread: Some(thread.clone()),
        id: "chief".into(),
        title: "chief".into(),
        agent: "claude".into(),
        kind: atomics_core::config::AgentKind::Claude,
        status,
        status_hint: None,
        chief: true,
        worktree: None,
        branch: None,
        root: std::path::PathBuf::from("/work"),
        project: None,
        ungrouped: false,
        task: None,
        scratch: false,
        sandbox: None,
        outside: false,
        can_receive: true,
        pending_approvals: 0,
        held_messages: 0,
        changes: None,
        activity: tool
            .map(|tool| ActivityInfo { recent_tools: vec![ActivityStep::from_tool(tool)], ..Default::default() }),
    };
    let board = |thread: &str, status: PlanStatus| crate::PlanRow {
        thread: thread.into(),
        created_at: 0,
        first_message: None,
        id: "p".into(),
        goal: "Ship it".into(),
        tab_name: "Ship it".into(),
        status,
        state: PlanState::Idle,
        approved: true,
        changed: false,
        done: 0,
        total: 2,
        attention: 0,
        last_active: 0,
        sessions: Vec::new(),
    };
    let open = [board(&thread, PlanStatus::Open)];
    assert!(waits_for_heartbeat(&chief(AgentStatus::Idle, None), &open));
    assert!(waits_for_heartbeat(&chief(AgentStatus::Working, Some("mcp__atomics__atomics_wait")), &open));
    // At work it spins; with nothing open nothing wakes it; another thread's board is not its own.
    assert!(!waits_for_heartbeat(&chief(AgentStatus::Working, Some("Bash")), &open));
    assert!(!waits_for_heartbeat(&chief(AgentStatus::Idle, None), &[board(&thread, PlanStatus::Completed)]));
    assert!(!waits_for_heartbeat(&chief(AgentStatus::Idle, None), &[board("", PlanStatus::Open)]));
}
