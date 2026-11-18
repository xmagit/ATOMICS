//! `atomics hook`: forwards lifecycle events and permission replies. Failures return
//! no decision so the agent can use its native approval prompt.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use atomics_core::hook::{HookReply, HookRequest};

pub fn run(socket: Option<std::path::PathBuf>, pane: Option<String>, chief: bool) {
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
    }
    if chief && let Some(text) = chief_update(&payload) {
        payload["atomics_chief_update"] = text.into();
    }
    prepare_tool_response(&mut payload, chief);
    let pane = pane.or_else(|| std::env::var(atomics_core::hook::HOOK_PANE_ENV).ok().filter(|pane| !pane.is_empty()));
    let ancestors = if pane.is_none()
        && payload.get("hook_event_name").and_then(serde_json::Value::as_str) == Some("SessionStart")
    {
        ancestors()
    } else {
        Vec::new()
    };
    let request = HookRequest { pane, ancestors, payload };
    let socket = socket.unwrap_or_else(atomics_core::paths::socket_path);
    let mut stdout = forward(&request, socket.clone());
    let delivery = notebook_delivery(&request, &socket);
    if let Some((context, _, _)) = &delivery {
        let mut value =
            stdout.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok()).unwrap_or_else(
                || serde_json::json!({"hookSpecificOutput":{"hookEventName":request.payload["hook_event_name"]}}),
            );
        let old = value["hookSpecificOutput"]["additionalContext"].as_str().unwrap_or("");
        value["hookSpecificOutput"]["additionalContext"] =
            serde_json::json!(if old.is_empty() { context.clone() } else { format!("{old}\n\n{context}") });
        stdout = Some(value.to_string());
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

/// A chief hook runs alongside the provider. Only its own main conversation's public text
/// accompanies the event to the host, including in a guest with a separate filesystem.
fn chief_update(payload: &serde_json::Value) -> Option<String> {
    if !matches!(payload["hook_event_name"].as_str()?, "PreToolUse" | "PostToolUse") || !payload["agent_id"].is_null() {
        return None;
    }
    let id = payload["session_id"].as_str()?;
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')) {
        return None;
    }
    let file = Path::new(payload["transcript_path"].as_str()?).canonicalize().ok()?;
    let name = file.file_name()?.to_str()?;
    let codex = name.starts_with("rollout-") && name.ends_with(&format!("-{id}.jsonl"));
    let root = if codex {
        crate::setup::agent_codex_home().join("sessions")
    } else if name == format!("{id}.jsonl") {
        crate::setup::agent_claude_dir().join("projects")
    } else {
        return None;
    };
    if !file.starts_with(root.canonicalize().ok()?) || !file.is_file() {
        return None;
    }
    let mut reader = std::fs::File::open(file).ok()?;
    const LIMIT: u64 = 128 * 1024;
    let offset = reader.metadata().ok()?.len().saturating_sub(LIMIT);
    reader.seek(SeekFrom::Start(offset)).ok()?;
    let mut tail = Vec::new();
    reader.take(LIMIT).read_to_end(&mut tail).ok()?;
    let tail = if offset == 0 { &tail[..] } else { &tail[tail.iter().position(|c| *c == b'\n')? + 1..] };
    assistant_update(std::str::from_utf8(tail).ok()?, id, codex)
}

/// Ignore tool I/O, reasoning, subagents and previous turns. A provider can flush the same
/// statement at several hooks; the host coalesces it against the current conversation.
fn assistant_update(tail: &str, id: &str, codex: bool) -> Option<String> {
    for line in tail.lines().rev() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if codex {
            if record["type"] != "event_msg" {
                continue;
            }
            match record["payload"]["type"].as_str() {
                Some("user_message" | "task_started" | "turn_started") => break,
                Some("agent_message") => {
                    return record["payload"]["message"].as_str().filter(|s| !s.trim().is_empty()).map(str::to_owned);
                }
                _ => {}
            }
        } else if record["sessionId"] == id && record["isSidechain"] != true {
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
    if chief && payload["tool_name"].as_str().is_some_and(|name| name.starts_with("mcp__")) {
        if let Some(response) = payload.get_mut("tool_response") {
            cap_response(response, MAX_TOOL_RESPONSE_BYTES);
        }
    } else if let Some(mut response) = payload.as_object_mut().and_then(|object| object.remove("tool_response")) {
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

fn forward(request: &HookRequest, socket: std::path::PathBuf) -> Option<String> {
    forward_with_wait_duration(request, socket, Duration::from_secs)
}

fn forward_with_wait_duration(
    request: &HookRequest,
    socket: std::path::PathBuf,
    wait_duration: impl FnOnce(u64) -> Duration,
) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    let permission =
        request.payload.get("hook_event_name").and_then(serde_json::Value::as_str) == Some("PermissionRequest");
    let timeout = if permission { atomics_core::hook::PERMISSION_WAIT_SECONDS + 10 } else { 12 };
    stream.set_read_timeout(Some(wait_duration(timeout))).ok()?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request).ok()?.as_bytes()).ok()?;
    writer.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line).ok()?;
    let reply: HookReply = serde_json::from_str(&line).ok()?;
    reply.stdout
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
        assert_eq!(assistant_update(claude, "guide", false).as_deref(), Some("Checking the river."));
        assert_eq!(assistant_update(claude, "other", false), None);
        assert_eq!(
            assistant_update(
                &format!(
                    "{claude}{{\"type\":\"user\",\"sessionId\":\"guide\",\"message\":{{\"content\":\"next turn\"}}}}\n"
                ),
                "guide",
                false
            ),
            None
        );
        let codex = concat!(
            r#"{"type":"event_msg","payload":{"type":"agent_message","phase":"commentary","message":"The five pages are ready."}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_reasoning","text":"private"}}"#,
            "\n"
        );
        assert_eq!(assistant_update(codex, "guide", true).as_deref(), Some("The five pages are ready."));
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
                pane: Some("review".into()),
                ancestors: vec![],
                payload: serde_json::json!({"hook_event_name":"PermissionRequest", "session_id":"review"}),
            },
            socket,
            |seconds| Duration::from_millis(seconds * 5),
        );
        server.join().unwrap();
        assert_eq!(reply.as_deref(), Some("reviewed"));
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
    fn ancestry_runs_only_at_session_start_and_uses_path() {
        if let Some(socket) = std::env::var_os("ATOMICS_HOOK_TEST_SOCKET") {
            run(Some(socket.into()), None, false);
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
            run(Some(socket.into()), None, false);
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
