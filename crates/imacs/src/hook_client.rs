//! `imacs hook`: forwards lifecycle events and permission replies. Failures return
//! no decision so the agent can use its native approval prompt.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use imacs_core::hook::{HookReply, HookRequest};

pub fn run(socket: Option<std::path::PathBuf>, pane: Option<String>) {
    // A globally installed Claude hook must not attach this pane to the default instance.
    if socket.is_none() && std::env::var("TERM_PROGRAM").as_deref() == Ok("imacs") {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&input) else { return };
    let ancestors = if pane.is_none()
        && payload.get("hook_event_name").and_then(serde_json::Value::as_str) == Some("SessionStart")
    {
        std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid="])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| imacs_core::hook::ancestor_pids(std::process::id(), &String::from_utf8_lossy(&output.stdout)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let request = HookRequest { pane, ancestors, payload };
    if let Some(stdout) = forward(&request, socket.unwrap_or_else(imacs_core::paths::socket_path)) {
        print!("{stdout}");
        let _ = std::io::stdout().flush();
    }
}

fn forward(request: &HookRequest, socket: std::path::PathBuf) -> Option<String> {
    let stream = UnixStream::connect(socket).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(12))).ok()?;
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
    fn ancestry_runs_only_at_session_start_and_uses_path() {
        if let Some(socket) = std::env::var_os("IMACS_HOOK_TEST_SOCKET") {
            run(Some(socket.into()), None);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hook.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let ps = dir.path().join("ps");
        std::fs::write(
            &ps,
            "#!/bin/sh\nprintf 'called\\n' >> \"$IMACS_PS_LOG\"\nprintf '%s 4242\\n4242 1\\n' \"$PPID\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o755)).unwrap();
        for event in ["SessionStart", "PermissionRequest", "PostToolUse", "Interrupt"] {
            let mut child = TestChild::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "hook_client::tests::ancestry_runs_only_at_session_start_and_uses_path"])
                    .env("IMACS_HOOK_TEST_SOCKET", &socket)
                    .env("IMACS_PS_LOG", dir.path().join("calls"))
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
