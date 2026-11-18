//! Lifecycle hooks bridge Claude Code and Codex to the app over a Unix socket.
//! ATOMICS passes socket and pane arguments directly to the handler. Hook stdout
//! delivers context or answers a permission request.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PERMISSION_WAIT_SECONDS: u64 = 540;

/// The pane of a session whose hook commands cannot name it. Codex trusts hook definitions by
/// their text, so its handlers stay identical across panes; ATOMICS sets this in the session's
/// environment instead, which the handlers inherit even through a wrapper on another machine.
pub const HOOK_PANE_ENV: &str = "ATOMICS_HOOK_PANE";

use crate::session::AgentStatus;

/// Events ATOMICS subscribes to. Each one either changes a session's status or is a
/// chance to deliver queued messages.
pub const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "SessionEnd",
    "CwdChanged",
];

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

/// Which of an agent's threads a hook event comes from. All of them inherit the session's pane,
/// so only the payload can tell them apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HookThread {
    /// The thread the session follows. Its id changes with `/clear` or `/resume`.
    #[default]
    Main,
    /// A thread the main one spawned. It reports under the main thread's session id with an
    /// `agent_id` of its own, and its permission requests are the session's to answer.
    Spawned,
    /// A thread the agent runs for itself, such as Codex's memory agent. It has its own session id
    /// and, as the agent keeps no transcript of it, a null `transcript_path`.
    Background,
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
    /// A failed tool call the user interrupted (Claude's `is_interrupt`).
    pub interrupted: bool,
    pub thread: HookThread,
}

impl HookEvent {
    pub fn is_trace_call(&self) -> bool {
        matches!(self.name.as_str(), "PreToolUse" | "PostToolUse")
            && self.tool_name.as_deref().is_some_and(|tool| tool.starts_with("mcp__"))
    }

    pub fn from_payload(payload: &Value) -> Option<HookEvent> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_string);
        let name = text("hook_event_name")?;
        let session_id = text("session_id")?;
        // CwdChanged reports the new directory separately from the common `cwd`.
        let cwd = text("new_cwd").or_else(|| text("cwd")).map(PathBuf::from);
        let thread = if payload.get("transcript_path").is_some_and(Value::is_null) {
            HookThread::Background
        } else if text("agent_id").is_some() {
            HookThread::Spawned
        } else {
            HookThread::Main
        };
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
            interrupted: payload.get("is_interrupt").and_then(Value::as_bool).unwrap_or(false),
            thread,
        })
    }

    pub fn heavy_check_warning(&self) -> Option<&'static str> {
        if self.name != "PreToolUse" {
            return None;
        }
        let command = self.tool_input.get("command").or_else(|| self.tool_input.get("cmd"))?.as_str()?;
        crate::governor::direct_heavy_command(command).then_some("Heavy check outside the queue. Run it through atomics queue --class test -- <command> so slots and free memory can protect this host. This warning does not block the command.")
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
            // Claude sends no Stop for a turn the user interrupts. A tool it was running then fails as
            // interrupted; any other failure leaves the turn going.
            "PostToolUseFailure" if self.interrupted => Some(AgentStatus::Idle),
            _ => None,
        }
    }

    /// Whether Claude Code reads `additionalContext` from this event's output, in the thread a
    /// message for the session is meant for.
    pub fn can_deliver(&self) -> bool {
        self.thread == HookThread::Main
            && matches!(self.name.as_str(), "SessionStart" | "UserPromptSubmit" | "PostToolUse" | "Stop")
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

/// Claude permission rules that make a Claude agent ATOMICS starts on this machine ask before it drops
/// entries of the stash every worktree shares, whatever else its settings allow. Claude asks in its own
/// terminal: its permission prompts do not reach ATOMICS's approval policy, which handles Codex's.
/// A `:*` rule matches the command's start; for git's own options before `stash` (`git -C ../app`,
/// `--no-pager`), a `*` matches any text, and only a rule's one trailing ` *` may match nothing.
pub const CLAUDE_STASH_ASK: [&str; 12] = [
    "Bash(git stash drop:*)",
    "Bash(git stash pop:*)",
    "Bash(git stash clear:*)",
    "Bash(git stash branch:*)",
    "Bash(git * stash drop)",
    "Bash(git * stash drop *)",
    "Bash(git * stash pop)",
    "Bash(git * stash pop *)",
    "Bash(git * stash clear)",
    "Bash(git * stash clear *)",
    "Bash(git * stash branch)",
    "Bash(git * stash branch *)",
];

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
    let mut events = events.to_vec();
    if !codex {
        events.push("PreToolUse");
    }
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
        settings["permissions"] = json!({ "ask": CLAUDE_STASH_ASK });
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
    native_messaging(&mut settings, true);
    for groups in settings["hooks"].as_object_mut().expect("hooks").values_mut() {
        for group in groups.as_array_mut().expect("groups") {
            for hook in group["hooks"].as_array_mut().expect("handlers") {
                let command = hook["command"].as_str().expect("command");
                hook["command"] = json!(format!("{command} --chief"));
            }
        }
    }
    settings
}

/// Only generated session settings are changed; user settings remain untouched.
pub fn native_messaging(settings: &mut Value, allow: bool) {
    if allow {
        settings.as_object_mut().expect("settings").remove("crossSessionInbound");
        if let Some(permissions) = settings["permissions"].as_object_mut() {
            permissions.remove("deny");
        }
    } else {
        settings["crossSessionInbound"] = json!("refuse");
        settings["permissions"]["deny"] = json!(["SendMessage", "ListAgents"]);
    }
}

/// TOML CLI overrides use the same schema as hooks.json.
pub fn codex_hook_args(atomics_exe: &Path, socket: &Path, pane: &str) -> Vec<String> {
    let settings = session_settings(atomics_exe, socket, pane, true);
    codex_settings_args(&settings)
}

/// Sandboxes use their existing MCP gateway; no host socket or capability enters the VM.
pub fn sandbox_notebook_settings(agent: &str) -> Value {
    let command = format!("python3 -c {} {}", shell_quote(include_str!("notebook_hook.py")), shell_quote(agent));
    let handler = json!([{"hooks":[{"type":"command","command":command,"timeout":15}]}]);
    let hooks: serde_json::Map<String, Value> = ["SessionStart", "UserPromptSubmit", "PostToolUse"]
        .iter()
        .map(|event| (event.to_string(), handler.clone()))
        .collect();
    json!({"hooks":hooks})
}

pub fn codex_settings_args(settings: &Value) -> Vec<String> {
    let mut args = Vec::new();
    for (event, groups) in settings["hooks"].as_object().expect("hooks") {
        let groups: toml::Value = serde_json::from_value(groups.clone()).expect("hook groups");
        args.extend(["-c".into(), format!("hooks.{event}={groups}")]);
    }
    args
}

/// One hook of [`codex_hook_args`] as Codex identifies it for trust. Codex runs it once its user
/// config holds `[hooks.state."<key>"] trusted_hash = "<hash>"`, which its `/hooks` review writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHookTrust {
    /// The event as ATOMICS names it in `-c hooks.<event>=…`.
    pub event: String,
    pub key: String,
    pub hash: String,
}

/// Codex's name for the config layer `-c` overrides form; its hooks' trust keys start with it.
pub const CODEX_SESSION_FLAGS_SOURCE: &str = "/<session-flags>/config.toml";

/// The trust key and hash of every hook [`codex_hook_args`] passes, computed as Codex 0.165+ does
/// (codex-rs/hooks `hook_key`, and `hook_hash` in engine/discovery.rs). The key is the layer, the
/// event's label and the group and handler index. The hash covers the normalized handler: its
/// timeout as Codex clamps it, its matcher only for events that take one, and the event's label.
pub fn codex_hook_trust(atomics_exe: &Path, socket: &Path) -> Vec<CodexHookTrust> {
    let settings = session_settings(atomics_exe, socket, "", true);
    let mut hooks = Vec::new();
    for (event, groups) in settings["hooks"].as_object().expect("hooks") {
        let label = codex_event_label(event).expect("an event Codex knows");
        for (group_index, group) in groups.as_array().expect("hook groups").iter().enumerate() {
            let matcher = group.get("matcher").filter(|_| codex_event_takes_matcher(event));
            for (handler_index, handler) in group["hooks"].as_array().expect("handlers").iter().enumerate() {
                let timeout = handler.get("timeout").and_then(Value::as_u64);
                let timeout = match event.as_str() {
                    "SessionEnd" | "Interrupt" => timeout.unwrap_or(1).clamp(1, 3),
                    _ => timeout.unwrap_or(600).max(1),
                };
                let mut normalized = json!({
                    "type": "command",
                    "command": handler["command"],
                    "timeout": timeout,
                    "async": handler.get("async").and_then(Value::as_bool).unwrap_or(false),
                });
                if let Some(message) = handler.get("statusMessage") {
                    normalized["statusMessage"] = message.clone();
                }
                let mut identity = json!({ "event_name": label, "hooks": [normalized] });
                if let Some(matcher) = matcher {
                    identity["matcher"] = matcher.clone();
                }
                hooks.push(CodexHookTrust {
                    event: event.clone(),
                    key: format!("{CODEX_SESSION_FLAGS_SOURCE}:{label}:{group_index}:{handler_index}"),
                    hash: codex_fingerprint(identity),
                });
            }
        }
    }
    hooks
}

/// Codex's `version_for_toml`: SHA-256 of the value as compact JSON with sorted keys.
fn codex_fingerprint(mut value: Value) -> String {
    use sha2::{Digest, Sha256};
    value.sort_all_objects();
    let digest = Sha256::digest(serde_json::to_vec(&value).expect("JSON"));
    format!("sha256:{}", digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
}

/// Codex's `hook_event_key_label`.
fn codex_event_label(event: &str) -> Option<&'static str> {
    Some(match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "PreModelRequest" => "pre_model_request",
        "PostModelRequest" => "post_model_request",
        "SessionStart" => "session_start",
        "SessionEnd" => "session_end",
        "UserPromptSubmit" => "user_prompt_submit",
        "SubagentStart" => "subagent_start",
        "SubagentStop" => "subagent_stop",
        "Stop" => "stop",
        "Interrupt" => "interrupt",
        _ => return None,
    })
}

/// Codex's `matcher_pattern_for_event`: other events ignore a matcher, also in the hash.
fn codex_event_takes_matcher(event: &str) -> bool {
    !matches!(event, "PreModelRequest" | "PostModelRequest" | "UserPromptSubmit" | "Stop" | "Interrupt")
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

pub(crate) fn shell_quote(text: &str) -> String {
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
    fn claude_command_hooks_include_pre_tool_use() {
        let settings = session_settings(Path::new("/bin/atomics"), Path::new("/tmp/isolated.sock"), "own", false);
        assert!(settings["hooks"]["PreToolUse"].is_array());
    }

    #[test]
    fn the_payload_tells_an_agents_threads_apart() {
        // The fields that differ in what Codex 0.166 sent one pane for its main thread, its memory
        // agent and a spawned agent, with the ids and paths shortened.
        let thread = |payload: Value| HookEvent::from_payload(&payload).unwrap().thread;
        for name in ["SessionStart", "UserPromptSubmit", "PreToolUse", "Stop", "SessionEnd"] {
            let main = json!({"hook_event_name":name, "session_id":"main", "cwd":"/work",
                "transcript_path":"/codex/sessions/rollout-main.jsonl"});
            assert_eq!(thread(main), HookThread::Main, "{name}");
            let memory = json!({"hook_event_name":name, "session_id":"memory", "cwd":"/codex/memories",
                "transcript_path":null});
            assert_eq!(thread(memory), HookThread::Background, "{name}");
        }
        for name in ["UserPromptSubmit", "PreToolUse", "PermissionRequest", "PostToolUse"] {
            let spawned = json!({"hook_event_name":name, "session_id":"main", "cwd":"/work",
                "transcript_path":"/codex/sessions/rollout-spawned.jsonl",
                "agent_id":"spawned", "agent_type":"default"});
            assert_eq!(thread(spawned), HookThread::Spawned, "{name}");
        }
        // An agent that sends neither field has one thread as far as ATOMICS can tell.
        assert_eq!(thread(json!({"hook_event_name":"Stop", "session_id":"other"})), HookThread::Main);
    }

    #[test]
    fn chief_mcp_pre_hooks_are_session_local_and_do_not_change_status() {
        let chief = chief_session_settings(Path::new("atomics"), Path::new("own.sock"), "chief");
        assert!(chief["hooks"]["PreToolUse"][0]["matcher"].is_null());
        assert!(chief["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str().unwrap().ends_with(" --chief"));
        assert!(
            !session_settings(Path::new("atomics"), Path::new("own.sock"), "worker", false)["hooks"]["PostToolUse"][0]
                ["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--chief")
        );
        assert!(chief["hooks"]["PreToolUse"][0]["hooks"][0]["command"].as_str().unwrap().contains("--pane chief"));
        assert!(
            session_settings(Path::new("atomics"), Path::new("own.sock"), "worker", false)["hooks"]
                .get("PreToolUse")
                .is_some()
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
                assert!(settings.get("permissions").is_none());
            } else {
                // Claude asks before it drops shared stash entries, even where its settings allow git.
                assert_eq!(settings["permissions"]["ask"], json!(CLAUDE_STASH_ASK));
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
        // The fingerprint vector of Codex's own test (codex-rs/config fingerprint_tests.rs).
        let table: toml::Table =
            toml::from_str("rows = [{ z = 2, a = 1 }, { z = 4, a = 3 }]\n[nested]\nz = \"text\"\na = true\n").unwrap();
        assert_eq!(
            codex_fingerprint(serde_json::to_value(table).unwrap()),
            "sha256:a5e4c8fbdbc75d185b0f2b5291bfb2de10e8c523d6c89248876fd78a97c83176"
        );
        for version in ["codex-cli 0.165.0", "codex-cli 0.166.0-alpha.1", "codex-cli 1.0.0"] {
            assert_eq!(codex_supports_hooks(version), Some(true));
        }
        assert_eq!(codex_supports_hooks("codex-cli 0.164.9"), Some(false));
        for version in ["codex-cli unknown", "unknown"] {
            assert_eq!(codex_supports_hooks(version), None);
        }
    }

    #[test]
    fn codex_trust_keys_and_hashes_match_what_codex_0165_lists_for_our_overrides() {
        // `hooks/list` of Codex 0.165's app-server, started with these `-c hooks.*` overrides.
        let listed = [
            ("session_start", "70afda6b0df1de224a62ba789d785ce775bba60539c4cde83240c360bd840388"),
            ("user_prompt_submit", "bee828a3ed88aa28c31775a1ab234a64479b938122be97feddabf502d1239c72"),
            ("pre_tool_use", "ecd947871d1b1c902431f400176051b253ba564c9b4c7436a81009a3f27c4327"),
            ("permission_request", "5d309a2fef30652bbf24fb3b7b055f20413d195c3752fe281a716c1575327293"),
            ("post_tool_use", "1ff441fd543ad37d64c79a288a754ac7bf7b366bfc8703d7ae31222b6fad0323"),
            ("stop", "d9be8d6089da4f6ad97408aa75431b919a2ab718757750c149a0feb479186abf"),
            ("session_end", "63c738e1fdf1353510101aabe6c4bb99fd5a6f6e29c91c09052ffe7a94476ebc"),
            ("interrupt", "3535f5fa6d4f34ee23c95d1c63fcca178d284f76813a8366c2ef3f672c2c57be"),
        ];
        let (exe, socket) = (Path::new("/usr/local/bin/atomics"), Path::new("/home/user/.atomics/atomics.sock"));
        let mut computed: Vec<_> =
            codex_hook_trust(exe, socket).into_iter().map(|hook| (hook.key, hook.hash)).collect();
        let mut expected: Vec<_> = listed
            .iter()
            .map(|(label, hash)| (format!("/<session-flags>/config.toml:{label}:0:0"), format!("sha256:{hash}")))
            .collect();
        computed.sort();
        expected.sort();
        assert_eq!(computed, expected);
        // Every pane passes the same hooks, and a new install path or socket asks for trust again.
        assert_eq!(codex_hook_trust(exe, socket), codex_hook_trust(exe, socket));
        let moved = codex_hook_trust(Path::new("/opt/atomics"), socket);
        assert!(moved.iter().zip(codex_hook_trust(exe, socket)).all(|(a, b)| a.key == b.key && a.hash != b.hash));
        // The hash covers what Codex normalizes; a handler field it does not model would go unhashed.
        let settings = session_settings(exe, socket, "", true);
        for groups in settings["hooks"].as_object().unwrap().values() {
            for group in groups.as_array().unwrap() {
                assert_eq!(group.as_object().unwrap().keys().collect::<Vec<_>>(), ["hooks"]);
                for handler in group["hooks"].as_array().unwrap() {
                    assert_eq!(handler.as_object().unwrap().keys().collect::<Vec<_>>(), ["command", "timeout", "type"]);
                }
            }
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
    fn a_tool_the_user_interrupts_ends_the_turn_and_another_failure_does_not() {
        let failure = |interrupt: Option<bool>| {
            let mut payload = json!({
                "hook_event_name": "PostToolUseFailure", "session_id": "s1", "cwd": "/x",
                "transcript_path": "/t.jsonl", "tool_name": "Bash", "tool_input": {"command": "sleep 600"},
                "tool_use_id": "toolu_1", "error": "Interrupted by user",
            });
            if let Some(interrupt) = interrupt {
                payload["is_interrupt"] = json!(interrupt);
            }
            HookEvent::from_payload(&payload).unwrap()
        };
        assert_eq!(failure(Some(true)).status(), Some(AgentStatus::Idle));
        assert_eq!(failure(Some(false)).status(), None);
        assert_eq!(failure(None).status(), None);
        assert!(EVENTS.contains(&"PostToolUseFailure"), "Claude agents report it");
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
        for command in [
            "cox hook",
            "/tmp/cox hook",
            "'/path with spaces/cox' hook",
            "\"/tmp/cox\" hook",
            "cox hook --chief",
            "imacs hook --chief",
        ] {
            assert!(is_cox_hook(&json!({"type": "command", "command": command})), "{command}");
        }
        for command in ["echo cox hook", "atomics hook", "my-cox hook", "cox doctor", "cox hook-status", "'cox hook'"] {
            assert!(!is_cox_hook(&json!({"type": "command", "command": command})), "{command}");
        }
        assert!(!is_cox_hook(&json!({"type": "prompt", "command": "cox hook"})));
    }

    /// Whether Claude's rule `Bash(<rule>)` matches `command`: `:*` at the end matches the command's
    /// start; otherwise each `*` matches any text, and a rule's one `*`, after a space at its end, also
    /// matches nothing at all (`git *` matches `git`).
    fn claude_rule_matches(rule: &str, command: &str) -> bool {
        let rule = rule.strip_prefix("Bash(").and_then(|rule| rule.strip_suffix(')')).unwrap();
        if let Some(prefix) = rule.strip_suffix(":*") {
            return command.starts_with(prefix);
        }
        if rule.matches('*').count() == 1
            && let Some(head) = rule.strip_suffix(" *")
            && command == head
        {
            return true;
        }
        let pieces: Vec<&str> = rule.split('*').collect();
        let [first, middle @ .., last] = &pieces[..] else { return command == rule };
        let Some(mut rest) = command.strip_prefix(first) else { return false };
        for piece in middle {
            let Some(at) = rest.find(piece) else { return false };
            rest = &rest[at + piece.len()..];
        }
        rest.len() >= last.len() && rest.ends_with(last)
    }

    #[test]
    fn claude_asks_before_every_shared_stash_drop_atomics_finds() {
        for command in [
            "git stash drop",
            "git stash drop stash@{1}",
            "git stash pop 2",
            "git stash clear",
            "git stash branch rescue stash@{1}",
            "git -C ../other stash drop",
            "git -C ../other stash drop stash@{1}",
            "git --no-pager -c color.ui=never stash pop",
            "git -C x stash clear",
            "git -C x -C y stash branch rescue",
        ] {
            assert!(!crate::stash::drops(command).is_empty(), "ATOMICS finds {command}");
            assert!(CLAUDE_STASH_ASK.iter().any(|rule| claude_rule_matches(rule, command)), "{command}");
        }
        for command in
            ["git stash list", "git -C x stash list", "git stash apply 1", "git -C x stash show -p", "git status"]
        {
            assert!(!CLAUDE_STASH_ASK.iter().any(|rule| claude_rule_matches(rule, command)), "{command}");
        }
    }
}
