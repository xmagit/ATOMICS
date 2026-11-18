//! The macOS command with a Lima stand-in: no VM, Nix, sign-ins or app window is touched.
#![cfg(target_os = "macos")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

struct Fixture {
    dir: test_support::TempDir,
    bundle: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with(serde_json::json!({}))
    }

    /// A bundle whose metadata has `extra` on top of the usual.
    fn with(extra: serde_json::Value) -> Self {
        let dir = test_support::tempdir().unwrap();
        let bundle = dir.path().join("installed");
        std::fs::create_dir(&bundle).unwrap();
        let mut metadata = serde_json::json!({
            "version":1,"instance":"atomics-test","user":"atomics","home":"/home/atomics",
            "macUser":"alice","installDir":bundle,"host":{"port":47800},
            "build":{"cpus":4,"memoryGiB":8,"diskGiB":100}
        });
        metadata.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        std::fs::write(bundle.join("atomics-vm.json"), metadata.to_string()).unwrap();
        // The system's shares, as the bundle's lima.yaml gives them (JSON, a YAML subset), and Lima's mounts of
        // the instance, which `limactl edit --set '.mounts = …'` replaces.
        let home = dir.path().join("home");
        let mounts = serde_json::json!([
            {"location": home.join("repos"), "mountPoint": home.join("repos"), "writable": false},
            {"location": home.join("atomics-exchange"), "mountPoint": home.join("atomics-exchange"), "writable": true}
        ]);
        for share in ["repos", "atomics-exchange"] {
            std::fs::create_dir_all(home.join(share)).unwrap();
        }
        std::fs::write(bundle.join("lima.yaml"), serde_json::json!({"vmType": "vz", "mounts": mounts}).to_string())
            .unwrap();
        std::fs::write(dir.path().join("lima-mounts"), mounts.to_string()).unwrap();
        std::fs::write(
            bundle.join("install.sh"),
            r#"#!/bin/sh
printf 'install %s\n' "$0" >> "$VM_LOG"
cp "${0%/*}/atomics-vm.json" "$ATOMICS_VM_BUNDLE/atomics-vm.json" || exit 1
case "$0" in */previous/install.sh) exit 0 ;; esac
test "$VM_MODE" != install-fail
"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_atomics"), bin.join("atomics")).unwrap();
        std::fs::create_dir_all(dir.path().join("share/atomics")).unwrap();
        std::fs::write(
            dir.path().join("share/atomics/build.json"),
            serde_json::json!({
                "binary": PathBuf::from(env!("CARGO_BIN_EXE_atomics")).canonicalize().unwrap(),
                "commit": COMMIT,
            })
            .to_string(),
        )
        .unwrap();
        test_support::write_program(bin.join("login-shell"), "#!/bin/sh\nexec /bin/sh -c \"$4\"\n");
        test_support::write_program(
            bin.join("old-atomics"),
            "#!/bin/sh\ntest \"$*\" = 'vm hold-admissions' || exit 1\necho \"error: unrecognized subcommand 'hold-admissions'\" >&2\nexit 2\n",
        );
        std::fs::create_dir_all(dir.path().join("home/Applications/ATOMICS.app")).unwrap();
        for program in ["open", "osascript"] {
            test_support::write_program(
                bin.join(program),
                "#!/bin/sh\nprintf '%s %s\\n' \"${0##*/}\" \"$*\" >> \"$VM_LOG\"\n",
            );
        }
        test_support::write_program(
            bin.join("guest-system"),
            r#"#!/bin/sh
set -eu
script=$(cat)
case "$script" in
  *--print-out-paths*)
    printf '%s\n' "$script" > "$VM_BUILD_SCRIPT"
    test "$VM_MODE" != build-fail
    printf '/tmp/atomics-vm-update.fixture\n/nix/store/00000000000000000000000000000000-nixos-system-test\n/nix/store/11111111111111111111111111111111-atomics-vm-mac\n' ;;
  *'switch-to-configuration'*)
    test "$VM_MODE" != boot-fail
    : > "$VM_BOOT_STAGED"
    if [ "$VM_MODE" = publication-fail ]; then
      # The guest selected the boot. Make the Mac's final rename fail.
      record="$LIMA_HOME/atomics-test/atomics/boot-accounts.json"
      if [ -f "$record" ]; then mv "$record" "$record.previous"; fi
      mkdir "$record"
    fi
    case "$VM_MODE" in
      changed|old-cli*|stop-fail|late-restart-busy|admission-race) printf 'changed\n' ;;
      restart-pending) printf 'restart-pending\n' ;;
      *) printf 'unchanged\n' ;;
    esac ;;
  'rm -rf -- '*) exit 0 ;;
  *'systemctl stop atomics-host.service'*) echo service-stop >> "$VM_LOG"; touch "$VM_BOOT_STAGED.quiesced" ;;
  *'systemctl start atomics-host.service'*) echo service-start >> "$VM_LOG"; rm -f "$VM_BOOT_STAGED.quiesced" ;;
  *'--format=ustar'*)
    # The bundle streams from the shell that built it (update.rs, fetch_bundle); never limactl copy.
    test "$VM_MODE" != fetch-fail
    COPYFILE_DISABLE=1 exec tar -cf - --format=ustar -C "$VM_BUNDLE" . ;;
  *) echo 'unexpected guest script' >&2; exit 1 ;;
esac
"#,
        );
        test_support::write_program(
            bin.join("ssh"),
            r#"#!/bin/sh
set -eu
case " $* " in *" -L "*) exec python3 "$ACCOUNT_FIXTURE" "$@" ;; esac
printf 'ssh %s\n' "$*" >> "$VM_LOG"
test "$1" = -F && test "$2" = "$LIMA_HOME/atomics-test/ssh.config"
case " $* " in *" -l admin lima-atomics-test bash --login -c 'exec bash -s' ") ;; *) exit 1 ;; esac
exec "$VM_GUEST"
"#,
        );
        test_support::write_program(
            bin.join("limactl"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$VM_LOG"
case "$1" in
  list)
    if [ "$2" = --json ]; then
      printf '{"name":"atomics-test","status":"Running"}\n'
    else
      test "$2" = --format && test "$3" = '{{json .Config}}' && test "$4" = atomics-test
      printf '{"cpus":4,"memory":"8GiB","disk":"100GiB","mounts":%s}\n' "$(cat "$VM_LIMA_MOUNTS")"
    fi ;;
  edit)
    test "$2" = --tty=false && test "$3" = --set && test "$5" = atomics-test
    case "$4" in '.mounts = '*) printf '%s' "${4#.mounts = }" > "$VM_LIMA_MOUNTS" ;; *) exit 1 ;; esac ;;
  shell)
    test "$2" = --workdir=/home/atomics && test "$3" = atomics-test
    test "$4" = bash && test "$5" = --login
    if [ "$6" = -c ]; then
      if [ "$7" = 'exec bash -s' ]; then
        # System changes come on stdin, in a plain bash the login shell execs (vm.rs, system_shell).
        exec "$VM_GUEST"
      fi
      # Questions run in a plain bash that the login shell execs: `exec bash -c "$1" atomics-vm-update SCRIPT`.
      script=$7
      if [ "$7" = 'exec bash -c "$1"' ]; then test "$8" = atomics-vm-update; script=$9; fi
      case "$script" in
        *'atomics vm hold-admissions'*)
          if [ "${VM_MODE#old-cli}" != "$VM_MODE" ] && [ ! -f "$RECORD/running" ]; then
            exec old-atomics vm hold-admissions
          fi
          : > "$VM_BOOT_STAGED.paused"
          echo admissions-paused
          cat >/dev/null
          rm -f "$VM_BOOT_STAGED.paused" ;;
        *'readlink -f /run/current-system'*) echo /nix/store/22222222222222222222222222222222-original ;;
        *'atomics ctl'*)
          if [ "$VM_MODE" = busy ]; then
            printf '{"ok":true,"result":{"sessions":[{"status":"needs you","title":"worker"}]}}\n'
          elif [ "$VM_MODE" = late-busy ] && [ -e "$VM_BUILD_SCRIPT" ]; then
            printf '{"ok":true,"result":{"sessions":[],"update_blockers":["shell running"]}}\n'
          elif [ "$VM_MODE" = late-restart-busy ] && [ -e "$VM_BOOT_STAGED" ]; then
            printf '{"ok":true,"result":{"sessions":[],"update_blockers":["shell running"]}}\n'
          elif [ "$VM_MODE" = unavailable ]; then
            exit 1
          else
            printf '{"ok":true,"result":{"sessions":[]}}\n'
          fi ;;
        *) echo 'unexpected guest question' >&2; exit 1 ;;
      esac
    else exit 1
    fi ;;
  stop)
    if [ "$VM_MODE" = admission-race ] && [ ! -f "$VM_BOOT_STAGED.paused" ]; then
      echo 'new work admitted at stop' >&2
      exit 1
    fi
    if [ "${VM_MODE#old-cli}" != "$VM_MODE" ] && [ ! -f "$RECORD/running" ]; then test -e "$VM_BOOT_STAGED.quiesced"; fi
    test "$VM_MODE" != old-cli-stop-fail
    test "$VM_MODE" != stop-fail ;;
  start)
    if [ "${VM_MODE#old-cli}" != "$VM_MODE" ]; then
      printf %s /nix/store/00000000000000000000000000000000-nixos-system-test > "$RECORD/running"
      rm -f "$VM_BOOT_STAGED.quiesced"
    fi ;;
  *) exit 1 ;;
esac
"#,
        );
        let instance = dir.path().join("lima/atomics-test");
        std::fs::create_dir_all(instance.join("atomics")).unwrap();
        std::fs::write(instance.join("ha.pid"), "101").unwrap();
        Self { dir, bundle }
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(["vm", "update"])
                .args(args)
                .env("HOME", self.dir.path().join("home"))
                .env("SHELL", self.dir.path().join("bin/login-shell"))
                .env("LIMA_HOME", self.dir.path().join("lima"))
                .env("PATH", test_support::path_with(&self.dir.path().join("bin")))
                .env("ATOMICS_HOME", self.dir.path().join("state"))
                .env("ATOMICS_PACKAGE", self.dir.path())
                .env("ATOMICS_SOCKET", self.dir.path().join("absent.sock"))
                .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
                .env("ATOMICS_CREDENTIALS_DIR", self.dir.path().join("credentials"))
                .env("ATOMICS_VM_BUNDLE", &self.bundle)
                .env(
                    "VM_BUNDLE",
                    if self.dir.path().join("next").is_dir() {
                        self.dir.path().join("next")
                    } else {
                        self.bundle.clone()
                    },
                )
                .env("VM_LOG", self.dir.path().join("log"))
                .env("VM_BUILD_SCRIPT", self.dir.path().join("build.sh"))
                .env("VM_BOOT_STAGED", self.dir.path().join("boot-staged"))
                .env("ACCOUNT_FIXTURE", PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-account.py"))
                .env("RECORD", self.dir.path())
                .env("VM_GUEST", self.dir.path().join("bin/guest-system"))
                .env("VM_MODE", mode)
                .env("VM_LIMA_MOUNTS", self.dir.path().join("lima-mounts"))
                .stdin(Stdio::null()),
        )
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log")).unwrap()
    }
}

#[test]
fn a_hardened_vm_is_built_and_switched_by_its_admin_and_asked_about_work_as_its_user() {
    let fixture = Fixture::with(serde_json::json!({"admin": "admin"}));
    let output = fixture.run("changed", &["--no-restart"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("VM system: changed"), "{stdout}");
    let log = fixture.log();
    let ssh = log.lines().filter(|line| line.starts_with("ssh ")).count();
    assert_eq!(ssh, 4, "the build, the bundle, the boot selection and the clean-up go over ssh as admin:\n{log}");
    assert!(
        log.contains("shell --workdir=/home/atomics atomics-test bash --login -c exec bash -c"),
        "busy checks stay with the user:\n{log}"
    );
    assert!(!log.contains("-c exec bash -s"), "nothing system-wide runs as the user:\n{log}");
}

#[test]
fn a_no_change_run_reinstalls_the_bundle_and_keeps_the_vm_running() {
    let fixture = Fixture::new();
    let output = fixture.run("unchanged", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        stdout,
        format!(
            "Source: github:xmagit/ATOMICS/{COMMIT}\n\
             Building the VM system and Mac bundle…\n\
             VM system: unchanged\nMac bundle: reinstalled\nVM: not restarted (system unchanged)\n"
        )
    );
    println!("{stdout}");
    let log = fixture.log();
    assert!(!log.lines().any(|line| line.starts_with("copy ")), "the bundle is never copied with limactl:\n{log}");
    assert!(log.contains("install "));
    assert!(!log.contains("osascript "), "an unchanged system should leave every window open:\n{log}");
    assert!(!log.contains("open "), "an unchanged system should not open a new window:\n{log}");
    assert!(!log.lines().any(|line| line == "stop atomics-test" || line.starts_with("start ")));
}

#[test]
fn an_unknown_or_dirty_package_commit_refuses_before_building_or_installing() {
    for commit in [None, Some("0123456789abcdef0123456789abcdef01234567-dirty")] {
        let fixture = Fixture::new();
        let path = fixture.dir.path().join("share/atomics/build.json");
        let mut metadata: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        metadata["commit"] = serde_json::json!(commit);
        std::fs::write(path, metadata.to_string()).unwrap();
        let output = fixture.run("changed", &["--no-restart"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("set [update] source to a local checkout"), "{error}");
        assert!(!fixture.dir.path().join("build.sh").exists());
        assert!(!fixture.log().contains("install "));
    }
}

#[test]
fn busy_work_is_refused_unless_yes_and_is_checked_again_after_building() {
    for mode in ["busy", "late-busy", "unavailable"] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, &[]);
        assert!(!output.status.success());
        assert!(!fixture.log().contains("switch-to-configuration"));
        assert!(!fixture.log().contains("install "));
    }
    let fixture = Fixture::new();
    let output = fixture.run("busy", &["--yes", "--no-restart"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Continuing with busy work (--yes)"));
}

#[test]
fn build_fetch_and_install_failures_leave_the_boot_selection_alone() {
    for mode in ["build-fail", "fetch-fail", "install-fail"] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, &["--no-restart"]);
        assert!(!output.status.success(), "{mode}");
        assert!(!fixture.log().contains("switch-to-configuration"), "{mode}");
        if mode == "install-fail" {
            assert!(fixture.log().contains("/previous/install.sh"), "restore the previous bundle");
        }
    }
}

#[test]
fn changed_system_restarts_only_when_requested_and_boot_failure_restores_the_bundle() {
    for args in [vec![], vec!["--no-restart"]] {
        let fixture = Fixture::new();
        let output = fixture.run("changed", &args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let log = fixture.log();
        assert_eq!(log.contains("stop atomics-test"), args.is_empty());
        assert_eq!(log.contains("start --tty=false atomics-test"), args.is_empty());
        assert!(!log.contains("osascript "), "the Mac window running its own host must stay open:\n{log}");
        assert!(!log.contains("open "), "the VM client must keep its existing window:\n{log}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("VM system: changed"));
    }
    let fixture = Fixture::new();
    let output = fixture.run("boot-fail", &[]);
    assert!(!output.status.success());
    assert!(fixture.log().contains("/previous/install.sh"));
    assert!(!fixture.log().contains("stop atomics-test"));
}

#[test]
fn the_vm_host_pauses_admissions_across_the_final_check_and_stop() {
    let fixture = Fixture::new();
    let output = fixture.run("admission-race", &[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let log = fixture.log();
    let pause = log.find("atomics vm hold-admissions").unwrap();
    let check = log.rfind("atomics ctl").unwrap();
    assert!(pause < check && check < log.find("stop atomics-test").unwrap());
    assert!(!fixture.dir.path().join("boot-staged.paused").exists());
}

#[test]
fn a_failed_stop_leaves_a_restart_owed_and_a_retry_restarts_once() {
    let fixture = Fixture::new();
    let first = fixture.run("stop-fail", &[]);
    assert!(!first.status.success());
    let error = String::from_utf8_lossy(&first.stderr);
    assert!(error.contains("restart pending") && error.contains("could not stop the VM"), "{error}");
    assert!(!error.contains("no ATOMICS socket"), "client updates have no local queue: {error}");
    assert!(fixture.log().contains("stop atomics-test"));
    assert!(!fixture.log().contains("start --tty=false atomics-test"));
    assert!(!fixture.log().contains("osascript ") && !fixture.log().contains("open "));
    assert!(!fixture.dir.path().join("boot-staged.paused").exists());
    let retry = fixture.run("restart-pending", &[]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    assert!(String::from_utf8_lossy(&retry.stdout).contains("VM system: restart pending"));
    let log = fixture.log();
    assert_eq!(log.lines().filter(|line| *line == "stop atomics-test").count(), 2, "{log}");
    assert_eq!(log.lines().filter(|line| *line == "start --tty=false atomics-test").count(), 1, "{log}");
}

#[test]
fn work_that_starts_after_staging_keeps_the_restart_pending() {
    let fixture = Fixture::new();
    let output = fixture.run("late-restart-busy", &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("restart pending") && error.contains("shell running"), "{error}");
    assert!(fixture.dir.path().join("boot-staged").exists());
    let log = fixture.log();
    assert!(!log.contains("stop atomics-test") && !log.contains("start --tty=false atomics-test"), "{log}");
}

#[test]
fn lima_mounts_that_differ_from_the_system_are_edited_while_the_vm_is_stopped() {
    let fixture = Fixture::new();
    let home = fixture.dir.path().join("home");
    let built = std::fs::read_to_string(fixture.dir.path().join("lima-mounts")).unwrap();
    // An instance made before hardened mode: ~/repos writable on the device, and no exchange.
    let old = serde_json::json!([{"location": home.join("repos"), "mountPoint": home.join("repos"), "writable": true}]);
    let set_old = || std::fs::write(fixture.dir.path().join("lima-mounts"), old.to_string()).unwrap();

    set_old();
    let pending = fixture.run("unchanged", &["--no-restart"]);
    assert!(pending.status.success(), "{}", String::from_utf8_lossy(&pending.stderr));
    let stdout = String::from_utf8_lossy(&pending.stdout);
    assert!(stdout.contains("repos: read-only in the system but writable in Lima"), "{stdout}");
    assert!(stdout.contains("restart pending (--no-restart)"), "a mount difference owes a restart: {stdout}");
    assert!(!fixture.log().lines().any(|line| line.starts_with("edit ")), "Lima's mounts change only while stopped");

    let output = fixture.run("unchanged", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("VM: restarted") && stdout.contains("Lima mounts: as the system mounts them"), "{stdout}");
    let log = fixture.log();
    let position = |prefix: &str| {
        log.lines().position(|line| line.starts_with(prefix)).unwrap_or_else(|| panic!("{prefix}: {log}"))
    };
    assert!(position("stop atomics-test") < position("edit --tty=false --set .mounts = "), "{log}");
    assert!(position("edit --tty=false --set .mounts = ") < position("start --tty=false atomics-test"), "{log}");
    let now: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.dir.path().join("lima-mounts")).unwrap()).unwrap();
    assert_eq!(now, serde_json::from_str::<serde_json::Value>(&built).unwrap());

    // Matching now: the next update restarts nothing.
    let again = fixture.run("unchanged", &[]);
    assert!(String::from_utf8_lossy(&again.stdout).contains("VM: not restarted (system unchanged)"));
}

#[test]
fn a_module_in_the_exchange_is_refused_before_anything_is_built() {
    let fixture = Fixture::new();
    let module = fixture.dir.path().join("home/atomics-exchange/team.nix");
    std::fs::write(&module, "{ }").unwrap();
    std::fs::write(
        fixture.dir.path().join("config.toml"),
        format!("[vm]\nmodules = [{:?}]\n", module.display().to_string()),
    )
    .unwrap();
    let output = fixture.run("unchanged", &[]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("[vm] modules: ") && error.contains("a folder the VM can write"), "{error}");
    assert!(!fixture.dir.path().join("build.sh").exists(), "nothing was built");
    assert!(!fixture.log().contains("install "), "nothing was installed");
}

#[test]
fn a_legacy_guest_keeps_the_boot_pending_until_interruption_is_requested() {
    let fixture = Fixture::new();
    let pending = fixture.run("old-cli", &[]);
    let message = format!("{}{}", String::from_utf8_lossy(&pending.stdout), String::from_utf8_lossy(&pending.stderr));
    assert!(message.contains("restart pending") && message.contains("--yes"), "{message}");
    assert!(fixture.dir.path().join("boot-staged").exists());
    assert!(!fixture.log().lines().any(|l| l == "service-stop" || l.starts_with("stop ")));
    let restart = fixture.run("old-cli", &["--yes"]);
    assert!(restart.status.success(), "{}", String::from_utf8_lossy(&restart.stderr));
    assert!(fixture.log().contains("start --tty=false atomics-test"));
}

#[test]
fn a_changed_system_can_restart_an_idle_legacy_guest() {
    let fixture = Fixture::new();
    let output = fixture.run("old-cli", &["--yes"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let log = fixture.log();
    assert!(log.contains("stop atomics-test") && log.contains("start --tty=false atomics-test"), "{log}");
    // Subsequent updates reach the new CLI and can use its admission guard.
    let again = fixture.run("old-cli", &[]);
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    let log = fixture.log();
    assert_eq!(log.lines().filter(|l| *l == "service-stop").count(), 1);
    assert_eq!(log.lines().filter(|l| *l == "stop atomics-test").count(), 2);
}

#[test]
fn a_failed_legacy_stop_restores_the_guest_host() {
    let fixture = Fixture::new();
    let output = fixture.run("old-cli-stop-fail", &["--yes"]);
    assert!(!output.status.success());
    let log = fixture.log();
    let transition: Vec<_> = log.lines().filter(|l| l.starts_with("service-") || l.starts_with("stop ")).collect();
    assert_eq!(transition, ["service-stop", "stop atomics-test", "service-start"]);
    assert!(!fixture.dir.path().join("boot-staged.quiesced").exists());
}

#[test]
fn a_failed_boot_selection_keeps_the_previous_account_after_reboot() {
    check_account_after_rollback("boot-fail");
}

#[test]
fn a_failed_bundle_install_keeps_the_previous_account_after_reboot() {
    check_account_after_rollback("install-fail");
}

#[test]
fn a_selected_boot_recovers_after_account_publication_fails() {
    let f = Fixture::with(serde_json::json!({"admin": "admin"}));
    let instance = f.dir.path().join("lima/atomics-test");
    let record = instance.join("atomics/boot-accounts.json");
    let old = "/nix/store/22222222222222222222222222222222-original";
    std::fs::write(&record, serde_json::json!({"systems": {old: "admin"}, "user_access": null}).to_string()).unwrap();
    let fail = f.run("publication-fail", &["--no-restart"]);
    assert!(!fail.status.success());
    assert!(f.dir.path().join("boot-staged").exists());
    std::fs::remove_dir(&record).unwrap();
    std::fs::rename(record.with_extension("json.previous"), &record).unwrap();
    std::fs::write(instance.join("ha.pid"), "102").unwrap();
    std::fs::write(f.dir.path().join("running"), "/nix/store/00000000000000000000000000000000-nixos-system-test")
        .unwrap();
    let retry = f.run("unchanged", &["--no-restart"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    let map: serde_json::Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(map["systems"]["/nix/store/00000000000000000000000000000000-nixos-system-test"], "admin");
}

#[test]
fn a_first_account_publication_can_retry_before_the_guest_reboots() {
    let f = Fixture::with(serde_json::json!({"admin": "admin"}));
    stage_admin_rename(&f);
    let record = f.dir.path().join("lima/atomics-test/atomics/boot-accounts.json");
    assert!(!record.exists());
    std::fs::write(f.dir.path().join("running-admin"), "admin").unwrap();
    let fail = f.run("publication-fail", &["--no-restart"]);
    assert!(String::from_utf8_lossy(&fail.stderr).contains("committing the boot selection"));
    assert!(f.dir.path().join("boot-staged").exists());
    std::fs::remove_dir(&record).unwrap();
    let saved = record.with_extension("json.previous");
    if saved.exists() {
        std::fs::rename(saved, &record).unwrap();
    }
    let retry = f.run("unchanged", &["--no-restart"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    let map: serde_json::Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(map["systems"]["/nix/store/22222222222222222222222222222222-original"], "admin");
    assert_eq!(map["systems"]["/nix/store/00000000000000000000000000000000-nixos-system-test"], "next-admin");
}

fn check_account_after_rollback(failure: &str) {
    let f = Fixture::new();
    let previous = std::fs::read(f.bundle.join("atomics-vm.json")).unwrap();
    stage_admin_rename(&f);
    let failed = f.run(failure, &["--no-restart"]);
    assert!(!failed.status.success());
    assert!(f.log().contains("/previous/install.sh"), "{failure} must have reached rollback");
    assert_eq!(std::fs::read(f.bundle.join("atomics-vm.json")).unwrap(), previous);
    let record = f.dir.path().join("lima/atomics-test/atomics/boot-accounts.json");
    let map: serde_json::Value = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(map["systems"], serde_json::json!({"/nix/store/22222222222222222222222222222222-original": null}));
    assert_eq!(map["pending"]["/nix/store/00000000000000000000000000000000-nixos-system-test"]["admin"], "next-admin");
    std::fs::write(f.dir.path().join("lima/atomics-test/ha.pid"), "102").unwrap();
    let retry = f.run("unchanged", &["--no-restart"]);
    assert!(retry.status.success(), "{failure}: {}", String::from_utf8_lossy(&retry.stderr));
}

fn stage_admin_rename(f: &Fixture) {
    let next = f.dir.path().join("next");
    std::fs::create_dir(&next).unwrap();
    for file in ["atomics-vm.json", "install.sh", "lima.yaml"] {
        std::fs::copy(f.bundle.join(file), next.join(file)).unwrap();
    }
    let file = next.join("atomics-vm.json");
    let mut info: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    info["admin"] = "next-admin".into();
    std::fs::write(file, info.to_string()).unwrap();
}
