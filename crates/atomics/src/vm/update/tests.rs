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
    assert_eq!(modules, [Module::Shared(module.canonicalize().unwrap()), Module::Copied(b"{ }".to_vec())]);
    assert_eq!(warnings.borrow().len(), 1);
    assert!(warnings.borrow()[0].contains("Relative paths in it will not resolve"));

    // Reach the fixture through ~ without writing into the real home folder.
    let home = atomics_core::paths::home();
    let parents = home.ancestors().count() - 1;
    let tilde = format!("~/{}{}", "../".repeat(parents), outside.strip_prefix("/").unwrap().display());
    assert_eq!(resolve_modules(&[tilde], &[], &|_| {}).unwrap(), [Module::Copied(b"{ }".to_vec())]);
    assert!(resolve_modules(&[shared.display().to_string()], &[], &|_| {}).is_err());
}

#[test]
fn source_uses_only_a_shared_checkout_head_otherwise_this_release() {
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
    let selected = source(checkout.to_str().unwrap(), &shares, "0.1.7");
    assert!(selected.starts_with("git+file:///"));
    assert!(selected.contains("checkout%20with%20space"));
    assert!(selected.ends_with(&format!("?rev={rev}")));
    for configured in ["", "/missing", checkout.to_str().unwrap()] {
        assert_eq!(source(configured, &[], "0.1.7"), "github:xmagit/ATOMICS/v0.1.7");
    }
    let worktree = dir.path().join("worktree");
    git(&["worktree", "add", "--quiet", "--detach", worktree.to_str().unwrap(), "HEAD"]);
    std::fs::write(worktree.join("flake.nix"), "{}").unwrap();
    assert!(source(worktree.to_str().unwrap(), &shares, "0.1.7").starts_with("git+file://"));
    assert_eq!(
        source(worktree.to_str().unwrap(), &[worktree.canonicalize().unwrap()], "0.1.7"),
        "github:xmagit/ATOMICS/v0.1.7",
        "unshared Git metadata cannot be read by the guest"
    );
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
    let script = build_script(
        "git+file:///repos/check%20out?rev=abcdef",
        &settings(&info(), &build_info(&info(), &lima()).unwrap()),
        &[Module::Shared(PathBuf::from("/repos/team's ${module}.nix")), Module::Copied(b"{ imports = []; }".to_vec())],
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
        "<--argstr><src><git+file:///repos/check%20out?rev=abcdef>",
        "<--max-jobs><auto>",
        "<--argstr><modules><[\"/repos/team's ${module}.nix\",\"module-1.nix\"]>",
        "<toplevel>",
        "<bundle>",
    ] {
        assert!(log.contains(argument), "missing {argument}: {log}");
    }
    assert!(EXPRESSION.contains("system.build.toplevel") && EXPRESSION.contains("macDesktop.bundle"));
    assert!(EXPRESSION.contains("./. + (\"/\" + path)"), "copied modules resolve beside the expression");
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
    // A failed window quit or --no-restart leaves this exact state. Retrying still owes a restart,
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
fn a_window_that_refuses_to_quit_is_named_and_keeps_the_restart_pending() {
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("osascript");
    crate::test_support::write_program(
        &program,
        "#!/bin/sh\nprintf 'Could not quit: ATOMICS (pid 1234)\\n' >&2\nexit 1\n",
    );
    let error = quit_windows(Command::new(program)).unwrap_err().to_string();
    assert!(
        error.contains("restart pending") && error.contains("pid 1234") && error.contains("vm update again"),
        "{error}"
    );
}

#[test]
fn the_window_reopens_without_the_updaters_atomics_variables() {
    let env = [
        ("ATOMICS_CONFIG", "/tmp/scratch/config.toml"),
        ("ATOMICS_HOME", "/tmp/scratch"),
        ("ATOMICS_SOCKET", "/tmp/scratch/atomics.sock"),
        ("ATOMICS_HOST_SOCKET", "/tmp/scratch/host.sock"),
        ("ATOMICS_TEST_RELEASE_FEED", "http://127.0.0.1:1/feed"),
        ("HOME", "/Users/alice"),
        ("PATH", "/usr/bin:/bin"),
        ("LANG", "en_US.UTF-8"),
    ]
    .map(|(key, value)| (key.into(), value.into()));
    let command = reopen(Path::new("/Applications/ATOMICS.app"), env);
    assert_eq!(command.get_program(), "open");
    assert_eq!(command.get_args().collect::<Vec<_>>(), ["-a", "/Applications/ATOMICS.app"]);
    let mut passed: Vec<_> =
        command.get_envs().filter_map(|(key, value)| Some((key.to_str()?, value?.to_str()?))).collect();
    passed.sort();
    assert_eq!(passed, [("HOME", "/Users/alice"), ("LANG", "en_US.UTF-8"), ("PATH", "/usr/bin:/bin")]);
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
