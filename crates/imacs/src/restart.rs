//! Validates and relaunches this installation, independently of the UI and its PTYs.
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub const VERSION: &str = env!("IMACS_BUILD_VERSION");

pub struct Target {
    pub executable: PathBuf,
    pub command: Vec<OsString>,
}

impl Target {
    pub fn current() -> Result<Self> {
        let executable = crate::setup::executable_path().context("locating this installation")?;
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let command = relaunch_command(&executable, &args, cfg!(target_os = "macos"))?;
        Ok(Self { executable, command })
    }

    pub fn validate(&self) -> Result<()> {
        check_version(&self.executable, Duration::from_secs(5))
    }

    pub fn spawn(&self, old_pid: u32, home: &Path) -> Result<u32> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(home.join("restart.log"))?;
        let mut command = Command::new("/bin/sh");
        // All paths and original arguments are positional parameters, never shell source.
        command
            .args(["-c", "while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done; shift; exec \"$@\"", "imacs-relaunch"])
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
        Ok(command.spawn().context("starting detached relauncher")?.id())
    }
}

fn relaunch_command(executable: &Path, args: &[OsString], macos: bool) -> Result<Vec<OsString>> {
    let mut command = if macos {
        let bundle = executable
            .ancestors()
            .find(|path| path.extension().is_some_and(|ext| ext == "app"))
            .context("restart on macOS requires an installed .app bundle; rebuild/install it first")?;
        let mut command = vec!["/usr/bin/open".into(), "-n".into(), "-a".into(), bundle.as_os_str().to_owned()];
        // LaunchServices does not inherit the caller's environment. Preserve instance
        // isolation explicitly, as well as the login environment used to launch agents.
        for key in [
            "IMACS_HOME",
            "IMACS_SOCKET",
            "IMACS_CONFIG",
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
    if !executable.is_file() {
        bail!("restart target is missing: {}", executable.display());
    }
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .with_context(|| format!("cannot run {} --version", executable.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn detached_relauncher_waits_for_old_pid_and_survives_hangup() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("relaunched");
        let mut old = Command::new("/bin/sleep").arg("30").spawn().unwrap();
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
        // SAFETY: reap only our own child, normally reparented when IMACS exits.
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "resumed");
    }

    #[test]
    fn relaunch_uses_the_same_bundle_or_binary_and_literal_arguments() {
        let executable = Path::new("/tmp/a '$ app/IMACS.app/Contents/MacOS/imacs");
        let args = vec!["/repo with spaces".into(), "--workspace".into(), "$(never execute)".into()];
        let mac = relaunch_command(executable, &args, true).unwrap();
        assert_eq!(&mac[..4], ["/usr/bin/open", "-n", "-a", "/tmp/a '$ app/IMACS.app"]);
        let separator = mac.iter().position(|arg| arg == "--args").unwrap();
        assert_eq!(&mac[separator + 1..], args);
        let linux = relaunch_command(executable, &args, false).unwrap();
        assert_eq!(linux[0], executable);
        assert_eq!(&linux[1..], args);
        assert!(relaunch_command(Path::new("/tmp/imacs"), &[], true).is_err());
    }

    #[test]
    fn version_check_refuses_missing_broken_nonexecutable_and_hung_targets() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("imacs");
        assert!(check_version(&exe, Duration::from_secs(1)).unwrap_err().to_string().contains("missing"));
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(check_version(&exe, Duration::from_secs(1)).is_err());
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&exe, "#!/bin/sh\ntest \"$1\" = --version\n").unwrap();
        check_version(&exe, Duration::from_secs(1)).unwrap();
        std::fs::write(&exe, "#!/bin/sh\nexit 42\n").unwrap();
        assert!(check_version(&exe, Duration::from_secs(1)).unwrap_err().to_string().contains("failed"));
        std::fs::write(&exe, "#!/bin/sh\nexec sleep 30\n").unwrap();
        assert!(check_version(&exe, Duration::from_millis(30)).unwrap_err().to_string().contains("timed out"));
    }
}
