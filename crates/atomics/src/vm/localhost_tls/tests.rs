use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

use super::*;

#[test]
fn generated_ca_limits_macos_ssl_to_the_three_loopback_names() {
    let storage = tempfile::tempdir().unwrap();
    let fake_keychain = tempfile::tempdir().unwrap();
    let keychain = Keychain::in_folder(fake_keychain.path());
    let ca = Ca::create(storage.path(), &keychain).unwrap();
    assert!(!ca.cert.with_file_name("key.pem").exists());
    assert_eq!(Ca::load(storage.path()).unwrap().unwrap().cert, ca.cert);
    let key = ca.key(&keychain, false).unwrap();
    let root = openssl(
        storage.path(),
        [OsStr::new("x509"), OsStr::new("-in"), ca.cert.as_os_str(), OsStr::new("-noout"), OsStr::new("-text")],
    )
    .unwrap();
    assert!(root.status.success());
    let root = String::from_utf8(root.stdout).unwrap();
    assert!(root.contains("Name Constraints: critical"));
    assert!(root.contains("CA:TRUE, pathlen:0"));

    let leaf = tempfile::tempdir().unwrap();
    ca.issue(leaf.path(), &key, EXTENSIONS, LEAF_LIFETIME_DAYS).unwrap();
    assert!(!leaf.path().join("key.pem").exists());
    let verify = |name: &str| {
        Command::new("/usr/bin/security")
            .current_dir(leaf.path())
            // Explicit fixture root and no keychain search. This never adds system trust.
            .args(["verify-cert", "-q", "-L", "-N", "-p", "ssl", "-n", name, "-c", "cert.pem", "-r"])
            .arg(&ca.cert)
            .output()
            .unwrap()
    };
    for name in NAMES {
        assert!(verify(name).status.success(), "macOS refused {name}");
    }
    for (name, san) in [
        ("example.org", "DNS:example.org"),
        ("sub.localhost", "DNS:sub.localhost"),
        ("127.0.0.2", "IP:127.0.0.2"),
        ("::2", "IP:::2"),
    ] {
        let extensions = EXTENSIONS.replace("DNS:localhost,IP:127.0.0.1,IP:::1", san);
        ca.issue(leaf.path(), &key, &extensions, LEAF_LIFETIME_DAYS).unwrap();
        assert!(!verify(name).status.success(), "the localhost CA authenticated {name}");
    }
}

#[test]
fn renewal_window_uses_the_leaf_validity_and_rejects_a_different_issuer() {
    let storage = tempfile::tempdir().unwrap();
    let other_storage = tempfile::tempdir().unwrap();
    let leaf = tempfile::tempdir().unwrap();
    let fake_keychain = tempfile::tempdir().unwrap();
    let keychain = Keychain::in_folder(fake_keychain.path());
    let ca = Ca::create(storage.path(), &keychain).unwrap();
    let other = Ca::create(other_storage.path(), &keychain).unwrap();
    ca.issue(leaf.path(), &ca.key(&keychain, false).unwrap(), EXTENSIONS, LEAF_LIFETIME_DAYS).unwrap();
    let leaf = fs::read(leaf.path().join("cert.pem")).unwrap();
    let issued = UnixTime::now().as_secs();
    let after_days = |days: u64| UnixTime::since_unix_epoch(Duration::from_secs(issued + days * 86400));
    assert!(ca.current_at(&leaf, renewal_check_time(after_days(75))).unwrap());
    assert!(!ca.current_at(&leaf, renewal_check_time(after_days(77))).unwrap());
    assert!(ca.current_at(&leaf, after_days(89)).unwrap());
    assert!(!ca.current_at(&leaf, after_days(91)).unwrap());
    assert!(!other.current_at(&leaf, after_days(1)).unwrap());
    assert!(!ca.current_at(b"", after_days(1)).unwrap());
}

#[test]
fn ssl_trust_is_checked_before_prompting_and_checked_again_afterward() {
    let calls = RefCell::new(Vec::new());
    let ca = Path::new("ca.pem");
    let leaf = Path::new("leaf.pem");
    let security = |args: &[&OsStr]| {
        let mut calls = calls.borrow_mut();
        let already_trusted = calls.iter().any(|call: &Vec<String>| call[0] == "add-trusted-cert");
        let success = args[0] != "verify-cert" || already_trusted;
        calls.push(args.iter().map(|arg| arg.to_str().unwrap().into()).collect());
        Ok(Output {
            status: ExitStatus::from_raw(if success { 0 } else { 256 }),
            stdout: if args[0] == "login-keychain" { b"\"/test/login.keychain-db\"\n".to_vec() } else { vec![] },
            stderr: vec![],
        })
    };
    ensure_trusted(ca, leaf, security).unwrap();
    ensure_trusted(ca, leaf, security).unwrap();
    let calls = calls.into_inner();
    assert_eq!(
        calls.iter().map(|call| call[0].as_str()).collect::<Vec<_>>(),
        ["verify-cert", "login-keychain", "add-trusted-cert", "verify-cert", "verify-cert"]
    );
    assert_eq!(
        calls[2],
        ["add-trusted-cert", "-r", "trustRoot", "-p", "ssl", "-k", "/test/login.keychain-db", "ca.pem"]
    );
    assert_eq!(calls[0], ["verify-cert", "-q", "-L", "-p", "ssl", "-n", "localhost", "-c", "leaf.pem"]);
}

#[test]
fn agent_environment_uses_the_same_paths_as_the_guest_login_profile() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("key.pem");
    assert_eq!(environment(true, &key).count(), 0);
    fs::write(&key, "fixture").unwrap();
    assert_eq!(environment(false, &key).count(), 0);
    let script =
        include_str!("../../../../../nix/vm/localhost-tls-env.sh").replacen(GUEST_KEY, key.to_str().unwrap(), 1);
    let output = Command::new("/bin/sh").env_clear().args(["-c", &(script + "\n/usr/bin/env")]).output().unwrap();
    assert!(output.status.success());
    let output = String::from_utf8(output.stdout).unwrap();
    let env = environment(true, &key).collect::<Vec<_>>();
    assert_eq!(env.len(), 4);
    for (name, value) in env {
        assert!(output.lines().any(|line| line == format!("{name}={value}")), "{name}");
    }
}
