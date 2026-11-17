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
        std::fs::create_dir_all(dir.path().join("home/Applications/ATOMICS.app")).unwrap();
        for program in ["open", "osascript"] {
            test_support::write_program(
                bin.join(program),
                "#!/bin/sh\nprintf '%s %s\\n' \"${0##*/}\" \"$*\" >> \"$VM_LOG\"\nif [ \"${0##*/}\" = osascript ] && [ \"$VM_MODE\" = quit-fail ]; then echo 'Could not quit: ATOMICS (pid 1234)' >&2; exit 1; fi\n",
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
    case "$VM_MODE" in
      changed|quit-fail) printf 'changed\n' ;;
      restart-pending) printf 'restart-pending\n' ;;
      *) printf 'unchanged\n' ;;
    esac ;;
  *'sudo sshd -T'*) echo 'fixture sshd' ;;
  'rm -rf -- '*) exit 0 ;;
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
printf 'ssh %s\n' "$*" >> "$VM_LOG"
test "$1" = -F && test "$2" = "$LIMA_HOME/atomics-test/ssh.config"
case "$*" in
  *atomics-vm-build*)
    record=null
    test "$VM_MODE" != status-record || record='{"version":1,"hardened":true,"modules":[]}'
    printf 'atomics-vm-build\n%s\n%s\nusers\n' "$record" "$record"
    exit 0 ;;
  *ATOMICS_MAC_SHARES*) printf '%s:ro %s:rw\n' "$HOME/repos" "$HOME/atomics-exchange"; exit 0 ;;
esac
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
  --version) echo 'limactl version 2.2.0' ;;
  list)
    if [ "$2" = --json ]; then
      if [ -f "$VM_LIMA_MOUNTS.other" ]; then cat "$VM_LIMA_MOUNTS.other"; echo; fi
      if [ "$VM_MODE" = bootstrap ] && [ ! -e "$VM_LIMA_MOUNTS.created" ]; then exit 0; fi
      state=Running
      test ! -f "$VM_LIMA_MOUNTS.stopped" || state=Stopped
      printf '{"name":"atomics-test","status":"%s"}\n' "$state"
    else
      test "$2" = --format && test "$3" = '{{json .Config}}' && test "$4" = atomics-test
      printf '{"cpus":4,"memory":"8GiB","disk":"100GiB","mounts":%s}\n' "$(cat "$VM_LIMA_MOUNTS")"
    fi ;;
  create)
    test "$VM_MODE" = bootstrap
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
        test "$VM_MODE" = bootstrap
        exec "$VM_GUEST" ;;
      esac
      if [ "$7" = 'exec bash -s' ]; then
        # System changes come on stdin, in a plain bash the login shell execs (vm.rs, system_shell).
        exec "$VM_GUEST"
      fi
      # Questions run in a plain bash that the login shell execs: `exec bash -c "$1" atomics-vm-update SCRIPT`.
      if [ "$VM_MODE" = bootstrap ] && [ "$8" = atomics-vm-check ]; then cat >/dev/null; exit 0; fi
      script=$7
      if [ "$7" = 'exec bash -c "$1"' ]; then test "$8" = atomics-vm-update; script=$9; fi
      case "$script" in
        *'atomics ctl'*)
          if [ "$VM_MODE" = busy ]; then
            printf '{"ok":true,"result":{"sessions":[{"status":"needs you","title":"worker"}]}}\n'
          elif [ "$VM_MODE" = late-busy ] && [ -e "$VM_BUILD_SCRIPT" ]; then
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
  stop) touch "$VM_LIMA_MOUNTS.stopped" ;;
  start) rm -f "$VM_LIMA_MOUNTS.stopped" ;;
  *) exit 1 ;;
esac
"#,
        );
        Self { dir, bundle }
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        self.command(mode, "update", args)
    }

    fn command(&self, mode: &str, command: &str, args: &[&str]) -> Output {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
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
                .env("VM_LOG", self.dir.path().join("log"))
                .env("VM_BUILD_SCRIPT", self.dir.path().join("build.sh"))
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
    assert!(log.contains("osascript -l JavaScript -e ObjC.import"));
    assert!(log.contains("open -a "));
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
        assert_eq!(fixture.log().contains("stop atomics-test"), args.is_empty());
        assert_eq!(fixture.log().contains("start --tty=false atomics-test"), args.is_empty());
        assert_eq!(fixture.log().contains("osascript -l JavaScript -e"), args.is_empty());
        assert_eq!(fixture.log().contains("open -a "), args.is_empty());
        assert!(String::from_utf8_lossy(&output.stdout).contains("VM system: changed"));
    }
    let fixture = Fixture::new();
    let output = fixture.run("boot-fail", &[]);
    assert!(!output.status.success());
    assert!(fixture.log().contains("/previous/install.sh"));
    assert!(!fixture.log().contains("stop atomics-test"));
}

#[test]
fn a_failed_window_quit_leaves_a_restart_owed_and_a_retry_restarts_once() {
    let fixture = Fixture::new();
    let first = fixture.run("quit-fail", &[]);
    assert!(!first.status.success());
    let error = String::from_utf8_lossy(&first.stderr);
    assert!(error.contains("restart pending") && error.contains("pid 1234"), "{error}");
    assert!(!error.contains("no ATOMICS socket"), "client updates have no local queue: {error}");
    assert!(!fixture.log().contains("stop atomics-test"));
    let retry = fixture.run("restart-pending", &[]);
    assert!(retry.status.success(), "{}", String::from_utf8_lossy(&retry.stderr));
    assert!(String::from_utf8_lossy(&retry.stdout).contains("VM system: restart pending"));
    let log = fixture.log();
    assert_eq!(log.lines().filter(|line| *line == "stop atomics-test").count(), 1, "{log}");
    assert_eq!(log.lines().filter(|line| *line == "start --tty=false atomics-test").count(), 1, "{log}");
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
    let devices: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut proposed = devices.clone();
    proposed[0]["writable"] = true.into();
    proposed.as_array_mut().unwrap().push(serde_json::json!({"location":root.join("extra"),"writable":true}));
    std::fs::write(fixture.bundle.join("lima.yaml"), serde_json::json!({"mounts":proposed}).to_string()).unwrap();
    std::fs::write(root.join("lima-mounts.stopped"), "").unwrap();
    let start = fixture.command("unchanged", "start", &[]);
    assert!(start.status.success(), "{}", String::from_utf8_lossy(&start.stderr));
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
fn first_setup_never_builds_from_an_unverified_module_or_from_a_writable_source() {
    if !std::path::Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists() {
        eprintln!("vm setup needs Rosetta on this Mac");
        return;
    }
    for input in ["module", "source", "installation", "other writer"] {
        let f = Fixture::new();
        let root = f.dir.path();
        let writable = root.join("home/atomics-exchange");
        let module = match input {
            "module" => writable.join("team.nix"),
            "other writer" => root.join("home/repos/team.nix"),
            _ => root.join("trusted.nix"),
        };
        if input == "other writer" {
            let other = serde_json::json!({"name":"other","status":"Stopped","config":{"mounts":[{"location":root.join("home/repos"),"writable":true}]}});
            std::fs::write(root.join("lima-mounts.other"), other.to_string()).unwrap();
        }
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
        assert!(
            error.contains(if matches!(input, "module" | "other writer") {
                "bootstrap_sha256"
            } else {
                "folder the VM can write"
            }),
            "{error}"
        );
        if input == "other writer" {
            // Repeat first setup in the fixture with the operator's digest of "{}".
            std::fs::remove_file(root.join("lima-mounts.created")).unwrap();
            use std::io::Write;
            let mut config = std::fs::OpenOptions::new().append(true).open(root.join("config.toml")).unwrap();
            writeln!(
                config,
                "[vm.bootstrap_sha256]\n{module:?} = '44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a'"
            )
            .unwrap();
            std::fs::write(f.bundle.join("config.d.toml"), "[client]\nconnect = \"vm\"\n").unwrap();
            let setup = f.command("bootstrap", "setup", &["--from", root.to_str().unwrap()]);
            assert!(setup.status.success(), "{}", String::from_utf8_lossy(&setup.stderr));
            let script =
                std::fs::read_to_string(root.join("build.sh")).expect("first setup used the verified snapshot");
            assert!(script.contains("module-0.nix") && script.contains("e30="));
            assert!(f.log().contains("exec sudo unshare --mount --propagation private"));
        }
    }
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
    assert!(status("status-record").contains("hardened mode active"));
    let path = f.dir.path().join("lima-mounts");
    let mut devices: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    devices[0]["writable"] = true.into();
    std::fs::write(path, devices.to_string()).unwrap();
    let mismatch = status("status-record");
    assert!(mismatch.contains("Lima mounts: pending") && mismatch.contains("hardened mode unknown"), "{mismatch}");
    assert!(!mismatch.contains("hardened mode active"));
}
