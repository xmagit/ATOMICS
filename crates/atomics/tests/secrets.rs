//! Exercise stdin and the CLI's public/error surfaces with a fresh, private credential store.
#![cfg(any(target_os = "linux", debug_assertions))]
#[path = "../../../tests/support.rs"]
mod test_support;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn secret_cli_canary_never_reaches_output_arguments_config_or_logs() {
    let dir = test_support::tempdir().unwrap();
    let run = |args: &[&str], value: Option<&str>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_atomics"))
            .args(args)
            .env("HOME", dir.path())
            .env("ATOMICS_HOME", dir.path().join("state"))
            .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
            .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
            .env_remove(atomics_core::paths::HOST_SOCKET_ENV)
            .env("ATOMICS_CREDENTIALS_DIR", dir.path().join("keychain"))
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(value) = value {
            child.stdin.take().unwrap().write_all(value.as_bytes()).unwrap();
        }
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    };
    let canary = format!("canary-cli-{}", uuid::Uuid::new_v4().simple());
    let add =
        run(&["secret", "add", "EXAMPLE_KEY", "--host", "api.example.com", "--header", "x-api-key"], Some(&canary));
    assert!(add.status.success(), "secret add failed");
    let list = run(&["secret", "list"], None);
    assert!(list.status.success());
    let list_text = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_text.contains("EXAMPLE_KEY") && list_text.contains("Relayed") && list_text.contains("api.example.com")
    );
    let exposed = run(&["secret", "add", "EXPOSED_KEY"], Some(&canary));
    assert!(exposed.status.success());
    assert!(String::from_utf8_lossy(&exposed.stderr).contains("agents can read and leak"));
    let bad_args = run(&["secret", "add", "BAD_KEY", &canary], None);
    let copy = run(&["secret", "copy", "EXAMPLE_KEY"], None);
    let admin_read =
        run(&["secret-broker-admin"], Some(&format!(r#"{{"op":"expose","name":"{canary}","session":"s1"}}"#)));
    let admin_bad = run(&["secret-broker-admin"], Some(&canary));
    assert!(!admin_read.status.success() && !admin_bad.status.success());
    assert!(!bad_args.status.success() && !copy.status.success());
    for output in [add, list, exposed, bad_args, copy, admin_read, admin_bad] {
        for bytes in [output.stdout, output.stderr] {
            assert!(!bytes.windows(canary.len()).any(|w| w == canary.as_bytes()));
        }
    }
    fn scan(path: &std::path::Path, canary: &str) {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().unwrap() == "keychain" {
                continue;
            }
            if path.is_dir() {
                scan(&path, canary);
            } else {
                let bytes = std::fs::read(path).unwrap();
                assert!(!bytes.windows(canary.len()).any(|w| w == canary.as_bytes()));
            }
        }
    }
    scan(dir.path(), &canary);
    assert!(run(&["secret", "rm", "EXAMPLE_KEY"], None).status.success());
    assert!(run(&["secret", "rm", "EXPOSED_KEY"], None).status.success());
    assert!(run(&["secret", "list"], None).stdout.is_empty());
}

#[test]
fn secret_cli_reads_a_piped_pem_to_eof_and_rejects_an_oversized_value() {
    let dir = test_support::tempdir().unwrap();
    let keychain = dir.path().join("keychain");
    let run = |name: &str, value: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_atomics"))
            .args(["secret", "add", name])
            .env("HOME", dir.path())
            .env("ATOMICS_HOME", dir.path().join("state"))
            .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
            .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
            .env_remove(atomics_core::paths::HOST_SOCKET_ENV)
            .env_remove("ATOMICS_SECRET_BROKER_SOCKET")
            .env("ATOMICS_CREDENTIALS_DIR", &keychain)
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(value.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let pem = "-----BEGIN PRIVATE KEY-----\ncanary-first-line\ncanary-second-line\n-----END PRIVATE KEY-----\n";
    let output = run("PEM_KEY", pem);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("canary"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("canary"));
    let read_catalog = || {
        let encoded = std::fs::read_to_string(keychain.join("atomics-credentials-v1@atomics")).unwrap();
        let bytes: Vec<_> =
            (0..encoded.len()).step_by(2).map(|i| u8::from_str_radix(&encoded[i..i + 2], 16).unwrap()).collect();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };
    assert_eq!(read_catalog()["secrets"]["PEM_KEY"]["value"], pem.strip_suffix('\n').unwrap());
    let overlong = format!("{}\ncanary-hidden-tail", "x".repeat(8192));
    let output = run("TOO_LARGE", &overlong);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds 8192 bytes"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("canary"));
    assert!(read_catalog()["secrets"].get("TOO_LARGE").is_none());
}

#[test]
fn terminal_secret_refuses_and_flushes_pasted_lines_before_restoring_echo() {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};

    fn settings(terminal: &File) -> libc::termios {
        // SAFETY: tcgetattr initializes this live terminal's termios before it is read.
        unsafe {
            let mut settings = std::mem::MaybeUninit::uninit();
            assert_eq!(libc::tcgetattr(terminal.as_raw_fd(), settings.as_mut_ptr()), 0);
            settings.assume_init()
        }
    }

    for (input, accepted) in [
        ("canary-single\n", true),
        ("canary-first\ncanary-second\n", false),
        ("canary-first\ncanary-unterminated", false),
        ("canary-first\n\n", false),
    ] {
        let dir = test_support::tempdir().unwrap();
        // SAFETY: openpty returns two owned descriptors, each transferred to exactly one File.
        let (mut master, mut slave) = unsafe {
            let (mut master, mut slave) = (-1, -1);
            assert_eq!(
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), std::ptr::null()),
                0
            );
            (File::from_raw_fd(master), File::from_raw_fd(slave))
        };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: these are live owned descriptors; keep them out of concurrent execs.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }
        let mut original = settings(&slave);
        original.c_lflag |= libc::ECHO | libc::ICANON;
        // SAFETY: original is the initialized termios from this live terminal.
        assert_eq!(unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &original) }, 0);
        let keychain = dir.path().join("keychain");
        let mut child = test_support::TestChild::spawn(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(["secret", "add", "TERMINAL_KEY"])
                .env("HOME", dir.path())
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
                .env_remove(atomics_core::paths::HOST_SOCKET_ENV)
                .env_remove("ATOMICS_SECRET_BROKER_SOCKET")
                .env("ATOMICS_CREDENTIALS_DIR", &keychain)
                .env("SHELL", "/bin/sh")
                .stdin(Stdio::from(slave.try_clone().unwrap())),
            test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        while settings(&slave).c_lflag & libc::ECHO != 0 {
            child.check_timeout().unwrap();
            assert!(child.child.try_wait().unwrap().is_none(), "secret prompt exited before reading input");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        master.write_all(input.as_bytes()).unwrap();
        let output = child.wait_output().unwrap();
        assert_eq!(output.status.success(), accepted, "{}", String::from_utf8_lossy(&output.stderr));
        for bytes in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(bytes).contains("canary"));
        }
        if !accepted {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Pipe multi-line values from a file"));
        }
        assert_eq!(keychain.join("atomics-credentials-v1@atomics").exists(), accepted);
        let restored = settings(&slave);
        assert_eq!(restored.c_lflag & (libc::ECHO | libc::ICANON), original.c_lflag & (libc::ECHO | libc::ICANON));
        // Read in noncanonical mode to include unfinished lines in the leak check.
        let mut immediate = restored;
        immediate.c_lflag &= !libc::ICANON;
        immediate.c_cc[libc::VMIN] = 0;
        immediate.c_cc[libc::VTIME] = 0;
        // SAFETY: immediate is initialized for this live terminal; VMIN/VTIME make the read bounded.
        assert_eq!(unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &immediate) }, 0);
        assert_eq!(slave.read(&mut [0; 256]).unwrap(), 0, "pasted input remained for the shell");
    }
}
