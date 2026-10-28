//! A record of how each run ended, in `crash.log` under the IMACS home: a start line, an exit
//! line on a clean quit, and panics with their backtrace. A start without an exit means the
//! last run died (killed, crashed, or the machine gave out), even when macOS kept no report.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The log is trimmed to its newest half once it grows past this.
const MAX_BYTES: u64 = 256 * 1024;

fn log_path(home: &Path) -> PathBuf {
    home.join("crash.log")
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

fn append(home: &Path, line: &str) {
    let path = log_path(home);
    let _ = std::fs::create_dir_all(home);
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_BYTES)
        && let Ok(text) = std::fs::read_to_string(&path)
    {
        let keep = &text[text.len() / 2..];
        let keep = keep.find('\n').map_or(keep, |i| &keep[i + 1..]);
        let _ = std::fs::write(&path, keep);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{} {line}", now());
    }
}

/// Records this run's start. Returns a notice when the previous run never recorded its exit.
pub fn started(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_path(home)).unwrap_or_default();
    let last_start = text.lines().rev().find(|line| line.split(' ').nth(1) == Some("start"));
    let unclean = last_start.and_then(|start| {
        let pid = start.split(' ').nth(2)?;
        let exited = text.lines().rev().take_while(|line| *line != start).any(|line| {
            let mut words = line.split(' ').skip(1);
            words.next() == Some("exit") && words.next() == Some(pid)
        });
        (!exited).then_some(pid.to_string())
    });
    append(home, &format!("start {}", std::process::id()));
    unclean.map(|pid| {
        format!("IMACS (pid {pid}) ended unexpectedly last time; see {}", imacs_core::paths::display(&log_path(home)))
    })
}

pub fn exited(home: &Path) {
    append(home, &format!("exit {}", std::process::id()));
}

/// Logs panics with their thread and backtrace, then lets the default hook print them.
pub fn install(home: &Path) {
    let home = home.to_path_buf();
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        append(&home, &format!("panic {} thread={thread}: {info}\n{backtrace}", std::process::id()));
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_without_an_exit_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(started(dir.path()), None, "a first run has nothing to report");
        exited(dir.path());
        assert_eq!(started(dir.path()), None, "a clean exit is not reported");
        // This start has no exit: the next start reports it.
        let notice = started(dir.path()).unwrap();
        assert!(notice.contains(&std::process::id().to_string()) && notice.contains("crash.log"), "{notice}");
        exited(dir.path());
        assert_eq!(started(dir.path()), None);
    }

    #[test]
    fn panics_are_logged_with_their_thread() {
        // The hook is process-wide, so it is installed in a child process of its own.
        if let Some(home) = std::env::var_os("IMACS_CRASH_TEST_HOME") {
            install(Path::new(&home));
            let worker = std::thread::Builder::new().name("worker".into()).spawn(|| panic!("boom")).unwrap();
            assert!(worker.join().is_err());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let output = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "crash::tests::panics_are_logged_with_their_thread", "--nocapture"])
                .env("IMACS_CRASH_TEST_HOME", dir.path()),
        );
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let log = std::fs::read_to_string(dir.path().join("crash.log")).unwrap();
        assert!(log.contains("thread=worker: ") && log.contains("boom"), "{log}");
    }

    #[test]
    fn the_log_is_trimmed_to_its_newest_half() {
        let dir = tempfile::tempdir().unwrap();
        let line = "x".repeat(1024);
        for _ in 0..300 {
            append(dir.path(), &line);
        }
        let size = std::fs::metadata(dir.path().join("crash.log")).unwrap().len();
        assert!(size <= MAX_BYTES + 2048, "{size}");
        let text = std::fs::read_to_string(dir.path().join("crash.log")).unwrap();
        assert!(text.lines().all(|l| l.ends_with(&line)), "trimming must keep whole lines");
    }
}
