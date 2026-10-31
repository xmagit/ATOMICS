#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use test_support::{CHILD_TIMEOUT, TestChild};

#[test]
fn statusline_forwards_live_input_and_preserves_user_stdout_even_without_atomics() {
    let dir = crate::test_support::tempdir().unwrap();
    let socket = dir.path().join("usage.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let input = json!({"workspace":{"project_dir":dir.path()},"rate_limits":{
        "five_hour":{"used_percentage":25,"resets_at":4102444800_i64}
    }})
    .to_string();
    for custom in [false, true] {
        if custom {
            std::fs::write(
                dir.path().join("settings.json"),
                json!({"statusLine":{
                    "type":"command","command":r#"cat > "$STATUSLINE_INPUT"; printf '\033[32mmy status\033[0m\n'"#
                }})
                .to_string(),
            )
            .unwrap();
        }
        let mut child = TestChild::spawn(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(["statusline", "--socket"])
                .arg(&socket)
                .env("HOME", dir.path())
                .env("CLAUDE_CONFIG_DIR", dir.path())
                .env("STATUSLINE_INPUT", dir.path().join("input"))
                .env_remove("ATOMICS_STATUSLINE_FORWARDED")
                .stdin(Stdio::piped()),
            CHILD_TIMEOUT,
        )
        .unwrap();
        child.child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let mut stream = loop {
            child.check_timeout().unwrap();
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(err) => panic!("statusline socket: {err}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["statusline"],
            serde_json::from_str::<Value>(&input).unwrap()
        );
        stream.write_all(b"\n").unwrap();
        let output = child.wait_output().unwrap();
        assert!(output.status.success());
        if custom {
            assert_eq!(output.stdout, b"\x1b[32mmy status\x1b[0m\n");
            assert_eq!(std::fs::read_to_string(dir.path().join("input")).unwrap(), input);
        } else {
            assert_eq!(String::from_utf8(output.stdout).unwrap(), "Claude · session 75% · weekly —\n");
        }
    }
    let output = test_support::output(
        Command::new(env!("CARGO_BIN_EXE_atomics"))
            .args(["statusline", "--socket"])
            .arg(dir.path().join("absent.sock"))
            .env("HOME", dir.path())
            .env("CLAUDE_CONFIG_DIR", dir.path())
            .env("ATOMICS_STATUSLINE_FORWARDED", "1"),
    );
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "forwarding must not recurse");
    let mut child = TestChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_atomics"))
            .args(["statusline", "--socket"])
            .arg(dir.path().join("absent.sock"))
            .env("HOME", dir.path())
            .env("CLAUDE_CONFIG_DIR", dir.path())
            .env("STATUSLINE_INPUT", dir.path().join("input"))
            .env_remove("ATOMICS_STATUSLINE_FORWARDED")
            .stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    child.child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    assert_eq!(child.wait_output().unwrap().stdout, b"\x1b[32mmy status\x1b[0m\n");
}
