use super::*;
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
            Said::Planned(Some("p1".into()), 2),
            Said::Chief("On it.".into()),
        ]
    );
    assert!(conversation(&TraceData::default()).is_empty());
}

#[test]
fn a_clients_own_choice_is_kept_by_name_and_anything_else_follows_the_host() {
    for mode in [Some(UiMode::Simple), Some(UiMode::Full), None] {
        assert_eq!(named_mode(mode_name(mode)), mode);
    }
    assert_eq!(named_mode(" simple\n"), Some(UiMode::Simple));
    assert_eq!(named_mode("fancy"), None);
}
