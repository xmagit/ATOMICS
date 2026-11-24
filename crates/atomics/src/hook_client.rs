//! `atomics hook`: forwards lifecycle events and permission replies. Failures return
//! no decision so the agent can use its native approval prompt.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use atomics_core::hook::{HookReply, HookRequest};

pub fn run(socket: Option<std::path::PathBuf>, pane: Option<String>, chief: bool, provider: Option<String>) {
    // A globally installed Claude hook must not attach this pane to the default instance.
    if socket.is_none() && std::env::var("TERM_PROGRAM").as_deref() == Ok("atomics") {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&input) else { return };
    let chief = chief || std::env::var("ATOMICS_CHIEF_HOOK").as_deref() == Ok("1");
    if let Some(fields) = payload.as_object_mut() {
        fields.remove("atomics_chief_update");
        fields.remove(atomics_core::hook::PROVIDER_FIELD);
        if let Some(provider) = provider.filter(|name| atomics_core::hook::HookProvider::from_name(name).is_some()) {
            fields.insert(atomics_core::hook::PROVIDER_FIELD.into(), provider.into());
        }
    }
    if chief && let Some(text) = chief_update(&payload) {
        payload["atomics_chief_update"] = text.into();
    }
    if payload.get("last_assistant_message").and_then(serde_json::Value::as_str).is_none_or(str::is_empty)
        && payload_event_name(&payload) == Some("Stop")
        && let Some(text) = stop_last_message(&payload)
    {
        payload["last_assistant_message"] = text.into();
    }
    prepare_tool_response(&mut payload, chief);
    let pane = pane.or_else(|| std::env::var(atomics_core::hook::HOOK_PANE_ENV).ok().filter(|pane| !pane.is_empty()));
    let ancestors =
        if pane.is_none() && payload_event_name(&payload) == Some("SessionStart") { ancestors() } else { Vec::new() };
    let chief_key = std::env::var(atomics_core::hook::CHIEF_HOOK_KEY_ENV).ok();
    let request = HookRequest { pane, ancestors, chief_key, payload };
    let socket = socket.unwrap_or_else(atomics_core::paths::socket_path);
    let Some(reply) = forward(&request, socket.clone()) else { return };
    let mut stdout = reply.stdout;
    let delivery = notebook_delivery(&request, &socket);
    if let Some((context, _, _)) = &delivery
        && let Some(event) = atomics_core::hook::HookEvent::from_payload(&request.payload)
    {
        stdout = event.merge_context(stdout.as_deref(), context);
    }
    if let Some(stdout) = stdout {
        print!("{stdout}");
        if std::io::stdout().flush().is_ok()
            && let Some((_, capability, ids)) = delivery
        {
            let _ = crate::ipc::request_at(
                &socket,
                &atomics_core::control::ControlRequest::Coordination {
                    capability,
                    request: atomics_core::notebook::Request::Ack { ids },
                },
            );
        }
    }
}

fn notebook_delivery(request: &HookRequest, socket: &std::path::Path) -> Option<(String, String, Vec<u64>)> {
    let event = atomics_core::hook::HookEvent::from_payload(&request.payload)?;
    if event.thread != atomics_core::hook::HookThread::Main
        || !matches!(event.name.as_str(), "SessionStart" | "UserPromptSubmit" | "PostToolUse")
        || !event.takes_context()
    {
        return None;
    }
    let capability = std::env::var(crate::notebook_auth::ENV).ok()?;
    let response = crate::ipc::request_at(
        socket,
        &atomics_core::control::ControlRequest::Coordination {
            capability: capability.clone(),
            request: atomics_core::notebook::Request::Drain { event: event.name, main: true },
        },
    )
    .ok()?;
    if !response.ok {
        return None;
    }
    let context = response.result["context"].as_str()?.to_owned();
    if context.is_empty() {
        return None;
    }
    let ids = serde_json::from_value(response.result["ids"].clone()).ok()?;
    Some((context, capability, ids))
}

const MAX_TOOL_RESPONSE_BYTES: usize = 256 * 1024;

const MAX_TOOL_SUMMARY_BYTES: usize = 1024;

fn payload_event_name(payload: &serde_json::Value) -> Option<&str> {
    payload
        .get("hook_event_name")
        .or_else(|| payload.get("hookName"))
        .and_then(serde_json::Value::as_str)
        .map(atomics_core::hook::canonical_event_name)
}

fn read_tail(file: &Path) -> Option<String> {
    let mut reader = std::fs::File::open(file).ok()?;
    const LIMIT: u64 = 128 * 1024;
    let offset = reader.metadata().ok()?.len().saturating_sub(LIMIT);
    reader.seek(SeekFrom::Start(offset)).ok()?;
    let mut tail = Vec::new();
    reader.take(LIMIT).read_to_end(&mut tail).ok()?;
    let tail = if offset == 0 { &tail[..] } else { &tail[tail.iter().position(|c| *c == b'\n')? + 1..] };
    std::str::from_utf8(tail).ok().map(str::to_owned)
}

fn copilot_session_state_dir() -> PathBuf {
    copilot_session_state_in(std::env::var_os("COPILOT_HOME"))
}

/// An empty `COPILOT_HOME` is unset, as Copilot reads it; never the current folder.
fn copilot_session_state_in(home: Option<std::ffi::OsString>) -> PathBuf {
    home.filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| atomics_core::paths::home().join(".copilot"))
        .join("session-state")
}

/// Copilot CLI's `Stop` hook carries `transcript_path` (`~/.copilot/session-state/<id>/events.jsonl`)
/// rather than `last_assistant_message`; read the latest assistant message from the current turn.
fn stop_last_message(payload: &serde_json::Value) -> Option<String> {
    if payload.get("text").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()) {
        return None;
    }
    let id = payload.get("session_id").or_else(|| payload.get("sessionId")).and_then(serde_json::Value::as_str)?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) {
        return None;
    }
    let path =
        payload.get("transcript_path").or_else(|| payload.get("transcriptPath")).and_then(serde_json::Value::as_str)?;
    let file = Path::new(path).canonicalize().ok()?;
    if !file.is_file() {
        return None;
    }
    if file.file_name().and_then(|n| n.to_str()) == Some("events.jsonl")
        && file.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some(id)
        && file.starts_with(copilot_session_state_dir().canonicalize().ok()?)
    {
        return copilot_assistant_update(&read_tail(&file)?);
    }
    None
}

/// Latest visible assistant reply from Copilot's `events.jsonl` in the current turn.
fn copilot_assistant_update(tail: &str) -> Option<String> {
    for line in tail.lines().rev() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match record["type"].as_str() {
            Some("user.message") => break,
            Some("assistant.message") => {
                if let Some(text) = record["data"]["content"].as_str()
                    && !text.trim().is_empty()
                {
                    return Some(text.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// A chief hook runs alongside the provider. Only its own main conversation's public text
/// accompanies the event to the host, including in a guest with a separate filesystem.
fn chief_update(payload: &serde_json::Value) -> Option<String> {
    // Cursor hands each assistant message of the turn to its own hook.
    if payload_event_name(payload)? == "AfterAgentResponse" {
        return (payload["agent_id"].is_null() && payload["parent_tool_call_id"].is_null())
            .then(|| payload["text"].as_str().filter(|text| !text.trim().is_empty()).map(str::to_owned))
            .flatten();
    }
    if !matches!(payload_event_name(payload)?, "PreToolUse" | "PostToolUse") || !payload["agent_id"].is_null() {
        return None;
    }
    let id = payload["session_id"].as_str()?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) {
        return None;
    }
    let file = Path::new(payload["transcript_path"].as_str()?).canonicalize().ok()?;
    let name = file.file_name()?.to_str()?;
    if name == "events.jsonl"
        && file.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) == Some(id)
        && file.starts_with(copilot_session_state_dir().canonicalize().ok()?)
        && file.is_file()
    {
        return copilot_assistant_update(&read_tail(&file)?);
    }
    // Codex's rollout is followed continuously by the host.
    if name != format!("{id}.jsonl") {
        return None;
    }
    let root = crate::setup::agent_claude_dir().join("projects");
    if !file.starts_with(root.canonicalize().ok()?) || !file.is_file() {
        return None;
    }
    assistant_update(&read_tail(&file)?, id)
}

/// Ignore tool I/O, reasoning, subagents and previous turns. A provider can flush the same
/// statement at several hooks; the host coalesces it against the current conversation.
fn assistant_update(tail: &str, id: &str) -> Option<String> {
    for line in tail.lines().rev() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if record["sessionId"] == id && record["isSidechain"] != true {
            let content = &record["message"]["content"];
            if record["type"] == "user"
                && (content.is_string()
                    || content.as_array().is_some_and(|a| a.iter().any(|item| item["type"] == "text")))
            {
                break;
            }
            if record["type"] == "assistant"
                && let Some(blocks) = content.as_array()
            {
                let text = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Only the chief's MCP trace needs a full response. Other hooks keep a small summary.
fn prepare_tool_response(payload: &mut serde_json::Value, chief: bool) {
    let mcp_tool = payload
        .get("tool_name")
        .or_else(|| payload.get("toolName"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.starts_with("mcp__") || name.starts_with("MCP:"));
    let key = ["tool_response", "tool_result", "toolResult", "tool_output"]
        .into_iter()
        .find(|key| payload.get(*key).is_some());
    let Some(key) = key else { return };
    if chief && mcp_tool {
        if let Some(response) = payload.get_mut(key) {
            cap_response(response, MAX_TOOL_RESPONSE_BYTES);
        }
    } else if let Some(mut response) = payload.as_object_mut().and_then(|object| object.remove(key)) {
        cap_response(&mut response, MAX_TOOL_SUMMARY_BYTES);
        payload["tool_response_summary"] = response;
    }
}

/// Keep a UTF-8 head and an explicit marker, including JSON escaping in the wire cap.
fn cap_response(response: &mut serde_json::Value, max_bytes: usize) {
    let serialized = response.to_string();
    if serialized.len() <= max_bytes {
        return;
    }
    let (mut low, mut high) = (0, serialized.len().min(max_bytes));
    while low < high {
        let mid = (low + high).div_ceil(2);
        let mut cut = mid;
        while !serialized.is_char_boundary(cut) {
            cut -= 1;
        }
        let candidate =
            serde_json::json!({"head": &serialized[..cut], "truncated": true, "original_bytes": serialized.len()});
        if candidate.to_string().len() <= max_bytes {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    while !serialized.is_char_boundary(low) {
        low -= 1;
    }
    *response = serde_json::json!({"head": &serialized[..low], "truncated": true, "original_bytes": serialized.len()});
}

/// The parent processes of this one, nearest first, so ATOMICS can tell which pane started it.
pub fn ancestors() -> Vec<u32> {
    std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| atomics_core::hook::ancestor_pids(std::process::id(), &String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

fn forward(request: &HookRequest, socket: std::path::PathBuf) -> Option<HookReply> {
    forward_with_wait_duration(request, socket, Duration::from_secs)
}

fn forward_with_wait_duration(
    request: &HookRequest,
    socket: std::path::PathBuf,
    wait_duration: impl FnOnce(u64) -> Duration,
) -> Option<HookReply> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    let permission = payload_event_name(&request.payload) == Some("PermissionRequest");
    let timeout = if permission { atomics_core::hook::PERMISSION_WAIT_SECONDS + 10 } else { 12 };
    stream.set_read_timeout(Some(wait_duration(timeout))).ok()?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request).ok()?.as_bytes()).ok()?;
    writer.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{CHILD_TIMEOUT, TestChild};
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

    #[test]
    fn chief_updates_are_only_visible_assistant_text_from_the_current_turn() {
        let claude = concat!(
            r#"{"type":"user","sessionId":"guide","message":{"content":"start"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"guide","message":{"content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":"Checking the river."},{"type":"tool_use","input":{"key":"private"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"guide","message":{"content":[{"type":"tool_result","content":"private output"}]}}"#,
            "\n"
        );
        assert_eq!(assistant_update(claude, "guide").as_deref(), Some("Checking the river."));
        assert_eq!(assistant_update(claude, "other"), None);
        assert_eq!(
            assistant_update(
                &format!(
                    "{claude}{{\"type\":\"user\",\"sessionId\":\"guide\",\"message\":{{\"content\":\"next turn\"}}}}\n"
                ),
                "guide"
            ),
            None
        );
    }

    #[test]
    fn hook_trace_response_cap_counts_escaping_and_keeps_a_utf8_head() {
        for response in [
            serde_json::json!("é\"\n".repeat(300_000)),
            serde_json::json!({"content":[{"type":"text", "text":"big ".repeat(100_000)}]}),
        ] {
            let original = response.to_string();
            let mut payload = serde_json::json!({"tool_response": response, "tool_use_id":"unchanged"});
            cap_response(&mut payload["tool_response"], MAX_TOOL_RESPONSE_BYTES);
            assert!(payload["tool_response"].to_string().len() <= MAX_TOOL_RESPONSE_BYTES);
            assert_eq!(payload["tool_response"]["truncated"], true);
            assert!(original.starts_with(payload["tool_response"]["head"].as_str().unwrap()));
            assert_eq!(payload["tool_use_id"], "unchanged");
        }
        let mut small = serde_json::json!({"tool_response":{"structuredContent":{"id":"one"}}});
        let original = small.clone();
        cap_response(&mut small["tool_response"], MAX_TOOL_RESPONSE_BYTES);
        assert_eq!(small, original);
    }

    #[test]
    fn only_chief_mcp_hooks_send_full_tool_responses() {
        for chief in [false, true] {
            for tool in ["Bash", "mcp__example__read"] {
                for response in [serde_json::json!({"id":"small"}), serde_json::json!("é\"\n".repeat(300_000))] {
                    let mut payload = serde_json::json!({
                        "tool_name":tool, "tool_response":response, "tool_use_id":"one"
                    });
                    prepare_tool_response(
                        &mut payload,
                        chief || std::env::var("ATOMICS_CHIEF_HOOK").as_deref() == Ok("1"),
                    );
                    if chief && tool.starts_with("mcp__") {
                        assert!(payload.get("tool_response_summary").is_none());
                        assert!(payload["tool_response"].to_string().len() <= MAX_TOOL_RESPONSE_BYTES);
                    } else {
                        assert!(payload.get("tool_response").is_none());
                        assert!(payload["tool_response_summary"].to_string().len() <= MAX_TOOL_SUMMARY_BYTES);
                    }
                    assert_eq!(payload["tool_use_id"], "one");
                }
            }
        }
    }

    #[test]
    fn process_ancestry_includes_the_real_parent() {
        // SAFETY: getppid has no preconditions. This also exercises the platform's ps flags.
        let parent = unsafe { libc::getppid() } as u32;
        assert!(ancestors().contains(&parent));
    }

    #[test]
    fn permission_forward_waits_beyond_the_ordinary_hook_timeout() {
        let started = std::time::Instant::now();
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("permission.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_millis(65));
            let _ = stream.write_all(b"{\"stdout\":\"reviewed\"}\n");
        });
        let reply = forward_with_wait_duration(
            &HookRequest {
                chief_key: None,
                pane: Some("review".into()),
                ancestors: vec![],
                payload: serde_json::json!({"hook_event_name":"PermissionRequest", "session_id":"review"}),
            },
            socket,
            |seconds| Duration::from_millis(seconds * 5),
        );
        server.join().unwrap();
        assert_eq!(reply.unwrap().stdout.as_deref(), Some("reviewed"));
        assert!(started.elapsed() < Duration::from_secs(1), "scaled permission wait must stay fast");
    }

    #[test]
    fn ordinary_hook_forward_expires_without_a_reply() {
        let started = std::time::Instant::now();
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("ordinary.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            // EOF proves the client timed out and closed, rather than the server replying.
            assert_eq!(stream.read(&mut [0]).unwrap(), 0);
        });
        let reply = forward_with_wait_duration(
            &HookRequest {
                chief_key: None,
                pane: None,
                ancestors: vec![],
                payload: serde_json::json!({"hook_event_name":"PostToolUse", "session_id":"fixture"}),
            },
            socket,
            |seconds| Duration::from_millis(seconds * 5),
        );
        assert!(reply.is_none());
        server.join().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn an_empty_copilot_home_is_unset_and_cursor_replies_reach_the_chief_trace() {
        let default = copilot_session_state_in(None);
        assert!(default.is_absolute() && default.ends_with(".copilot/session-state"));
        assert_eq!(copilot_session_state_in(Some("".into())), default);
        assert_eq!(copilot_session_state_in(Some("/cop".into())), Path::new("/cop/session-state"));

        let reply = |extra: serde_json::Value| {
            let mut payload = serde_json::json!({"hook_event_name":"afterAgentResponse", "conversation_id":"c",
                "cursor_version":"2026.11.22-e9042b2", "text":"Planned the parser."});
            payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            chief_update(&payload)
        };
        assert_eq!(reply(serde_json::json!({})).as_deref(), Some("Planned the parser."));
        assert_eq!(reply(serde_json::json!({"parent_tool_call_id":"tool-1"})), None, "a subagent's is not the chief's");
        assert_eq!(reply(serde_json::json!({"text":"  "})), None);
    }

    #[test]
    fn copilot_stop_reads_last_assistant_message_from_events_jsonl() {
        let log = concat!(
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"Older turn\"}}\n",
            "{\"type\":\"user.message\",\"data\":{\"content\":\"Hi\"}}\n",
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"First reply\"}}\n",
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"Final Copilot reply\"}}\n",
        );
        assert_eq!(copilot_assistant_update(log).as_deref(), Some("Final Copilot reply"));
        let no_reply_yet = concat!(
            "{\"type\":\"assistant.message\",\"data\":{\"content\":\"Older turn\"}}\n",
            "{\"type\":\"user.message\",\"data\":{\"content\":\"New prompt\"}}\n",
        );
        assert_eq!(copilot_assistant_update(no_reply_yet), None);
        // Paths outside Copilot's session-state directory are rejected.
        let dir = crate::test_support::tempdir().unwrap();
        let outside = dir.path().join("events.jsonl");
        std::fs::write(&outside, log).unwrap();
        let payload = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "cop-1",
            "transcript_path": outside.to_string_lossy(),
        });
        assert_eq!(stop_last_message(&payload), None);
    }

    #[test]
    fn ancestry_runs_only_at_session_start_and_uses_path() {
        if let Some(socket) = std::env::var_os("ATOMICS_HOOK_TEST_SOCKET") {
            run(Some(socket.into()), None, false, None);
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ps = dir.path().join("ps");
        crate::test_support::write_program(
            &ps,
            "#!/bin/sh\nprintf 'called\\n' >> \"$ATOMICS_PS_LOG\"\nprintf '%s 4242\\n4242 1\\n' \"$PPID\"\n",
        );
        for event in ["SessionStart", "PermissionRequest", "PostToolUse", "Interrupt"] {
            let mut child = TestChild::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "hook_client::tests::ancestry_runs_only_at_session_start_and_uses_path"])
                    .env("ATOMICS_HOOK_TEST_SOCKET", &socket)
                    .env("ATOMICS_PS_LOG", dir.path().join("calls"))
                    .env("PATH", dir.path())
                    .env_remove(atomics_core::hook::HOOK_PANE_ENV)
                    .stdin(Stdio::piped()),
                CHILD_TIMEOUT,
            )
            .unwrap();
            child
                .child
                .stdin
                .take()
                .unwrap()
                .write_all(serde_json::json!({"session_id":"fixture", "hook_event_name":event}).to_string().as_bytes())
                .unwrap();
            let (mut stream, _) = loop {
                child.check_timeout().unwrap();
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("hook connection: {error}"),
                }
            };
            // macOS hands accepted sockets the listener's non-blocking mode; reads must wait.
            stream.set_nonblocking(false).unwrap();
            let remaining = child.remaining().max(Duration::from_millis(1));
            stream.set_read_timeout(Some(remaining)).unwrap();
            stream.set_write_timeout(Some(remaining)).unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: HookRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.ancestors, if event == "SessionStart" { vec![4242] } else { vec![] });
            stream.write_all(b"{\"stdout\":null}\n").unwrap();
            let output = child.wait_output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        }
        assert_eq!(std::fs::read_to_string(dir.path().join("calls")).unwrap(), "called\n");
    }

    #[test]
    fn a_pane_from_the_session_environment_replaces_ancestry() {
        if let Some(socket) = std::env::var_os("ATOMICS_HOOK_PANE_TEST_SOCKET") {
            run(Some(socket.into()), None, false, None);
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        // A ps that fails proves ancestry is never consulted.
        let ps = dir.path().join("ps");
        crate::test_support::write_program(&ps, "#!/bin/sh\nexit 1\n");
        let mut child = TestChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "hook_client::tests::a_pane_from_the_session_environment_replaces_ancestry"])
                .env("ATOMICS_HOOK_PANE_TEST_SOCKET", &socket)
                .env(atomics_core::hook::HOOK_PANE_ENV, "codex-pane")
                .env("PATH", dir.path())
                .stdin(Stdio::piped()),
            CHILD_TIMEOUT,
        )
        .unwrap();
        child
            .child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"session_id":"codex-session", "hook_event_name":"SessionStart"}"#)
            .unwrap();
        let (mut stream, _) = loop {
            child.check_timeout().unwrap();
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("hook connection: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        let remaining = child.remaining().max(Duration::from_millis(1));
        stream.set_read_timeout(Some(remaining)).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        let request: HookRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(request.pane.as_deref(), Some("codex-pane"));
        assert!(request.ancestors.is_empty());
        stream.write_all(b"{\"stdout\":null}\n").unwrap();
        let output = child.wait_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
}
