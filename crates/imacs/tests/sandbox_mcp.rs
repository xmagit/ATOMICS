//! The fake sbx records registration; the stand-in gateway launches that exact host command.
//! No VM or Docker daemon is used. The echo socket exposes the requests sent by the real MCP server.
#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::{fs::PermissionsExt, net::UnixListener};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use imacs_core::isolation;
use serde_json::{Value, json};
use test_support::{CHILD_TIMEOUT, TestChild};

fn registered_command(args: &[String]) -> Command {
    let program = args.iter().position(|s| s == "--command").unwrap() + 1;
    let mut command = Command::new(&args[program]);
    for field in &args[program + 1..] {
        let field = field.strip_prefix("--args=").unwrap();
        // Each flag contains one CSV field. This also exercises commas and spaces in paths.
        let decoded = field
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map_or_else(|| field.to_string(), |s| s.replace("\"\"", "\""));
        command.arg(decoded);
    }
    command
}

#[test]
fn registered_host_mcp_reports_and_scoped_ticket_requests_cross_the_stand_in_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("imacs, test.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let sbx = dir.path().join("sbx");
    std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sbx"), &sbx).unwrap();
    std::fs::set_permissions(&sbx, std::fs::Permissions::from_mode(0o755)).unwrap();
    let args = isolation::mcp_add_args("imacs-session-a", Path::new(env!("CARGO_BIN_EXE_imacs")), &socket);
    assert!(test_support::output(Command::new(&sbx).args(&args)).status.success());
    assert!(
        test_support::output(Command::new(&sbx).args(isolation::mcp_load_args("imacs-session-a", "imacs-session-a")))
            .status
            .success()
    );
    let recorded = std::fs::read_to_string(dir.path().join("calls.log")).unwrap();
    let registered: Vec<String> = recorded.lines().next().unwrap().split('\t').map(String::from).collect();
    let mut child = TestChild::spawn(registered_command(&registered).stdin(Stdio::piped()), CHILD_TIMEOUT).unwrap();
    let calls = [
        json!({"id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"id":2,"method":"tools/list"}),
        json!({"id":3,"method":"tools/call","params":{"name":"imacs_report","arguments":{"status":"done","summary":"From the VM"}}}),
        json!({"id":4,"method":"tools/call","params":{"name":"imacs_agent_ticket_get","arguments":{"plan":true}}}),
        json!({"id":5,"method":"tools/call","params":{"name":"imacs_agent_ticket_comment","arguments":{"text":"Task details"}}}),
        json!({"id":6,"method":"tools/call","params":{"name":"imacs_start","arguments":{}}}),
        json!({"id":7,"method":"tools/call","params":{"name":"imacs_agent_ticket_get","arguments":{"pane":"another-session"}}}),
    ];
    let mut stdin = child.child.stdin.take().unwrap();
    for call in calls {
        writeln!(stdin, "{call}").unwrap();
    }
    drop(stdin);
    for expected in [
        json!({"cmd":"report","pane":"imacs-session-a","status":"done","summary":"From the VM"}),
        json!({"cmd":"agent_ticket_get","pane":"imacs-session-a","plan":true}),
        json!({"cmd":"agent_ticket_comment","pane":"imacs-session-a","text":"Task details"}),
    ] {
        let mut stream = loop {
            child.check_timeout().unwrap();
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => panic!("echo socket: {e}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&line).unwrap(), expected);
        writeln!(stream, "{}", json!({"ok":true,"result":expected})).unwrap();
    }
    let output = child.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let replies: Vec<Value> =
        String::from_utf8(output.stdout).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect();
    assert_eq!(replies.len(), 7);
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 3);
    for reply in &replies[2..5] {
        assert_eq!(reply["result"]["isError"], false, "{reply}");
    }
    for reply in &replies[5..] {
        assert_eq!(reply["result"]["isError"], true, "{reply}");
    }
    assert!(
        test_support::output(Command::new(&sbx).args(["mcp", "rm", "--force", "imacs-session-a"])).status.success()
    );
}
