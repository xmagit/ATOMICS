//! The macOS command with a Lima stand-in: no VM, Nix, sign-ins or app window is touched.
#![cfg(target_os = "macos")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BOOT_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lima-cidata.py");
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
if ! [ "${0%/*}/atomics-vm.json" -ef "$VM_BUNDLE/atomics-vm.json" ]; then
  cp "${0%/*}/atomics-vm.json" "$VM_BUNDLE/atomics-vm.json" || exit 1
fi
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
    printf 'build modules\n' >> "$VM_LOG"
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
    if [ "$VM_MODE" = interrupt-after-selection ] || [ "$VM_MODE" = interrupt-before-selection ]; then
      if [ "$VM_MODE" = interrupt-after-selection ]; then
        echo /nix/store/00000000000000000000000000000000-nixos-system-test > "$VM_LOG.selected"
      fi
      touch "$VM_LOG.selecting"
      exec sleep 120
    fi
    case "$VM_MODE" in
      changed|restart-pending|publication-fail) echo /nix/store/00000000000000000000000000000000-nixos-system-test > "$VM_LOG.selected" ;;
    esac
    case "$VM_MODE" in
      changed|old-cli*|stop-fail|late-restart-busy|admission-race) printf 'changed\n' ;;
      restart-pending) printf 'restart-pending\n' ;;
      *) printf 'unchanged\n' ;;
    esac ;;
  *'readlink -f /nix/var/nix/profiles/system'*)
    current=/nix/store/22222222222222222222222222222222-original
    if [ -f "$RECORD/running" ]; then current=$(cat "$RECORD/running"); fi
    echo "$current"
    if [ -f "$VM_LOG.selected" ]; then cat "$VM_LOG.selected"; else echo "$current"; fi ;;
  'readlink -f /run/current-system') cat "$RECORD/running" ;;
  *'cat /proc/sys/kernel/random/uuid'*) echo c9e61338-2ec0-45ed-8354-888fccbfbb51 ;;
  *'sudo -n cat /root/.atomics-vm-admin'*)
    test "$(cat "$RECORD/running-admin")" = atomics
    echo c9e61338-2ec0-45ed-8354-888fccbfbb51
    cat "$RECORD/running" ;;
  'rm -rf -- '*) exit 0 ;;
  *'systemctl stop atomics-host.service'*) echo service-stop >> "$VM_LOG"; touch "$VM_BOOT_STAGED.quiesced" ;;
  *'systemctl start atomics-host.service'*) echo service-start >> "$VM_LOG"; rm -f "$VM_BOOT_STAGED.quiesced" ;;
  *'--format=ustar'*)
    # The bundle streams from the shell that built it (update.rs, fetch_bundle); never limactl copy.
    test "$VM_MODE" != fetch-fail
    bundle=$VM_BUNDLE
    if [ -d "$bundle.next" ]; then bundle=$bundle.next; fi
    COPYFILE_DISABLE=1 exec tar -cf - --format=ustar -C "$bundle" . ;;
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
admin=admin
if [ -f "$RECORD/running-admin" ]; then admin=$(cat "$RECORD/running-admin"); fi
case " $* " in *" -l $admin lima-atomics-test "*) ;; *) exit 1 ;; esac
case "$*" in
  *" bash --login -c 'exec bash -s -- --print-sshd-configs --hardened true'")
    cat >/dev/null
    echo '{"user":"fixture user","admin":"fixture admin"}'
    exit 0 ;;
  *"unshare --mount"*)
    exec "$VM_GUEST" ;;
  *atomics-vm-build*)
    record=null
    test "$VM_MODE" != status-record || record='{"version":1,"hardened":true,"modules":[]}'
    printf 'atomics-vm-build\n%s\n%s\nusers\n' "$record" "$record"
    exit 0 ;;
  *ATOMICS_MAC_SHARES*) printf '%s:ro %s:rw\n' "$HOME/repos" "$HOME/atomics-exchange"; exit 0 ;;
esac
case " $* " in *" bash --login -c 'exec bash -s' ") ;; *) exit 1 ;; esac
exec "$VM_GUEST"
"#,
        );
        test_support::write_program(
            bin.join("limactl"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$VM_LOG"
case "$1" in
  --version) echo 'limactl version 2.2.0' ;;
  list)
    if [ "$2" = --json ]; then
      if ! [ "$LIMA_HOME" -ef "${VM_LIMA_MOUNTS%/*}/lima" ]; then
        if [ -f "$LIMA_HOME/fixture-list" ]; then cat "$LIMA_HOME/fixture-list"; fi
        exit 0
      fi
      if [ -f "$VM_LIMA_MOUNTS.other" ]; then cat "$VM_LIMA_MOUNTS.other"; echo; fi
      if [ "$VM_MODE" = bootstrap ] && [ ! -e "$VM_LIMA_MOUNTS.created" ]; then exit 0; fi
      state=Running
      test ! -f "$VM_LIMA_MOUNTS.stopped" || state=Stopped
      printf '{"name":"atomics-test","status":"%s","config":{"mounts":%s}}\n' "$state" "$(cat "$VM_LIMA_MOUNTS")"
    else
      test "$2" = --format && test "$3" = '{{json .Config}}' && test "$4" = atomics-test
      printf '{"cpus":4,"memory":"8GiB","disk":"100GiB","mounts":%s}\n' "$(cat "$VM_LIMA_MOUNTS")"
    fi ;;
  create)
    test "$VM_MODE" = bootstrap
    rm -f "$LIMA_HOME/atomics-test/cidata.iso"
    case "$5" in *' | .mounts[].writable = false')
      if [ ! -f "$VM_LIMA_MOUNTS.unexpected" ]; then
        python3 -c 'import json, sys; p = sys.argv[1]; mounts = json.load(open(p)); json.dump([dict(m, writable=False) for m in mounts], open(p, "w"))' "$VM_LIMA_MOUNTS"
      fi ;;
    esac
    touch "$VM_LIMA_MOUNTS.created" "$VM_LIMA_MOUNTS.stopped" ;;
  edit)
    test "$VM_MODE" != edit-fail || exit 1
    test "$2" = --tty=false && test "$3" = --set && test "$5" = atomics-test
    case "$4" in '.mounts = '*) printf '%s' "${4#.mounts = }" > "$VM_LIMA_MOUNTS" ;; *) exit 1 ;; esac ;;
  shell)
    test "$2" = --workdir=/home/atomics && test "$3" = atomics-test
    test "$4" = bash && test "$5" = --login
    if [ "$6" = -c ]; then
      case "$7" in 'exec sudo unshare --mount --propagation private '*)
        printf '%s\n' "$7" > "$VM_BUILD_SCRIPT.shell"
        exec "$VM_GUEST" ;;
      esac
      if [ "$7" = 'exec bash -s' ]; then
        # System changes come on stdin, in a plain bash the login shell execs (vm.rs, system_shell).
        exec "$VM_GUEST"
      fi
      if [ "$7" = 'exec bash -s -- --print-sshd-configs --hardened false' ]; then
        cat >/dev/null
        echo '{"user":"fixture user","admin":""}'
        exit 0
      fi
      if [ "$VM_MODE" = bootstrap ] && [ "${8:-}" = atomics-vm-check ]; then
        test "$9" = --sshd-configs
        test "${11}" = --hardened
        case "${12}" in
          true) test "${10}" = '{"user":"fixture user","admin":"fixture admin"}' ;;
          false) test "${10}" = '{"user":"fixture user","admin":""}' ;;
          *) exit 1 ;;
        esac
        echo 'check guest' >> "$VM_LOG"
        cat >/dev/null
        exit 0
      fi
      # Questions run in a plain bash that the login shell execs: `exec bash -c "$1" atomics-vm-update SCRIPT`.
      script=$7
      if [ "$7" = 'exec bash -c "$1"' ]; then test "$8" = atomics-vm-update; script=$9; fi
      case "$script" in
        *'atomics vm hold-admissions'*)
          if [ "${VM_MODE#old-cli}" != "$VM_MODE" ] && [ ! -f "$VM_BOOT_STAGED.rebooted" ]; then
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
          elif [ -e "$VM_LIMA_MOUNTS.used" ]; then
            printf '{"ok":true,"result":{"sessions":[{"status":"idle"}],"update_blockers":[]}}\n'
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
    if [ "${VM_MODE#old-cli}" != "$VM_MODE" ] && [ ! -f "$VM_BOOT_STAGED.rebooted" ]; then test -e "$VM_BOOT_STAGED.quiesced"; fi
    test "$VM_MODE" != old-cli-stop-fail
    test "$VM_MODE" != stop-fail
    touch "$VM_LIMA_MOUNTS.stopped" ;;
  start)
    test "${VM_FAIL_START:-0}" = 0
    if [ ! -f "$VM_LIMA_MOUNTS.first-start" ]; then cp "$VM_LIMA_MOUNTS" "$VM_LIMA_MOUNTS.first-start"; fi
    if [ -f "$VM_LOG.selected" ]; then
      printf %s "$(cat "$VM_LOG.selected")" > "$RECORD/running"
      python3 -c 'import json,sys; info=json.load(open(sys.argv[1])); print(info.get("admin") or info["user"],end="")' "$VM_BUNDLE/atomics-vm.json" > "$RECORD/running-admin"
    fi
    rm -f "$VM_BOOT_STAGED.quiesced"
    touch "$VM_BOOT_STAGED.rebooted"
    echo 102 > "$LIMA_HOME/atomics-test/ha.pid"
    devices=$VM_LIMA_MOUNTS
    if [ -f "$devices.unexpected" ]; then devices=$devices.unexpected; fi
    python3 "$VM_BOOT_FIXTURE" "$LIMA_HOME/atomics-test" "$(cat "$devices")"
    rm -f "$VM_LIMA_MOUNTS.stopped" ;;
  *) exit 1 ;;
esac
"#,
        );
        let instance = dir.path().join("lima/atomics-test");
        std::fs::create_dir_all(instance.join("atomics")).unwrap();
        std::fs::write(instance.join("ha.pid"), "101").unwrap();
        std::fs::write(dir.path().join("running-admin"), metadata["admin"].as_str().unwrap_or("atomics")).unwrap();
        Self { dir, bundle }
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        self.command(mode, "update", args)
    }

    fn command(&self, mode: &str, command: &str, args: &[&str]) -> Output {
        test_support::output(&mut self.process(mode, command, args))
    }

    fn process(&self, mode: &str, command: &str, args: &[&str]) -> Command {
        // Model the boot separately from mutable YAML. Tests may supply a peer's earlier boot.
        let root = self.dir.path();
        std::fs::create_dir_all(root.join("lima")).unwrap();
        let mounts = |path: &std::path::Path| {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap()
        };
        let boot = root.join("lima-mounts.boot");
        let initial = mounts(&if boot.exists() { boot } else { root.join("lima-mounts") });
        if (mode != "bootstrap" || root.join("lima-mounts.created").is_file())
            && !root.join("lima-mounts.stopped").is_file()
        {
            write_boot_devices(&root.join("lima/atomics-test"), &initial);
        }
        let other = root.join("lima-mounts.other");
        if other.exists() {
            let peer = mounts(&other);
            let dir = root.join("lima").join(peer["name"].as_str().unwrap());
            if !dir.join("cidata.iso").exists() {
                write_boot_devices(&dir, &peer["config"]["mounts"]);
            }
        }
        let mut process = Command::new(env!("CARGO_BIN_EXE_atomics"));
        process
            .args(["vm", command])
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
            .env("VM_BUNDLE", &self.bundle)
            .env("VM_BOOT_FIXTURE", BOOT_FIXTURE)
            .env("VM_BOOT_STAGED", root.join("boot-staged"))
            .env("ACCOUNT_FIXTURE", PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vm-account.py"))
            .env("RECORD", root)
            .env("VM_LOG", self.dir.path().join("log"))
            .env("VM_BUILD_SCRIPT", self.dir.path().join("build.sh"))
            .env("VM_GUEST", self.dir.path().join("bin/guest-system"))
            .env("VM_MODE", mode)
            .env("VM_LIMA_MOUNTS", self.dir.path().join("lima-mounts"))
            .stdin(Stdio::null());
        process
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log")).unwrap()
    }

    fn built_module_path(&self) -> PathBuf {
        let script = std::fs::read_to_string(self.dir.path().join("build.sh")).unwrap();
        let args = script.split_once("--argstr modules '").unwrap().1.split_once('\'').unwrap().0;
        let inputs: serde_json::Value = serde_json::from_str(args).unwrap();
        PathBuf::from(inputs[0]["path"].as_str().unwrap())
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
    let next = f.bundle.with_extension("next");
    std::fs::create_dir(&next).unwrap();
    for file in ["atomics-vm.json", "install.sh", "lima.yaml"] {
        std::fs::copy(f.bundle.join(file), next.join(file)).unwrap();
    }
    let file = next.join("atomics-vm.json");
    let mut info: serde_json::Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    info["admin"] = "next-admin".into();
    std::fs::write(file, info.to_string()).unwrap();
}

#[test]
fn retained_setup_refuses_a_wider_bundle_before_install_or_boot() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    let fixture = Fixture::with(serde_json::json!({"admin": "admin"}));
    let root = fixture.dir.path();
    std::fs::create_dir_all(root.join("state/vm")).unwrap();
    std::fs::write(
        root.join("state/vm/atomics-test.json"),
        r#"{"instance":"atomics-test","image":"original.raw","created_at":1}"#,
    )
    .unwrap();
    std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
    let incoming = root.join("bundle");
    std::fs::create_dir(&incoming).unwrap();
    for name in ["atomics-vm.json", "install.sh"] {
        std::fs::copy(fixture.bundle.join(name), incoming.join(name)).unwrap();
    }
    std::fs::write(root.join("atomics-vm.raw"), "").unwrap();
    let original = std::fs::read(fixture.bundle.join("lima.yaml")).unwrap();
    let mut wider: serde_json::Value = serde_json::from_slice(&original).unwrap();
    wider["mounts"][0]["writable"] = true.into();
    std::fs::write(incoming.join("lima.yaml"), wider.to_string()).unwrap();

    let output = fixture.command("unchanged", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!output.status.success());
    let log = fixture.log();
    assert!(
        !log.lines().any(|line| ["install ", "edit ", "start "].iter().any(|prefix| line.starts_with(prefix))),
        "{log}"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("retained"));
    assert_eq!(std::fs::read(fixture.bundle.join("lima.yaml")).unwrap(), original);
}

#[test]
fn start_on_an_older_disk_never_adds_a_device_from_a_replaced_bundle() {
    let fixture = Fixture::new();
    let root = fixture.dir.path();
    let path = root.join("lima-mounts");
    let mut devices: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut proposed = devices.clone();
    proposed[0]["writable"] = true.into();
    proposed[1]["writable"] = false.into();
    proposed.as_array_mut().unwrap().push(serde_json::json!({"location":root.join("extra"),"writable":true}));
    std::fs::write(fixture.bundle.join("lima.yaml"), serde_json::json!({"mounts":proposed}).to_string()).unwrap();
    std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
    let start = fixture.command("unchanged", "start", &[]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stderr));
    devices[1]["writable"] = false.into();
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap(), devices);
}

#[test]
fn deferred_mounts_are_enforced_by_start_and_stopped_update_after_an_edit_failure() {
    let fixture = Fixture::new();
    let root = fixture.dir.path();
    let path = root.join("lima-mounts");
    let expected: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut old = expected.clone();
    old[0]["writable"] = true.into();
    std::fs::write(&path, old.to_string()).unwrap();
    let staged = fixture.run("changed", &["--no-restart"]);
    assert!(staged.status.success(), "{}", String::from_utf8_lossy(&staged.stderr));
    // A later reinstall cannot replace the policy that belongs to this disk's selected boot.
    std::fs::write(fixture.bundle.join("lima.yaml"), serde_json::json!({"mounts": old}).to_string()).unwrap();
    std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
    for command in ["start", "update"] {
        let failed = fixture.command("edit-fail", command, &[]);
        assert!(!failed.status.success(), "{command} booted with a pending edit");
        assert!(!fixture.log().contains("start --tty=false"), "{}", fixture.log());
    }
    let retry = fixture.command("unchanged", "start", &[]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(), expected);
    assert!(fixture.log().contains("start --tty=false atomics-test"));
    assert!(String::from_utf8_lossy(&staged.stdout).contains("Lima mounts: pending"));
}

#[test]
fn first_setup_checks_the_source_and_installation_independently() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    for input in ["source", "installation"] {
        let f = Fixture::new();
        let root = f.dir.path();
        let writable = root.join("home/atomics-exchange");
        let module = root.join("trusted.nix");
        std::fs::write(&module, "{}").unwrap();
        let mut config = format!("[vm]\nmodules = [{module:?}]\n");
        if input == "source" {
            config.push_str(&format!("[update]\nsource = {writable:?}\n"));
        } else if input == "installation" {
            let path = f.bundle.join("lima.yaml");
            let mut lima: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            lima["mounts"].as_array_mut().unwrap().push(serde_json::json!({"location":f.bundle,"writable":true}));
            std::fs::write(&path, lima.to_string()).unwrap();
            std::fs::write(root.join("lima-mounts"), lima["mounts"].to_string()).unwrap();
        }
        std::fs::write(root.join("config.toml"), config).unwrap();
        std::fs::write(root.join("atomics-vm.raw"), "").unwrap();
        std::os::unix::fs::symlink(&f.bundle, root.join("bundle")).unwrap();
        let output = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{input}");
        assert!(!root.join("build.sh").exists(), "setup used {input} before checking it: {error}");
        assert!(error.contains("folder the VM can write"), "{error}");
    }
}

/// A standard VM with writable repos. Its team module builds a bundle with repos read-only.
fn team_onboarding() -> (Fixture, PathBuf) {
    let f = Fixture::new();
    let root = f.dir.path();
    let repos = root.join("home/repos").canonicalize().unwrap();
    let team = repos.join("team");
    std::fs::create_dir(&team).unwrap();
    let module = team.join("team.nix");
    std::fs::write(&module, "{ imports = [ ./defaults.nix ]; }").unwrap();
    std::fs::write(team.join("defaults.nix"), "{ atomics.vm.hardened = true; }").unwrap();
    std::fs::write(root.join("config.toml"), format!("[vm]\nmodules = [{module:?}]\n")).unwrap();
    std::fs::write(root.join("atomics-vm.raw"), "").unwrap();
    std::os::unix::fs::symlink(&f.bundle, root.join("bundle")).unwrap();
    std::fs::write(f.bundle.join("config.d.toml"), "[client]\nconnect = \"vm\"\n").unwrap();

    let next = f.bundle.with_extension("next");
    std::fs::create_dir(&next).unwrap();
    for name in ["atomics-vm.json", "install.sh", "config.d.toml"] {
        std::fs::copy(f.bundle.join(name), next.join(name)).unwrap();
    }
    let mut devices: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("lima-mounts")).unwrap()).unwrap();
    devices[0]["location"] = serde_json::json!(repos);
    devices[0]["mountPoint"] = serde_json::json!(repos);
    std::fs::write(next.join("lima.yaml"), serde_json::json!({"mounts":devices}).to_string()).unwrap();
    devices[0]["writable"] = true.into();
    std::fs::write(f.bundle.join("lima.yaml"), serde_json::json!({"mounts":devices}).to_string()).unwrap();
    std::fs::write(root.join("lima-mounts"), devices.to_string()).unwrap();

    (f, module)
}

fn pin_standalone_module(fixture: &Fixture, module: &std::path::Path) {
    std::fs::copy(module.with_file_name("defaults.nix"), module).unwrap();
    std::fs::write(fixture.dir.path().join("config.toml"), format!(
        "[vm]\nmodules = [{module:?}]\n[vm.bootstrap_sha256]\n{module:?} = '7fb1ca641632eb4336345ad31a4cbe80d1e985ce03cf9b881efd168d0032abeb'\n"
    )).unwrap();
}

#[test]
fn a_new_vm_imports_a_team_module_in_place_before_running_the_guest_check() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let repos = module.parent().unwrap().parent().unwrap();
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(setup.status.success(), "{}", String::from_utf8_lossy(&setup.stderr));
    let imported = f.built_module_path();
    assert_eq!(imported, module);
    let first: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("lima-mounts.first-start")).unwrap()).unwrap();
    assert!(first.iter().all(|device| device["writable"] == false), "bootstrap devices: {first:?}");
    let contents = std::fs::read_to_string(&imported).unwrap();
    let relative = contents.split_whitespace().find(|word| word.starts_with("./")).unwrap();
    assert_eq!(std::fs::read_to_string(imported.with_file_name(relative)).unwrap(), "{ atomics.vm.hardened = true; }");
    let script = std::fs::read_to_string(root.join("build.sh")).unwrap();
    assert!(!script.contains("module-0.nix"), "{script}");
    let log = f.log();
    let shell = std::fs::read_to_string(root.join("build.sh.shell")).unwrap();
    assert!(!shell.contains(repos.to_str().unwrap()), "the builder hid the module's own share: {shell}");
    assert!(log.find("build modules\n").unwrap() < log.find("check guest\n").unwrap(), "{log}");
    let devices: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("lima-mounts")).unwrap()).unwrap();
    assert_eq!(devices[0]["writable"], false);
}

#[test]
fn a_read_only_share_uses_the_pinned_module_on_setup_and_update() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let definition = std::fs::read(f.bundle.with_extension("next").join("lima.yaml")).unwrap();
    let layout: serde_json::Value = serde_json::from_slice(&definition).unwrap();
    let repos = &layout["mounts"][0];
    assert_eq!(repos["writable"], false);
    assert_eq!(repos["location"], repos["mountPoint"]);
    assert!(module.starts_with(repos["location"].as_str().unwrap()));
    std::fs::write(f.bundle.join("lima.yaml"), definition).unwrap();
    std::fs::write(root.join("lima-mounts"), layout["mounts"].to_string()).unwrap();
    pin_standalone_module(&f, &module);

    for (cmd, args) in [("setup", &["--from", root.to_str().unwrap()][..]), ("update", &["--no-restart"][..])] {
        let result = f.command("bootstrap", cmd, args);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{cmd}: {stderr}");
        assert!(stderr.contains("using the verified copy"), "{cmd}: {stderr}");
        assert_eq!(f.built_module_path(), PathBuf::from("module-0.nix"), "{cmd}");
        std::fs::remove_file(root.join("build.sh")).unwrap();
    }
    std::fs::write(&module, "{}").unwrap();
    let changed = f.run("bootstrap", &["--no-restart"]);
    assert!(!changed.status.success());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("differs from its verified bootstrap SHA-256"));
    assert!(!root.join("build.sh").exists());
}

#[test]
fn a_failed_first_start_allows_pinless_retry_only_while_boot_history_is_intact() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    for change in ["none", "boot media", "writable devices"] {
        let (f, module) = team_onboarding();
        let root = f.dir.path();
        let instance = root.join("lima/atomics-test");
        let mut setup = f.process("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
        let failed = test_support::output(setup.env("VM_FAIL_START", "1"));
        assert!(!failed.status.success());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("start atomics-test failed"));
        assert!(!instance.join("cidata.iso").exists() && !root.join("lima-mounts.first-start").exists());
        if change == "boot media" {
            write_boot_devices(&instance, &serde_json::json!([]));
        } else if change == "writable devices" {
            let layout: serde_json::Value =
                serde_json::from_slice(&std::fs::read(f.bundle.join("lima.yaml")).unwrap()).unwrap();
            std::fs::write(root.join("lima-mounts"), layout["mounts"].to_string()).unwrap();
        }
        let retry = f.run("bootstrap", &["--no-restart"]);
        let stderr = String::from_utf8_lossy(&retry.stderr);
        if change != "none" {
            assert!(!retry.status.success() && stderr.contains("bootstrap_sha256"), "{change}: {stderr}");
            assert!(!root.join("build.sh").exists());
        } else {
            assert!(retry.status.success(), "{stderr}");
            assert_eq!(f.built_module_path(), module);
            assert!(module.with_file_name("defaults.nix").is_file());
            let first: Vec<serde_json::Value> =
                serde_json::from_slice(&std::fs::read(root.join("lima-mounts.first-start")).unwrap()).unwrap();
            assert!(first.iter().all(|mount| mount["writable"] == false));
        }
    }
}

#[test]
fn other_writers_require_a_pin_even_if_the_new_vm_has_never_booted() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let repos = module.parent().unwrap().parent().unwrap();
    for state in ["Running", "Stopped"] {
        let peer =
            serde_json::json!({"name":"other","status":state,"config":{"mounts":[{"location":repos,"writable":true}]}});
        std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
        let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
        assert!(!setup.status.success() && !root.join("build.sh").exists());
        assert!(String::from_utf8_lossy(&setup.stderr).contains("bootstrap_sha256"));
        std::fs::remove_file(root.join("lima-mounts.created")).unwrap();
    }
    assert!(!f.log().contains("start --tty=false"), "setup should refuse before first boot");
    // A digest covers a standalone module, not its unverified siblings.
    pin_standalone_module(&f, &module);
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(setup.status.success(), "{}", String::from_utf8_lossy(&setup.stderr));
    let script = std::fs::read_to_string(root.join("build.sh")).unwrap();
    assert!(script.contains("module-0.nix"));
    let shell = std::fs::read_to_string(root.join("build.sh.shell")).unwrap();
    assert!(shell.contains(repos.to_str().unwrap()), "the peer-writable share must stay hidden: {shell}");
}

#[test]
fn a_used_vm_cannot_build_an_unpinned_module_from_its_own_writable_share() {
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let output = f.run("unchanged", &["--no-restart"]);
    assert!(!output.status.success());
    assert!(!root.join("build.sh").exists());
    if std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        // The first boot's host can report work that already finished before setup reached the build.
        std::fs::write(root.join("lima-mounts.used"), "").unwrap();
        let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
        assert!(!setup.status.success());
        assert!(String::from_utf8_lossy(&setup.stderr).contains("bootstrap_sha256"));
        assert!(!root.join("build.sh").exists());
    }
    pin_standalone_module(&f, &module);
    std::fs::write(root.join("lima-mounts.created"), "").unwrap();
    let retry = f.run("bootstrap", &["--no-restart"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    assert!(std::fs::read_to_string(root.join("build.sh")).unwrap().contains("module-0.nix"));
}

#[test]
fn a_first_build_that_leaves_the_team_module_writable_is_never_selected() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    let (f, _) = team_onboarding();
    let root = f.dir.path();
    std::fs::copy(f.bundle.join("lima.yaml"), f.bundle.with_extension("next").join("lima.yaml")).unwrap();
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!setup.status.success());
    assert!(String::from_utf8_lossy(&setup.stderr).contains("nothing was switched"));
    assert!(root.join("build.sh").exists());
    assert_eq!(
        f.log().lines().filter(|line| line.starts_with("install ")).count(),
        1,
        "only the initial installer ran"
    );
}

#[test]
fn a_corrected_first_build_reuses_the_read_only_boot_for_sibling_imports() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    for intervening_boot in [false, true] {
        let (f, module) = team_onboarding();
        let root = f.dir.path();
        let result = f.bundle.with_extension("next").join("lima.yaml");
        let corrected = std::fs::read(&result).unwrap();
        std::fs::copy(f.bundle.join("lima.yaml"), &result).unwrap();
        let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
        assert!(!setup.status.success() && String::from_utf8_lossy(&setup.stderr).contains("nothing was switched"));
        std::fs::write(&result, corrected).unwrap();
        std::fs::remove_file(root.join("build.sh")).unwrap();
        if intervening_boot {
            std::fs::write(root.join("lima/atomics-test/fixture-boot-id"), "unobserved-boot").unwrap();
        }
        let retry = f.run("bootstrap", &["--no-restart"]);
        if intervening_boot {
            assert!(!retry.status.success() && String::from_utf8_lossy(&retry.stderr).contains("bootstrap_sha256"));
            assert!(!root.join("build.sh").exists());
        } else {
            assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
            let script = std::fs::read_to_string(root.join("build.sh")).unwrap();
            assert!(script.contains(module.to_str().unwrap()) && !script.contains("module-0.nix"));
            let shell = std::fs::read_to_string(root.join("build.sh.shell")).unwrap();
            assert!(!shell.contains(module.parent().unwrap().parent().unwrap().to_str().unwrap()));
        }
    }
}

#[test]
fn a_new_disk_requires_verified_read_only_devices_even_for_a_pinned_module() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let (f, module) = team_onboarding();
    pin_standalone_module(&f, &module);
    let root = f.dir.path();
    // Lima kept the writable definitions at creation and used them at boot despite the subsequent edit.
    std::fs::copy(root.join("lima-mounts"), root.join("lima-mounts.unexpected")).unwrap();
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!setup.status.success() && String::from_utf8_lossy(&setup.stderr).contains("read-only devices"));
    assert!(!root.join("build.sh").exists());
}

#[test]
fn setup_syncs_secrets_using_the_install_directory_of_its_own_bundle() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let (f, _) = team_onboarding();
    let root = f.dir.path();
    let default = root.join("home/.local/share/atomics-vm");
    std::fs::create_dir_all(&default).unwrap();
    for name in ["install.sh", "lima.yaml", "config.d.toml"] {
        std::fs::copy(f.bundle.join(name), default.join(name)).unwrap();
    }
    let mut info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.bundle.join("atomics-vm.json")).unwrap()).unwrap();
    info["instance"] = "other-vm".into();
    info["installDir"] = serde_json::json!(default);
    std::fs::write(default.join("atomics-vm.json"), info.to_string()).unwrap();
    let mut setup = f.process("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    let output = test_support::output(setup.env_remove("ATOMICS_VM_BUNDLE"));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(root.join("state/vm/atomics-test-secrets.json").is_file());
}

#[test]
fn status_checks_the_running_record_and_the_lima_devices_before_reporting_hardened() {
    let f = Fixture::with(serde_json::json!({"admin":"admin"}));
    let status = |mode| {
        let output = f.command(mode, "status", &[]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(status("status-no-record").contains("hardened mode unknown"));
    assert!(status("status-record").contains("hardened mode unknown"), "no saved policy for this disk");
    let policy = f.dir.path().join("lima/atomics-test/atomics/mounts.json");
    std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
    std::fs::copy(f.dir.path().join("lima-mounts"), policy).unwrap();
    assert!(status("status-record").contains("hardened mode active"));
    let path = f.dir.path().join("lima-mounts");
    let mut devices: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    devices[0]["writable"] = true.into();
    std::fs::write(path, devices.to_string()).unwrap();
    let mismatch = status("status-record");
    assert!(mismatch.contains("Lima mounts: pending") && mismatch.contains("hardened mode unknown"), "{mismatch}");
    assert!(!mismatch.contains("hardened mode active"));
}

#[test]
fn ordinary_update_masks_a_peer_writable_subfolder_at_evaluation() {
    let f = Fixture::new();
    let root = f.dir.path();
    let repos = root.join("home/repos");
    let module = repos.join("team.nix");
    let generated = repos.join("generated");
    std::fs::create_dir(&generated).unwrap();
    std::fs::write(&module, "{ imports = [ ./generated/defaults.nix ]; }").unwrap();
    std::fs::write(root.join("config.toml"), format!("[vm]\nmodules = [{module:?}]\n")).unwrap();
    let peer = serde_json::json!({"name":"peer","status":"Stopped","config":{"mounts":[{"location":generated,"writable":true}]}});
    std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
    let output = f.run("unchanged", &["--no-restart"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let shell = std::fs::read_to_string(root.join("build.sh.shell")).unwrap_or_default();
    assert!(shell.contains(generated.to_str().unwrap()), "{shell}");
}

#[test]
fn policy_and_lifecycle_lock_require_host_only_paths() {
    for folder in ["lima/atomics-test/atomics", "lima/_config", "lima"] {
        let f = Fixture::new();
        let root = f.dir.path();
        let writable = root.join(folder);
        std::fs::create_dir_all(&writable).unwrap();
        let peer = serde_json::json!({"name":"peer","status":"Stopped","config":{"mounts":[{"location":writable,"writable":true}]}});
        std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
        std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
        let start = f.command("unchanged", "start", &[]);
        assert!(!start.status.success(), "{folder} could supply the policy or lock");
        assert!(!f.log().contains("start --tty=false"));
    }
}

#[test]
fn default_remembered_and_registered_lima_homes_count_even_for_the_same_instance_name() {
    for other in ["home/.lima", "previous-home", "registered-home"] {
        let f = Fixture::new();
        let root = f.dir.path();
        let home = root.join(other);
        std::fs::create_dir_all(&home).unwrap();
        if other == "previous-home" {
            std::fs::create_dir_all(root.join("state/vm")).unwrap();
            let record = serde_json::json!({"instance":"old","image":"vm.raw","created_at":1,"lima_home":home});
            std::fs::write(root.join("state/vm/old.json"), record.to_string()).unwrap();
        } else if other == "registered-home" {
            let registry = root.join("home/.lima/_config/atomics/homes");
            std::fs::create_dir_all(&registry).unwrap();
            std::fs::write(registry.join("example.json"), serde_json::to_vec(&home).unwrap()).unwrap();
        }
        let peer = serde_json::json!({"name":"atomics-test","status":"Stopped","config":{"mounts":[{"location":f.bundle,"writable":true}]}});
        std::fs::write(home.join("fixture-list"), peer.to_string()).unwrap();
        let output = f.run("unchanged", &["--no-restart"]);
        assert!(!output.status.success() && !root.join("build.sh").exists(), "missed {other}");
    }
}

#[test]
fn a_running_peers_boot_devices_count_after_its_yaml_changes() {
    let f = Fixture::new();
    let root = f.dir.path();
    let peer = serde_json::json!({"name":"peer","status":"Running","config":{"mounts":[{"location":f.bundle,"writable":false}]}});
    std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
    write_boot_devices(&root.join("lima/peer"), &serde_json::json!([{"location":f.bundle,"writable":true}]));
    let output = f.run("unchanged", &["--no-restart"]);
    assert!(!output.status.success() && !root.join("build.sh").exists(), "trusted edited YAML over boot devices");
}

fn write_boot_devices(instance: &std::path::Path, mounts: &serde_json::Value) {
    assert!(
        Command::new("python3").arg(BOOT_FIXTURE).arg(instance).arg(mounts.to_string()).status().unwrap().success()
    );
}

#[test]
fn deferred_admin_is_used_only_after_the_selected_boot_starts() {
    for controlled in [false, true] {
        let f = Fixture::new();
        let root = f.dir.path();
        let next = f.bundle.with_extension("next");
        std::fs::create_dir(&next).unwrap();
        for name in ["lima.yaml", "install.sh"] {
            std::fs::copy(f.bundle.join(name), next.join(name)).unwrap();
        }
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.bundle.join("atomics-vm.json")).unwrap()).unwrap();
        meta["admin"] = "admin".into();
        std::fs::write(next.join("atomics-vm.json"), meta.to_string()).unwrap();
        let first = f.run("changed", &["--no-restart"]);
        assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
        std::fs::copy(next.join("atomics-vm.json"), f.bundle.join("atomics-vm.json")).unwrap();
        std::fs::write(root.join("log"), "").unwrap();
        let again = f.run("restart-pending", &["--no-restart"]);
        assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
        assert!(!f.log().contains("ssh -F"), "the running boot still has its original user");
        if controlled {
            std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
            assert!(f.command("unchanged", "start", &[]).status.success());
        } else {
            std::fs::write(root.join("lima/atomics-test/fixture-boot-id"), "iid-next").unwrap();
            std::fs::write(root.join("lima/atomics-test/ha.pid"), "102").unwrap();
            std::fs::copy(root.join("log.selected"), root.join("running")).unwrap();
            std::fs::write(root.join("running-admin"), "admin").unwrap();
        }
        let restarted = f.run("unchanged", &["--no-restart"]);
        assert!(restarted.status.success(), "{}", String::from_utf8_lossy(&restarted.stderr));
        assert!(f.log().contains("-l admin lima-atomics-test"), "controlled start: {controlled}");
    }
}

#[test]
fn an_interrupted_admin_change_recovers_the_account_of_the_system_that_booted() {
    for (mode, original) in [
        ("interrupt-after-selection", Some("old-admin")),
        ("interrupt-before-selection", Some("old-admin")),
        ("interrupt-before-selection", None),
    ] {
        let f = Fixture::with(serde_json::json!({"admin": original}));
        let root = f.dir.path();
        std::fs::write(root.join("running-admin"), original.unwrap_or("atomics")).unwrap();
        std::fs::write(root.join("running"), "/nix/store/22222222222222222222222222222222-original").unwrap();
        // Establish the disk's original identity before installing a different administrator.
        let initial = f.run("unchanged", &["--no-restart"]);
        assert!(initial.status.success(), "{}", String::from_utf8_lossy(&initial.stderr));
        let next = f.bundle.with_extension("next");
        std::fs::create_dir(&next).unwrap();
        for name in ["lima.yaml", "install.sh"] {
            std::fs::copy(f.bundle.join(name), next.join(name)).unwrap();
        }
        let mut meta: serde_json::Value =
            serde_json::from_slice(&std::fs::read(f.bundle.join("atomics-vm.json")).unwrap()).unwrap();
        meta["admin"] = "new-admin".into();
        std::fs::write(next.join("atomics-vm.json"), meta.to_string()).unwrap();
        let mut update = test_support::TestChild::spawn(
            &mut f.process(mode, "update", &["--no-restart"]),
            test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        while !root.join("log.selecting").exists() {
            if update.child.try_wait().unwrap().is_some() {
                panic!(
                    "update exited before selection: {}",
                    String::from_utf8_lossy(&update.wait_output().unwrap().stderr)
                );
            }
            update.check_timeout().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(update); // Kill the updater and its stand-in while the switch command is outstanding.
        std::fs::copy(next.join("atomics-vm.json"), f.bundle.join("atomics-vm.json")).unwrap();
        std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
        let start = f.command("unchanged", "start", &[]);
        assert!(start.status.success(), "{mode}: {}", String::from_utf8_lossy(&start.stderr));
        let policy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("lima/atomics-test/atomics/mounts.json")).unwrap())
                .unwrap();
        let admin = if mode == "interrupt-after-selection" { Some("new-admin") } else { original };
        assert_eq!(policy["identity"]["current_admin"], serde_json::json!(admin));
        std::fs::write(root.join("log"), "").unwrap();
        let retry = f.run("unchanged", &["--no-restart"]);
        assert!(retry.status.success(), "{mode}: {}", String::from_utf8_lossy(&retry.stderr));
        if let Some(admin) = admin {
            assert!(f.log().contains(&format!("-l {admin} lima-atomics-test")));
        }
    }
}

#[test]
fn retained_setup_needs_disk_identity_even_if_the_install_folder_matches() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let f = Fixture::new();
    let root = f.dir.path();
    std::fs::create_dir_all(root.join("state/vm")).unwrap();
    std::fs::write(
        root.join("state/vm/atomics-test.json"),
        r#"{"instance":"atomics-test","image":"old.raw","created_at":1}"#,
    )
    .unwrap();
    std::fs::write(root.join("atomics-vm.raw"), "").unwrap();
    std::os::unix::fs::symlink(&f.bundle, root.join("bundle")).unwrap();
    let output = f.command("unchanged", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(!f.log().lines().any(|line| line.starts_with("install ")), "a matching stale installer ran");
}

#[test]
fn a_refused_first_build_leaves_read_only_devices_for_recovery() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let peer = serde_json::json!({"name":"peer","status":"Stopped","config":{"mounts":[{"location":module.parent(),"writable":true}]}});
    std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!setup.status.success() && !root.join("build.sh").exists());
    let start = f.command("bootstrap", "start", &[]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stderr));
    let devices: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(root.join("lima-mounts.first-start")).unwrap()).unwrap();
    assert!(devices.iter().all(|s| s["writable"] == false), "{devices:?}");
}

#[test]
fn an_unidentified_former_device_requires_a_standalone_pin() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let (f, module) = team_onboarding();
    let root = f.dir.path();
    let peer = serde_json::json!({"name":"peer","status":"Stopped","config":{"mounts":[]}});
    std::fs::write(root.join("lima-mounts.other"), peer.to_string()).unwrap();
    write_boot_devices(
        &root.join("lima/peer"),
        &serde_json::json!([{"location":root.join("old"), "mountPoint":"/old", "writable":true}]),
    );
    let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!setup.status.success() && !root.join("build.sh").exists());
    assert!(String::from_utf8_lossy(&setup.stderr).contains("bootstrap_sha256"));
    pin_standalone_module(&f, &module);
    let retry = f.run("bootstrap", &["--no-restart"]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    assert!(std::fs::read_to_string(root.join("build.sh")).unwrap().contains("module-0.nix"));
}

#[test]
fn retained_setup_reconciles_the_running_devices_when_yaml_already_matches() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        return;
    }
    let f = Fixture::new();
    let root = f.dir.path();
    let update = f.run("unchanged", &["--no-restart"]);
    assert!(update.status.success(), "{}", String::from_utf8_lossy(&update.stderr));
    std::fs::write(
        root.join("state/vm/atomics-test.json"),
        r#"{"instance":"atomics-test","image":"original.raw","created_at":1}"#,
    )
    .unwrap();
    std::fs::write(root.join("atomics-vm.raw"), "").unwrap();
    std::os::unix::fs::symlink(&f.bundle, root.join("bundle")).unwrap();
    let mut live: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("lima-mounts")).unwrap()).unwrap();
    live[0]["writable"] = true.into();
    std::fs::write(root.join("lima-mounts.boot"), live.to_string()).unwrap();
    let setup = f.command("late-busy", "setup", &["--from", root.to_str().unwrap()]);
    assert!(!setup.status.success());
    assert!(
        String::from_utf8_lossy(&setup.stderr).contains("work in the VM is busy"),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );
}
