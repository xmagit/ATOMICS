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
