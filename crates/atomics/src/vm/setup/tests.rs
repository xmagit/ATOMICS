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
    std::fs::write(root.join("sshd.sh"), "set -eu\nsudo sshd -T\n").unwrap();
    std::fs::write(
        root.join("check.sh"),
        r#"set -euo pipefail
. "$VM_SCRIPT"
check_arguments "$@"
check_sshd
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
test "$(cat)" = 'set -eu
sudo sshd -T'
test "${FAIL_ADMIN-false}" = false || exit 1
printf 'passwordauthentication %s\nkbdinteractiveauthentication no\npermitrootlogin no\nstreamlocalbindunlink yes\n' "${PASSWORD_AUTH-no}"
"#,
    );
    crate::test_support::write_program(
        root.join("bin/sudo"),
        "#!/bin/sh\necho 'guest user must not use sudo' >&2\nexit 1\n",
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
    let run = |password: &str, fail_admin: bool| {
        let mut runner = check_runner(&info, &root.join("bin/limactl"), root).unwrap();
        runner
            .env("PATH", crate::test_support::path_with(&root.join("bin")))
            .env("HOME", root.join("home"))
            .env("VM_SCRIPT", root.join("guest-check.sh"))
            .env("VM_SSH_LOG", root.join("ssh.log"))
            .env("VM_PROBE", root.join("probe"))
            .env("VM_CLOSED", root.join("closed"))
            .env("ATOMICS_MAC_SHARES", "/read-only:ro")
            .env("PASSWORD_AUTH", password)
            .env("FAIL_ADMIN", fail_admin.to_string());
        runner.output().unwrap()
    };
    let passed = run("no", false);
    assert!(passed.status.success(), "{}", String::from_utf8_lossy(&passed.stderr));
    assert!(String::from_utf8_lossy(&passed.stdout).contains("sshd: keys only"));
    assert!(
        std::fs::read_to_string(root.join("probe")).unwrap().starts_with(root.join("home/repos").to_str().unwrap())
    );
    assert!(root.join("closed").exists());
    assert!(!run("yes", false).status.success(), "password authentication still fails the check");
    assert!(!run("no", true).status.success(), "admin inspection failures must not fall back to the guest's sudo");
}
