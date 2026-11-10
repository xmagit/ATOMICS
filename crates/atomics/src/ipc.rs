//! The app side of the hook bridge: a unix socket that answers `atomics hook` at once
//! from shared state, so a hidden or busy window never slows an agent down.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::host::Window;
use anyhow::{Result, bail};
use atomics_core::control::{ControlRequest, ControlResponse, describe_diff};
use atomics_core::hook::{HookEvent, HookReply, HookRequest, HookThread, context_output};
use serde_json::Value;

#[derive(Default)]
pub struct Hub {
    pub chief_running: bool,
    /// Messages waiting for sessions that run outside ATOMICS, by Claude session id.
    pub outbox: HashMap<String, Vec<String>>,
    pub session_context: String,
    pub external_prefix: String,
}

pub struct HookMessage {
    pub pane: Option<String>,
    pub ancestors: Vec<u32>,
    pub event: HookEvent,
    pub delivered: usize,
    pub approval_reply: Option<Arc<ApprovalReply>>,
}

#[derive(Default)]
pub struct ApprovalReply {
    state: Mutex<ApprovalState>,
    ready: Condvar,
}

#[derive(Default)]
enum ApprovalState {
    #[default]
    Waiting,
    Answered(HookReply),
    Expired,
}

impl ApprovalReply {
    pub fn expired(&self) -> bool {
        matches!(*self.state.lock().unwrap(), ApprovalState::Expired)
    }

    pub fn answer(&self, reply: HookReply) -> bool {
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, ApprovalState::Waiting) {
            return false;
        }
        *state = ApprovalState::Answered(reply);
        self.ready.notify_one();
        true
    }

    pub fn wait(&self, timeout: Duration) -> HookReply {
        self.wait_until(|| timeout, || false)
    }

    fn wait_until(&self, timeout: impl Fn() -> Duration, disconnected: impl Fn() -> bool) -> HookReply {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap();
        while matches!(*state, ApprovalState::Waiting) {
            let remaining = timeout().saturating_sub(started.elapsed());
            if remaining.is_zero() || disconnected() {
                break;
            }
            let (next, _) = self.ready.wait_timeout(state, remaining.min(Duration::from_millis(250))).unwrap();
            state = next;
        }
        match std::mem::replace(&mut *state, ApprovalState::Expired) {
            ApprovalState::Answered(reply) => reply,
            _ => HookReply::default(),
        }
    }
}

/// What the app answers to a control request. Diffs are computed, and sandbox commands run, on
/// the socket thread so the UI never waits on git or sbx.
pub enum ControlOutcome {
    Done(ControlResponse),
    Diff {
        root: PathBuf,
        base_ref: String,
        patch: bool,
        path: Option<String>,
        session: Value,
    },
    /// `then` runs once the command has, done or not.
    Exec {
        job: crate::sandbox::exec::Exec,
        session: Value,
        then: Box<dyn FnOnce() + Send>,
    },
}

pub struct ControlMessage {
    pub request: ControlRequest,
    pub reply: Sender<ControlOutcome>,
}

pub struct Channels {
    pub hooks: crate::host::Sender<HookMessage>,
    pub control: crate::host::Sender<ControlMessage>,
    pub usage: Sender<crate::usage::Report>,
    pub governor: Arc<crate::governor::Governor>,
}

impl Hub {
    /// The stdout for one hook call: session context on start, plus any queued messages.
    pub fn reply(&mut self, event: &HookEvent) -> (HookReply, usize) {
        let mut parts = Vec::new();
        if event.name == "SessionStart" && event.thread == HookThread::Main && !self.session_context.is_empty() {
            parts.push(self.session_context.clone());
        }
        let mut delivered = 0;
        if event.can_deliver()
            && let Some(messages) = self.outbox.remove(&event.session_id)
        {
            delivered = messages.len();
            for message in messages {
                parts.push(format!("{}\n{message}", self.external_prefix));
            }
        }
        let stdout = (!parts.is_empty()).then(|| context_output(&event.name, &parts.join("\n\n")));
        (HookReply { stdout }, delivered)
    }
}

/// Serializes stale-socket recovery and owns the socket name until runtime shutdown.
/// The lock file stays in place: unlinking it would let two processes lock different inodes.
#[derive(Debug)]
pub struct SocketGuard {
    _lock: std::fs::File,
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    pub(crate) fn unlink(&self) {
        use std::os::unix::fs::MetadataExt;
        if std::fs::symlink_metadata(&self.path).is_ok_and(|meta| meta.dev() == self.device && meta.ino() == self.inode)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        self.unlink();
    }
}

fn bind(path: &Path) -> Result<(UnixListener, SocketGuard)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    atomics_core::paths::validate_socket_path(path)?;
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
        let metadata = std::fs::metadata(parent)?;
        // A public writable parent lets another user pre-create either the socket or its lock.
        // SAFETY: geteuid has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o022 != 0 {
            bail!("ATOMICS_SOCKET must be in a directory you own that other users cannot write: {}", parent.display());
        }
    }
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(PathBuf::from(lock_path))?;
    // SAFETY: the descriptor stays open in SocketGuard for the whole ownership interval.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("cannot own control socket {}: {}", path.display(), std::io::Error::last_os_error());
    }
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                bail!("refusing to remove non-socket {}", path.display());
            }
            match UnixStream::connect(path) {
                Ok(_) => bail!("another ATOMICS is already listening on {}", path.display()),
                Err(error)
                    if matches!(error.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound) => {
                }
                Err(error) => return Err(error.into()),
            }
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    let guard = SocketGuard { _lock: lock, path: path.to_path_buf(), device: meta.dev(), inode: meta.ino() };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

/// Binds the socket, refusing to steal it from another running ATOMICS.
pub fn serve(path: &Path, hub: Arc<Mutex<Hub>>, channels: Channels, window: Window) -> Result<SocketGuard> {
    let (listener, guard) = bind(path)?;
    std::thread::Builder::new().name("atomics-hooks".into()).spawn(move || {
        let channels = Arc::new(channels);
        for stream in listener.incoming().flatten() {
            let (hub, channels, window) = (hub.clone(), channels.clone(), window.clone());
            std::thread::spawn(move || {
                if let Err(err) = handle(stream, &hub, &channels, &window) {
                    eprintln!("ATOMICS: hook connection failed: {err:#}");
                }
            });
        }
    })?;
    Ok(guard)
}

// A full plan can carry MAX_TASKS text payloads plus its goal. JSON escaping can expand
// each text byte to six bytes; reserve a frame for ids, field names and other metadata.
const MAX_REQUEST: usize =
    (atomics_core::ops::MAX_TASKS + 1) * atomics_core::ops::MAX_TEXT_BYTES * 6 + atomics_core::wire::MAX_FRAME_BYTES;
const MAX_RESPONSE: usize = 32 * 1024 * 1024;

fn read_line_bounded(reader: impl BufRead, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((limit + 1) as u64).read_until(b'\n', &mut bytes)?;
    if bytes.len() > limit || !bytes.ends_with(b"\n") {
        bail!("control socket line exceeds {limit} bytes or is missing its newline");
    }
    Ok(bytes)
}

fn read_request(reader: impl BufRead) -> Result<Value> {
    Ok(serde_json::from_slice(&read_line_bounded(reader, MAX_REQUEST)?)?)
}

fn handle(stream: UnixStream, hub: &Mutex<Hub>, channels: &Channels, window: &Window) -> Result<()> {
    handle_with_wait_duration(stream, hub, channels, window, Duration::from_secs)
}

// Keep timeout selection identical when tests scale seconds down to milliseconds.
fn handle_with_wait_duration(
    stream: UnixStream,
    hub: &Mutex<Hub>,
    channels: &Channels,
    window: &Window,
    wait_duration: impl Fn(u64) -> Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let value = read_request(BufReader::new(&stream))?;
    if let Some(payload) = value.get("statusline") {
        if payload["rate_limits"].is_object() {
            let usage = atomics_core::usage::Usage::claude(payload, crate::usage::now());
            if channels.usage.send(crate::usage::Report::Claude(usage)).is_ok() {
                window.request_repaint();
            }
        }
        (&stream).write_all(b"\n")?;
        return Ok(());
    }
    if value.get("cmd").and_then(Value::as_str) == Some("queue") {
        return channels.governor.serve(&stream, &value);
    }
    if value.get("cmd").is_some() {
        let response = match serde_json::from_value::<ControlRequest>(value) {
            Ok(request) => control(request, channels, window, &stream),
            Err(err) => ControlResponse::error(format!("bad request: {err}")),
        };
        let mut writer = &stream;
        writer.write_all(serde_json::to_string(&response)?.as_bytes())?;
        writer.write_all(b"\n")?;
        return Ok(());
    }
    let request: HookRequest = serde_json::from_value(value)?;
    let Some(event) = HookEvent::from_payload(&request.payload) else {
        bail!("hook payload without event name or session id");
    };
    let (mut reply, delivered) = hub.lock().unwrap().reply(&event);
    let permission = event.name == "PermissionRequest";
    let approval_reply = permission.then(|| Arc::new(ApprovalReply::default()));
    let _ = channels.hooks.send(HookMessage {
        pane: request.pane,
        ancestors: request.ancestors,
        event,
        delivered,
        approval_reply: approval_reply.clone(),
    });
    window.request_repaint();
    if let Some(approval_reply) = approval_reply {
        // After this window, leave Codex's native prompt available to the user.
        let chief_running = hub.lock().unwrap().chief_running;
        reply = approval_reply.wait_until(
            || {
                let seconds = if chief_running && hub.lock().unwrap().chief_running {
                    atomics_core::hook::PERMISSION_WAIT_SECONDS
                } else {
                    10
                };
                wait_duration(seconds)
            },
            || disconnected(&stream),
        );
    }
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(&reply)?.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

/// Hooks send one line, then only read. Peeking does not consume their request or block the waiter.
fn disconnected(stream: &UnixStream) -> bool {
    let mut byte = 0u8;
    // SAFETY: `byte` is writable for one byte and the stream owns this live descriptor.
    let read = unsafe {
        libc::recv(stream.as_raw_fd(), (&mut byte as *mut u8).cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT)
    };
    read == 0
        || (read < 0
            && !matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ))
}

fn control(request: ControlRequest, channels: &Channels, window: &Window, stream: &UnixStream) -> ControlResponse {
    let (reply, outcome) = std::sync::mpsc::channel();
    if channels.control.send(ControlMessage { request, reply }).is_err() {
        return ControlResponse::error("ATOMICS is shutting down");
    }
    window.request_repaint();
    match outcome.recv_timeout(Duration::from_secs(300)) {
        Ok(ControlOutcome::Done(response)) => response,
        Ok(ControlOutcome::Diff { root, base_ref, patch, path, session }) => {
            match atomics_core::git::diff_against_base(&root, &base_ref, None) {
                Ok(diff) => ControlResponse::ok(serde_json::json!({
                    "session": session,
                    "root": root,
                    "diff": describe_diff(&diff, patch, path.as_deref()),
                })),
                Err(err) => ControlResponse::error(format!("{err:#}")),
            }
        }
        Ok(ControlOutcome::Exec { job, session, then }) => {
            let executed = job.run(&channels.governor, || crate::governor::hung_up(stream).unwrap_or(true));
            then();
            match executed.and_then(|done| Ok(serde_json::to_value(done)?)) {
                Ok(mut result) => {
                    result["session"] = session;
                    result["sandbox"] = job.sandbox.into();
                    result["workdir"] = job.workdir.display().to_string().into();
                    ControlResponse::ok(result)
                }
                Err(err) => ControlResponse::error(format!("{err:#}")),
            }
        }
        Err(_) => ControlResponse::error("ATOMICS did not answer in time"),
    }
}

/// Why a request did not get an answer.
#[derive(Debug)]
pub enum RequestError {
    /// Nothing listens on the socket.
    NotRunning(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::NotRunning(message) => f.write_str(message),
            RequestError::Failed(error) => write!(f, "{error:#}"),
        }
    }
}

impl<E: Into<anyhow::Error>> From<E> for RequestError {
    fn from(error: E) -> Self {
        RequestError::Failed(error.into())
    }
}

/// Sends one control request to a running ATOMICS (for `atomics ctl` and `atomics mcp`).
pub fn request(request: &ControlRequest) -> Result<ControlResponse> {
    request_at(&atomics_core::paths::socket_path(), request).map_err(|error| match error {
        RequestError::NotRunning(message) => anyhow::anyhow!(message),
        RequestError::Failed(error) => error,
    })
}

pub fn request_at(path: &Path, request: &ControlRequest) -> Result<ControlResponse, RequestError> {
    let stream = UnixStream::connect(path)
        .map_err(|e| RequestError::NotRunning(format!("ATOMICS is not running ({}: {e})", path.display())))?;
    // A sandbox command may wait its timeout for a slot, then run for as long.
    let timeout = match request {
        ControlRequest::SecretRemoved { .. } => 5,
        ControlRequest::Exec { timeout_secs, .. } => 2 * crate::sandbox::exec::timeout(*timeout_secs).as_secs() + 120,
        _ => 320,
    };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request)?.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(serde_json::from_slice(&read_line_bounded(BufReader::new(&stream), MAX_RESPONSE)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Read;

    #[test]
    fn audit_p7_control_requests_stop_at_the_byte_limit() {
        const LIMIT: usize = 128;
        let mut oversized = std::io::Cursor::new(vec![b'x'; LIMIT * 2]);
        assert!(read_line_bounded(&mut oversized, LIMIT).unwrap_err().to_string().contains("exceeds"));
        assert_eq!(oversized.position(), (LIMIT + 1) as u64);
        assert!(read_request(&b"{\"cmd\":\"list\"}"[..]).is_err());
        assert_eq!(read_request(&b"{\"cmd\":\"list\"}\n"[..]).unwrap()["cmd"], "list");
        let padded = format!("{}{{\"cmd\":\"list\"}}\n", " ".repeat(LIMIT - 15));
        assert_eq!(padded.len(), LIMIT);
        assert!(read_line_bounded(padded.as_bytes(), LIMIT).is_ok());
    }

    #[test]
    fn review_p7_control_socket_accepts_a_plan_at_the_task_and_text_limits() {
        use atomics_core::ops::{MAX_TASKS, MAX_TEXT_BYTES};
        let tasks: Vec<_> =
            (0..MAX_TASKS).map(|id| json!({"id":id.to_string(), "brief":"b".repeat(MAX_TEXT_BYTES)})).collect();
        let mut encoded = serde_json::to_vec(&json!({"cmd":"plan_set", "goal":"large plan", "tasks":tasks})).unwrap();
        encoded.push(b'\n');
        assert!(encoded.len() > 1024 * 1024);
        let request = read_request(encoded.as_slice()).unwrap();
        assert_eq!(request["tasks"].as_array().unwrap().len(), MAX_TASKS);
    }

    #[test]
    fn review_p7_public_writable_socket_parents_are_refused_before_creating_a_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket = dir.path().join("control.sock");
        let error = bind(&socket).unwrap_err().to_string();
        assert!(error.contains("ATOMICS_SOCKET") && error.contains("other users"), "{error}");
        assert!(!socket.exists());
        assert!(!dir.path().join("control.sock.lock").exists());
    }

    #[test]
    fn audit_p7_stale_socket_recovery_has_one_owner_and_cleanup_keeps_a_replacement() {
        const STALE: &str = "ATOMICS_STALE_SOCKET_FIXTURE";
        const TEST: &str = "ipc::tests::audit_p7_stale_socket_recovery_has_one_owner_and_cleanup_keeps_a_replacement";
        if let Some(path) = std::env::var_os(STALE) {
            drop(UnixListener::bind(path).unwrap());
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let path = dir.path().join("control.sock");
        // A parallel test can fork while a parent-created listener is open and
        // keep it live after this test closes it. Only this child owns the setup
        // descriptor; its exit makes the socket stale before either contender runs.
        let created = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(STALE, &path)
                .env("HOME", dir.path()),
        );
        assert!(created.status.success(), "{created:?}");
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    let (barrier, path) = (barrier.clone(), &path);
                    scope.spawn(move || {
                        barrier.wait();
                        bind(path)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1, "{results:?}");
        assert!(UnixStream::connect(&path).is_ok());
        assert!(bind(&path).is_err());
        // Ownership cleanup must not unlink someone else's replacement inode.
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        drop(results);
        assert!(UnixStream::connect(&path).is_ok());
        drop(replacement);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"keep this regular file").unwrap();
        assert!(bind(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep this regular file");
    }

    #[test]
    fn control_socket_wakes_the_runtime_before_its_next_tick() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let wake = crate::host::Wake::default();
        let (control, requests) = wake.channel::<ControlMessage>();
        let channels = Channels {
            hooks: std::sync::mpsc::channel().0.into(),
            control,
            usage: std::sync::mpsc::channel().0,
            governor: Arc::new(crate::governor::Governor::new(Default::default(), Default::default())),
        };
        let server = std::thread::spawn(move || {
            handle(server, &Mutex::new(Hub::default()), &channels, &Window::default()).unwrap();
        });
        let runtime = std::thread::spawn(move || {
            // A long deadline makes this fail if the control path stops notifying.
            let started = std::time::Instant::now();
            wake.wait_until(started + Duration::from_secs(5));
            assert!(started.elapsed() < Duration::from_secs(2));
            let message = requests.try_recv().expect("wake must follow enqueuing the request");
            assert!(matches!(message.request, ControlRequest::List));
            message.reply.send(ControlOutcome::Done(ControlResponse::ok(json!([])))).unwrap();
        });
        writeln!(client, "{}", json!({"cmd":"list"})).unwrap();
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        assert!(serde_json::from_str::<ControlResponse>(&line).unwrap().ok);
        runtime.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn statusline_socket_routes_account_usage_without_a_session_or_lifecycle_event() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (hooks, hook_rx) = std::sync::mpsc::channel();
        let (control, _) = std::sync::mpsc::channel();
        let (usage, usage_rx) = std::sync::mpsc::channel();
        let governor = Arc::new(crate::governor::Governor::new(Default::default(), Default::default()));
        writeln!(
            client,
            "{}",
            json!({"statusline":{"rate_limits":{
                "five_hour":{"used_percentage":22,"resets_at":4102444800_i64}
            }}})
        )
        .unwrap();
        handle(
            server,
            &Mutex::new(Hub::default()),
            &Channels { hooks: hooks.into(), control: control.into(), usage, governor },
            &Window::default(),
        )
        .unwrap();
        let crate::usage::Report::Claude(usage) = usage_rx.try_recv().unwrap() else { panic!("claude report") };
        assert_eq!(usage.session.unwrap().left_percent, 78.0);
        assert!(usage.weekly.is_none());
        assert!(usage.observed_at > 0);
        assert!(hook_rx.try_recv().is_err(), "usage must not change agent status");
        let mut acknowledged = [0];
        client.read_exact(&mut acknowledged).unwrap();
        assert_eq!(acknowledged, *b"\n");
    }

    #[test]
    fn initial_claude_statusline_without_limits_does_not_erase_another_sessions_report() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (hooks, _) = std::sync::mpsc::channel();
        let (control, _) = std::sync::mpsc::channel();
        let (usage, usage_rx) = std::sync::mpsc::channel();
        let governor = Arc::new(crate::governor::Governor::new(Default::default(), Default::default()));
        writeln!(client, "{}", json!({"statusline":{"session_id":"new-session"}})).unwrap();
        handle(
            server,
            &Mutex::new(Hub::default()),
            &Channels { hooks: hooks.into(), control: control.into(), usage, governor },
            &Window::default(),
        )
        .unwrap();
        assert!(usage_rx.try_recv().is_err());
    }

    #[test]
    fn exec_runs_on_the_socket_thread_in_a_slot_then_tells_the_app() {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = crate::sandbox::fake::install(dir.path()).display().to_string();
        std::fs::write(dir.path().join("exec-runs"), "").unwrap();
        std::fs::create_dir(dir.path().join("sandboxes")).unwrap();
        std::fs::write(dir.path().join("sandboxes/atomics-s1"), "{}").unwrap();
        let workdir = dir.path().canonicalize().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let (hooks, _) = std::sync::mpsc::channel();
        let (control, requests) = std::sync::mpsc::channel::<ControlMessage>();
        let (finished, then_ran) = std::sync::mpsc::channel();
        let governor = Arc::new(crate::governor::Governor::new(
            atomics_core::governor::ResourceConfig::default(),
            Default::default(),
        ));
        let slots = governor.clone();
        // The app: answers with the job, and hears when it has run.
        let app = std::thread::spawn(move || {
            let message = requests.recv().unwrap();
            let ControlRequest::Exec { session, command, .. } = message.request else { panic!("an exec request") };
            let job = crate::sandbox::exec::Exec {
                env: Vec::new(),
                sbx,
                sandbox: "atomics-s1".into(),
                workdir,
                command,
                timeout: Duration::from_secs(60),
                class: atomics_core::governor::SlotClass::Test,
                ancestors: vec![],
            };
            let then = Box::new(move || finished.send(()).unwrap());
            let _ = message.reply.send(ControlOutcome::Exec { job, session: json!(session), then });
        });
        writeln!(client, "{}", json!({"cmd":"exec", "session":"s1", "command":"echo checked; exit 2"})).unwrap();
        let server = std::thread::spawn(move || {
            let channels = Channels {
                hooks: hooks.into(),
                control: control.into(),
                usage: std::sync::mpsc::channel().0,
                governor,
            };
            handle(server, &Mutex::new(Hub::default()), &channels, &Window::default()).unwrap();
        });
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        let response: ControlResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result["session"], "s1");
        assert_eq!(response.result["sandbox"], "atomics-s1");
        assert_eq!(
            (response.result["exit_code"].clone(), response.result["stdout"].clone()),
            (json!(2), json!("checked\n"))
        );
        then_ran.recv_timeout(Duration::from_secs(2)).expect("the app hears the command finished");
        assert_eq!(slots.snapshot(), (vec![], vec![]), "the slot is free again");
        app.join().unwrap();
        server.join().unwrap();
    }

    fn event(name: &str) -> HookEvent {
        HookEvent::from_payload(&json!({ "hook_event_name": name, "session_id": "s1", "cwd": "/w" })).unwrap()
    }

    #[test]
    fn permission_reply_expires_atomically_and_cannot_be_answered_twice() {
        let reply = ApprovalReply::default();
        assert!(!reply.expired());
        assert!(reply.wait(Duration::ZERO).stdout.is_none());
        assert!(reply.expired());
        assert!(!reply.answer(HookReply { stdout: Some("late".into()) }));
        let reply = Arc::new(ApprovalReply::default());
        let waiting = reply.clone();
        let thread = std::thread::spawn(move || waiting.wait(Duration::from_secs(1)));
        assert!(reply.answer(HookReply { stdout: Some("allowed".into()) }));
        assert!(!reply.answer(HookReply { stdout: Some("duplicate".into()) }));
        assert_eq!(thread.join().unwrap().stdout.as_deref(), Some("allowed"));
        assert!(!reply.answer(HookReply::default()));
    }

    #[test]
    fn permission_reply_expires_when_the_hook_disconnects_before_its_timeout() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let (hooks, received) = std::sync::mpsc::channel();
        let handler = std::thread::spawn(move || {
            handle(
                server,
                &Mutex::new(Hub { chief_running: true, ..Hub::default() }),
                &Channels {
                    hooks: hooks.into(),
                    control: std::sync::mpsc::channel().0.into(),
                    usage: std::sync::mpsc::channel().0,
                    governor: Arc::new(crate::governor::Governor::new(Default::default(), Default::default())),
                },
                &Window::default(),
            )
        });
        writeln!(
            client,
            "{}",
            json!({"payload":{
                "hook_event_name":"PermissionRequest", "session_id":"worker"
            }})
        )
        .unwrap();
        let pending = received.recv_timeout(Duration::from_secs(2)).unwrap().approval_reply.unwrap();
        assert!(!pending.expired());
        drop(client);
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(2));
        while !pending.expired() {
            assert!(Instant::now() < deadline, "the closed hook kept its reply alive");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!pending.answer(HookReply { stdout: Some("late".into()) }));
        assert!(handler.join().unwrap().is_err(), "writing to the closed hook must fail");
    }

    #[test]
    fn permission_reply_wait_depends_on_a_running_chief() {
        let started = Instant::now();
        for (chief_running, chief_exits) in [(false, false), (true, false), (true, true)] {
            let (server, mut client) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let (hooks, received) = std::sync::mpsc::channel();
            let (control, _) = std::sync::mpsc::channel();
            let hub = Arc::new(Mutex::new(Hub { chief_running, ..Hub::default() }));
            let server_hub = hub.clone();
            let handler = std::thread::spawn(move || {
                let governor = Arc::new(crate::governor::Governor::new(Default::default(), Default::default()));
                handle_with_wait_duration(
                    server,
                    &server_hub,
                    &Channels {
                        hooks: hooks.into(),
                        control: control.into(),
                        governor,
                        usage: std::sync::mpsc::channel().0,
                    },
                    &Window::default(),
                    |seconds| Duration::from_millis(seconds * 5),
                )
                .unwrap();
            });
            let requested_at = Instant::now();
            writeln!(
                client,
                "{}",
                json!({"pane":"review", "payload":{
                    "hook_event_name":"PermissionRequest", "session_id":"review"
                }})
            )
            .unwrap();
            let pending = received.recv_timeout(Duration::from_secs(2)).unwrap().approval_reply.unwrap();
            if chief_running {
                // Beyond the ordinary 10 s fallback and the former 12 s client timeout.
                std::thread::sleep(Duration::from_millis(65));
                assert!(!pending.expired(), "a running chief must keep the approval open");
            }
            if chief_exits {
                hub.lock().unwrap().chief_running = false;
            }
            let answerable = chief_running && !chief_exits;
            if answerable {
                assert!(pending.answer(HookReply { stdout: Some("reviewed".into()) }));
            }
            let mut line = String::new();
            BufReader::new(client).read_line(&mut line).unwrap();
            if !answerable {
                assert!(requested_at.elapsed() >= Duration::from_millis(50), "fallback must wait its timeout");
                assert!(pending.expired());
                assert!(!pending.answer(HookReply { stdout: Some("late".into()) }));
            }
            assert_eq!(
                serde_json::from_str::<HookReply>(&line).unwrap().stdout.as_deref(),
                answerable.then_some("reviewed")
            );
            handler.join().unwrap();
        }
        assert!(started.elapsed() < Duration::from_secs(1), "scaled permission waits must stay fast");
    }

    #[test]
    fn context_and_queued_messages_reach_only_the_main_thread() {
        let mut hub = Hub {
            session_context: "This session runs inside ATOMICS.".into(),
            external_prefix: "From ATOMICS:".into(),
            ..Hub::default()
        };
        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);
        for thread in [HookThread::Background, HookThread::Spawned] {
            for name in ["SessionStart", "PostToolUse"] {
                let (reply, delivered) = hub.reply(&HookEvent { thread, ..event(name) });
                assert!(reply.stdout.is_none(), "{thread:?} {name}");
                assert_eq!(delivered, 0, "{thread:?} {name}");
            }
        }
        let (reply, delivered) = hub.reply(&event("SessionStart"));
        assert_eq!(delivered, 1);
        let stdout = reply.stdout.unwrap();
        assert!(stdout.contains("runs inside ATOMICS") && stdout.contains("stop and run the tests"));
    }

    #[test]
    fn queued_messages_wait_for_a_deliverable_event() {
        let mut hub = Hub { external_prefix: "From ATOMICS:".into(), ..Hub::default() };
        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);

        let (reply, delivered) = hub.reply(&event("Notification"));
        assert!(reply.stdout.is_none());
        assert_eq!(delivered, 0);

        let (reply, delivered) = hub.reply(&event("PostToolUse"));
        assert_eq!(delivered, 1);
        let out: Value = serde_json::from_str(&reply.stdout.unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "From ATOMICS:\nstop and run the tests");
        assert!(hub.outbox.is_empty());
    }

    #[test]
    fn session_start_gets_context() {
        let mut hub = Hub { session_context: "You run in ATOMICS.".into(), ..Hub::default() };
        let (reply, _) = hub.reply(&event("SessionStart"));
        assert!(reply.stdout.unwrap().contains("You run in ATOMICS."));
        assert!(hub.reply(&event("Stop")).0.stdout.is_none());
    }
}
