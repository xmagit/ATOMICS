//! The machine-wide limits behind every agent IMACS starts: a Cargo jobserver,
//! toolchain launch budgets, and slots for whole test runs and QA instances.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use imacs_core::governor::{ResourceConfig, SlotClass, SlotQueue, Ticket, Toolchain};
use serde_json::{Value, json};

/// Detect from the worktree root even when an agent starts in a subdirectory.
pub fn toolchain_env(cwd: &Path, config: &ResourceConfig, cores: usize) -> Vec<(String, String)> {
    let root = imacs_core::git::git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|root| std::path::PathBuf::from(root.trim()))
        .unwrap_or_else(|_| cwd.to_path_buf());
    Toolchain::detect(&root).into_iter().flat_map(|toolchain| config.env(toolchain, cores)).collect()
}

/// A GNU make jobserver in a named pipe. cargo takes a token per job, so agents that inherit
/// it share `tokens` jobs between them, however many run.
pub struct Jobserver {
    path: PathBuf,
    tokens: usize,
    /// Held open so the pipe and its tokens live as long as IMACS.
    pipe: File,
}

impl Jobserver {
    pub fn create(path: &Path, tokens: usize) -> Result<Jobserver> {
        if let Some(parent) = path.parent() {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
            // Someone else's folder, or a link to one, must not hold our tokens.
            for dir in parent.ancestors().take(2) {
                use std::os::unix::fs::MetadataExt;
                let meta = std::fs::symlink_metadata(dir)?;
                // SAFETY: getuid has no preconditions.
                if meta.file_type().is_symlink() || meta.uid() != unsafe { libc::getuid() } {
                    bail!("{} is not a private folder of this user", dir.display());
                }
            }
            // Tighten our own pool folder, never chmod an existing ancestor (which
            // could be XDG_RUNTIME_DIR or the caller's temporary directory).
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let _ = std::fs::remove_file(path);
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: c_path is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error()).context(format!("creating {}", path.display()));
        }
        let mut pipe = OpenOptions::new().read(true).write(true).open(path)?;
        pipe.write_all(&vec![b'+'; tokens])?;
        Ok(Jobserver { path: path.to_path_buf(), tokens, pipe })
    }

    /// A private pool specific to this home: Linux's XDG runtime folder when
    /// available, otherwise the established user-temporary location.
    pub fn path_for(home: &Path) -> PathBuf {
        if let Some(runtime) = imacs_core::paths::runtime_dir_for(home) {
            return runtime.join("jobserver/jobserver.fifo");
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir()
            .join(format!("imacs-{uid}"))
            .join(imacs_core::paths::runtime_home_hash(home))
            .join("jobserver.fifo")
    }

    pub fn tokens(&self) -> usize {
        self.tokens
    }

    /// Empties the pipe and puts exactly `tokens` back, so leaked tokens return and extra
    /// ones go. Call it only while nothing holds a token. Returns how many it found.
    fn reset(&self) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        let fd = self.pipe.as_raw_fd();
        // SAFETY: fcntl on our own open descriptor; only O_NONBLOCK is added.
        unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        let (mut found, mut buf) = (0, [0u8; 256]);
        loop {
            match (&self.pipe).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => found += n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            }
        }
        (&self.pipe).write_all(&vec![b'+'; self.tokens])?;
        Ok(found)
    }

    /// Only CARGO_MAKEFLAGS: plain `make` misreads a fifo jobserver before GNU make 4.4.
    pub fn env(&self) -> [(String, String); 1] {
        [("CARGO_MAKEFLAGS".into(), format!("-j --jobserver-auth=fifo:{}", self.path.display()))]
    }

    /// A job killed while it held a token never gives it back. When no compiler runs anywhere,
    /// every token belongs in the pipe, so the pool is reset to its full size.
    pub fn refill_when_idle(self: &Arc<Self>) {
        let jobserver = Arc::downgrade(self);
        let _ = std::thread::Builder::new().name("imacs-jobserver".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(10));
                let Some(jobserver) = jobserver.upgrade() else { return };
                if !compilers_running() {
                    let _ = jobserver.reset();
                }
            }
        });
    }
}

impl Drop for Jobserver {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Whether a process that may hold a token runs. cargo itself only holds tokens while one of
/// its compilers or build scripts runs, and a long `cargo run` must not block the refill.
fn compilers_running() -> bool {
    let Ok(output) = std::process::Command::new("ps").args(["-axo", "args="]).output() else { return true };
    if !output.status.success() {
        return true;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(holds_tokens)
}

/// From a process's command line: compilers and build scripts, and sccache compiling for cargo
/// (with arguments; the cache server itself runs without any).
fn holds_tokens(args: &str) -> bool {
    let mut words = args.split_whitespace();
    let Some(program) = words.next().map(|p| p.rsplit('/').next().unwrap_or(p)) else { return false };
    matches!(program, "rustc" | "clippy-driver" | "rustdoc" | "cc" | "c++" | "clang" | "clang++" | "ld" | "ld64")
        || program.starts_with("build-script-")
        || (program == "sccache" && words.next().is_some())
}

pub struct Governor {
    queue: Mutex<SlotQueue>,
    restarting: AtomicBool,
    changed: Condvar,
    config: Mutex<ResourceConfig>,
    repaint: Option<egui::Context>,
    pub cores: usize,
}

impl Governor {
    pub fn new(config: ResourceConfig, repaint: Option<egui::Context>) -> Governor {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Governor {
            queue: Mutex::default(),
            restarting: AtomicBool::new(false),
            changed: Condvar::new(),
            config: Mutex::new(config),
            repaint,
            cores,
        }
    }

    pub fn set_config(&self, config: ResourceConfig) {
        *self.config.lock().unwrap() = config;
        self.grant();
    }

    pub fn slots(&self, class: SlotClass) -> usize {
        self.config.lock().unwrap().slots(class, self.cores)
    }

    fn grant(&self) {
        let config = self.config.lock().unwrap().clone();
        self.queue.lock().unwrap().grant(|class| config.slots(class, self.cores));
        self.changed.notify_all();
        if let Some(ctx) = &self.repaint {
            ctx.request_repaint();
        }
    }

    pub fn snapshot(&self) -> (Vec<Ticket>, Vec<Ticket>) {
        let queue = self.queue.lock().unwrap();
        (queue.running().to_vec(), queue.waiting().cloned().collect())
    }

    /// Freeze queue admission under the same lock used to enqueue work.
    pub fn begin_restart(&self, force: bool) -> Result<()> {
        let queue = self.queue.lock().unwrap();
        if !force && (!queue.running().is_empty() || queue.waiting().next().is_some()) {
            bail!("queue slots are held or waiting; wait for queued work to finish");
        }
        self.restarting.store(true, Ordering::Release);
        Ok(())
    }

    pub fn cancel_restart(&self) {
        self.restarting.store(false, Ordering::Release);
    }

    #[cfg(test)]
    pub fn restarting_for_test(&self) -> bool {
        self.restarting.load(Ordering::Acquire)
    }

    /// Serves one `imacs queue` connection: waits for a slot, telling the client its place,
    /// then holds the slot until the client hangs up (its command ended, or it died).
    pub fn serve(&self, stream: &UnixStream, request: &Value) -> Result<()> {
        let class = request.get("class").and_then(Value::as_str).and_then(SlotClass::parse).unwrap_or(SlotClass::Test);
        let label = request.get("label").and_then(Value::as_str).unwrap_or_default().to_string();
        let ancestors = request
            .get("ancestors")
            .and_then(Value::as_array)
            .map(|pids| pids.iter().filter_map(Value::as_u64).map(|pid| pid as u32).collect())
            .unwrap_or_default();
        let mut told = None;
        let _slot = self.acquire(class, label, ancestors, |position| {
            if position != told {
                send(stream, &json!({ "position": position }))?;
                told = position;
            }
            if hung_up(stream)? {
                bail!("client left the queue");
            }
            Ok(())
        })?;
        send(stream, &json!({ "granted": true }))?;
        stream.set_read_timeout(None)?;
        let _ = std::io::copy(&mut &*stream, &mut std::io::sink());
        Ok(())
    }

    /// Waits for a slot of `class`, which is held until the returned [`Slot`] drops. `waiting` hears
    /// the place in line about twice a second; an error from it gives the place up.
    pub fn acquire(
        &self,
        class: SlotClass,
        label: String,
        ancestors: Vec<u32>,
        mut waiting: impl FnMut(Option<usize>) -> Result<()>,
    ) -> Result<Slot<'_>> {
        let id = {
            let mut queue = self.queue.lock().unwrap();
            if self.restarting.load(Ordering::Acquire) {
                bail!("IMACS is restarting");
            }
            queue.enqueue(class, label, ancestors)
        };
        let slot = Slot { governor: self, id };
        self.grant();
        loop {
            let position = {
                let queue = self.queue.lock().unwrap();
                if queue.is_running(id) {
                    return Ok(slot);
                }
                queue.position(id)
            };
            waiting(position)?;
            let queue = self.queue.lock().unwrap();
            drop(self.changed.wait_timeout(queue, Duration::from_millis(500)).unwrap());
        }
    }
}

/// A place in a slot queue, waiting or running, given up when dropped.
pub struct Slot<'a> {
    governor: &'a Governor,
    id: u64,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.governor.queue.lock().unwrap().release(self.id);
        self.governor.grant();
    }
}

fn send(stream: &UnixStream, value: &Value) -> Result<()> {
    let mut writer = stream;
    writer.write_all(format!("{value}\n").as_bytes())?;
    Ok(())
}

/// Whether the client closed its end while it waited.
pub(crate) fn hung_up(stream: &UnixStream) -> Result<bool> {
    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
    let mut byte = [0u8; 1];
    Ok(match (&mut &*stream).read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(err) if matches!(err.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => false,
        Err(_) => true,
    })
}

/// Exit code for "cannot wait for a slot here": the command did not run.
const NO_SLOT_HERE: i32 = 75;
/// How long a client keeps reconnecting while IMACS restarts before it runs anyway.
const RECONNECT_FOR: Duration = Duration::from_secs(120);

/// `imacs queue`: waits for a slot in the running IMACS, then runs the command and exits
/// with its status. Without a running IMACS the command runs right away.
pub fn run_queued(socket: &Path, class: SlotClass, command: &[String]) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("imacs queue: nothing to run");
        return 2;
    };
    let label = command.join(" ");
    let held = match hold_slot(socket, class, &label) {
        Ok(held) => held,
        Err(code) => return code,
    };
    let status = std::process::Command::new(program).args(args).status();
    drop(held);
    match status {
        Ok(status) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(1)
        }),
        Err(err) => {
            eprintln!("imacs queue: could not run {program}: {err}");
            127
        }
    }
}

/// The held slot's connection, or None to run without one (no IMACS). Err is an exit code.
fn hold_slot(socket: &Path, class: SlotClass, label: &str) -> Result<Option<UnixStream>, i32> {
    let mut deadline: Option<Instant> = None;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                // IMACS is back: a later restart gets the full reconnect time again.
                deadline = None;
                match wait_for_slot(stream, class, label) {
                    Ok(stream) => return Ok(Some(stream)),
                    Err(SlotError::Refused(reason)) => {
                        eprintln!("imacs queue: IMACS cannot queue ({reason}); running without a slot");
                        return Ok(None);
                    }
                    // IMACS went away while this waited: wait for it to come back and queue again.
                    Err(SlotError::Lost(err)) => eprintln!("imacs queue: {err:#}; reconnecting"),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "imacs queue: this sandbox blocks the IMACS socket ({err}); run this command outside \
                     the sandbox so it can wait for a {} slot",
                    class.name()
                );
                return Err(NO_SLOT_HERE);
            }
            Err(_) if deadline.is_none() && !socket.exists() => return Ok(None),
            Err(err) if deadline.is_some_and(|d| Instant::now() > d) => {
                eprintln!("imacs queue: IMACS did not come back ({err}); running without a slot");
                return Ok(None);
            }
            Err(_) => {}
        }
        deadline.get_or_insert_with(|| Instant::now() + RECONNECT_FOR);
        std::thread::sleep(Duration::from_secs(2));
    }
}

enum SlotError {
    /// IMACS answered but will not queue (an older build, or a bad request): final.
    Refused(String),
    /// The connection broke: IMACS may be restarting.
    Lost(anyhow::Error),
}

fn wait_for_slot(stream: UnixStream, class: SlotClass, label: &str) -> Result<UnixStream, SlotError> {
    let lost = |err: anyhow::Error| SlotError::Lost(err);
    let ancestors = crate::hook_client::ancestors();
    let request = json!({ "cmd": "queue", "class": class.name(), "label": label, "ancestors": ancestors });
    send(&stream, &request).map_err(lost)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| lost(e.into()))?);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| lost(e.into()))? == 0 {
            return Err(lost(anyhow::anyhow!("IMACS closed the queue")));
        }
        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
            return Err(SlotError::Refused(format!("unreadable reply {:?}", line.trim())));
        };
        if reply.get("granted").is_some() {
            return Ok(stream);
        }
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(SlotError::Refused(error.to_string()));
        }
        if let Some(position) = reply.get("position").and_then(Value::as_u64) {
            eprintln!("imacs queue: waiting for a {} slot ({position} in line)", class.name());
        }
    }
}

/// One-minute load average, or None where the platform has none.
pub fn load_average() -> Option<f64> {
    let mut loads = [0f64; 3];
    // SAFETY: loads has room for the 3 values requested.
    (unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } >= 1).then_some(loads[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn restart_refuses_held_and_waiting_slots_and_force_freezes_admission() {
        let governor = Governor::new(ResourceConfig::default(), None);
        let id = governor.queue.lock().unwrap().enqueue(SlotClass::Test, "full test suite".into(), vec![]);
        assert!(governor.begin_restart(false).is_err(), "waiting work must not be lost");
        governor.grant();
        assert!(!governor.snapshot().0.is_empty());
        assert!(governor.begin_restart(false).is_err(), "held slots must not be lost");
        governor.begin_restart(true).unwrap();
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(governor.serve(&stream, &json!({"cmd":"queue"})).unwrap_err().to_string().contains("restarting"));
        governor.cancel_restart();
        governor.queue.lock().unwrap().release(id);
        governor.begin_restart(false).unwrap();
        assert!(governor.restarting_for_test());
    }

    #[test]
    fn toolchain_env_detects_sibling_projects_from_a_worktree_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git").args(["init", "-q"]).arg(dir.path()).status().unwrap();
        assert!(init.success());
        let nested = dir.path().join("scripts");
        std::fs::create_dir(&nested).unwrap();
        let config = ResourceConfig::default();
        assert!(toolchain_env(&nested, &config, 18).is_empty());
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/Example.csproj"), "").unwrap();
        assert_eq!(toolchain_env(&nested, &config, 18), config.env(Toolchain::Dotnet, 18));
        let disabled = ResourceConfig {
            dotnet: imacs_core::governor::DotnetConfig { enabled: false, ..Default::default() },
            ..Default::default()
        };
        assert!(toolchain_env(&nested, &disabled, 18).is_empty());
    }

    #[test]
    fn jobserver_pipe_holds_its_tokens_and_resets_leaks() {
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let jobserver = Jobserver::create(&dir.path().join("pool/jobs"), 3).unwrap();
        let metadata = std::fs::metadata(dir.path().join("pool/jobs")).unwrap();
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o750,
            "ancestors must retain their permissions"
        );
        let mut client = OpenOptions::new().read(true).write(true).open(dir.path().join("pool/jobs")).unwrap();
        // A client takes two tokens and dies without returning them.
        let mut taken = [0u8; 2];
        client.read_exact(&mut taken).unwrap();
        assert_eq!(&taken, b"++");
        assert_eq!(jobserver.reset().unwrap(), 1, "one token was left in the pipe");
        assert_eq!(jobserver.reset().unwrap(), 3, "the reset put all three back");
        let mut all = [0u8; 3];
        client.read_exact(&mut all).unwrap();
        assert_eq!(jobserver.tokens(), 3);
        let [(name, flags)] = jobserver.env();
        assert_eq!(name, "CARGO_MAKEFLAGS");
        assert_eq!(flags, format!("-j --jobserver-auth=fifo:{}", dir.path().join("pool/jobs").display()));
        assert_eq!(std::fs::metadata(dir.path().join("pool")).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn pools_are_private_to_the_user_and_the_home_and_removed_with_it() {
        let a = Jobserver::path_for(Path::new("/Users/x/.imacs"));
        let b = Jobserver::path_for(Path::new("/private/tmp/qa/state"));
        assert_ne!(a, b);
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let expected = match imacs_core::paths::runtime_dir_for(Path::new("/Users/x/.imacs")) {
            Some(runtime) => runtime.join("jobserver/jobserver.fifo"),
            None => std::env::temp_dir().join(format!("imacs-{uid}/51d177b92c46bedb/jobserver.fifo")),
        };
        assert_eq!(a, expected);
        assert_eq!(a, Jobserver::path_for(Path::new("/Users/x/.imacs")));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool/jobs");
        drop(Jobserver::create(&path, 1).unwrap());
        assert!(!path.exists() && !dir.path().join("pool").exists(), "the pool and its folder go with it");
    }

    #[test]
    fn a_pool_folder_owned_through_a_link_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        assert!(Jobserver::create(&dir.path().join("link/hash/jobs"), 1).is_err());
    }

    #[test]
    fn only_compilers_and_build_scripts_hold_tokens() {
        for args in [
            "rustc --crate-name x",
            "/opt/homebrew/bin/clippy-driver -",
            "cc -c a.c",
            "/x/target/debug/build/ring-1/build-script-build",
            "/opt/homebrew/bin/sccache /opt/homebrew/bin/rustc --crate-name y",
        ] {
            assert!(holds_tokens(args), "{args}");
        }
        for args in ["cargo run", "imacs", "sleep 5", "cargo-watch -x test", "bacon", "/opt/homebrew/bin/sccache", ""] {
            assert!(!holds_tokens(args), "{args}");
        }
    }

    #[test]
    fn an_imacs_that_cannot_queue_runs_the_command_right_away() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("old");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                send(&stream, &json!({ "ok": false, "error": "bad request: unknown variant `queue`" })).unwrap();
            }
        });
        let started = Instant::now();
        assert!(hold_slot(&socket, SlotClass::Test, "cargo test").unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(2), "a refusal must not be retried");
    }

    fn client(socket: &Path, class: &str) -> (UnixStream, BufReader<UnixStream>) {
        let stream = UnixStream::connect(socket).unwrap();
        send(&stream, &json!({ "cmd": "queue", "class": class, "label": class, "ancestors": [] })).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        (stream, reader)
    }

    fn next(reader: &mut BufReader<UnixStream>) -> Value {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn serve_on(socket: &Path, governor: Arc<Governor>) {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let governor = governor.clone();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    let _ = governor.serve(&stream, &serde_json::from_str(&line).unwrap());
                });
            }
        });
    }

    #[test]
    fn slots_are_held_until_the_client_hangs_up() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        let governor =
            Arc::new(Governor::new(ResourceConfig { test_slots: Some(1), ..ResourceConfig::default() }, None));
        serve_on(&socket, governor.clone());
        let (first, mut first_reader) = client(&socket, "test");
        assert_eq!(next(&mut first_reader)["granted"], true);
        let (_second, mut second_reader) = client(&socket, "test");
        assert_eq!(next(&mut second_reader)["position"], 1);
        let (gone, mut gone_reader) = client(&socket, "test");
        assert_eq!(next(&mut gone_reader)["position"], 2);
        drop(gone);
        drop(gone_reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        while governor.snapshot().1.len() != 1 {
            assert!(Instant::now() < deadline, "a waiter that hung up must leave the queue");
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(first);
        drop(first_reader);
        assert_eq!(next(&mut second_reader)["granted"], true);
        let (running, waiting) = governor.snapshot();
        assert_eq!((running.len(), waiting.len()), (1, 0));
    }

    #[test]
    fn the_command_runs_with_its_exit_status_without_a_running_imacs() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        assert_eq!(run_queued(&missing, SlotClass::Test, &["sh".into(), "-c".into(), "exit 7".into()]), 7);
        assert_eq!(run_queued(&missing, SlotClass::Test, &[]), 2);
    }

    #[test]
    fn a_client_waiting_through_an_imacs_restart_queues_again() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        // A socket file with nobody listening: IMACS is restarting.
        drop(UnixListener::bind(&socket).unwrap());
        let restart = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                std::fs::remove_file(&socket).unwrap();
                serve_on(&socket, Arc::new(Governor::new(ResourceConfig::default(), None)));
            })
        };
        let started = Instant::now();
        let held = hold_slot(&socket, SlotClass::Test, "cargo test").unwrap();
        restart.join().unwrap();
        assert!(held.is_some(), "the client must get a slot from the restarted IMACS");
        assert!(started.elapsed() >= Duration::from_secs(3), "it must not run before IMACS is back");
    }

    #[test]
    fn load_average_is_available() {
        assert!(load_average().is_some_and(|load| load >= 0.0));
    }
}
