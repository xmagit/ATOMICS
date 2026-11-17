//! A queue command owns its child process group and forwards terminal stop signals to it.
#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn queue_forwards_stop_signals_to_its_command() {
    let dir = test_support::tempdir().unwrap();
    let socket = dir.path().join("queue.sock");
    let marker = dir.path().join("child.pid");
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(&stream).read_line(&mut request).unwrap();
        writeln!(stream, "{{\"granted\":true}}").unwrap();
        let _ = std::io::copy(&mut stream, &mut std::io::sink());
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_atomics"));
    // SAFETY: only signal(2) runs in this child's pre-exec callback.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut child = test_support::TestChild::spawn(
        command
            .env("ATOMICS_HOME", dir.path().join("state"))
            .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
            .env("ATOMICS_SOCKET", &socket)
            .env_remove("ATOMICS_HOST_SOCKET")
            .args(["queue", "--", "/bin/sh", "-c", "echo $$ > \"$1\"; exec sleep 60", "queue-test"])
            .arg(&marker),
        Duration::from_secs(10),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    let command_pid: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    // An inherited ignored HUP (nohup) stays ignored, without changing stop forwarding.
    unsafe { libc::kill(child.child.id() as i32, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(50));
    assert!(child.child.try_wait().unwrap().is_none());
    // SAFETY: both PIDs belong to processes this test started.
    unsafe { libc::kill(child.child.id() as i32, libc::SIGTERM) };
    let output = child.wait_output().unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(command_pid, 0) } == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    let alive = unsafe { libc::kill(command_pid, 0) } == 0;
    if alive {
        // Clean up the test's child even when the regression fails.
        unsafe { libc::kill(command_pid, libc::SIGKILL) };
    }
    assert!(!alive, "a stop signal to queue must also stop its command");
    assert_eq!(output.status.code(), Some(128 + libc::SIGTERM));
}
