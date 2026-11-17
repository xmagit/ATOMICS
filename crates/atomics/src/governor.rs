//! The machine-wide limits behind every agent ATOMICS starts: a Cargo jobserver,
//! toolchain launch budgets, and slots for whole test runs and QA instances.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atomics_core::governor::{ResourceConfig, SlotClass, SlotQueue, Ticket, Toolchain};
use serde_json::{Value, json};

/// Detect from the worktree root even when an agent starts in a subdirectory.
pub fn toolchain_env(cwd: &Path, config: &ResourceConfig, cores: usize) -> Vec<(String, String)> {
    let root = atomics_core::git::git(cwd, &["rev-parse", "--show-toplevel"])
        .map(|root| std::path::PathBuf::from(root.trim()))
        .unwrap_or_else(|_| cwd.to_path_buf());
    Toolchain::detect(&root).into_iter().flat_map(|toolchain| config.env(toolchain, cores)).collect()
}

/// A GNU make jobserver in a named pipe. cargo takes a token per job, so agents that inherit
/// it share `tokens` jobs between them, however many run.
pub struct Jobserver {
    path: PathBuf,
    tokens: usize,
    /// Held open so the pipe and its tokens live as long as ATOMICS.
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
        if let Some(runtime) = atomics_core::paths::runtime_dir_for(home) {
            return runtime.join("jobserver/jobserver.fifo");
        }
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        std::env::temp_dir()
            .join(format!("atomics-{uid}"))
            .join(atomics_core::paths::runtime_home_hash(home))
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
        let _ = std::thread::Builder::new().name("atomics-jobserver".into()).spawn(move || {
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
    repaint: crate::host::Window,
    pub cores: usize,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    memory: Arc<dyn Fn() -> Option<atomics_core::machine::Memory> + Send + Sync>,
}

impl Governor {
    pub fn new(config: ResourceConfig, repaint: crate::host::Window) -> Governor {
        let epoch = Instant::now();
        Self::with_readings(
            config,
            repaint,
            Arc::new(move || epoch.elapsed().as_secs()),
            Arc::new(crate::machine::memory),
        )
    }

    fn with_readings(
        config: ResourceConfig,
        repaint: crate::host::Window,
        clock: Arc<dyn Fn() -> u64 + Send + Sync>,
        memory: Arc<dyn Fn() -> Option<atomics_core::machine::Memory> + Send + Sync>,
    ) -> Governor {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Governor {
            queue: Mutex::default(),
            restarting: AtomicBool::new(false),
            changed: Condvar::new(),
            config: Mutex::new(config),
            repaint,
            cores,
            clock,
            memory,
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
        let mut queue = self.queue.lock().unwrap();
        let before = queue.running().to_vec();
        if !self.restarting.load(Ordering::Acquire) {
            queue.grant_at(|class| config.slots(class, self.cores), &config, (self.memory)(), (self.clock)());
        }
        let changed = before != queue.running();
        drop(queue);
        if changed {
            self.changed.notify_all();
            self.repaint.request_repaint();
        }
    }

    pub fn snapshot(&self) -> (Vec<Ticket>, Vec<Ticket>) {
        let queue = self.queue.lock().unwrap();
        (queue.running().to_vec(), queue.waiting().cloned().collect())
    }

    pub fn status(&self) -> Value {
        self.grant();
        let now = (self.clock)();
        let memory = (self.memory)();
        let config = self.config.lock().unwrap().clone();
        let queue = self.queue.lock().unwrap();
        let describe = |ticket: &Ticket| {
            let mut value = serde_json::to_value(ticket).expect("queue ticket");
            value["held_secs"] = json!(ticket.started_at.map(|start| now.saturating_sub(start)));
            value["waiting_secs"] = json!(now.saturating_sub(ticket.enqueued_at));
            value["holds_slot"] = json!(ticket.holds_slot());
            value
        };
        json!({
            "running": queue.running().iter().map(describe).collect::<Vec<_>>(),
            "waiting": queue.waiting().map(describe).collect::<Vec<_>>(),
            "limits": { "test": config.slots(SlotClass::Test, self.cores), "qa": config.slots(SlotClass::Qa, self.cores) },
            "memory": memory.as_ref().map(|m| json!({ "available": m.available, "floor": config.memory_floor(m.total), "reservation_gib": config.memory_reservation_gib })),
        })
    }

    pub fn action(&self, id: u64, action: &str, owner: &str, chief_override: bool) -> Result<()> {
        let mut queue = self.queue.lock().unwrap();
        let result = match action {
            "cancel" => queue.cancel(id, owner, chief_override),
            "renew" => queue.renew(id, owner, (self.clock)()),
            "pin" => queue.pin(id, owner, chief_override),
            _ => Err("unknown queue action; use cancel, renew or pin"),
        };
        result.map_err(anyhow::Error::msg)?;
        drop(queue);
        self.changed.notify_all();
        self.repaint.request_repaint();
        self.grant();
        Ok(())
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

    /// Serves one `atomics queue` connection: waits for a slot, telling the client its place,
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
        let owner = request.get("owner").and_then(Value::as_str).unwrap_or("local").to_string();
        let lease = self.config.lock().unwrap().qa_lease_secs;
        let slot = self.acquire_owned(class, label, ancestors, owner, lease, |position| {
            if position != told {
                send(stream, &json!({ "position": position }))?;
                told = position;
            }
            if hung_up(stream)? {
                bail!("client left the queue");
            }
            Ok(())
        })?;
        send(stream, &json!({ "granted": true, "id": slot.id }))?;
        loop {
            self.grant();
            if !self.queue.lock().unwrap().is_running(slot.id) {
                send(stream, &json!({ "cancelled": true, "id": slot.id }))?;
                break;
            }
            stream.set_read_timeout(Some(Duration::from_millis(500)))?;
            let mut byte = [0];
            match (&mut &*stream).read(&mut byte) {
                Ok(0) => break,
                Err(e) if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
                _ => {}
            }
        }
        Ok(())
    }

    /// Waits for a slot of `class`, which is held until the returned [`Slot`] drops. `waiting` hears
    /// the place in line about twice a second; an error from it gives the place up.
    #[cfg(test)]
    pub fn acquire(
        &self,
        class: SlotClass,
        label: String,
        ancestors: Vec<u32>,
        waiting: impl FnMut(Option<usize>) -> Result<()>,
    ) -> Result<Slot<'_>> {
        self.acquire_owned(class, label, ancestors, "host".into(), None, waiting)
    }

    pub(crate) fn acquire_owned(
        &self,
        class: SlotClass,
        label: String,
        ancestors: Vec<u32>,
        owner: String,
        lease_secs: Option<u64>,
        mut waiting: impl FnMut(Option<usize>) -> Result<()>,
    ) -> Result<Slot<'_>> {
        let id = {
            let mut queue = self.queue.lock().unwrap();
            if self.restarting.load(Ordering::Acquire) {
                bail!("ATOMICS is restarting");
            }
            queue.enqueue_owned(class, label, ancestors, owner, lease_secs, (self.clock)())
        };
        let slot = Slot { governor: self, id };
        self.repaint.request_repaint();
        self.grant();
        loop {
            let position = {
                let queue = self.queue.lock().unwrap();
                if queue.is_running(id) {
                    return Ok(slot);
                }
                queue.position(id)
            };
            if position.is_none() {
                bail!("queue job cancelled");
            }
            waiting(position)?;
            let queue = self.queue.lock().unwrap();
            drop(self.changed.wait_timeout(queue, Duration::from_millis(500)).unwrap());
            self.grant();
        }
    }
}

/// A place in a slot queue, waiting or running, given up when dropped.
pub struct Slot<'a> {
    governor: &'a Governor,
    id: u64,
}

impl Slot<'_> {
    pub(crate) fn cancelled(&self) -> bool {
        !self.governor.queue.lock().unwrap().is_running(self.id)
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.governor.queue.lock().unwrap().release(self.id);
        self.governor.changed.notify_all();
        self.governor.repaint.request_repaint();
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
/// How long a client keeps reconnecting while ATOMICS restarts before it runs anyway.
const RECONNECT_FOR: Duration = Duration::from_secs(120);

// 0 while waiting, -1 across spawn, then the process group owned by this CLI.
static QUEUED_CHILD: AtomicI32 = AtomicI32::new(0);
static QUEUED_SIGNAL: AtomicI32 = AtomicI32::new(0);

/// Install only in the queue CLI, before waiting. Terminal stop signals must also reach
/// the separate process group used for owner-scoped cancellation.
pub fn install_queue_signals() -> std::io::Result<()> {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // Keep nohup/background-shell dispositions, as the host's signal setup does.
        // SAFETY: a null new action only queries the initialized output buffer.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if previous.sa_sigaction == libc::SIG_IGN {
            continue;
        }
        // SAFETY: the callback only uses atomics and async-signal-safe kill/_exit.
        unsafe {
            signal_hook::low_level::register(signal, move || {
                let pid = QUEUED_CHILD.load(Ordering::SeqCst);
                if pid > 0 {
                    libc::kill(-pid, signal);
                } else if pid == -1 {
                    QUEUED_SIGNAL.store(signal, Ordering::SeqCst);
                } else {
                    libc::_exit(128 + signal);
                }
            })?;
        }
    }
    Ok(())
}

/// `atomics queue`: waits for a slot in the running ATOMICS, then runs the command and exits
/// with its status. Without a running ATOMICS the command runs right away, and says so.
pub fn run_queued(socket: &Path, class: SlotClass, command: &[String]) -> i32 {
    let Some((program, args)) = command.split_first() else {
        eprintln!("atomics queue: nothing to run");
        return 2;
    };
    let label = command.join(" ");
    let held = match hold_slot(socket, class, &label) {
        Ok(Queued::Held(stream)) => Some(stream),
        Ok(Queued::Without(why)) => {
            eprintln!("atomics queue: {why}; running without a {} slot", class.name());
            None
        }
        Err(code) => return code,
    };
    let mut command = std::process::Command::new(program);
    command.args(args);
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    prepare_worker(&mut command);
    QUEUED_CHILD.store(-1, Ordering::SeqCst);
    let status = command.spawn().and_then(|mut child| {
        QUEUED_CHILD.store(child.id() as i32, Ordering::SeqCst);
        let signal = QUEUED_SIGNAL.swap(0, Ordering::SeqCst);
        if signal != 0 {
            // SAFETY: this is the process group of the child just started above.
            unsafe {
                libc::kill(-(child.id() as i32), signal);
            }
        }
        if let Some(stream) = &held {
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        }
        let mut reply = String::new();
        let mut reader = held.as_ref().map(BufReader::new);
        loop {
            if let Some(status) = child.try_wait()? {
                break Ok(status);
            }
            if let Some(reader) = &mut reader {
                match reader.read_line(&mut reply) {
                    Ok(0) => {} // A host restart must not kill a running command.
                    Ok(_) => {
                        if serde_json::from_str::<Value>(&reply).ok().is_some_and(|v| v["cancelled"] == true) {
                            // SAFETY: this process group belongs to the child we just started.
                            unsafe {
                                libc::kill(-(child.id() as i32), libc::SIGTERM);
                            }
                            let until = Instant::now() + Duration::from_secs(2);
                            while Instant::now() < until {
                                if let Some(status) = child.try_wait()? {
                                    return Ok(status);
                                }
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            // SAFETY: the same still-running child owns this process group.
                            unsafe {
                                libc::kill(-(child.id() as i32), libc::SIGKILL);
                            }
                            break child.wait();
                        }
                        reply.clear();
                    }
                    Err(error)
                        if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    QUEUED_CHILD.store(0, Ordering::SeqCst);
    drop(held);
    match status {
        Ok(status) => status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(1)
        }),
        Err(err) => {
            eprintln!("atomics queue: could not run {program}: {err}");
            127
        }
    }
}

/// Children must not inherit the Linux service's OOM protection or compete at host priority.
pub(crate) fn prepare_worker(command: &mut std::process::Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                let fd = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_RDONLY);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut first = 0u8;
                let read = libc::read(fd, (&mut first as *mut u8).cast(), 1);
                libc::close(fd);
                if read != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                // Never try to lower an existing positive score (which needs privilege).
                if first == b'-' {
                    let fd = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let written = libc::write(fd, c"0".as_ptr().cast(), 1);
                    libc::close(fd);
                    if written != 1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                libc::nice(10);
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// What `atomics queue` runs its command with.
#[derive(Debug)]
enum Queued {
    /// The connection that holds the slot until it closes.
    Held(UnixStream),
    /// No slot, and why; said before the command runs.
    Without(String),
}

/// The held slot, or why the command runs without one. Err is an exit code.
fn hold_slot(socket: &Path, class: SlotClass, label: &str) -> Result<Queued, i32> {
    let mut deadline: Option<Instant> = None;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                // ATOMICS is back: a later restart gets the full reconnect time again.
                deadline = None;
                match wait_for_slot(stream, class, label) {
                    Ok(stream) => return Ok(Queued::Held(stream)),
                    Err(SlotError::Refused(reason)) if reason.contains("cancelled") => {
                        eprintln!("atomics queue: {reason}");
                        return Err(NO_SLOT_HERE);
                    }
                    Err(SlotError::Refused(reason)) => {
                        return Ok(Queued::Without(format!("ATOMICS cannot queue ({reason})")));
                    }
                    // ATOMICS went away while this waited: wait for it to come back and queue again.
                    Err(SlotError::Lost(err)) => eprintln!("atomics queue: {err:#}; reconnecting"),
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "atomics queue: this sandbox blocks the ATOMICS socket ({err}); run this command outside \
                     the sandbox so it can wait for a {} slot",
                    class.name()
                );
                return Err(NO_SLOT_HERE);
            }
            Err(_) if deadline.is_none() && !socket.exists() => {
                return Ok(Queued::Without(format!(
                    "no ATOMICS socket at {} (ATOMICS_SOCKET names the one to queue in)",
                    socket.display()
                )));
            }
            Err(err) if deadline.is_some_and(|d| Instant::now() > d) => {
                return Ok(Queued::Without(format!("ATOMICS did not come back ({err})")));
            }
            Err(_) => {}
        }
        deadline.get_or_insert_with(|| Instant::now() + RECONNECT_FOR);
        std::thread::sleep(Duration::from_secs(2));
    }
}

enum SlotError {
    /// ATOMICS answered but will not queue (an older build, or a bad request): final.
    Refused(String),
    /// The connection broke: ATOMICS may be restarting.
    Lost(anyhow::Error),
}

fn wait_for_slot(stream: UnixStream, class: SlotClass, label: &str) -> Result<UnixStream, SlotError> {
    let lost = |err: anyhow::Error| SlotError::Lost(err);
    let ancestors = crate::hook_client::ancestors();
    let owner = queue_owner();
    let request =
        json!({ "cmd": "queue", "class": class.name(), "label": label, "ancestors": ancestors, "owner": owner });
    send(&stream, &request).map_err(lost)?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| lost(e.into()))?);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).map_err(|e| lost(e.into()))? == 0 {
            return Err(lost(anyhow::anyhow!("ATOMICS closed the queue")));
        }
        let Ok(reply) = serde_json::from_str::<Value>(&line) else {
            return Err(SlotError::Refused(format!("unreadable reply {:?}", line.trim())));
        };
        if reply.get("granted").is_some() {
            if let Some(id) = reply.get("id").and_then(Value::as_u64) {
                eprintln!("atomics queue: job {id} ({})", class.name());
            }
            return Ok(stream);
        }
        if let Some(error) = reply.get("error").and_then(Value::as_str) {
            return Err(SlotError::Refused(error.to_string()));
        }
        if let Some(position) = reply.get("position").and_then(Value::as_u64) {
            eprintln!("atomics queue: waiting for a {} slot ({position} in line)", class.name());
        }
    }
}

pub fn queue_owner() -> String {
    std::env::var(atomics_core::hook::HOOK_PANE_ENV)
        .or_else(|_| std::env::var("ATOMICS_PANE"))
        .unwrap_or_else(|_| "local".into())
}

pub fn control_queue(socket: &Path, action: &str, id: Option<u64>, chief_override: bool) -> i32 {
    use atomics_core::control::ControlRequest;
    let request = if action == "status" {
        ControlRequest::QueueStatus
    } else {
        ControlRequest::QueueAction {
            id: id.unwrap_or_default(),
            action: action.into(),
            pane: Some(queue_owner()),
            chief_override,
        }
    };
    match crate::ipc::request_at(socket, &request) {
        Ok(response) if response.ok => {
            println!("{}", serde_json::to_string_pretty(&response.result).unwrap());
            0
        }
        Ok(response) => {
            eprintln!("atomics queue: {}", response.error.unwrap_or_default());
            1
        }
        Err(error) => {
            eprintln!("atomics queue: {error:?}");
            1
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

    fn injected(
        config: ResourceConfig,
        clock: Arc<std::sync::atomic::AtomicU64>,
        available: Arc<std::sync::atomic::AtomicU64>,
    ) -> Governor {
        Governor::with_readings(
            config,
            Default::default(),
            Arc::new(move || clock.load(Ordering::Relaxed)),
            Arc::new(move || {
                Some(atomics_core::machine::Memory { total: 64 << 30, available: available.load(Ordering::Relaxed) })
            }),
        )
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn queued_linux_child_resets_oom_score_and_lowers_priority() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "cat /proc/self/oom_score_adj; ps -o nice= -p $$"]);
        prepare_worker(&mut command);
        let output = command.output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let mut lines = output.lines();
        // A negative score is reset to 0; a positive one stays (lowering it needs privilege; CI runners start at 500).
        let parent: i32 = std::fs::read_to_string("/proc/self/oom_score_adj").unwrap().trim().parse().unwrap();
        assert_eq!(lines.next(), Some(parent.max(0).to_string().as_str()));
        let nice: i32 = lines.next().unwrap().trim().parse().unwrap();
        // SAFETY: getpriority has no pointer arguments.
        let parent_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        assert_eq!(nice, (parent_nice + 10).min(19));
    }

    #[test]
    fn cancelling_a_running_job_terminates_only_its_own_command() {
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("s");
        let governor = Arc::new(Governor::new(ResourceConfig::default(), Default::default()));
        serve_on(&socket, governor.clone());
        let child = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                run_queued(&socket, SlotClass::Test, &["/bin/sh".into(), "-c".into(), "exec sleep 30".into()])
            })
        };
        let until = Instant::now() + Duration::from_secs(5);
        let (id, owner) = loop {
            if let Some(ticket) = governor.snapshot().0.first() {
                break (ticket.id, ticket.owner.clone());
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(governor.action(id, "cancel", "someone-else", false).is_err());
        governor.action(id, "cancel", &owner, false).unwrap();
        assert_eq!(child.join().unwrap(), 128 + libc::SIGTERM);
        assert!(governor.snapshot().0.is_empty());
    }

    #[test]
    fn status_lists_holders_elapsed_time_and_waiters_without_enqueuing() {
        use std::sync::atomic::AtomicU64;
        let clock = Arc::new(AtomicU64::new(10));
        let governor = injected(
            ResourceConfig { test_slots: Some(1), ..Default::default() },
            clock.clone(),
            Arc::new(AtomicU64::new(32 << 30)),
        );
        let a = governor.queue.lock().unwrap().enqueue_owned(
            SlotClass::Test,
            "cargo test".into(),
            vec![],
            "session-a".into(),
            None,
            10,
        );
        governor.grant();
        let b = governor.queue.lock().unwrap().enqueue_owned(
            SlotClass::Test,
            "tsc".into(),
            vec![],
            "session-b".into(),
            None,
            12,
        );
        clock.store(25, Ordering::Relaxed);
        for _ in 0..2 {
            let status = governor.status();
            assert_eq!(status["running"][0]["id"], a);
            assert_eq!(status["running"][0]["owner"], "session-a");
            assert_eq!(status["running"][0]["held_secs"], 15);
            assert_eq!(status["waiting"][0]["id"], b);
            assert_eq!(status["waiting"][0]["waiting_secs"], 13);
            assert_eq!(status["running"].as_array().unwrap().len(), 1);
            assert_eq!(status["waiting"].as_array().unwrap().len(), 1);
        }
    }

    #[test]
    fn memory_recovery_is_rechecked_while_a_client_waits() {
        use std::sync::atomic::AtomicU64;
        let available = Arc::new(AtomicU64::new(4 << 30));
        let governor = injected(ResourceConfig::default(), Arc::new(AtomicU64::new(0)), available.clone());
        let mut waited = false;
        let slot = governor
            .acquire(SlotClass::Test, "check".into(), vec![], |position| {
                assert_eq!(position, Some(1));
                waited = true;
                available.store(32 << 30, Ordering::Relaxed);
                Ok(())
            })
            .unwrap();
        assert!(waited);
        assert!(governor.queue.lock().unwrap().is_running(slot.id));
    }

    #[test]
    fn cancelling_a_waiter_does_not_allow_it_to_run() {
        use std::sync::atomic::AtomicU64;
        let governor = injected(ResourceConfig::default(), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let error = governor
            .acquire_owned(SlotClass::Test, "check".into(), vec![], "own".into(), None, |_| {
                let id = governor.snapshot().1[0].id;
                assert!(governor.action(id, "cancel", "other", false).is_err());
                governor.action(id, "cancel", "own", false)
            })
            .err()
            .unwrap();
        assert!(error.to_string().contains("cancelled"));
        assert!(governor.snapshot().0.is_empty() && governor.snapshot().1.is_empty());
    }

    #[test]
    fn qa_expiry_and_pin_free_slots_even_with_a_connected_client() {
        use std::sync::atomic::AtomicU64;
        let clock = Arc::new(AtomicU64::new(0));
        let governor = injected(ResourceConfig::default(), clock.clone(), Arc::new(AtomicU64::new(32 << 30)));
        let first = governor
            .acquire_owned(SlotClass::Qa, "preview".into(), vec![], "own".into(), Some(10), |_| Ok(()))
            .unwrap();
        clock.store(10, Ordering::Relaxed);
        let second = governor.acquire(SlotClass::Qa, "next".into(), vec![], |_| Ok(())).unwrap();
        assert!(governor.snapshot().0.iter().find(|t| t.id == first.id).unwrap().expired);
        governor.action(second.id, "pin", "chief", true).unwrap();
        let _third = governor.acquire(SlotClass::Qa, "third".into(), vec![], |_| Ok(())).unwrap();
        assert_eq!(governor.snapshot().0.iter().filter(|t| t.holds_slot()).count(), 1);
    }

    #[test]
    fn restart_refuses_held_and_waiting_slots_and_force_freezes_admission() {
        let governor = Governor::new(ResourceConfig::default(), Default::default());
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
        let dir = crate::test_support::tempdir().unwrap();
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
            dotnet: atomics_core::governor::DotnetConfig { enabled: false, ..Default::default() },
            ..Default::default()
        };
        assert!(toolchain_env(&nested, &disabled, 18).is_empty());
    }

    #[test]
    fn jobserver_pipe_holds_its_tokens_and_resets_leaks() {
        let dir = crate::test_support::tempdir().unwrap();
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
        let a = Jobserver::path_for(Path::new("/Users/x/.atomics"));
        let b = Jobserver::path_for(Path::new("/private/tmp/qa/state"));
        assert_ne!(a, b);
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let expected = match atomics_core::paths::runtime_dir_for(Path::new("/Users/x/.atomics")) {
            Some(runtime) => runtime.join("jobserver/jobserver.fifo"),
            None => std::env::temp_dir().join(format!("atomics-{uid}/c32c7b261409a862/jobserver.fifo")),
        };
        assert_eq!(a, expected);
        assert_eq!(a, Jobserver::path_for(Path::new("/Users/x/.atomics")));
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("pool/jobs");
        drop(Jobserver::create(&path, 1).unwrap());
        assert!(!path.exists() && !dir.path().join("pool").exists(), "the pool and its folder go with it");
    }

    #[test]
    fn a_pool_folder_owned_through_a_link_is_refused() {
        let dir = crate::test_support::tempdir().unwrap();
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
        for args in ["cargo run", "atomics", "sleep 5", "cargo-watch -x test", "bacon", "/opt/homebrew/bin/sccache", ""]
        {
            assert!(!holds_tokens(args), "{args}");
        }
    }

    #[test]
    fn an_atomics_that_cannot_queue_runs_the_command_right_away() {
        let dir = crate::test_support::tempdir().unwrap();
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
        let Ok(Queued::Without(why)) = hold_slot(&socket, SlotClass::Test, "cargo test") else { panic!("a slot") };
        assert!(why.contains("cannot queue") && why.contains("unknown variant"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(2), "a refusal must not be retried");
    }

    /// An agent whose terminal names no socket (or the wrong one) is told it runs without a slot.
    #[test]
    fn a_missing_socket_is_named_rather_than_skipped_silently() {
        let dir = crate::test_support::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        let started = Instant::now();
        let Ok(Queued::Without(why)) = hold_slot(&missing, SlotClass::Test, "cargo test") else { panic!("a slot") };
        assert!(why.contains(&missing.display().to_string()) && why.contains("ATOMICS_SOCKET"), "{why}");
        assert!(started.elapsed() < Duration::from_secs(2), "nothing to wait for");
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
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("s");
        let governor = Arc::new(Governor::new(
            ResourceConfig { test_slots: Some(1), ..ResourceConfig::default() },
            Default::default(),
        ));
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
    fn the_command_runs_with_its_exit_status_without_a_running_atomics() {
        let dir = crate::test_support::tempdir().unwrap();
        let missing = dir.path().join("missing.sock");
        assert_eq!(run_queued(&missing, SlotClass::Test, &["sh".into(), "-c".into(), "exit 7".into()]), 7);
        assert_eq!(run_queued(&missing, SlotClass::Test, &[]), 2);
    }

    #[test]
    fn a_client_waiting_through_an_atomics_restart_queues_again() {
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("s");
        // A socket file with nobody listening: ATOMICS is restarting.
        drop(UnixListener::bind(&socket).unwrap());
        let restart = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3));
                std::fs::remove_file(&socket).unwrap();
                serve_on(&socket, Arc::new(Governor::new(ResourceConfig::default(), Default::default())));
            })
        };
        let started = Instant::now();
        let held = hold_slot(&socket, SlotClass::Test, "cargo test").unwrap();
        restart.join().unwrap();
        assert!(matches!(held, Queued::Held(_)), "the client must get a slot from the restarted ATOMICS: {held:?}");
        assert!(started.elapsed() >= Duration::from_secs(3), "it must not run before ATOMICS is back");
    }

    #[test]
    fn load_average_is_available() {
        assert!(load_average().is_some_and(|load| load >= 0.0));
    }
}
