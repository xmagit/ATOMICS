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
