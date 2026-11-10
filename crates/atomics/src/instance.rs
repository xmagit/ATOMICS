//! One runtime owner per `ATOMICS_HOME`, enforced with `flock` on `instance.lock`.
//!
//! The headless host (`atomics host`) holds the lock exclusively; every desktop holds it shared.
//! Desktops therefore still start side by side as before (a second one runs without the socket),
//! but a host never shares a home with a desktop or with another host. The kernel releases the
//! lock when the process ends, however it ends, and the file is opened close-on-exec so agents
//! never inherit it.

use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const FILE: &str = "instance.lock";

/// Held for the life of the process; dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Why the lock was not taken.
#[derive(Debug)]
pub enum Refused {
    /// One or more desktops run against this home.
    Desktop(PathBuf),
    /// A headless host runs against this home; its pid when it could be read.
    Host(PathBuf, Option<u32>),
    /// The lock file could not be opened or locked.
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Desktop(home) => write!(
                f,
                "the ATOMICS desktop is running for {}; quit it first, or give the host its own ATOMICS_HOME",
                home.display()
            ),
            Refused::Host(home, pid) => {
                let pid = pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default();
                write!(
                    f,
                    "a headless ATOMICS host{pid} is running for {}; stop it first (atomics host takes the whole home)",
                    home.display()
                )
            }
            Refused::Io(path, error) => write!(f, "cannot lock {}: {error}", path.display()),
        }
    }
}

impl Lock {
    /// The headless host's exclusive lock. Records the pid for the refusal messages of others.
    pub fn host(home: &Path) -> Result<Lock, Refused> {
        let (mut file, path) = open(home)?;
        if !try_lock(&file, libc::LOCK_EX).map_err(|e| Refused::Io(path.clone(), e))? {
            // Shared holders are desktops; an exclusive one is another host.
            return Err(if try_lock(&file, libc::LOCK_SH).unwrap_or(false) {
                Refused::Desktop(home.to_path_buf())
            } else {
                Refused::Host(home.to_path_buf(), recorded_pid(&mut file))
            });
        }
        let pid = std::process::id().to_string();
        let recorded = file.set_len(0).and_then(|()| file.rewind()).and_then(|()| writeln!(file, "{pid}"));
        recorded.map_err(|e| Refused::Io(path, e))?;
        Ok(Lock { _file: file })
    }

    /// A desktop's shared lock: refused only while a headless host holds this home.
    pub fn desktop(home: &Path) -> Result<Lock, Refused> {
        let (mut file, path) = open(home)?;
        if !try_lock(&file, libc::LOCK_SH).map_err(|e| Refused::Io(path, e))? {
            return Err(Refused::Host(home.to_path_buf(), recorded_pid(&mut file)));
        }
        Ok(Lock { _file: file })
    }
}

fn open(home: &Path) -> Result<(File, PathBuf), Refused> {
    let path = home.join(FILE);
    let opened = atomics_core::store::private_dir(home).and_then(|()| {
        // std opens files close-on-exec, so no child process holds the lock after we exit.
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).open(&path)
    });
    opened.map(|file| (file, path.clone())).map_err(|e| Refused::Io(path, e))
}

/// `Ok(false)` when another holder conflicts; other failures are errors.
fn try_lock(file: &File, operation: libc::c_int) -> io::Result<bool> {
    loop {
        // SAFETY: the descriptor is open for the life of `file`.
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(code) if code == libc::EWOULDBLOCK => return Ok(false),
            _ => return Err(error),
        }
    }
}

fn recorded_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.rewind().ok()?;
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // flock locks belong to open file descriptions, so two opens in one process conflict
    // exactly as two processes would.

    /// Takes a lock that should be free. Other tests in this process fork children all the time,
    /// and each holds every open descriptor for the moment before its exec closes them, so a lock
    /// just released can look held briefly. Anything that keeps it longer still fails.
    fn take(lock: impl Fn() -> Result<Lock, Refused>) -> Lock {
        let deadline = std::time::Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
        loop {
            match lock() {
                Ok(lock) => return lock,
                Err(refused) if std::time::Instant::now() >= deadline => panic!("the lock stayed taken: {refused}"),
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn desktops_share_a_home_but_a_host_excludes_them_both_ways() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path().join("state");
        let first = take(|| Lock::desktop(&home));
        let second = take(|| Lock::desktop(&home));
        let refused = Lock::host(&home).unwrap_err();
        assert!(matches!(refused, Refused::Desktop(_)), "{refused:?}");
        assert!(refused.to_string().contains("desktop is running"), "{refused}");
        drop((first, second));

        let host = take(|| Lock::host(&home));
        let pid = std::process::id();
        let refused = Lock::desktop(&home).unwrap_err();
        assert!(matches!(refused, Refused::Host(_, Some(recorded)) if recorded == pid), "{refused:?}");
        assert!(refused.to_string().contains(&format!("pid {pid}")), "{refused}");
        let refused = Lock::host(&home).unwrap_err();
        assert!(matches!(refused, Refused::Host(_, Some(recorded)) if recorded == pid), "{refused:?}");
        drop(host);

        drop(take(|| Lock::desktop(&home)));
        take(|| Lock::host(&home));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.join(FILE)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn the_lock_is_not_inherited_by_child_processes() {
        let dir = crate::test_support::tempdir().unwrap();
        let home = dir.path().join("state");
        let host = take(|| Lock::host(&home));
        // A child that outlives the host must not keep the home locked.
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        drop(host);
        let taken = std::panic::catch_unwind(|| take(|| Lock::host(&home)));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(taken.is_ok(), "the child kept the lock");
    }
}
