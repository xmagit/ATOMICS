use super::*;

fn info() -> BundleInfo {
    serde_json::from_value(json!({
        "version": 1, "instance": "atomics-test", "user": "atomics", "home": "/home/atomics",
        "macUser": "alice", "installDir": "/Users/alice/.local/share/atomics-vm", "host": {"port": 47800}
    }))
    .unwrap()
}

fn lima() -> LimaConfig {
    serde_json::from_value(json!({
        "cpus": 8, "memory": "24GiB", "disk": "200GiB",
        "mounts": [
            {"location": "/Users/alice/repos", "mountPoint": "/Users/alice/repos"},
            {"location": "/elsewhere", "mountPoint": "/different"}
        ]
    }))
    .unwrap()
}

#[test]
fn settings_preserve_the_build_and_recover_old_bundles() {
    let mut info = info();
    let recovered = build_info(&info, &lima()).unwrap();
    assert_eq!(
        settings(&info, &recovered),
        json!({
            "macUser": "alice", "user": "atomics", "instance": "atomics-test",
            "cpus": 8, "memoryGiB": 24, "diskGiB": 200, "host": true,
            "hostPort": 47800, "installDir": "/Users/alice/.local/share/atomics-vm"
        })
    );
    info.build = Some(serde_json::from_value(json!({"cpus": 4, "memoryGiB": 8, "diskGiB": 100})).unwrap());
    assert_eq!(build_info(&info, &lima()).unwrap().memory_gib, 8, "saved inputs take precedence");
    let mut invalid = lima();
    invalid.memory = "24.5GiB".into();
    assert!(build_info(&self::info(), &invalid).is_err(), "never guess old settings");
    assert_eq!(shared_paths(&lima()), [PathBuf::from("/Users/alice/repos")]);
    assert!(!in_share(Path::new("/Users/alice/repos-other"), &shared_paths(&lima())));
}

#[test]
fn modules_in_shares_keep_relative_files_and_outside_modules_are_single_files() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("repos");
    std::fs::create_dir(&shared).unwrap();
    let module = shared.join("team.nix");
    std::fs::write(&module, "{ imports = [ ./extra.nix ]; }").unwrap();
    std::fs::write(shared.join("extra.nix"), "{}").unwrap();
    let outside = dir.path().join("outside.nix");
    std::fs::write(&outside, "{ }").unwrap();
    let warnings = std::cell::RefCell::new(Vec::new());
    let modules = resolve_modules(
        &[module.display().to_string(), outside.display().to_string()],
        &[shared.canonicalize().unwrap()],
        &|warning| warnings.borrow_mut().push(warning),
    )
    .unwrap();
    assert_eq!(
        modules,
        [
            Module::Shared(module.canonicalize().unwrap()),
            Module::Copied(outside.canonicalize().unwrap(), b"{ }".to_vec())
        ]
    );
    assert_eq!(warnings.borrow().len(), 1);
    assert!(warnings.borrow()[0].contains("Relative paths in it will not resolve"));

    // Reach the fixture through ~ without writing into the real home folder.
    let home = atomics_core::paths::home();
    let parents = home.ancestors().count() - 1;
    let tilde = format!("~/{}{}", "../".repeat(parents), outside.strip_prefix("/").unwrap().display());
    assert_eq!(
        resolve_modules(&[tilde], &[], &|_| {}).unwrap(),
        [Module::Copied(outside.canonicalize().unwrap(), b"{ }".to_vec())]
    );
    assert!(resolve_modules(&[shared.display().to_string()], &[], &|_| {}).is_err());
}

#[test]
fn source_uses_only_a_shared_checkout_head_otherwise_the_full_build_commit() {
    let commit = Some("0123456789abcdef0123456789abcdef01234567");
    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout with space");
    std::fs::create_dir(&checkout).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["-c", "user.name=alice", "-c", "user.email=alice@example.com"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "--quiet"]);
    git(&["commit", "--quiet", "--allow-empty", "-m", "Fixture"]);
    std::fs::write(checkout.join("flake.nix"), "untracked content is not built").unwrap();
    let rev = git(&["rev-parse", "HEAD"]);
    let shares = [dir.path().canonicalize().unwrap()];
    let selected = source(checkout.to_str().unwrap(), &shares, None).unwrap();
    assert!(selected.starts_with("git+file:///"));
    assert!(selected.contains("checkout%20with%20space"));
    assert!(selected.ends_with(&format!("?rev={rev}")));
    for configured in ["", "/missing", checkout.to_str().unwrap()] {
        assert_eq!(
            source(configured, &[], commit).unwrap(),
            "github:xmagit/ATOMICS/0123456789abcdef0123456789abcdef01234567"
        );
    }
    let worktree = dir.path().join("worktree");
    git(&["worktree", "add", "--quiet", "--detach", worktree.to_str().unwrap(), "HEAD"]);
    std::fs::write(worktree.join("flake.nix"), "{}").unwrap();
    assert!(source(worktree.to_str().unwrap(), &shares, None).unwrap().starts_with("git+file://"));
    assert_eq!(
        source(worktree.to_str().unwrap(), &[worktree.canonicalize().unwrap()], commit).unwrap(),
        "github:xmagit/ATOMICS/0123456789abcdef0123456789abcdef01234567",
        "unshared Git metadata cannot be read by the guest"
    );
}

#[test]
fn source_refuses_unknown_dirty_short_or_malformed_build_commits_instead_of_using_a_tag() {
    for commit in [
        None,
        Some(""),
        Some("unknown"),
        Some("v0.1.7"),
        Some("abcdef0"),
        Some("v0.1.7-3-gabcdef0"),
        Some("0123456789abcdef0123456789abcdef01234567-dirty"),
        Some("z123456789abcdef0123456789abcdef01234567"),
        Some("0123456789abcdef0123456789abcdef012345678"),
    ] {
        for configured in ["", "/missing"] {
            let error = source(configured, &[], commit).unwrap_err().to_string();
            assert!(error.contains("set [update] source to a local checkout"), "{commit:?}: {error}");
        }
    }
}

#[test]
fn busy_refuses_turns_and_user_waits_and_yes_is_an_explicit_override() {
    for status in ["working", "starting", "needs you"] {
        let answer = json!({"ok":true,"result":{"sessions":[{"title":"worker","status":status}]}});
        let blockers = busy(&answer).unwrap();
        let error = refuse_busy(&blockers, false).unwrap_err().to_string();
        assert!(error.contains("wait for this work to finish") && error.contains("--yes"));
        assert!(refuse_busy(&blockers, true).is_ok());
    }
    let answer = json!({"ok":true,"result":{"sessions":[],"update_blockers":["editor open", "background work"]}});
    assert_eq!(busy(&answer).unwrap(), ["editor open", "background work"]);
    assert!(busy(&json!({"ok": false, "error": "unreachable"})).is_err());
    assert!(busy(&json!({"ok":true,"result":{"sessions":[]}})).unwrap().is_empty());
}

#[test]
fn expression_and_nix_arguments_preserve_settings_paths_and_copied_modules() {
    let dir = tempfile::tempdir().unwrap();
    crate::test_support::write_program(
        dir.path().join("nix"),
        r#"#!/bin/sh
printf '<%s>' "$@" >> "$NIX_LOG"
printf '\n' >> "$NIX_LOG"
out=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --out-link) out=$2; shift 2 ;;
    toplevel) printf '/nix/store/00000000000000000000000000000000-nixos-system-test\n'; shift ;;
    bundle) printf '/nix/store/11111111111111111111111111111111-atomics-vm-mac\n'; shift ;;
    --extra-experimental-features|--max-jobs|-f) shift 2 ;;
    --argstr) shift 3 ;;
    build|--impure|--print-out-paths) shift ;;
    *) printf 'unexpected nix argument: %s\n' "$1" >&2; exit 1 ;;
  esac
done
test -n "$out"
test -z "${FAIL_BUILD-}"
"#,
    );
    let src = source("", &[], Some("0123456789abcdef0123456789abcdef01234567")).unwrap();
    let script = build_script(
        &src,
        &settings(&info(), &build_info(&info(), &lima()).unwrap()),
        &[
            Module::Shared(PathBuf::from("/repos/team's ${module}.nix")),
            Module::Copied(PathBuf::from("/Users/alice/team.nix"), b"{ imports = []; }".to_vec()),
        ],
    );
    let run = |fail: bool| {
        let script_file = dir.path().join("build.sh");
        std::fs::write(&script_file, &script).unwrap();
        let mut command = Command::new("bash");
        command
            .arg(script_file)
            .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
            .env("NIX_LOG", dir.path().join("log"))
            .env("TMPDIR", dir.path());
        if fail {
            command.env("FAIL_BUILD", "1");
        }
        command.output().unwrap()
    };
    let output = run(false);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let paths = String::from_utf8(output.stdout).unwrap();
    let stage = PathBuf::from(paths.lines().next().unwrap());
    assert_eq!(std::fs::read_to_string(stage.join("system.nix")).unwrap(), EXPRESSION);
    assert_eq!(std::fs::read_to_string(stage.join("module-1.nix")).unwrap(), "{ imports = []; }");
    assert!(!stage.join("module-0.nix").exists(), "shared module was not copied");
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
    for argument in [
        "<--argstr><src><github:xmagit/ATOMICS/0123456789abcdef0123456789abcdef01234567>",
        "<--max-jobs><auto>",
        "<--argstr><modules><[{\"name\":\"/repos/team's ${module}.nix\",\"path\":\"/repos/team's ${module}.nix\"},{\"name\":\"/Users/alice/team.nix\",\"path\":\"module-1.nix\"}]>",
        "<toplevel>",
        "<bundle>",
    ] {
        assert!(log.contains(argument), "missing {argument}: {log}");
    }
    assert!(EXPRESSION.contains("system.build.toplevel") && EXPRESSION.contains("macDesktop.bundle"));
    assert!(EXPRESSION.contains("./. + (\"/\" + module.path)"), "copied modules resolve beside the expression");
    assert!(
        EXPRESSION.contains("environment.etc.\"atomics/vm-build.json\"")
            && EXPRESSION.contains("builtins.hashFile \"sha256\" module.file")
            && EXPRESSION.contains("hardened = config.atomics.vm.hardened"),
        "the system records the modules it was built with, by their paths on the Mac, and hardened mode"
    );
    assert!(!run(true).status.success());
    std::fs::remove_dir_all(stage).unwrap();
}

#[test]
fn boot_selection_changes_only_after_a_successful_build_and_rolls_back_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    crate::test_support::write_program(dir.path().join("sudo"), "#!/bin/sh\nexec \"$@\"\n");
    crate::test_support::write_program(
        dir.path().join("nix-env"),
        r#"#!/bin/sh
test "$1" = -p && test "$3" = --set || exit 1
ln -sfn "$4" "$2"
printf 'profile %s\n' "$4" >> "$BOOT_LOG"
"#,
    );
    let root = dir.path().canonicalize().unwrap();
    let current = root.join("old");
    let next = root.join("new");
    for system in [&current, &next] {
        std::fs::create_dir_all(system.join("bin")).unwrap();
        crate::test_support::write_program(
            system.join("bin/switch-to-configuration"),
            r#"#!/bin/sh
printf 'boot %s\n' "$0" >> "$BOOT_LOG"
case "$0" in */new/*) test -z "${FAIL_BOOT-}" ;; *) exit 0 ;; esac
"#,
        );
    }
    let profile = dir.path().join("profile");
    std::os::unix::fs::symlink(&current, &profile).unwrap();
    let run = |top: &Path, fail: bool| {
        let script = switch_script(top.to_str().unwrap())
            .replace("/run/current-system", current.to_str().unwrap())
            .replace("/nix/var/nix/profiles/system", profile.to_str().unwrap());
        let mut command = Command::new("bash");
        command
            .args(["-c", &script])
            .env("PATH", format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap()))
            .env("BOOT_LOG", dir.path().join("log"));
        if fail {
            command.env("FAIL_BOOT", "1");
        }
        command.output().unwrap()
    };
    let unchanged = run(&current, false);
    assert!(unchanged.status.success());
    assert_eq!(String::from_utf8(unchanged.stdout).unwrap().trim(), "unchanged");
    assert!(!dir.path().join("log").exists(), "no boot commands on a no-change run");
    let failed = run(&next, true);
    assert!(!failed.status.success());
    assert_eq!(std::fs::read_link(&profile).unwrap(), current);
    assert!(std::fs::read_to_string(dir.path().join("log")).unwrap().contains("/old/bin/switch-to-configuration"));
    assert!(run(&next, false).status.success());
    assert_eq!(std::fs::read_link(&profile).unwrap(), next);
    // A failed stop or --no-restart leaves this exact state. Retrying still owes a restart,
    // even though the boot profile already points to the new build; do not stage it a second time.
    let before = std::fs::read_to_string(dir.path().join("log")).unwrap();
    let retry = run(&next, false);
    assert!(retry.status.success());
    assert_eq!(String::from_utf8(retry.stdout).unwrap().trim(), "restart-pending");
    assert_eq!(std::fs::read_to_string(dir.path().join("log")).unwrap(), before);
    let booted = switch_script(next.to_str().unwrap())
        .replace("/run/current-system", next.to_str().unwrap())
        .replace("/nix/var/nix/profiles/system", profile.to_str().unwrap());
    let booted = Command::new("bash").args(["-c", &booted]).output().unwrap();
    assert!(booted.status.success());
    assert_eq!(String::from_utf8(booted.stdout).unwrap().trim(), "unchanged");
    // After --no-restart, selecting the running system must also undo the pending next boot.
    let restored = run(&current, false);
    assert!(restored.status.success());
    assert_eq!(String::from_utf8(restored.stdout).unwrap().trim(), "unchanged");
    assert_eq!(std::fs::read_link(profile).unwrap(), current);
}

#[test]
fn the_same_vm_client_reconnects_after_restart() {
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use atomics_web::driver::Status;
    use tungstenite::WebSocket;
    use tungstenite::handshake::server::{Request, Response};

    use crate::client::ClientApp;
    use crate::client::link::{Address, Endpoint};

    let serve = |listener: TcpListener| -> JoinHandle<WebSocket<TcpStream>> {
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            #[allow(clippy::result_large_err)] // tungstenite's callback contract
            let upgrade = |request: &Request, mut response: Response| {
                assert_eq!(request.headers()["Authorization"], "Bearer vm-token");
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", atomics_core::wire::SUBPROTOCOL.parse().unwrap());
                Ok(response)
            };
            tungstenite::accept_hdr(stream, upgrade).unwrap()
        })
    };
    let wait_for = |app: &mut ClientApp, done: fn(&Status) -> bool| {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
        while !done(app.driver().status()) {
            assert!(Instant::now() < deadline, "unexpected client status: {:?}", app.driver().status());
            app.pump();
            thread::sleep(Duration::from_millis(5));
        }
    };

    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = Arc::new(Mutex::new(Some(first.local_addr().unwrap())));
    let first = serve(first);
    let dir = tempfile::tempdir().unwrap();
    let mut info = info();
    info.install_dir = dir.path().display().to_string();
    let gate =
        crate::client::vm::Gate::with(Some(info), dir.path().into(), crate::relay::Keychain::in_folder(dir.path()));
    let resolve = endpoint.clone();
    let mut app = ClientApp::fixed_vm(&egui::Context::default(), gate, move || {
        let addr = resolve.lock().unwrap().context("the VM is stopped")?;
        Ok(Endpoint {
            addr: Address::Tcp(addr),
            host: addr.to_string(),
            token: zeroize::Zeroizing::new("vm-token".into()),
        })
    });
    wait_for(&mut app, |status| *status == Status::Live);
    let old_host = first.join().unwrap();

    let mut new_host = None;
    restart_guest(
        || {
            *endpoint.lock().unwrap() = None;
            drop(old_host);
            wait_for(&mut app, |status| matches!(status, Status::Reconnecting { .. }));
            Ok(())
        },
        || Ok(()),
        || {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            *endpoint.lock().unwrap() = Some(listener.local_addr()?);
            new_host = Some(serve(listener));
            Ok(())
        },
    )
    .unwrap();

    wait_for(&mut app, |status| *status == Status::Live);
    let _new_host = new_host.unwrap().join().unwrap();
}

#[test]
fn a_failed_start_after_a_successful_stop_names_vm_start() {
    let calls = std::cell::RefCell::new(Vec::new());
    let result = restart_guest(
        || {
            calls.borrow_mut().push("stop");
            Ok(())
        },
        || {
            calls.borrow_mut().push("mounts");
            Ok(())
        },
        || {
            calls.borrow_mut().push("start");
            bail!("start failed");
        },
    );
    let error = format!("{:#}", result.unwrap_err());
    assert!(error.contains("VM is stopped") && error.contains("atomics vm start"), "{error}");
    assert!(!error.contains("atomics vm update"), "{error}");
    assert_eq!(*calls.borrow(), ["stop", "mounts", "start"]);
}

#[test]
fn lima_mounts_that_cannot_match_leave_the_vm_stopped_and_say_how_to_go_on() {
    let calls = std::cell::RefCell::new(Vec::new());
    let error = restart_guest(
        || {
            calls.borrow_mut().push("stop");
            Ok(())
        },
        || {
            calls.borrow_mut().push("mounts");
            bail!("limactl edit failed");
        },
        || {
            calls.borrow_mut().push("start");
            Ok(())
        },
    )
    .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("VM is stopped") && error.contains("limactl edit failed"), "{error}");
    assert!(error.contains("atomics vm update again") && error.contains("atomics vm start"), "{error}");
    assert_eq!(*calls.borrow(), ["stop", "mounts"], "a VM whose mounts differ from its system is not started");
}

#[test]
fn repeated_busy_checks_announce_yes_only_once_without_skipping_refusals() {
    let blockers = vec!["agent working".into()];
    let mut policy = BusyPolicy { yes: true, ..Default::default() };
    assert!(policy.check(&[]).unwrap().is_none());
    assert!(policy.check(&blockers).unwrap().unwrap().contains("Continuing with busy work (--yes)"));
    assert!(policy.check(&blockers).unwrap().is_none());
    let mut refuse = BusyPolicy::default();
    assert!(refuse.check(&[]).unwrap().is_none());
    assert!(refuse.check(&blockers).is_err());
    assert!(refuse.check(&blockers).is_err());
}

fn hardened() -> BundleInfo {
    BundleInfo { admin: Some("admin".into()), ..info() }
}

/// One ustar entry, as GNU tar writes it.
fn entry(name: &str, kind: u8, mode: u32, data: &[u8]) -> Vec<u8> {
    let mut header = [0_u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    header[100..108].copy_from_slice(format!("{mode:07o}\0").as_bytes());
    header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    header[156] = kind;
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].fill(b' ');
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = header.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out
}

fn archive(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut out = entries.concat();
    out.extend_from_slice(&[0; 1024]);
    out
}

#[test]
fn the_mac_bundle_never_comes_through_the_vms_own_user_in_hardened_mode() {
    // `limactl copy` and `limactl shell` log in as the agents' user, whose ssh login an agent can hook to
    // serve a tree of its own, which install.sh would then run on the Mac.
    let source = include_str!("../update.rs");
    assert!(!source.contains(r#""copy""#), "the bundle must not be copied with limactl copy");
    let command = bundle_source(&hardened()).unwrap();
    assert_eq!(command.get_program(), "ssh", "the admin's own ssh connection, not limactl");
    let args: Vec<_> = command.get_args().map(|arg| arg.to_str().unwrap()).collect();
    assert!(args.windows(2).any(|pair| pair == ["-l", "admin"]), "{args:?}");
    assert!(args.contains(&"ControlMaster=no") && args.contains(&"ControlPath=none"), "no shared master: {args:?}");

    // The stream from that connection unpacks into exactly the built tree.
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    crate::test_support::write_program(
        bin.join("ssh"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$SSH_LOG\"\nexec bash -s\n",
    );
    let store = dir.path().join("store");
    std::fs::create_dir_all(store.join("bin")).unwrap();
    std::fs::write(store.join("install.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(store.join("bin/claude"), "#!/bin/sh\n").unwrap();
    std::fs::write(store.join("atomics-vm.json"), "{}").unwrap();
    std::process::Command::new("chmod").args(["0755"]).arg(store.join("bin/claude")).status().unwrap();
    let fetch = |store: &Path, target: &Path| {
        let mut command = bundle_source(&hardened()).unwrap();
        command
            .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
            .env("SSH_LOG", dir.path().join("ssh.log"));
        fetch_bundle(command, store.to_str().unwrap(), target)
    };
    let target = dir.path().join("bundle");
    fetch(&store, &target).unwrap();
    assert!(std::fs::read_to_string(dir.path().join("ssh.log")).unwrap().contains("admin\n"));
    assert_eq!(std::fs::read_to_string(target.join("install.sh")).unwrap(), "#!/bin/sh\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(target.join("bin/claude")).unwrap().permissions().mode() & 0o777, 0o755);
    assert_eq!(std::fs::metadata(target.join("install.sh")).unwrap().permissions().mode() & 0o777, 0o644);
    validate_bundle(&target).unwrap();

    // A tree with a link in it is refused while unpacking, before anything could run.
    std::os::unix::fs::symlink("/etc", store.join("bin/elsewhere")).unwrap();
    assert!(fetch(&store, &dir.path().join("linked")).is_err());
}

#[test]
fn the_bundle_stream_holds_only_plain_files_and_folders_inside_the_bundle() {
    let unpack = |entries: &[Vec<u8>]| {
        let dir = tempfile::tempdir().unwrap();
        let result = extract_bundle(&archive(entries)[..], &dir.path().join("bundle"));
        (dir, result)
    };
    let (dir, ok) = unpack(&[
        entry("./", b'5', 0o755, b""),
        entry("./bin/", b'5', 0o755, b""),
        entry("./bin/codex", b'0', 0o555, b"#!/bin/sh\n"),
        entry("./install.sh", b'0', 0o444, &[b'x'; 700]),
    ]);
    ok.unwrap();
    assert_eq!(std::fs::read(dir.path().join("bundle/install.sh")).unwrap(), [b'x'; 700]);
    assert_eq!(std::fs::read(dir.path().join("bundle/bin/codex")).unwrap(), b"#!/bin/sh\n");

    for (what, bad) in [
        ("a symlink", entry("./install.sh", b'2', 0o777, b"")),
        ("a hard link", entry("./install.sh", b'1', 0o644, b"")),
        ("a GNU long name", entry("././@LongLink", b'L', 0o644, b"install.sh\0")),
        ("a pax header", entry("./PaxHeaders/x", b'x', 0o644, b"")),
        ("a device", entry("./dev", b'3', 0o644, b"")),
        ("a parent path", entry("../install.sh", b'0', 0o644, b"")),
        ("an absolute path", entry("/tmp/install.sh", b'0', 0o644, b"")),
    ] {
        assert!(unpack(&[bad]).1.is_err(), "{what} must be refused");
    }
    let twice = entry("./install.sh", b'0', 0o644, b"a");
    assert!(unpack(&[twice.clone(), twice]).1.is_err(), "an entry twice must be refused");
    assert!(unpack(&[entry("./bin/x", b'0', 0o644, b"")]).1.is_err(), "a file before its folder");
    let mut corrupt = entry("./install.sh", b'0', 0o644, b"a");
    corrupt[0] = b'X';
    assert!(unpack(&[corrupt]).1.is_err(), "a bad checksum must be refused");
    let truncated = entry("./install.sh", b'0', 0o644, b"abc");
    assert!(extract_bundle(&truncated[..512], &tempfile::tempdir().unwrap().path().join("b")).is_err());
}

fn share(path: &str, writable: bool) -> Share {
    Share { location: PathBuf::from(path), mount_point: PathBuf::from(path), writable }
}

#[test]
fn lima_mounts_parse_with_limas_defaults() {
    let lima: LimaConfig = serde_json::from_value(json!({
        "cpus": 8, "memory": "24GiB", "disk": "200GiB",
        "mounts": [
            {"location": "/Users/alice/repos", "mountPoint": "/Users/alice/repos", "writable": true},
            {"location": "/Users/alice/.atomics/briefs"}
        ]
    }))
    .unwrap();
    assert_eq!(lima.shares(), [share("/Users/alice/repos", true), share("/Users/alice/.atomics/briefs", false)]);
}

#[test]
fn mount_reconciliation_plans_exactly_the_systems_shares() {
    let built = [
        share("/Users/alice/repos", false),
        share("/Users/alice/.atomics/briefs", false),
        share("/Users/alice/atomics-exchange", true),
    ];
    assert_eq!(plan_mounts(&built, &built), MountPlan::default(), "nothing to edit when they match");
    let reordered = [built[2].clone(), built[0].clone(), built[1].clone()];
    assert_eq!(plan_mounts(&built, &reordered), MountPlan::default(), "order is not a difference");

    // An instance made before hardened mode: ~/repos writable, no exchange, and the whole home shared.
    let old =
        [share("/Users/alice/repos", true), share("/Users/alice/.atomics/briefs", false), share("/Users/alice", false)];
    let plan = plan_mounts(&built, &old);
    assert_eq!(plan.changes.len(), 3, "{:#?}", plan.changes);
    assert!(plan.changes[0].starts_with("/Users/alice/repos: read-only in the system but writable in Lima"));
    assert!(plan.changes[0].contains("root in the VM could write it"));
    assert_eq!(plan.changes[1], "/Users/alice/atomics-exchange: not shared by Lima; add it, writable");
    assert_eq!(plan.changes[2], "/Users/alice: shared by Lima (read-only) but not by the system; remove it");
    let set = plan.set.unwrap();
    let list: Value = serde_json::from_str(set.strip_prefix(".mounts = ").unwrap()).unwrap();
    assert_eq!(
        list,
        json!([
            {"location": "/Users/alice/repos", "mountPoint": "/Users/alice/repos", "writable": false},
            {"location": "/Users/alice/.atomics/briefs", "mountPoint": "/Users/alice/.atomics/briefs", "writable": false},
            {"location": "/Users/alice/atomics-exchange", "mountPoint": "/Users/alice/atomics-exchange", "writable": true}
        ]),
        "the edit replaces Lima's list with the system's"
    );

    let read_only_exchange = [built[0].clone(), built[1].clone(), share("/Users/alice/atomics-exchange", false)];
    assert_eq!(
        plan_mounts(&built, &read_only_exchange).changes,
        ["/Users/alice/atomics-exchange: writable in the system but read-only in Lima; make it writable"]
    );
    let elsewhere = Share { mount_point: PathBuf::from("/mnt/repos"), ..built[0].clone() };
    let moved = plan_mounts(&built, &[elsewhere, built[1].clone(), built[2].clone()]);
    assert!(moved.changes[0].contains("mounts it at /mnt/repos in the guest; mount it at /Users/alice/repos"));
    let twice = [built[0].clone(), built[0].clone(), built[1].clone(), built[2].clone()];
    assert!(plan_mounts(&built, &twice).set.is_some(), "a share listed twice is rewritten too");
}

#[test]
fn mount_reconciliation_edits_once_and_proves_the_result() {
    let built = [share("/Users/alice/repos", false), share("/Users/alice/atomics-exchange", true)];
    let old = vec![share("/Users/alice/repos", true)];
    let instance = std::cell::RefCell::new(old.clone());
    let edits = std::cell::RefCell::new(Vec::new());
    apply_mounts_with(
        &built,
        || Ok(instance.borrow().clone()),
        |set| {
            edits.borrow_mut().push(set.to_string());
            *instance.borrow_mut() = built.to_vec();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(edits.borrow().len(), 1);
    assert!(edits.borrow()[0].starts_with(".mounts = [{\"location\":\"/Users/alice/repos\""));

    // Matching already: Lima is not edited.
    apply_mounts_with(&built, || Ok(built.to_vec()), |_| panic!("no edit when the mounts match")).unwrap();

    // A mount Lima adds to every instance survives the edit, and that is an error, not a pass.
    let error = apply_mounts_with(
        &built,
        || Ok(vec![built[0].clone(), built[1].clone(), share("/Users/alice", true)]),
        |_| Ok(()),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("/Users/alice: shared by Lima (writable)") && error.contains("override.yaml"), "{error}");

    let error = apply_mounts_with(&built, || Ok(old.clone()), |_| bail!("limactl edit failed")).unwrap_err();
    assert!(error.to_string().contains("limactl edit failed"));
}

#[test]
fn bundle_shares_come_from_json_bundles_directly_and_from_lima_for_yaml_ones() {
    let dir = crate::test_support::tempdir().unwrap();
    std::fs::write(
        dir.path().join("lima.yaml"),
        json!({"vmType": "vz", "mounts": [
            {"location": "/Users/alice/repos", "mountPoint": "/Users/alice/repos", "writable": false},
            {"location": "/Users/alice/atomics-exchange", "mountPoint": "/Users/alice/atomics-exchange", "writable": true}
        ]})
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        bundle_shares(dir.path()).unwrap(),
        [share("/Users/alice/repos", false), share("/Users/alice/atomics-exchange", true)]
    );

    // YAML goes through Lima's own parser, which prints the list as JSON.
    let yaml = dir.path().join("system.yaml");
    std::fs::write(&yaml, "mounts:\n- location: /Users/alice/repos\n  writable: false\n").unwrap();
    let limactl = dir.path().join("limactl");
    crate::test_support::write_program(
        &limactl,
        r#"#!/bin/sh
printf '%s\n' "$@" > "$LIMACTL_LOG"
test "$1 $2" = "template yq" || exit 1
printf '%s\n' '[{"location":"/Users/alice/repos","mountPoint":"/Users/alice/repos","writable":false}]'
"#,
    );
    let mut command = Command::new(&limactl);
    command.env("LIMACTL_LOG", dir.path().join("log"));
    assert_eq!(yaml_shares(command, &yaml).unwrap(), [share("/Users/alice/repos", false)]);
    let log = std::fs::read_to_string(dir.path().join("log")).unwrap();
    assert!(log.contains(yaml.to_str().unwrap()) && log.contains("to_json(0)"), "{log}");
    let failing = dir.path().join("failing");
    crate::test_support::write_program(&failing, "#!/bin/sh\necho 'cannot parse' >&2\nexit 1\n");
    let error = yaml_shares(Command::new(&failing), &yaml).unwrap_err().to_string();
    assert!(error.contains("cannot parse"), "{error}");
}

#[test]
fn modules_update_sources_and_the_install_folder_are_refused_inside_a_writable_share() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repos = root.join("repos");
    let exchange = root.join("atomics-exchange");
    for folder in [&repos, &exchange, &repos.join("checkout"), &exchange.join("checkout")] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(repos.join("team.nix"), "{ }").unwrap();
    std::fs::write(exchange.join("team.nix"), "{ }").unwrap();
    let shares = [
        Share { location: repos.clone(), mount_point: repos.clone(), writable: false },
        Share { location: exchange.clone(), mount_point: exchange.clone(), writable: true },
    ];
    let writable = writable_folders(&shares);
    assert_eq!(writable, std::slice::from_ref(&exchange));
    let path = |path: &Path| path.display().to_string();
    let install = root.join("bundle").display().to_string();

    refuse_writable(&[path(&repos.join("team.nix"))], &path(&repos.join("checkout")), &install, &writable)
        .expect("read-only shares and folders outside the shares are fine");

    let error = refuse_writable(&[path(&exchange.join("team.nix"))], "", &install, &writable).unwrap_err().to_string();
    assert!(error.starts_with("[vm] modules: ") && error.contains("a folder the VM can write"), "{error}");
    assert!(error.contains("The exchange folder"), "{error}");

    // A link in a read-only share that leads into the exchange is the exchange's.
    std::os::unix::fs::symlink(exchange.join("team.nix"), repos.join("linked.nix")).unwrap();
    assert!(refuse_writable(&[path(&repos.join("linked.nix"))], "", &install, &writable).is_err());

    let error = refuse_writable(&[], &path(&exchange.join("checkout")), &install, &writable).unwrap_err().to_string();
    assert!(error.starts_with("[update] source: "), "{error}");
    // A worktree outside the exchange whose git folder is inside it: git would read the VM's config.
    std::fs::write(repos.join("checkout/.git"), format!("gitdir: {}\n", exchange.join("checkout").display())).unwrap();
    let error = refuse_writable(&[], &path(&repos.join("checkout")), &install, &writable).unwrap_err().to_string();
    assert!(error.starts_with("[update] source: ") && error.contains("atomics-exchange"), "{error}");

    let error = refuse_writable(&[], "", &path(&exchange.join("bundle")), &writable).unwrap_err().to_string();
    assert!(error.starts_with("The bundle's install folder"), "{error}");

    // APFS ignores case: the same folder under another spelling is still the exchange.
    let shouting = root.join("ATOMICS-EXCHANGE");
    if shouting.exists() {
        assert!(refuse_writable(&[path(&shouting.join("team.nix"))], "", &install, &writable).is_err());
    }
}

#[test]
fn a_new_vm_takes_its_first_modules_from_a_writable_share_and_the_new_system_must_close_it() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (repos, exchange) = (root.join("repos"), root.join("atomics-exchange"));
    for folder in [&repos, &exchange] {
        std::fs::create_dir_all(folder).unwrap();
    }
    std::fs::write(repos.join("team.nix"), "{ }").unwrap();
    let modules = [repos.join("team.nix").display().to_string()];
    let install = root.join("bundle").display().to_string();
    let check = |shares: &[Share]| refuse_writable(&modules, "", &install, &writable_folders(shares));
    // The standard image shares ~/repos writable: the team module that hardens the VM sits in a folder it can write.
    let standard = [Share { location: repos.clone(), mount_point: repos.clone(), writable: true }];
    let hardened = [
        Share { location: repos.clone(), mount_point: repos.clone(), writable: false },
        Share { location: exchange.clone(), mount_point: exchange.clone(), writable: true },
    ];

    // `atomics vm update` keeps refusing it before the build.
    let error = before_build(check(&standard), false).unwrap_err().to_string();
    assert!(error.starts_with("[vm] modules: ") && error.contains("a folder the VM can write"), "{error}");
    // `atomics vm setup`, on the VM it just created (nothing has run there), builds from it this once.
    before_build(check(&standard), true).expect("a new VM's first build takes the module");
    before_build(Ok(()), true).unwrap();
    before_build(Ok(()), false).unwrap();

    // After the build the new system's shares decide, for a new VM too: hardened mode makes ~/repos read-only,
    // and a module that leaves it writable is still refused, so nothing is switched.
    check(&hardened).expect("the hardened system shares ~/repos read-only");
    assert!(check(&standard).is_err(), "a system that keeps the module's share writable is refused after the build");
}
