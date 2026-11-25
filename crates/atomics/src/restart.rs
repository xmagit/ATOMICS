//! Validates and relaunches this installation, independently of the UI and its PTYs.
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub use crate::build_info::revision as version;

/// Runs a command in a systemd scope of its own.
const SCOPE: [&str; 6] = ["systemd-run", "--user", "--scope", "--quiet", "--collect", "--"];
const SCOPE_WAIT: Duration = Duration::from_secs(2);

/// Whether `pid` runs in a service of a systemd user manager, as a window started from a desktop's menu does. A
/// service that ends with its main process takes what is left in it along, a relauncher too.
fn in_service(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|cgroups| names_service(&cgroups))
}

fn names_service(cgroups: &str) -> bool {
    cgroups.lines().filter_map(|line| line.splitn(3, ':').nth(2)).any(|path| {
        let mut units = path.split('/').skip_while(|unit| !(unit.starts_with("user@") && unit.ends_with(".service")));
        units.next().is_some() && units.last().is_some_and(|unit| unit.ends_with(".service"))
    })
}

/// Whether this machine's user manager starts scopes.
fn scopes_work() -> bool {
    let mut command = Command::new(SCOPE[0]);
    command.args(&SCOPE[1..]).arg("true").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let Ok(mut child) = command.spawn() else { return false };
    let deadline = Instant::now() + SCOPE_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

pub struct Target {
    pub executable: PathBuf,
    pub command: Vec<OsString>,
}

impl Target {
    /// This installation with this process's arguments, but `workspace` in place of any
    /// `--workspace` they had: a restart keeps the workspace open now, even one picked after the start.
    pub fn current(workspace: Option<&str>) -> Result<Self> {
        let executable = crate::setup::executable_path().context("locating this installation")?;
        let args = with_workspace(std::env::args_os().skip(1).collect(), workspace);
        let command = relaunch_command(&executable, &args, cfg!(target_os = "macos"))?;
        Ok(Self { executable, command })
    }

    /// The program at `executable`, with what `current` gives this one: the window moving to where it was installed.
    pub fn at(executable: PathBuf, workspace: Option<&str>) -> Result<Self> {
        let args = with_workspace(std::env::args_os().skip(1).collect(), workspace);
        let command = relaunch_command(&executable, &args, cfg!(target_os = "macos"))?;
        Ok(Self { executable, command })
    }

    /// This installation with other arguments: the window switching from a host's client to the host.
    pub fn with_args(args: &[OsString]) -> Result<Self> {
        let executable = crate::setup::executable_path().context("locating this installation")?;
        let command = relaunch_command(&executable, args, cfg!(target_os = "macos"))?;
        Ok(Self { executable, command })
    }

    pub fn validate(&self) -> Result<()> {
        check_version(&self.executable, Duration::from_secs(5))
    }

    pub fn spawn(&self, old_pid: u32, home: &Path) -> Result<u32> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(home.join("restart.log"))?;
        let scoped = in_service(std::process::id()) && scopes_work();
        let mut command = Command::new(if scoped { SCOPE[0] } else { "/bin/sh" });
        if scoped {
            command.args(&SCOPE[1..]).arg("/bin/sh");
        }
        // All paths and original arguments are positional parameters, never shell source.
        command
            .args([
                "-c",
                "while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; shift; exec \"$@\"",
                "atomics-relaunch",
            ])
            .arg(old_pid.to_string())
            .args(&self.command)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let relauncher = command.spawn().context("starting detached relauncher")?.id();
        // It keeps its process id on its way into the scope. This process must not end before it is there.
        let deadline = Instant::now() + SCOPE_WAIT;
        while scoped && in_service(relauncher) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(relauncher)
    }
}

/// `args` without their `--workspace NAME` (`-w NAME`, `--workspace=NAME`, `-wNAME`), then
/// `--workspace=<workspace>` when one is open (one argument, so no name reads as an option). The
/// other arguments keep their order.
fn with_workspace(args: Vec<OsString>, workspace: Option<&str>) -> Vec<OsString> {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let text = arg.to_string_lossy();
        if text == "--" {
            kept.push(arg);
            kept.extend(args.by_ref());
            break;
        }
        if text == "--workspace" || text == "-w" {
            args.next();
            continue;
        }
        if text.starts_with("--workspace=") || (text.starts_with("-w") && text.len() > 2 && !text.starts_with("--")) {
            continue;
        }
        kept.push(arg);
    }
    if let Some(name) = workspace {
        // Before a `--`, so it stays an option.
        let at = kept.iter().position(|arg| arg == "--").unwrap_or(kept.len());
        kept.insert(at, OsString::from(format!("--workspace={name}")));
    }
    kept
}

/// The `.app` that `executable` is the program of. Only its own `Contents/MacOS/atomics` makes a folder one: a
/// development build somewhere under a folder that happens to end in `.app` is not in a bundle, and an update must not
/// swap that folder away.
pub(crate) fn bundle_of(executable: &Path) -> Option<&Path> {
    let bundle = executable.ancestors().nth(3)?;
    let inside = executable.strip_prefix(bundle).ok()?;
    (bundle.extension().is_some_and(|ext| ext == "app") && inside == Path::new("Contents/MacOS/atomics"))
        .then_some(bundle)
}

fn relaunch_command(executable: &Path, args: &[OsString], macos: bool) -> Result<Vec<OsString>> {
    let mut command = if macos {
        let bundle = bundle_of(executable)
            .context("restart on macOS requires an installed .app bundle; rebuild/install it first")?;
        let mut command = vec!["/usr/bin/open".into(), "-n".into(), "-a".into(), bundle.as_os_str().to_owned()];
        // LaunchServices does not inherit the caller's environment. Preserve instance
        // isolation explicitly, as well as the login environment used to launch agents.
        for key in [
            "ATOMICS_HOME",
            "ATOMICS_SOCKET",
            atomics_core::paths::HOST_SOCKET_ENV,
            "ATOMICS_CONFIG",
            "ATOMICS_VM_BUNDLE",
            "ATOMICS_CREDENTIALS_DIR",
            "ATOMICS_HOST",
            "HOME",
            "PATH",
            "SHELL",
            "XDG_CONFIG_HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
        ] {
            if let Some(value) = std::env::var_os(key) {
                let mut variable = OsString::from(format!("{key}="));
                variable.push(value);
                command.extend(["--env".into(), variable]);
            }
        }
        command.push("--args".into());
        command
    } else {
        vec![executable.as_os_str().to_owned()]
    };
    command.extend_from_slice(args);
    Ok(command)
}

fn check_version(executable: &Path, timeout: Duration) -> Result<()> {
    run_version(executable, timeout, Stdio::null())
}

/// What `executable --version` prints.
pub fn version_output(executable: &Path) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut output = tempfile::tempfile().context("making a file for the version's output")?;
    run_version(executable, Duration::from_secs(5), output.try_clone()?.into())?;
    output.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    output.take(4096).read_to_string(&mut text)?;
    Ok(text)
}

fn run_version(executable: &Path, timeout: Duration, stdout: Stdio) -> Result<()> {
    if !executable.is_file() {
        bail!("restart target is missing: {}", executable.display());
    }
    let mut command = Command::new(executable);
    command.arg("--version").stdin(Stdio::null()).stdout(stdout).stderr(Stdio::null()).process_group(0);
    let mut child =
        spawn_when_free(&mut command).with_context(|| format!("cannot run {} --version", executable.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("{} --version failed ({status})", executable.display());
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            // SAFETY: the child owns the process group established above.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!("{} --version timed out", executable.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Starts `command`. A program just copied can be busy for a moment: a fork elsewhere in this process inherits the
/// copy's write handle until it execs, and the system refuses to run a file open for writing.
fn spawn_when_free(command: &mut Command) -> std::io::Result<std::process::Child> {
    const TRIES: u32 = 40;
    for _ in 1..TRIES {
        match command.spawn() {
            Err(why) if why.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(50));
            }
            outcome => return outcome,
        }
    }
    command.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_program_still_open_for_writing_runs_once_it_is_free() {
        use std::io::Write;
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("program");
        let mut writer = std::fs::File::create(&program).unwrap();
        writer.write_all(b"#!/bin/sh\necho 'atomics 2.0.0 (v2)'\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let closes = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(writer);
        });
        let output = version_output(&program).unwrap();
        closes.join().unwrap();
        assert_eq!(output.trim(), "atomics 2.0.0 (v2)");
    }

    #[test]
    fn detached_relauncher_waits_for_old_pid_and_survives_hangup() {
        let dir = crate::test_support::tempdir().unwrap();
        let marker = dir.path().join("relaunched");
        let mut old = Command::new(crate::test_support::program("sleep")).arg("30").spawn().unwrap();
        let target = Target {
            executable: PathBuf::from("/unused"),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf resumed > \"$1\"".into(),
                "relaunch-test".into(),
                marker.as_os_str().to_owned(),
            ],
        };
        let pid = target.spawn(old.id(), dir.path()).unwrap() as i32;
        // SAFETY: read the identity of the child spawned above, then signal it only.
        unsafe {
            assert_eq!(libc::getpgid(pid), pid);
            assert_eq!(libc::getsid(pid), pid);
            assert_eq!(libc::kill(pid, libc::SIGHUP), 0);
        }
        std::thread::sleep(Duration::from_millis(150));
        assert!(!marker.exists(), "must wait for the old process to exit");
        old.kill().unwrap();
        old.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < deadline, "relauncher did not survive SIGHUP");
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: reap only our own child, normally reparented when ATOMICS exits.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "resumed");
    }

    const SERVICE_TEST: &str = "ATOMICS_TEST_RELAUNCH";

    /// What the next test runs as a service: it starts a relauncher and ends.
    #[test]
    fn service_that_relaunches() {
        let Some(dir) = std::env::var_os(SERVICE_TEST).map(PathBuf::from) else { return };
        let script = "printf resumed > \"$1\"";
        let mut command: Vec<OsString> = ["/bin/sh", "-c", script, "relaunch-test"].map(Into::into).into();
        command.push(dir.join("relaunched").into());
        assert!(in_service(std::process::id()));
        let target = Target { executable: PathBuf::from("/unused"), command };
        let relauncher = target.spawn(std::process::id(), &dir).unwrap();
        assert!(!in_service(relauncher));
    }

    #[test]
    fn a_relauncher_outlives_a_service_that_ends_with_its_main_process() {
        if !scopes_work() {
            eprintln!("skipped: no systemd user manager");
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let status = Command::new("systemd-run")
            .args(["--user", "--quiet", "--wait", "--collect", "--property=ExitType=main"])
            .arg(format!("--setenv={SERVICE_TEST}={}", dir.path().display()))
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "restart::tests::service_that_relaunches"])
            .status()
            .unwrap();
        assert!(status.success(), "the service started its relauncher");
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        while !dir.path().join("relaunched").exists() {
            assert!(Instant::now() < deadline, "the relauncher ended with the service");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_service_of_the_user_manager_is_told_from_a_scope_and_from_the_manager_itself() {
        let app = "/user.slice/user-1000.slice/user@1000.service/app.slice";
        assert!(names_service(&format!("0::{app}/app-atomics@1.service\n")));
        assert!(names_service(&format!("12:pids:/elsewhere\n0::{app}/app-atomics.slice/app-atomics@1.service\n")));
        assert!(!names_service(&format!("0::{app}/app-terminal-1.scope\n")));
        assert!(!names_service("0::/user.slice/user-1000.slice/user@1000.service\n"));
        assert!(!names_service("0::/system.slice/atomics.service\n"), "a system service has no user manager to ask");
        assert!(!names_service("") && !in_service(u32::MAX));
    }

    #[test]
    fn relaunch_uses_the_same_bundle_or_binary_and_literal_arguments() {
        let executable = Path::new("/tmp/a '$ app/ATOMICS.app/Contents/MacOS/atomics");
        let args = vec!["/repo with spaces".into(), "--workspace".into(), "$(never execute)".into()];
        let mac = relaunch_command(executable, &args, true).unwrap();
        assert_eq!(&mac[..4], ["/usr/bin/open", "-n", "-a", "/tmp/a '$ app/ATOMICS.app"]);
        let separator = mac.iter().position(|arg| arg == "--args").unwrap();
        assert_eq!(&mac[separator + 1..], args);
        let linux = relaunch_command(executable, &args, false).unwrap();
        assert_eq!(linux[0], executable);
        assert_eq!(&linux[1..], args);
        assert!(relaunch_command(Path::new("/tmp/atomics"), &[], true).is_err());
        // A development build under a folder that only happens to end in `.app` is not in a bundle.
        assert!(relaunch_command(Path::new("/tmp/work.app/target/debug/atomics"), &[], true).is_err());
    }

    #[test]
    fn mac_handoff_keeps_the_selected_vm_and_its_isolated_identity() {
        if !crate::test_support::isolated_test(
            "restart::tests::mac_handoff_keeps_the_selected_vm_and_its_isolated_identity",
        ) {
            return;
        }
        for (key, value) in [
            ("ATOMICS_VM_BUNDLE", "/scratch/vm"),
            ("ATOMICS_CREDENTIALS_DIR", "/scratch/credentials"),
            ("ATOMICS_HOST", "qa-host"),
        ] {
            // SAFETY: isolated_test runs this test alone in its own process.
            unsafe {
                std::env::set_var(key, value);
            }
            let command = relaunch_command(
                Path::new("/scratch/QA.app/Contents/MacOS/atomics"),
                &["--connect".into(), "vm".into()],
                true,
            )
            .unwrap();
            assert!(
                command
                    .windows(2)
                    .any(|args| args == [OsString::from("--env"), OsString::from(format!("{key}={value}"))]),
                "missing {key}"
            );
        }
    }

    #[test]
    fn a_restart_keeps_the_open_workspace_in_place_of_the_one_it_started_with() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        // Started from the Dock with no arguments: the workspace open now is named.
        assert_eq!(with_workspace(vec![], Some("team")), args(&["--workspace=team"]));
        // Each way of naming one is replaced; the rest keep their order.
        for started in [
            &["/repo", "--workspace", "daily", "--size", "800x600"][..],
            &["/repo", "-w", "daily", "--size", "800x600"],
            &["/repo", "--workspace=daily", "--size", "800x600"],
            &["/repo", "-wdaily", "--size", "800x600"],
        ] {
            assert_eq!(
                with_workspace(args(started), Some("team")),
                args(&["/repo", "--size", "800x600", "--workspace=team"]),
                "{started:?}"
            );
            assert_eq!(with_workspace(args(started), None), args(&["/repo", "--size", "800x600"]), "{started:?}");
        }
        // A workspace named like an option stays one value; nothing after `--` is read as an option.
        assert_eq!(
            with_workspace(args(&["host", "--", "-w"]), Some("-x")),
            args(&["host", "--workspace=-x", "--", "-w"])
        );
    }

    #[test]
    fn version_check_refuses_missing_broken_nonexecutable_and_hung_targets() {
        let dir = crate::test_support::tempdir().unwrap();
        let exe = dir.path().join("atomics");
        let timeout = crate::test_support::scaled_timeout(Duration::from_secs(60));
        assert!(check_version(&exe, timeout).unwrap_err().to_string().contains("missing"));
        crate::test_support::write_program(&exe, "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_version(&exe, timeout).is_err());
        crate::test_support::write_program(&exe, "#!/bin/sh\ntest \"$1\" = --version\n");
        check_version(&exe, timeout).unwrap();
        crate::test_support::write_program(&exe, "#!/bin/sh\nexit 42\n");
        assert!(check_version(&exe, timeout).unwrap_err().to_string().contains("failed"));
        crate::test_support::write_program(&exe, "#!/bin/sh\nexec sleep 30\n");
        assert!(check_version(&exe, Duration::from_millis(30)).unwrap_err().to_string().contains("timed out"));
    }
}
