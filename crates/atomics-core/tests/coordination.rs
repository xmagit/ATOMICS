use atomics_core::{config::Config, hook, isolation, plan::NotebookEntry};
use serde_json::json;
use std::path::Path;

#[test]
fn coordination_defaults_and_opt_in_roundtrip() {
    let config: Config = toml::from_str(
        "[plan]\nnotebook=true\ncoordination='allow'\nnative_agent_messaging=false\n[chief]\nnative_messaging='refuse'",
    )
    .unwrap();
    let value = serde_json::to_value(config).unwrap();
    assert_eq!(value["plan"]["coordination"], "allow");
    assert_eq!(value["plan"]["native_agent_messaging"], false);
    assert_eq!(value["chief"]["native_messaging"], "refuse");
    let defaults = serde_json::to_value(Config::default()).unwrap();
    assert_eq!(defaults["plan"]["coordination"], "relay");
    assert_eq!(defaults["plan"]["native_agent_messaging"], true);
    assert_eq!(defaults["chief"]["native_messaging"], "allow");
}

#[test]
fn coordination_task_native_messaging_is_allowed_by_default_and_can_be_refused() {
    let exe = Path::new("atomics");
    let socket = Path::new("own.sock");
    for mut settings in [hook::session_settings(exe, socket, "worker", false), isolation::claude_settings()] {
        hook::native_messaging(&mut settings, Config::default().plan.native_agent_messaging);
        assert!(settings.get("crossSessionInbound").is_none());
        assert!(settings["permissions"].get("deny").is_none());
        hook::native_messaging(&mut settings, false);
        assert_eq!(settings["crossSessionInbound"], "refuse");
        assert_eq!(settings["permissions"]["deny"], json!(["SendMessage", "ListAgents"]));
    }
    let chief = hook::chief_session_settings(exe, socket, "chief");
    assert!(chief.get("crossSessionInbound").is_none());
    assert!(chief["permissions"].get("deny").is_none());
}

#[test]
fn coordination_sandbox_adapters_only_offer_data_boundaries() {
    for agent in ["claude", "codex"] {
        let settings = hook::sandbox_notebook_settings(agent);
        let hooks = settings["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 3);
        for event in ["SessionStart", "UserPromptSubmit", "PostToolUse"] {
            let command = hooks[event][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(command.contains("atomics_note_delivery"));
            assert!(!command.contains("ATOMICS_NOTE_CAPABILITY"));
            assert!(!command.contains("--socket"));
            assert!(command.ends_with(&format!(" {agent}")));
        }
    }
}

#[test]
fn coordination_addressed_notebook_entries_keep_session_thread_and_visibility() {
    let value = json!({"id":1,"task":"a","agent":"test","at":42,"text":"fact","kind":"message",
        "session":"session-a","to":"b","reply_to":3,"hidden":true});
    let entry: NotebookEntry = serde_json::from_value(value).unwrap();
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(value["session"], "session-a");
    assert_eq!(value["to"], "b");
    assert_eq!(value["reply_to"], 3);
    assert_eq!(value["hidden"], true);
    assert_eq!(entry.as_data()["provenance"]["session"], "session-a");
}
