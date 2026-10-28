//! The bridge between Claude Code hooks and the app.
//!
//! Claude Code runs `cox hook` for each configured event and pipes the event JSON
//! to it. `cox hook` forwards that JSON, plus the `COX_PANE` it inherited when cox
//! started the agent, to the app over a unix socket as one line, and prints
//! whatever the app answers. The answer is how messages reach sessions that run
//! outside cox: they are delivered as `additionalContext` on the next tool call,
//! prompt or turn end.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::AgentStatus;

/// Events cox subscribes to. Each one either changes a session's status or is a
/// chance to deliver queued messages.
pub const EVENTS: &[&str] =
    &["SessionStart", "UserPromptSubmit", "PostToolUse", "Notification", "Stop", "SessionEnd", "CwdChanged"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRequest {
    /// `COX_PANE` of the process that ran the hook; absent for sessions started outside cox.
    pub pane: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookReply {
    /// Printed verbatim to stdout for Claude Code to act on.
    pub stdout: Option<String>,
}

/// The fields of a hook payload cox cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEvent {
    pub name: String,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub last_assistant_message: Option<String>,
    pub stop_hook_active: bool,
}

impl HookEvent {
    pub fn from_payload(payload: &Value) -> Option<HookEvent> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        let name = text("hook_event_name")?;
        let session_id = text("session_id")?;
        // CwdChanged reports the new directory separately from the common `cwd`.
        let cwd = text("new_cwd").or_else(|| text("cwd")).map(PathBuf::from);
        Some(HookEvent {
            name,
            session_id,
            cwd,
            prompt: text("prompt"),
            notification_type: text("notification_type"),
            message: text("message"),
            tool_name: text("tool_name"),
            last_assistant_message: text("last_assistant_message"),
            stop_hook_active: payload.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// The status this event moves the session into, if it says anything about it.
    pub fn status(&self) -> Option<AgentStatus> {
        match self.name.as_str() {
            "SessionStart" => Some(AgentStatus::Idle),
            "UserPromptSubmit" | "PostToolUse" => Some(AgentStatus::Working),
            "Notification" => match self.notification_type.as_deref() {
                Some("permission_prompt") => Some(AgentStatus::NeedsInput),
                _ => None,
            },
            "Stop" => Some(AgentStatus::Done),
            "SessionEnd" => Some(AgentStatus::Exited),
            _ => None,
        }
    }

    /// Whether Claude Code reads `additionalContext` from this event's output.
    pub fn can_deliver(&self) -> bool {
        matches!(self.name.as_str(), "SessionStart" | "UserPromptSubmit" | "PostToolUse" | "Stop")
    }
}

/// Builds the hook stdout that hands `context` to Claude. On `Stop` this also keeps
/// the turn going, so a queued message is acted on rather than just noted.
pub fn context_output(event: &str, context: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Settings that point every cox event at `cox hook`. Passed to agents cox starts
/// with `--settings`; identical handlers in other settings files run only once.
pub fn claude_settings(cox_exe: &Path) -> Value {
    let command = format!("{} hook", shell_quote(&cox_exe.to_string_lossy()));
    let handler = json!([{ "hooks": [{ "type": "command", "command": command, "timeout": 10 }] }]);
    let hooks: serde_json::Map<String, Value> =
        EVENTS.iter().map(|event| (event.to_string(), handler.clone())).collect();
    json!({ "hooks": hooks })
}

/// Merges cox's hooks into an existing settings document (for `cox install-hooks`),
/// leaving every other handler alone and never adding the same one twice.
pub fn merge_into_settings(existing: &mut Value, cox_exe: &Path) {
    let ours = claude_settings(cox_exe);
    if !existing.is_object() {
        *existing = json!({});
    }
    let hooks = existing.as_object_mut().expect("object").entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    for (event, groups) in ours["hooks"].as_object().expect("object") {
        let list = hooks.as_object_mut().expect("object").entry(event.clone()).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else { continue };
        let group = &groups[0];
        if !list.contains(group) {
            list.push(group.clone());
        }
    }
}

fn shell_quote(text: &str) -> String {
    if text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cwd_changes_and_statuses() {
        let event = HookEvent::from_payload(&json!({
            "hook_event_name": "CwdChanged", "session_id": "s1",
            "cwd": "/old", "old_cwd": "/old", "new_cwd": "/new"
        }))
        .unwrap();
        assert_eq!(event.cwd, Some(PathBuf::from("/new")));
        assert_eq!(event.status(), None);

        let permission = HookEvent::from_payload(&json!({
            "hook_event_name": "Notification", "session_id": "s1", "cwd": "/x",
            "notification_type": "permission_prompt", "message": "Claude needs your permission"
        }))
        .unwrap();
        assert_eq!(permission.status(), Some(AgentStatus::NeedsInput));
        assert!(!permission.can_deliver());

        assert!(HookEvent::from_payload(&json!({ "session_id": "s1" })).is_none());
    }

    #[test]
    fn context_output_shape() {
        let out: Value = serde_json::from_str(&context_output("Stop", "do X")).unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "Stop");
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "do X");
    }

    #[test]
    fn settings_cover_every_event_and_merge_idempotently() {
        let exe = Path::new("/Applications/My Tools/cox");
        let settings = claude_settings(exe);
        for event in EVENTS {
            let command = settings["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(command, "'/Applications/My Tools/cox' hook");
        }

        let mut existing = json!({ "model": "opus", "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }] } });
        merge_into_settings(&mut existing, exe);
        merge_into_settings(&mut existing, exe);
        assert_eq!(existing["model"], "opus");
        assert_eq!(existing["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(existing["hooks"]["CwdChanged"].as_array().unwrap().len(), 1);
    }
}
