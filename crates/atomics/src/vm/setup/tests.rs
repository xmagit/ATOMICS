use super::*;

#[test]
fn applying_an_unchanged_config_reports_no_change_instead_of_a_write() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("host.toml");
    let text = "[chief]\nwake_secs = 60\n";
    std::fs::write(&file, text).unwrap();
    let proposal = Proposal {
        scope: Scope::Host,
        file: file.to_string_lossy().into(),
        before: Some(text.into()),
        after: text.into(),
        diff: String::new(),
        changes: Vec::new(),
        warnings: Vec::new(),
    };
    assert!(apply_config(&proposal, 1).unwrap().starts_with("No changes to "));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no extra snapshot or temporary file");
    std::fs::remove_file(&file).unwrap();
    let proposal = Proposal { before: None, ..proposal };
    assert!(apply_config(&proposal, 2).unwrap().starts_with("Wrote "));
    assert_eq!(std::fs::read_to_string(file).unwrap(), text);
}

#[test]
fn hardened_checks_use_admin_ssh_and_probe_the_guests_disk_with_read_only_shares() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("bin")).unwrap();
    std::fs::write(root.join("guest-check.sh"), CHECK_SCRIPT).unwrap();
    std::fs::write(
        root.join("check.sh"),
        r#"set -euo pipefail
. "$VM_SCRIPT"
if [ "$1" = --print-sshd-configs ]; then main "$@"; exit; fi
check_arguments "$@"
check_sshd
check_lima_mounts
check_host_agent
"#,
    )
    .unwrap();
    crate::test_support::write_program(
        root.join("bin/ssh"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "$@" > "$VM_SSH_LOG"
test "$1" = -F
case " $* " in *' -l admin lima-vm-test '*) ;; *) exit 1 ;; esac
test "${FAIL_ADMIN-false}" = false || exit 1
for remote; do :; done
export SSH_CONNECTION='192.0.2.1 4444 192.0.2.2 22' ADMIN_PROBE=true
test "$remote" = "bash --login -c 'exec bash -s -- --print-sshd-configs --hardened true'"
exec bash -s -- --print-sshd-configs --hardened true
"#,
    );
    crate::test_support::write_program(
        root.join("bin/id"),
        "#!/bin/sh\ncase \"$*\" in '-nu 1000') echo atomics;; '-nu 1001') echo admin;; *) exec /usr/bin/id \"$@\";; esac\n",
    );
    crate::test_support::write_program(
        root.join("bin/sudo"),
        r#"#!/bin/sh
set -eu
test "${ADMIN_PROBE-false}" = true
test "$1 $2 $3" = 'sshd -T -C'
case "$4" in
  user=atomics,host=192.0.2.1,addr=192.0.2.1,laddr=192.0.2.2,lport=22) keys=$KEY_FILES ;;
  user=admin,host=192.0.2.1,addr=192.0.2.1,laddr=192.0.2.2,lport=22) keys=$ADMIN_KEY_FILES ;;
  *) exit 1 ;;
esac
printf 'PasswordAuthentication %s\nKbdInteractiveAuthentication no\nPermitRootLogin no\nStreamLocalBindUnlink yes\n' "$PASSWORD_AUTH"
printf 'AuthorizedKeysFile %s\n' "$keys"
"#,
    );
    crate::test_support::write_program(
        root.join("bin/limactl"),
        r#"#!/bin/sh
set -eu
test "$1" = shell && test "$2" = --workdir=/home/atomics && test "$3" = vm-test
test "$4" = bash && test "$5" = --login
shift 8
exec bash -s -- "$@"
"#,
    );
    crate::test_support::write_program(
        root.join("bin/atomics"),
        r#"#!/bin/sh
set -eu
test "$1" = ctl
case "$2" in
  *'"start"'*) printf '%s' "$2" | jq -r .folder > "$VM_PROBE"; printf '{"result":{"id":"probe"}}\n' ;;
  *'"list"'*) printf '{"result":{"sessions":[{"id":"probe","pid":1234}]}}\n' ;;
  *'"close"'*) printf closed > "$VM_CLOSED" ;;
  *) exit 1 ;;
esac
"#,
    );
    crate::test_support::write_program(
        root.join("bin/readlink"),
        r#"#!/bin/sh
if [ "$2" = /proc/1234/cwd ]; then cat "$VM_PROBE"; else printf '%s\n' "$2"; fi
"#,
    );
    let info: BundleInfo = serde_json::from_value(serde_json::json!({
        "version":1,"instance":"vm-test","user":"atomics","home":"/home/atomics",
        "macUser":"user","installDir":"/bundle","admin":"admin","host":{"port":47800}
    }))
    .unwrap();
    let key_file = "/etc/ssh/lima_keys/%u";
    let run = |password: &str, fail_admin: bool, key_files: &str, admin_keys: &str| {
        std::fs::write(
            root.join("lima-mounts"),
            r#"[{"location":"/read-only","mountPoint":"/read-only","writable":false}]"#,
        )
        .unwrap();
        let mut runner = check_runner(&info, &root.join("bin/limactl"), root).unwrap();
        runner
            .env("PATH", crate::test_support::path_with(&root.join("bin")))
            .env("HOME", root.join("home"))
            .env("VM_SCRIPT", root.join("guest-check.sh"))
            .env("VM_SSH_LOG", root.join("ssh.log"))
            .env("VM_PROBE", root.join("probe"))
            .env("VM_CLOSED", root.join("closed"))
            .env("ATOMICS_MAC_SHARES", "/read-only:ro")
            .env("KEY_FILES", key_files)
            .env("ADMIN_KEY_FILES", admin_keys)
            .env("PASSWORD_AUTH", password)
            .env("FAIL_ADMIN", fail_admin.to_string());
        runner.output().unwrap()
    };
    let passed = run("no", false, key_file, key_file);
    assert!(passed.status.success(), "{}", String::from_utf8_lossy(&passed.stderr));
    assert!(String::from_utf8_lossy(&passed.stdout).contains("sshd: keys only"));
    assert!(
        String::from_utf8_lossy(&passed.stdout)
            .contains("lima mounts: as this system mounts them (the Mac's Lima config)"),
        "Lima's mounts reach the guest's check: {}",
        String::from_utf8_lossy(&passed.stdout)
    );
    assert!(
        std::fs::read_to_string(root.join("probe")).unwrap().starts_with(root.join("home/repos").to_str().unwrap())
    );
    assert!(root.join("closed").exists());
    assert!(!run("no", false, &format!("%h/.ssh/authorized_keys {key_file}"), key_file).status.success());
    assert!(!run("no", false, "/etc/ssh/lima_keys/%U", key_file).status.success());
    assert!(!run("no", false, key_file, "%h/.ssh/authorized_keys").status.success());
    assert!(!run("yes", false, key_file, key_file).status.success(), "password authentication still fails the check");
    assert!(
        !run("no", true, key_file, key_file).status.success(),
        "admin inspection failures must not fall back to the guest's sudo"
    );
}

/// scripts/check-atomics-vm.sh's comparison of Lima's mounts with the shares this system mounts, as the chief's
/// throwaway VM runs it: `--lima-mounts` is `limactl list --format '{{json .Config.Mounts}}' INSTANCE`.
#[test]
fn the_guest_check_fails_when_lima_shares_writable_what_the_system_mounts_read_only() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("guest-check.sh"), CHECK_SCRIPT).unwrap();
    std::fs::create_dir(root.join("cidata")).unwrap();
    std::fs::create_dir(root.join("bin")).unwrap();
    let shares = "/Users/alice/repos:ro /Users/alice/.atomics/briefs:ro /Users/alice/atomics-exchange:rw";
    let lima = |repos: bool, extra: &str| {
        format!(
            r#"[{{"location":"/Users/alice/repos","mountPoint":"/Users/alice/repos","writable":{repos}}},
                {{"location":"/Users/alice/.atomics/briefs","mountPoint":"/Users/alice/.atomics/briefs","writable":false}},
                {{"location":"/Users/alice/atomics-exchange","mountPoint":"/Users/alice/atomics-exchange","writable":true}}{extra}]"#
        )
    };
    let run = |args: &[&str], cidata: bool| {
        let output = Command::new("bash")
            .args(["-c", r#"set -euo pipefail; . "$VM_SCRIPT"; check_arguments "$@"; check_lima_mounts"#, "check"])
            .args(args)
            .env("PATH", crate::test_support::path_with(&root.join("bin")))
            .env("VM_SCRIPT", root.join("guest-check.sh"))
            .env("ATOMICS_MAC_SHARES", shares)
            .env("ATOMICS_CHECK_CIDATA", if cidata { root.join("cidata") } else { root.join("absent") })
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (ok, out, err) = run(&["--hardened", "true", "--lima-mounts", &lima(false, "")], false);
    assert!(ok, "{err}");
    assert_eq!(out, "lima mounts: as this system mounts them (the Mac's Lima config)\n");

    // The system mounts ~/repos read-only, but the Lima device is writable, so root can remount it.
    let (ok, _, err) = run(&["--hardened", "true", "--lima-mounts", &lima(true, "")], false);
    assert!(!ok);
    assert_eq!(
        err,
        "share /Users/alice/repos: this system mounts it read-only, but Lima (the Mac's Lima config) shares it \
         writable, so root here could mount it read-write. On the Mac, run atomics vm update: it makes the Lima \
         mount read-only\n"
    );

    let extra = r#",{"location":"/Users/alice","mountPoint":"/Users/alice","writable":true}"#;
    let (ok, _, err) = run(&["--lima-mounts", &lima(false, extra)], false);
    assert!(
        !ok && err.contains("Lima (the Mac's Lima config) shares /Users/alice (rw), which this system does not mount"),
        "{err}"
    );
    let (ok, _, err) = run(&["--lima-mounts", "not json"], false);
    assert!(!ok && err.contains("not the JSON list Lima prints"), "{err}");

    // This boot's cidata, where the guest's user may read it: Lima writes "ro,nofail" for a read-only mount.
    let cidata = |repos: &str| {
        std::fs::write(
            root.join("cidata/user-data"),
            format!(
                "#cloud-config\nmounts:\n- [vz-rosetta, /mnt/lima-rosetta, virtiofs, defaults, \"0\", \"0\"]\n\
                 - [lima-1, /Users/alice/repos, virtiofs, \"{repos}\", \"0\", \"0\"]\n\
                 - [lima-2, /Users/alice/.atomics/briefs, virtiofs, \"ro,nofail\", \"0\", \"0\"]\n\
                 - [lima-3, /Users/alice/atomics-exchange, virtiofs, \"nofail\", \"0\", \"0\"]\n"
            ),
        )
        .unwrap()
    };
    cidata("ro,nofail");
    let (ok, out, err) = run(&["--hardened", "true"], true);
    assert!(ok && out.contains("(this boot's cidata)"), "{out}{err}");
    let (ok, out, err) = run(&["--hardened", "true", "--lima-mounts", &lima(false, "")], true);
    assert!(ok && out.contains("(the Mac's Lima config and this boot's cidata)"), "{out}{err}");
    cidata("nofail");
    let (ok, _, err) = run(&["--hardened", "true", "--lima-mounts", &lima(false, "")], true);
    assert!(
        !ok && err.contains("Lima (this boot's cidata) shares it writable"),
        "a config fixed but not yet booted: {err}"
    );

    // Nothing to compare with: hardened guests fail and say how to run it; others note the skip.
    let (ok, _, err) = run(&["--hardened", "true"], false);
    assert!(
        !ok && err.contains("--lima-mounts \"$(limactl list --format '{{json .Config.Mounts}}' INSTANCE)\""),
        "{err}"
    );
    let (ok, out, _) = run(&[], false);
    assert!(ok && out.contains("not compared"), "{out}");
}

/// `--device-probe`, which the chief runs as root in a throwaway VM: root remounts each read-only share read-write
/// and tries to write it. Here a folder that refuses writes stands in for a read-only device.
#[test]
fn the_root_device_probe_passes_only_when_root_cannot_write_a_read_only_share() {
    let dir = crate::test_support::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let bin = root.join("bin");
    let share = root.join("repos");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&share).unwrap();
    std::fs::write(root.join("guest-check.sh"), CHECK_SCRIPT).unwrap();
    std::fs::write(
        root.join("desktop.env"),
        format!("ATOMICS_MAC_SHARES=\"{}:ro {}:rw\"\n", share.display(), root.join("exchange").display()),
    )
    .unwrap();
    crate::test_support::write_program(bin.join("id"), "#!/bin/sh\necho \"${FAKE_UID-0}\"\n");
    crate::test_support::write_program(bin.join("findmnt"), "#!/bin/sh\necho \"${FAKE_OPTIONS-ro,nosuid,nodev}\"\n");
    crate::test_support::write_program(
        bin.join("mount"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MOUNT_LOG\"\n[ -z \"${REFUSE_REMOUNT-}\" ] || [ \"$2\" != remount,rw ] || { echo 'mount: permission denied' >&2; exit 32; }\n",
    );
    let run = |env: &[(&str, &str)]| {
        let mut command = Command::new("bash");
        command
            .args([root.join("guest-check.sh").to_str().unwrap(), "--device-probe"])
            .env("PATH", crate::test_support::path_with(&bin))
            .env("ATOMICS_CHECK_DESKTOP_ENV", root.join("desktop.env"))
            .env("MOUNT_LOG", root.join("mount.log"));
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    use std::os::unix::fs::PermissionsExt;
    let mode = |mode| std::fs::set_permissions(&share, std::fs::Permissions::from_mode(mode)).unwrap();

    // A read-only device: the remount goes through, the write does not.
    mode(0o555);
    if std::fs::write(share.join("root-can"), "").is_ok() {
        // Running as root, a folder's mode stops nothing; this stand-in for a read-only device needs a user.
        mode(0o755);
        return;
    }
    let (ok, out, err) = run(&[]);
    assert!(ok, "{err}");
    assert!(out.contains("read-only at the device: root remounted it read-write, and could not write it"), "{out}");
    let log = std::fs::read_to_string(root.join("mount.log")).unwrap();
    assert!(log.contains(&format!("-o remount,rw {}\n-o remount,ro {}\n", share.display(), share.display())), "{log}");
    assert!(!log.contains("exchange"), "writable shares are not probed: {log}");
    let (ok, out, _) = run(&[("REFUSE_REMOUNT", "1")]);
    assert!(ok && out.contains("root's remount was refused (mount: permission denied)"), "{out}");

    // A writable device: root writes the Mac's folder. The probe fails, removes its file and remounts read-only.
    mode(0o755);
    std::fs::remove_file(root.join("mount.log")).unwrap();
    let (ok, _, err) = run(&[]);
    assert!(!ok && err.contains("but root wrote a file there after a remount"), "{err}");
    assert_eq!(std::fs::read_dir(&share).unwrap().count(), 0, "the probe's file is removed");
    assert!(
        std::fs::read_to_string(root.join("mount.log"))
            .unwrap()
            .ends_with(&format!("-o remount,ro {}\n", share.display()))
    );

    let (ok, _, err) = run(&[("FAKE_OPTIONS", "rw,nosuid")]);
    assert!(!ok && err.contains("not mounted read-only here"), "{err}");
    let (ok, _, err) = run(&[("FAKE_UID", "1000")]);
    assert!(!ok && err.contains("run it as root"), "{err}");
}

/// A bundle installed in a fresh folder, for the steps that read it.
fn installed_bundle(admin: bool) -> (crate::test_support::TempDir, PathBuf) {
    let dir = crate::test_support::tempdir().unwrap();
    let mut info = serde_json::json!({
        "version": 1, "instance": "vm-test", "user": "atomics", "home": "/home/atomics",
        "macUser": "alice", "installDir": dir.path(), "host": {"port": 47800}
    });
    if admin {
        info["admin"] = "admin".into();
    }
    std::fs::write(dir.path().join(super::super::BUNDLE_INFO), info.to_string()).unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn generic_guest() -> super::super::built::Guest {
    super::super::built::Guest { running: None, next_boot: None, wheel: true, admin: false }
}

/// `atomics vm setup` on an instance it did not just create applies nothing, and says that the VM does not run the
/// `[vm] modules` configured since, with the command that fixes it.
#[test]
fn setup_says_when_modules_configured_since_are_not_what_runs_and_names_the_fix() {
    let (_dir, installed) = installed_bundle(false);
    let module = installed.join("team.nix");
    std::fs::write(&module, "{ atomics.vm.hardened = true; }").unwrap();
    let modules = [module.display().to_string()];
    let mut out = Vec::new();
    modules_step(
        &mut out,
        false,
        &installed,
        &modules,
        |_| panic!("an instance set up before is not updated by setup"),
        |info| {
            assert_eq!(info.instance, "vm-test");
            Ok(generic_guest())
        },
    )
    .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(!text.contains("Apply your NixOS modules"), "{text}");
    assert!(text.contains("── Your NixOS modules and hardened mode\n"), "{text}");
    assert!(text.contains("The VM does not run the system this Mac configures:\n"), "{text}");
    assert!(text.contains("the running system does not record its [vm] modules"), "{text}");
    assert!(text.contains("Fix: atomics vm update "), "{text}");

    // A new instance gets them applied, and the check then reads the updated bundle: hardened, with an admin.
    let mut out = Vec::new();
    let applied = std::cell::Cell::new(false);
    modules_step(
        &mut out,
        true,
        &installed,
        &modules,
        |_| {
            let (_hardened_dir, hardened) = installed_bundle(true);
            std::fs::copy(hardened.join(super::super::BUNDLE_INFO), installed.join(super::super::BUNDLE_INFO)).unwrap();
            applied.set(true);
            Ok(())
        },
        |info| {
            assert_eq!(info.admin.as_deref(), Some("admin"), "the check asks the updated bundle's admin");
            Ok(generic_guest())
        },
    )
    .unwrap();
    assert!(applied.get());
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("── Apply your NixOS modules\n"), "{text}");
    assert!(
        text.contains(
            "hardened mode: configured, but not active: its user atomics is in wheel and may use sudo without a password"
        ),
        "{text}"
    );
    assert!(text.contains("Fix: atomics vm update "), "{text}");
}

/// `atomics vm config --apply` writes the bundle's settings, then says what the VM runs against the config as it is.
#[test]
fn config_apply_says_when_the_vm_does_not_run_what_this_mac_configures() {
    use super::super::built::{Drift, Report};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("host.toml");
    let proposal = Proposal {
        scope: Scope::Host,
        file: file.to_string_lossy().into(),
        before: None,
        after: "[client]\nconnect = \"vm\"\n".into(),
        diff: "+[client]\n+connect = \"vm\"\n".into(),
        changes: Vec::new(),
        warnings: Vec::new(),
    };
    let drift = Report::Drift(Drift {
        modules: vec!["/Users/alice/repos/team/vm.nix: in [vm] modules, not in the running system".into()],
        hardened: None,
        staged: false,
    });
    let mut out = Vec::new();
    config_with(&mut out, Some(proposal), true, 1, || Ok(drift.clone())).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("Wrote {}", file.display())), "{text}");
    assert!(
        text.ends_with(
            "\nThe VM does not run the system this Mac configures:\n\
             \x20 /Users/alice/repos/team/vm.nix: in [vm] modules, not in the running system\n\
             Fix: atomics vm update (builds the system they describe and restarts the VM into it)\n"
        ),
        "{text}"
    );

    // Nothing left to write still says what the VM runs.
    let mut out = Vec::new();
    config_with(&mut out, None, true, 2, || Ok(drift.clone())).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("The host config file already has the bundle's settings.\n"), "{text}");
    assert!(text.contains("Fix: atomics vm update"), "{text}");

    // Without --apply nothing is written, and the guest is not asked.
    let mut out = Vec::new();
    config_with(&mut out, None, false, 3, || panic!("not asked without --apply")).unwrap();
}

#[test]
fn setup_applies_modules_to_its_new_vm_as_a_new_vm_update() {
    let update = new_vm_update();
    assert!(update.new_vm, "only setup's own new instance may take its modules from a share the VM can write");
    assert!(!update.yes && !update.no_restart, "it still asks before busy work and restarts as an update does");
}
