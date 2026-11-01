//! Lifecycle hooks bridge Claude Code and Codex to the app over a Unix socket.
//! ATOMICS passes socket and pane arguments directly to the handler. Hook stdout
//! delivers context or answers a permission request.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PERMISSION_WAIT_SECONDS: u64 = 540;

use crate::session::AgentStatus;

/// Events ATOMICS subscribes to. Each one either changes a session's status or is a
/// chance to deliver queued messages.
pub const EVENTS: &[&str] =
    &["SessionStart", "UserPromptSubmit", "PostToolUse", "Notification", "Stop", "SessionEnd", "CwdChanged"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRequest {
    /// Explicit pane argument; absent for sessions started outside ATOMICS.
    pub pane: Option<String>,
    #[serde(default)]
    pub ancestors: Vec<u32>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookReply {
    /// Printed verbatim to stdout for the agent to act on.
    pub stdout: Option<String>,
}

/// The fields of a hook payload ATOMICS cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEvent {
    pub name: String,
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    pub notification_type: Option<String>,
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Value,
    pub tool_use_id: Option<String>,
    pub tool_response: Value,
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
            tool_input: payload.get("tool_input").cloned().unwrap_or(Value::Null),
            tool_use_id: text("tool_use_id"),
            tool_response: payload.get("tool_response").cloned().unwrap_or(Value::Null),
            last_assistant_message: text("last_assistant_message"),
            stop_hook_active: payload.get("stop_hook_active").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// The status this event moves the session into, if it says anything about it.
    pub fn status(&self) -> Option<AgentStatus> {
        match self.name.as_str() {
            "SessionStart" => Some(AgentStatus::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Some(AgentStatus::Working),
            "PermissionRequest" => Some(AgentStatus::NeedsInput),
            "Notification" => match self.notification_type.as_deref() {
                Some("permission_prompt") => Some(AgentStatus::NeedsInput),
                _ => None,
            },
            "Stop" => Some(AgentStatus::Done),
            "SessionEnd" => Some(AgentStatus::Exited),
            "Interrupt" => Some(AgentStatus::Idle),
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

/// Settings that point every ATOMICS event at `atomics hook`. Passed to agents ATOMICS starts
/// with `--settings`; identical handlers in other settings files run only once.
pub fn claude_settings(atomics_exe: &Path) -> Value {
    let command = format!("{} hook", shell_quote(&atomics_exe.to_string_lossy()));
    let handler = json!([{ "hooks": [{ "type": "command", "command": command, "timeout": 10 }] }]);
    let hooks: serde_json::Map<String, Value> =
        EVENTS.iter().map(|event| (event.to_string(), handler.clone())).collect();
    json!({ "hooks": hooks })
}

/// Session-local handlers carry their destination without exporting it to the agent.
pub fn session_settings(atomics_exe: &Path, socket: &Path, pane: &str, codex: bool) -> Value {
    let mut command = format!(
        "{} hook --socket {}",
        shell_quote(&atomics_exe.to_string_lossy()),
        shell_quote(&socket.to_string_lossy())
    );
    if !codex {
        command.push_str(&format!(" --pane {}", shell_quote(pane)));
    }
    let events: &[&str] = if codex {
        &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PermissionRequest",
            "PostToolUse",
            "Stop",
            "SessionEnd",
            "Interrupt",
        ]
    } else {
        EVENTS
    };
    let hooks: serde_json::Map<String, Value> = events
        .iter()
        .map(|event| {
            let timeout = match *event {
                "PermissionRequest" => PERMISSION_WAIT_SECONDS + 60,
                "Interrupt" => 3,
                _ => 15,
            };
            (event.to_string(), json!([{ "hooks": [{ "type": "command", "command": command, "timeout": timeout }] }]))
        })
        .collect();
    let mut settings = json!({ "hooks": hooks });
    if !codex {
        settings["statusLine"] = json!({
            "type": "command",
            "command": format!("{} statusline --socket {}", shell_quote(&atomics_exe.to_string_lossy()), shell_quote(&socket.to_string_lossy()))
        });
    }
    settings
}

/// Only chief sessions pay for MCP start hooks; ordinary Claude sessions keep EVENTS.
pub fn chief_session_settings(atomics_exe: &Path, socket: &Path, pane: &str) -> Value {
    let mut settings = session_settings(atomics_exe, socket, pane, false);
    for groups in settings["hooks"].as_object_mut().expect("hooks").values_mut() {
        for group in groups.as_array_mut().expect("groups") {
            for hook in group["hooks"].as_array_mut().expect("handlers") {
                let command = hook["command"].as_str().expect("command");
                hook["command"] = json!(format!("ATOMICS_CHIEF=1 {command}"));
            }
        }
    }
    let mut groups = settings["hooks"]["PostToolUse"].clone();
    groups[0]["matcher"] = json!("mcp__.*");
    settings["hooks"]["PreToolUse"] = groups;
    settings
}

/// TOML CLI overrides use the same schema as hooks.json.
pub fn codex_hook_args(atomics_exe: &Path, socket: &Path, pane: &str) -> Vec<String> {
    let settings = session_settings(atomics_exe, socket, pane, true);
    let mut args = Vec::new();
    for (event, groups) in settings["hooks"].as_object().expect("hooks") {
        let groups: toml::Value = serde_json::from_value(groups.clone()).expect("hook groups");
        args.extend(["-c".into(), format!("hooks.{event}={groups}")]);
    }
    args
}

/// Walk a ps PID/PPID table, nearest ancestor first. PID identity avoids ambiguous cwd matches.
pub fn ancestor_pids(pid: u32, table: &str) -> Vec<u32> {
    let parents: std::collections::HashMap<u32, u32> = table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut ancestors = Vec::new();
    let mut current = pid;
    while let Some(&parent) = parents.get(&current) {
        if parent <= 1 || parent == pid || ancestors.contains(&parent) {
            break;
        }
        ancestors.push(parent);
        current = parent;
    }
    ancestors
}

pub fn codex_supports_hooks(version: &str) -> Option<bool> {
    let version = version.strip_prefix("codex-cli ")?;
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next()?.parse::<u32>().ok()?;
    Some(major > 0 || minor >= 165)
}

/// Merges ATOMICS's hooks into an existing settings document (for `atomics install-hooks`),
/// leaving every other handler alone and never adding the same one twice.
pub fn merge_into_settings(existing: &mut Value, atomics_exe: &Path) {
    let ours = claude_settings(atomics_exe);
    if !existing.is_object() {
        *existing = json!({});
    }
    let hooks = existing.as_object_mut().expect("object").entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    for groups in hooks.as_object_mut().expect("object").values_mut().filter_map(Value::as_array_mut) {
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
            let before = handlers.len();
            handlers.retain(|handler| !is_cox_hook(handler));
            handlers.len() == before || !handlers.is_empty()
        });
    }
    hooks.as_object_mut().expect("object").retain(|_, groups| !groups.as_array().is_some_and(Vec::is_empty));
    for (event, groups) in ours["hooks"].as_object().expect("object") {
        let list = hooks.as_object_mut().expect("object").entry(event.clone()).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else { continue };
        let group = &groups[0];
        if !list.contains(group) {
            list.push(group.clone());
        }
    }
}

/// Old handlers keep invoking a binary that no longer exists after a rename (cox, then IMACS).
pub fn has_cox_hooks(settings: &Value) -> bool {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|events| events.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .any(is_cox_hook)
}

fn is_cox_hook(handler: &Value) -> bool {
    if handler.get("type").and_then(Value::as_str) != Some("command") {
        return false;
    }
    let Some(words) = handler.get("command").and_then(Value::as_str).and_then(shlex::split) else { return false };
    words.first().is_some_and(|exe| Path::new(exe).file_name().is_some_and(|name| name == "cox" || name == "imacs"))
        && words.get(1).is_some_and(|arg| arg == "hook")
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
    fn chief_mcp_pre_hooks_are_session_local_and_do_not_change_status() {
        let chief = chief_session_settings(Path::new("atomics"), Path::new("own.sock"), "chief");
        assert_eq!(chief["hooks"]["PreToolUse"][0]["matcher"], "mcp__.*");
        assert!(
            chief["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap().starts_with("ATOMICS_CHIEF=1 ")
        );
        assert!(
            !session_settings(Path::new("atomics"), Path::new("own.sock"), "worker", false)["hooks"]["PostToolUse"][0]
                ["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("ATOMICS_CHIEF")
        );
        assert!(chief["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("--pane chief"));
        assert!(
            session_settings(Path::new("atomics"), Path::new("own.sock"), "worker", false)["hooks"]
                .get("PreToolUse")
                .is_none()
        );
        assert!(claude_settings(Path::new("atomics"))["hooks"].get("PreToolUse").is_none());
        let payload = json!({"hook_event_name":"PostToolUse", "session_id":"chief", "tool_name":"mcp__example__read", "tool_use_id":"toolu_one", "tool_response":{"structuredContent":{"id":"1"}}});
        let event = HookEvent::from_payload(&payload).unwrap();
        assert_eq!(event.tool_use_id.as_deref(), Some("toolu_one"));
        assert_eq!(event.tool_response["structuredContent"]["id"], "1");
        assert_eq!(event.status(), Some(AgentStatus::Working));
        let missing =
            HookEvent::from_payload(&json!({"hook_event_name":"PostToolUse", "session_id":"worker"})).unwrap();
        assert_eq!(missing.tool_response, Value::Null);
        assert_eq!(missing.tool_use_id, None);
    }

    #[test]
    fn session_hooks_route_explicitly_and_codex_overrides_are_valid_toml() {
        let exe = Path::new("/Applications/My Tools/atomics");
        let socket = Path::new("/tmp/own instance/socket");
        for codex in [false, true] {
            let settings = session_settings(exe, socket, "pane-1", codex);
            let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str().unwrap();
            let mut expected = vec![exe.to_str().unwrap(), "hook", "--socket", socket.to_str().unwrap()];
            if !codex {
                expected.extend(["--pane", "pane-1"]);
            }
            assert_eq!(shlex::split(command).unwrap(), expected);
            assert!(!command.contains("ATOMICS_SOCKET"));
            assert_eq!(settings["hooks"].get("PermissionRequest").is_some(), codex);
            if codex {
                assert_eq!(settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"], 600);
                assert!(settings.get("statusLine").is_none());
            } else {
                assert_eq!(settings["statusLine"]["type"], "command");
                assert_eq!(
                    shlex::split(settings["statusLine"]["command"].as_str().unwrap()).unwrap(),
                    [exe.to_str().unwrap(), "statusline", "--socket", socket.to_str().unwrap()]
                );
            }
        }
        assert_eq!(session_settings(exe, socket, "pane-1", true), session_settings(exe, socket, "pane-2", true));
        assert_eq!(ancestor_pids(40, "PID PPID\n40 30\n30 20\n20 1\n99 20"), vec![30, 20]);
        assert_eq!(ancestor_pids(40, "40 30\n30 40"), vec![30]);
        assert!(ancestor_pids(40, "invalid").is_empty());
        let args = codex_hook_args(exe, socket, "pane-1");
        for pair in args.as_chunks::<2>().0 {
            assert_eq!(pair[0], "-c");
            let parsed: toml::Value = toml::from_str(&pair[1]).unwrap();
            assert!(parsed["hooks"].is_table());
        }
        for version in ["codex-cli 0.165.0", "codex-cli 0.166.0-alpha.1", "codex-cli 1.0.0"] {
            assert_eq!(codex_supports_hooks(version), Some(true));
        }
        assert_eq!(codex_supports_hooks("codex-cli 0.164.9"), Some(false));
        for version in ["codex-cli unknown", "unknown"] {
            assert_eq!(codex_supports_hooks(version), None);
        }
    }

    #[test]
    fn parses_recorded_codex_0165_permission_completion_and_interrupt() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/codex-0.165-hooks.json")).unwrap();
        assert_eq!(fixture["recorded_codex_version"], "codex-cli 0.165.0");
        let events: Vec<_> = fixture["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|payload| HookEvent::from_payload(payload).unwrap())
            .collect();
        assert_eq!(
            events.iter().map(|event| event.name.as_str()).collect::<Vec<_>>(),
            ["PermissionRequest", "PostToolUse", "Interrupt"]
        );
        assert_eq!(
            events.iter().map(HookEvent::status).collect::<Vec<_>>(),
            [Some(AgentStatus::NeedsInput), Some(AgentStatus::Working), Some(AgentStatus::Idle)]
        );
        assert!(events.iter().all(|event| event.session_id == events[0].session_id && event.cwd == events[0].cwd));
        assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
        let permission = crate::approvals::ApprovalRequest::from_event(&events[0]);
        let completed = crate::approvals::ApprovalRequest::from_event(&events[1]);
        assert!(permission.command.as_deref().unwrap().ends_with("/fixture-allowed"));
        assert_eq!(permission.reason.as_deref(), Some("Record Codex 0.165 hook payload"));
        assert!(completed.reason.is_none());
        assert!(permission.same_command(&completed));
        assert!(events[2].tool_name.is_none());
        assert!(events[2].tool_input.is_null());
    }

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
        let exe = Path::new("/Applications/My Tools/atomics");
        let settings = claude_settings(exe);
        for event in EVENTS {
            let command = settings["hooks"][event][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(command, "'/Applications/My Tools/atomics' hook");
        }

        let mut existing = json!({ "model": "opus", "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "say done" }] }] } });
        merge_into_settings(&mut existing, exe);
        merge_into_settings(&mut existing, exe);
        assert_eq!(existing["model"], "opus");
        assert_eq!(existing["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(existing["hooks"]["CwdChanged"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_removes_legacy_handlers_and_preserves_other_hooks() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"matcher": "*", "hooks": [
                    {"type": "command", "command": "'/Applications/My Tools/cox' hook", "timeout": 10},
                    {"type": "command", "command": "say done"}
                ]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "cox hook"}]}],
                "SessionStart": [{"hooks": [{"type": "command", "command": "/usr/local/bin/cox hook"}]}]
            }
        });
        assert!(has_cox_hooks(&settings));
        let exe = Path::new("/Applications/My Tools/atomics");
        merge_into_settings(&mut settings, exe);
        assert!(!has_cox_hooks(&settings));
        assert_eq!(settings["model"], "opus");
        assert_eq!(
            settings["hooks"]["Stop"][0],
            json!({"matcher": "*", "hooks": [
                {"type": "command", "command": "say done"}
            ]})
        );
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert!(settings["hooks"].get("PreToolUse").is_none());
        assert_eq!(settings["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        let once = settings.clone();
        merge_into_settings(&mut settings, exe);
        assert_eq!(settings, once);
    }

    #[test]
    fn legacy_detection_matches_the_executable_and_subcommand() {
        for command in ["cox hook", "/tmp/cox hook", "'/path with spaces/cox' hook", "\"/tmp/cox\" hook"] {
            assert!(is_cox_hook(&json!({"type": "command", "command": command})), "{command}");
        }
        for command in ["echo cox hook", "atomics hook", "my-cox hook", "cox doctor", "cox hook-status", "'cox hook'"] {
            assert!(!is_cox_hook(&json!({"type": "command", "command": command})), "{command}");
        }
        assert!(!is_cox_hook(&json!({"type": "prompt", "command": "cox hook"})));
    }
}
