use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Unlike tempfile's best-effort Drop, test cleanup must report failures and verify removal.
pub struct TempDir(Option<tempfile::TempDir>);

/// In a test's child process, the folder its parent made for the child's temporary folders. The
/// parent removes and checks them with it, once the child and its process group are gone. A test's
/// folders usually drop as its closure returns, before the app it drives has shut down and while
/// the app's background work, such as a ticket job against a fake server, may still write to them.
pub const CHILD_FOLDERS: &str = "ATOMICS_TEST_CHILD_FOLDERS";

pub fn tempdir() -> io::Result<TempDir> {
    match std::env::var_os(CHILD_FOLDERS) {
        Some(parent) => tempfile::tempdir_in(parent),
        None => tempfile::tempdir(),
    }
    .map(|dir| TempDir(Some(dir)))
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
        if std::env::var_os(CHILD_FOLDERS).is_some_and(|parent| dir.path().starts_with(parent)) {
            let _ = dir.keep();
            return;
        }
        let path = dir.path().to_path_buf();
        let result = dir.close();
        if !std::thread::panicking() {
            result.unwrap_or_else(|err| {
                panic!("test left temporary resources at {}: {err}\n{}", path.display(), leftovers(&path))
            });
            assert!(!path.exists(), "test leaked {}\n{}", path.display(), leftovers(&path));
        }
    }
}

/// What is still in `path`, and which processes have it as their working directory or hold a file
/// in it (Linux `/proc` only): whatever wrote there after the test.
fn leftovers(path: &std::path::Path) -> String {
    let mut out = String::new();
    let mut folders = vec![path.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            out.push_str(&format!("  left: {}\n", entry.path().display()));
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                folders.push(entry.path());
            }
        }
    }
    for process in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let base = process.path();
        let fds = std::fs::read_dir(base.join("fd")).into_iter().flatten().flatten().map(|fd| fd.path());
        let holds = std::iter::once(base.join("cwd"))
            .chain(fds)
            .any(|link| std::fs::read_link(link).is_ok_and(|target| target.starts_with(path)));
        if holds {
            let command = std::fs::read(base.join("cmdline")).unwrap_or_default();
            let command = String::from_utf8_lossy(&command).replace('\0', " ");
            out.push_str(&format!("  held by pid {}: {command}\n", process.file_name().to_string_lossy()));
        }
    }
    out
}

/// Whether a read with a timeout just has nothing yet. Interrupted counts: a blocking socket read
/// with a timeout ends with EINTR rather than restarting, even with SA_RESTART.
#[allow(dead_code)] // Used by the WebSocket fixtures; this support file is shared by test crates.
pub fn nothing_yet(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted)
}

/// Resolve a fixture's helper on the development shell's PATH, including Nix profiles.
#[allow(dead_code)] // Shared by unit and integration test crates.
pub fn program(name: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(&std::env::var_os("PATH").expect("test helpers need PATH"))
        .map(|dir| dir.join(name))
        .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .unwrap_or_else(|| panic!("test helper {name} is missing from PATH; use scripts/check.sh"))
        .to_string_lossy()
        .into_owned()
}

/// A PATH containing only the named helpers, so installed agents cannot enter a fixture.
#[allow(dead_code)]
pub fn link_tools(dir: &std::path::Path, names: &[&str]) {
    std::fs::create_dir_all(dir).unwrap();
    for name in names {
        std::os::unix::fs::symlink(program(name), dir.join(name)).unwrap();
    }
}

/// Put fixture overrides before the real helper PATH, without assuming /usr/bin or /bin.
#[allow(dead_code)]
pub fn path_with(dir: &std::path::Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(dir.to_path_buf())
            .chain(std::env::split_paths(&std::env::var_os("PATH").expect("test helpers need PATH"))),
    )
    .unwrap()
}

/// A login shell that preserves the fixture's PATH and reads no user or system startup files.
#[allow(dead_code)]
pub fn login_shell(path: &std::path::Path) {
    let bash = program("bash").replace('\'', "'\\''");
    write_program(
        path,
        format!(
            "#!/bin/sh\nunset BASH_ENV ENV\n[ \"${{1:-}}\" != -l ] || shift\nexec '{bash}' --noprofile --norc \"$@\"\n"
        ),
    );
}

/// Writes a program that a test will run, or that a program it runs will run, such as a git hook.
/// Make such a file only here or with [`copy_program`], never with `std::fs::write` or
/// `std::fs::copy`: whatever a parallel test forks while the file is open for writing holds a copy
/// of that descriptor until it execs, and while one exists the kernel refuses to run the file
/// (ETXTBSY, "Text file busy"). So a child writes it: only that child ever has the descriptor, and
/// it has exited when this returns.
pub fn write_program(path: impl AsRef<std::path::Path>, text: impl AsRef<[u8]>) {
    let path = path.as_ref();
    let mut writer = Command::new(program("sh"))
        .args(["-c", "exec cat > \"$0\""])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("start the child that writes a test's program");
    let written = writer.stdin.take().unwrap().write_all(text.as_ref());
    let status = writer.wait().unwrap();
    assert!(status.success() && written.is_ok(), "writing {}: {status}, {written:?}", path.display());
    make_executable(path);
}

/// [`write_program`], for a file that already exists, such as a fixture.
pub fn copy_program(from: impl AsRef<std::path::Path>, to: impl AsRef<std::path::Path>) {
    let to = to.as_ref();
    let status = Command::new("cp")
        .arg("--")
        .args([from.as_ref(), to])
        .stdin(Stdio::null())
        .status()
        .expect("start the child that copies a test's program");
    assert!(status.success(), "copying to {}: {status}", to.display());
    make_executable(to);
}

/// Changing the mode opens nothing for writing, so this is safe in this process.
fn make_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        std::env::var("ATOMICS_TEST_TIMEOUT_SCALE").ok().as_deref(),
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

    #[test]
    fn programs_are_written_and_copied_whole_and_run_at_once() {
        let dir = tempdir().unwrap();
        let run = |program: &std::path::Path| {
            let output = Command::new(program).stdin(Stdio::null()).output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        // More than a pipe holds, so the child must be draining while this writes.
        let large = format!("#!/bin/sh\n#{}\necho first\n", "x".repeat(256 * 1024));
        let program = dir.path().join("a program");
        write_program(&program, &large);
        assert_eq!(std::fs::read_to_string(&program).unwrap(), large);
        assert_eq!(run(&program), b"first\n");
        write_program(&program, "#!/bin/sh\necho second\n");
        assert_eq!(run(&program), b"second\n", "an existing program is replaced");
        let copy = dir.path().join("a copy");
        copy_program(&program, &copy);
        assert_eq!(run(&copy), b"second\n");
    }

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
        if let Some(ready) = std::env::var_os("ATOMICS_TIMEOUT_TEST_READY") {
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
                .env("ATOMICS_TIMEOUT_TEST_READY", &ready),
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
        if std::env::var_os("ATOMICS_CAPTURE_TEST_CHILD").is_some() {
            println!("captured stdout");
            eprintln!("captured stderr");
            return;
        }
        let output = output(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "test_support::tests::captures_successful_child_output", "--nocapture"])
                .env("ATOMICS_CAPTURE_TEST_CHILD", "1"),
        );
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("captured stdout"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("captured stderr"));
    }
}
