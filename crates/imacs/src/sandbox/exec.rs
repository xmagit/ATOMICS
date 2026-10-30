//! `imacs_exec`: the chief's checks of a sandboxed session's work. Builds and tests run the agent's
//! code, so they run in the session's sandbox rather than on this machine, and wait for the same
//! test and QA slots as `imacs queue`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use imacs_core::governor::SlotClass;
use imacs_core::isolation;
use serde::Serialize;

use super::{capture, sbx_command, wait};
use crate::governor::Governor;

/// How long a command may run unless the chief says otherwise, and the most it may ask for.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const MAX_TIMEOUT_SECS: u64 = 3600;
/// How much of the end of each stream comes back: where test failures and errors are.
const MAX_OUTPUT: u64 = 40_000;
/// Past its timeout, how long sbx gets to start a stopped sandbox and end the command in the VM
/// before it is killed here.
const GRACE: Duration = Duration::from_secs(30);

/// The timeout a request asks for, within 1 s and [`MAX_TIMEOUT_SECS`].
pub fn timeout(secs: Option<u64>) -> Duration {
    Duration::from_secs(secs.unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(1, MAX_TIMEOUT_SECS))
}

/// One command for a session's sandbox.
#[derive(Debug, Clone)]
pub struct Exec {
    pub sbx: String,
    pub sandbox: String,
    /// The session's worktree, at the path its sandbox mounts it.
    pub workdir: PathBuf,
    pub command: String,
    /// How long the command may run, and how long it may wait for a slot first.
    pub timeout: Duration,
    pub class: SlotClass,
    /// The session's terminal process, so its tile shows the slot.
    pub ancestors: Vec<u32>,
}

/// How a command went. Each stream is its last [`MAX_OUTPUT`] bytes; `*_bytes` is its whole size.
#[derive(Debug, Serialize)]
pub struct Executed {
    /// The command's exit code, 128 + the signal that ended it, or none when sbx had to be killed.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub class: SlotClass,
    pub waited_secs: f64,
    pub secs: f64,
    pub stdout: String,
    pub stderr: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}

impl Exec {
    /// Waits for a slot of the command's class, at most its timeout, then runs it there. Gives up
    /// waiting once `hung_up` says the caller left.
    pub fn run(&self, governor: &Governor, hung_up: impl Fn() -> bool) -> Result<Executed> {
        let started = Instant::now();
        let class = self.class.name();
        let slot = governor.acquire(self.class, label(&self.command), self.ancestors.clone(), |position| {
            if hung_up() {
                bail!("the caller left while waiting for a {class} slot");
            }
            if started.elapsed() >= self.timeout {
                let ahead = position.map_or(0, |p| p.saturating_sub(1));
                bail!(
                    "no {class} slot came free within {} s ({ahead} ahead in line); try again later",
                    self.timeout.as_secs()
                );
            }
            Ok(())
        })?;
        let waited = started.elapsed();
        let result = self.execute();
        drop(slot);
        Ok(Executed { waited_secs: tenths(waited), ..result? })
    }

    fn execute(&self) -> Result<Executed> {
        let secs = self.timeout.as_secs().max(1);
        let args = isolation::exec_args(&self.sandbox, &self.workdir, &self.command, secs);
        let (mut stdout, mut stderr) = (capture()?, capture()?);
        let started = Instant::now();
        let mut child = sbx_command(&self.sbx, &args, false, &stdout, &stderr)?
            .spawn()
            .with_context(|| format!("running {}", self.sbx))?;
        let status = wait(&mut child, started + Duration::from_secs(secs) + GRACE, None)?;
        let elapsed = started.elapsed();
        let exit_code = status.map(|status| {
            use std::os::unix::process::ExitStatusExt;
            status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
        });
        // coreutils' timeout exits 124 when it stops the command, 137 when it had to kill it.
        let timed_out =
            status.is_none() || (matches!(exit_code, Some(124 | 137)) && elapsed >= Duration::from_secs(secs));
        let (stdout, stdout_bytes) = tail(&mut stdout)?;
        let (stderr, stderr_bytes) = tail(&mut stderr)?;
        Ok(Executed {
            exit_code,
            timed_out,
            class: self.class,
            waited_secs: 0.0,
            secs: tenths(elapsed),
            stdout,
            stderr,
            stdout_bytes,
            stderr_bytes,
        })
    }
}

/// What the slot shows as running: the command without the values of `NAME=value` words, which
/// may be secrets.
fn label(command: &str) -> String {
    let words = command.split_whitespace().map(|word| match word.split_once('=') {
        Some((name, _))
            if name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            format!("{name}=…")
        }
        _ => word.to_string(),
    });
    format!("imacs_exec: {}", words.collect::<Vec<_>>().join(" "))
}

/// The end of a captured stream, from a whole character, and the stream's size.
fn tail(file: &mut File) -> Result<(String, u64)> {
    let size = file.seek(SeekFrom::End(0))?;
    let start = size.saturating_sub(MAX_OUTPUT);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if start == 0 {
        return Ok((String::from_utf8_lossy(&bytes).into_owned(), size));
    }
    let partial = bytes.iter().take(3).take_while(|byte| *byte & 0xC0 == 0x80).count();
    let text = String::from_utf8_lossy(&bytes[partial..]);
    Ok((format!("… (first {} bytes left out)\n{text}", start + partial as u64), size))
}

fn tenths(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::fake;
    use imacs_core::governor::ResourceConfig;
    use std::path::Path;
    use std::sync::Arc;

    /// A fake sbx that runs exec commands, with sandbox `imacs-s1`, and a worktree beside it.
    fn setup() -> (crate::test_support::TempDir, String, PathBuf) {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).display().to_string();
        std::fs::write(dir.path().join("exec-runs"), "").unwrap();
        std::fs::create_dir(dir.path().join("sandboxes")).unwrap();
        std::fs::write(dir.path().join("sandboxes/imacs-s1"), "{}").unwrap();
        std::fs::create_dir(dir.path().join("work tree")).unwrap();
        let worktree = dir.path().join("work tree").canonicalize().unwrap();
        (dir, sbx, worktree)
    }

    fn job(sbx: &str, workdir: &Path, command: &str, secs: u64) -> Exec {
        Exec {
            sbx: sbx.into(),
            sandbox: "imacs-s1".into(),
            workdir: workdir.to_path_buf(),
            command: command.into(),
            timeout: Duration::from_secs(secs),
            class: SlotClass::Test,
            ancestors: vec![4242],
        }
    }

    fn governor(test_slots: usize) -> Arc<Governor> {
        Arc::new(Governor::new(ResourceConfig { test_slots: Some(test_slots), ..Default::default() }, None))
    }

    #[test]
    fn runs_the_command_in_the_sandbox_worktree_with_its_status_and_both_streams() {
        let (dir, sbx, worktree) = setup();
        let command = "pwd; echo broken >&2; exit 3";
        let done = job(&sbx, &worktree, command, 600).run(&governor(1), || false).unwrap();
        assert_eq!(fake::calls_to(dir.path(), &["exec"]), [isolation::exec_args("imacs-s1", &worktree, command, 600)]);
        assert_eq!((done.exit_code, done.timed_out), (Some(3), false));
        assert_eq!(done.stdout.trim(), worktree.display().to_string());
        assert_eq!((done.stderr.as_str(), done.stderr_bytes), ("broken\n", 7));
        assert_eq!(done.class, SlotClass::Test);

        // sbx's own failures come back as they are, such as a sandbox that is gone.
        let gone = Exec { sandbox: "imacs-gone".into(), ..job(&sbx, &worktree, "true", 600) };
        let done = gone.run(&governor(1), || false).unwrap();
        assert_eq!(done.exit_code, Some(125));
        assert!(done.stderr.contains("imacs-gone"), "{done:?}");
    }

    #[test]
    fn a_command_past_its_timeout_is_stopped_in_the_vm() {
        let (_dir, sbx, worktree) = setup();
        let started = Instant::now();
        let done = job(&sbx, &worktree, "echo started; exec sleep 30", 1).run(&governor(1), || false).unwrap();
        assert!(
            started.elapsed() < crate::test_support::scaled_timeout(Duration::from_secs(10)),
            "it must not wait for the command"
        );
        assert_eq!((done.exit_code, done.timed_out), (Some(124), true));
        assert_eq!(done.stdout, "started\n");
    }

    #[test]
    fn output_keeps_the_end_of_each_stream_and_its_whole_size() {
        let (_dir, sbx, worktree) = setup();
        let command = "i=0; while [ $i -lt 5000 ]; do echo \"line $i of the test output\"; i=$((i+1)); done; \
                       printf 'é%.0s' $(seq 1 30000) >&2";
        let done = job(&sbx, &worktree, command, 600).run(&governor(1), || false).unwrap();
        assert_eq!(done.exit_code, Some(0));
        assert!(done.stdout_bytes > 100_000, "{}", done.stdout_bytes);
        assert!(done.stdout.len() < MAX_OUTPUT as usize + 100);
        assert!(done.stdout.starts_with("… (first "), "{}", &done.stdout[..60]);
        assert!(done.stdout.ends_with("line 4999 of the test output\n"));
        // 60000 bytes of two-byte characters, cut at a whole one.
        assert_eq!(done.stderr_bytes, 60_000);
        let kept = done.stderr.split_once('\n').unwrap().1;
        assert!(kept.chars().all(|c| c == 'é') && kept.len() <= MAX_OUTPUT as usize, "{}", kept.len());
    }

    #[test]
    fn commands_wait_for_a_test_slot_and_hold_it_while_they_run() {
        let (dir, sbx, worktree) = setup();
        let governor = governor(1);
        // A full test run holds the only test slot.
        let held = governor.acquire(SlotClass::Test, "cargo test".into(), vec![], |_| Ok(())).unwrap();
        let check = {
            let (governor, job) = (governor.clone(), job(&sbx, &worktree, "TOKEN=secret cargo test -p core", 600));
            std::thread::spawn(move || job.run(&governor, || false))
        };
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
        while governor.snapshot().1.is_empty() {
            assert!(Instant::now() < deadline, "the check must queue for the slot");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (running, waiting) = governor.snapshot();
        assert_eq!((running.len(), waiting[0].class), (1, SlotClass::Test));
        assert_eq!(waiting[0].label, "imacs_exec: TOKEN=… cargo test -p core", "no environment values");
        assert_eq!(waiting[0].ancestors, [4242], "the session's tile shows the slot");
        std::thread::sleep(Duration::from_millis(300));
        assert!(fake::calls_to(dir.path(), &["exec"]).is_empty(), "nothing runs before the slot is free");
        drop(held);
        let done = check.join().unwrap().unwrap();
        assert!(done.waited_secs >= 0.3, "{done:?}");
        assert_eq!(fake::calls_to(dir.path(), &["exec"]).len(), 1);
        assert_eq!(governor.snapshot(), (vec![], vec![]), "the slot is free again");

        // While a check runs it holds the slot, and QA checks use the QA slots.
        let slow = Exec { class: SlotClass::Qa, ..job(&sbx, &worktree, "sleep 1", 600) };
        let check = {
            let governor = governor.clone();
            std::thread::spawn(move || slow.run(&governor, || false))
        };
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
        while governor.snapshot().0.is_empty() {
            assert!(Instant::now() < deadline, "the check must take a slot");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(governor.snapshot().0[0].class, SlotClass::Qa);
        assert_eq!(check.join().unwrap().unwrap().class, SlotClass::Qa);
        assert!(governor.snapshot().0.is_empty());
    }

    #[test]
    fn a_check_gives_up_its_place_when_no_slot_comes_free_or_the_caller_leaves() {
        let (dir, sbx, worktree) = setup();
        let governor = governor(1);
        let _held = governor.acquire(SlotClass::Test, "cargo test".into(), vec![], |_| Ok(())).unwrap();
        let error = job(&sbx, &worktree, "cargo test", 1).run(&governor, || false).unwrap_err();
        assert!(error.to_string().contains("no test slot came free within 1 s (0 ahead in line)"), "{error:#}");
        let error = job(&sbx, &worktree, "cargo test", 600).run(&governor, || true).unwrap_err();
        assert!(error.to_string().contains("left"), "{error:#}");
        assert!(fake::calls_to(dir.path(), &["exec"]).is_empty());
        assert!(governor.snapshot().1.is_empty(), "neither stays in line");
    }

    #[test]
    fn labels_leave_out_assignment_values_only() {
        assert_eq!(
            label("env RUST_LOG=debug _X=1 cargo test -- --test-threads=2 --format=x 1=2"),
            "imacs_exec: env RUST_LOG=… _X=… cargo test -- --test-threads=2 --format=x 1=2"
        );
    }
}
