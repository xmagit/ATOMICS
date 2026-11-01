//! `atomics hook`: forwards lifecycle events and permission replies. Failures return
//! no decision so the agent can use its native approval prompt.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use atomics_core::hook::{HookReply, HookRequest};

pub fn run(socket: Option<std::path::PathBuf>, pane: Option<String>) {
    // A globally installed Claude hook must not attach this pane to the default instance.
    if socket.is_none() && std::env::var("TERM_PROGRAM").as_deref() == Ok("atomics") {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&input) else { return };
    prepare_tool_response(&mut payload, std::env::var("ATOMICS_CHIEF").as_deref() == Ok("1"));
    let ancestors = if pane.is_none()
        && payload.get("hook_event_name").and_then(serde_json::Value::as_str) == Some("SessionStart")
    {
        ancestors()
    } else {
        Vec::new()
    };
    let request = HookRequest { pane, ancestors, payload };
    if let Some(stdout) = forward(&request, socket.unwrap_or_else(atomics_core::paths::socket_path)) {
        print!("{stdout}");
        let _ = std::io::stdout().flush();
    }
}

const MAX_TOOL_RESPONSE_BYTES: usize = 256 * 1024;

const MAX_TOOL_SUMMARY_BYTES: usize = 1024;

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
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};

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
                    prepare_tool_response(&mut payload, chief);
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
            run(Some(socket.into()), None);
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ps = dir.path().join("ps");
        std::fs::write(
            &ps,
            "#!/bin/sh\nprintf 'called\\n' >> \"$ATOMICS_PS_LOG\"\nprintf '%s 4242\\n4242 1\\n' \"$PPID\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
        for event in ["SessionStart", "PermissionRequest", "PostToolUse", "Interrupt"] {
            let mut child = TestChild::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "hook_client::tests::ancestry_runs_only_at_session_start_and_uses_path"])
                    .env("ATOMICS_HOOK_TEST_SOCKET", &socket)
                    .env("ATOMICS_PS_LOG", dir.path().join("calls"))
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
}
