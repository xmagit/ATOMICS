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

fn said(name: &str, field: &str, text: &str, session: &str) -> HookEvent {
    HookEvent::from_payload(&serde_json::json!({"hook_event_name": name, "session_id": session, field: text})).unwrap()
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
fn a_providers_quota_is_its_least_left_and_says_when_it_is_unknown_or_stale() {
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
    assert_eq!(quota(&old, now), Quota::Stale(Some(40.0)));
    let reset = Usage { session: window(5.0, now - 1), weekly: None, observed_at: now - 60, ..Usage::default() };
    assert_eq!(quota(&reset, now), Quota::Stale(None), "its window reset since the reading");
    let none = Usage { observed_at: now - 60, ..Usage::default() };
    assert_eq!(quota(&none, now), Quota::Unknown);
    let running_low = Usage { session: window(12.0, now + 600), observed_at: now - 60, ..Usage::default() };
    assert!(low(quota(&running_low, now)));
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

#[test]
fn a_chat_is_titled_without_the_scope_atomics_put_before_its_prompt() {
    assert_eq!(history::title("[Plan plan-1]\nIn relay: use a Sol agent"), "In relay: use a Sol agent");
    assert_eq!(history::title("[Plan plan-1] Time to clean up"), "Time to clean up");
    assert_eq!(history::title("[Chat chat-1]"), "");
    assert_eq!(history::title("Plan the week"), "Plan the week");
}

#[test]
fn the_rail_lists_a_chat_once_and_never_beside_its_board() {
    use atomics_core::projection::ChiefConversation;
    let chat = |n: char| format!("chat-{}", n.to_string().repeat(32));
    let (a, b, c, d) = (chat('a'), chat('b'), chat('c'), chat('d'));
    let conversation = |id: &str, plan: Option<&str>| ChiefConversation {
        title: String::new(),
        plan: plan.map(str::to_string),
        thread: None,
        agent: atomics_core::config::ChiefAgent::Claude,
        conversation: id.into(),
        turns: Vec::new(),
        output: Vec::new(),
    };
    // `a` acquired a board, `b` is the thread a board was planned in, `d` was closed here.
    let history = [conversation(&a, Some("p1")), conversation(&b, None), conversation(&c, None)];
    let opened = [c.clone(), d.clone(), b.clone()];
    let dismissed = std::collections::HashSet::from([d.clone()]);
    let rows = chat_rows(&history, &opened, [b.as_str(), ""].into_iter(), &dismissed);
    assert_eq!(rows, [c]);
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
    let out = |text: &str, provider_chat: &str| Outgoing {
        provider_chat: provider_chat.into(),
        text: text.into(),
        plan: None,
        at: 0.0,
        sent: true,
    };
    let text = "[Plan plan-1]\nIn relay-backend: close the ticket";
    // The board's chief runs in its own thread: its turns carry that thread as their chat.
    let users = std::collections::HashSet::from([(Some(thread.as_str()), text)]);
    assert!(delivered(&out(text, &thread), &users));
    // The workspace chief's turns carry no chat, and a named chat's carry its own.
    assert!(delivered(&out(text, ""), &std::collections::HashSet::from([(None, text)])));
    let chat = format!("chat-{}", "b".repeat(32));
    let named = format!("[Chat {chat}]\nHello");
    assert!(delivered(&out(&named, ""), &std::collections::HashSet::from([(Some(chat.as_str()), "Hello")])));
    // Not yet in the conversation, or only in another thread's: still on its way.
    assert!(!delivered(&out(text, &thread), &std::collections::HashSet::from([(None, "other")])));
    assert!(!delivered(&out(text, ""), &users));
}
