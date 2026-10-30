use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Unlike tempfile's best-effort Drop, test cleanup must report failures and verify removal.
pub struct TempDir(Option<tempfile::TempDir>);

pub fn tempdir() -> io::Result<TempDir> {
    tempfile::tempdir().map(|dir| TempDir(Some(dir)))
}

#[allow(dead_code)] // Used by the Linux reflink test; this support file is shared by test crates.
pub fn tempdir_in(parent: impl AsRef<std::path::Path>) -> io::Result<TempDir> {
    tempfile::tempdir_in(parent).map(|dir| TempDir(Some(dir)))
}

impl TempDir {
    pub fn path(&self) -> &std::path::Path {
        self.0.as_ref().unwrap().path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let dir = self.0.take().unwrap();
        let path = dir.path().to_path_buf();
        let result = dir.close();
        if !std::thread::panicking() {
            result.unwrap_or_else(|err| panic!("test left temporary resources at {}: {err}", path.display()));
            assert!(!path.exists(), "test leaked {}", path.display());
        }
    }
}

pub const CHILD_TIMEOUT: Duration = Duration::from_secs(180);

/// A fixture's hang guard, not the behavior under test. The explicit scale defaults to one;
/// a one-minute load average above the available core count adds a factor of four.
pub fn scaled_timeout(base: Duration) -> Duration {
    let mut load = 0.0;
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let samples = unsafe { libc::getloadavg(&mut load, 1) };
    timeout_for_load(
        base,
        std::env::var("IMACS_TEST_TIMEOUT_SCALE").ok().as_deref(),
        (samples == 1).then_some(load),
        cores,
    )
}

fn timeout_for_load(base: Duration, configured: Option<&str>, load: Option<f64>, cores: usize) -> Duration {
    let scale = configured
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0);
    let busy = load.is_some_and(|load| load > cores.max(1) as f64);
    Duration::try_from_secs_f64(base.as_secs_f64() * scale * if busy { 4.0 } else { 1.0 }).unwrap_or(base)
}

/// Capture to files so an inherited pipe writer cannot keep a finished test waiting for EOF.
pub struct TestChild {
    pub child: Child,
    stdout: File,
    stderr: File,
    deadline: Instant,
    timeout: Duration,
}

impl TestChild {
    pub fn spawn(command: &mut Command, timeout: Duration) -> io::Result<Self> {
        let timeout = scaled_timeout(timeout);
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        use std::os::unix::process::CommandExt;
        let child = command.process_group(0).stdout(stdout.try_clone()?).stderr(stderr.try_clone()?).spawn()?;
        Ok(Self { child, stdout, stderr, deadline: Instant::now() + timeout, timeout })
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn check_timeout(&mut self) -> io::Result<()> {
        if !self.remaining().is_zero() {
            return Ok(());
        }
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (stdout, stderr) = self.logs()?;
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "child {} timed out after {:?}\nstdout: {}\nstderr: {}",
                self.child.id(),
                self.timeout,
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ),
        ))
    }

    pub fn wait_output(&mut self) -> io::Result<Output> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                let (stdout, stderr) = self.logs()?;
                return Ok(Output { status, stdout, stderr });
            }
            self.check_timeout()?;
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn logs(&mut self) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut captured = Vec::new();
        for file in [&mut self.stdout, &mut self.stderr] {
            file.seek(SeekFrom::Start(0))?;
            let length = file.metadata()?.len();
            let mut bytes = Vec::new();
            file.take(length).read_to_end(&mut bytes)?;
            captured.push(bytes);
        }
        Ok((captured.remove(0), captured.remove(0)))
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        // Descendants can retain files/sockets even after their direct parent has exited.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn output(command: &mut Command) -> Output {
    let mut child = TestChild::spawn(command.stdin(Stdio::null()), CHILD_TIMEOUT).expect("spawn test child");
    child.wait_output().expect("test child must finish within its timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn fixture_waits_scale_with_configuration_and_one_minute_host_load() {
        let base = Duration::from_millis(500);
        assert_eq!(timeout_for_load(base, None, Some(8.0), 8), base);
        assert_eq!(timeout_for_load(base, None, Some(8.1), 8), base * 4);
        assert_eq!(timeout_for_load(base, Some("2"), Some(8.1), 8), base * 8);
        assert_eq!(timeout_for_load(base, Some("2.5"), None, 8), Duration::from_millis(1250));
        for invalid in ["", "bad", "0", "-1", "NaN", "inf"] {
            assert_eq!(timeout_for_load(base, Some(invalid), Some(0.0), 8), base);
        }
    }

    #[test]
    fn timeout_kills_and_reaps_child_with_diagnostics() {
        if let Some(ready) = std::env::var_os("IMACS_TIMEOUT_TEST_READY") {
            println!("child stdout before blocking");
            eprintln!("child stderr before blocking");
            io::stdout().flush().unwrap();
            io::stderr().flush().unwrap();
            std::fs::write(ready, "ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        let dir = tempdir().unwrap();
        let ready = dir.path().join("ready");
        let mut child = TestChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "test_support::tests::timeout_kills_and_reaps_child_with_diagnostics", "--nocapture"])
                .env("IMACS_TIMEOUT_TEST_READY", &ready),
            CHILD_TIMEOUT,
        )
        .unwrap();
        while !ready.exists() {
            child.check_timeout().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        child.deadline = Instant::now();
        let error = child.wait_output().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("child stdout before blocking"));
        assert!(error.to_string().contains("child stderr before blocking"));
        assert!(!child.child.try_wait().unwrap().unwrap().success());
    }

    #[test]
    fn captures_successful_child_output() {
        if std::env::var_os("IMACS_CAPTURE_TEST_CHILD").is_some() {
            println!("captured stdout");
            eprintln!("captured stderr");
            return;
        }
        let output = output(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "test_support::tests::captures_successful_child_output", "--nocapture"])
                .env("IMACS_CAPTURE_TEST_CHILD", "1"),
        );
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("captured stdout"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("captured stderr"));
    }
}
