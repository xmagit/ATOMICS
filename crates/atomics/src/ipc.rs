//! The app side of the hook bridge: a unix socket that answers `atomics hook` at once
//! from shared state, so a hidden or busy window never slows an agent down.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::host::Window;
use anyhow::{Result, bail};
use atomics_core::control::{ControlRequest, ControlResponse, describe_diff_page};
use atomics_core::hook::{HookEvent, HookReply, HookRequest, HookThread};
use serde_json::Value;

#[derive(Default)]
pub struct Hub {
    pub chief_running: bool,
    chief_hooks: HashMap<String, ChiefHook>,
    retired_chief_panes: HashSet<String>,
    retired_chief_roots: HashSet<String>,
    /// Messages waiting for sessions that run outside ATOMICS, by Claude session id.
    pub outbox: HashMap<String, Vec<String>>,
    pub(crate) attachments: HashMap<String, Vec<HookAttachment>>,
    pub session_context: String,
    pub external_prefix: String,
    /// ATOMICS's own line for a session's next prompt, by Claude session id: given once, with that prompt, in
    /// ATOMICS's name and not the user's (such as that the user writes from simple mode).
    pub prompt_context: HashMap<String, String>,
}

pub(crate) struct HookAttachment {
    pub pane: String,
    pub delivery: Arc<crate::app::attachments::DeliveryTicket>,
    pub message: String,
}

#[derive(Default)]
pub(crate) struct HookBatch {
    messages: Vec<String>,
    attachments: Vec<HookAttachment>,
}

impl HookBatch {
    fn len(&self) -> usize {
        self.messages.len() + self.attachments.len()
    }
}

pub type TraceAck = Sender<std::io::Result<()>>;

pub struct IncomingHook {
    pub trace_ack: Option<TraceAck>,
    pub message: HookMessage,
    /// Only attachment-bearing hooks wait for State to identify their recipient.
    pub recipient: Option<Sender<String>>,
}

struct ChiefHook {
    key: String,
    process: ProcessIdentity,
    executable: PathBuf,
    root: Option<String>,
    launch_root: Option<String>,
    preparing: bool,
    started: bool,
    instructions: Option<String>,
    instructions_delivered: bool,
    queued: usize,
    replying: usize,
}
/// How ATOMICS's own context line for a prompt is framed: never as the user's words.
pub const PROMPT_CONTEXT_PREFIX: &str = "From ATOMICS, not the user:";

pub struct HookMessage {
    pub pane: Option<String>,
    pub ancestors: Vec<u32>,
    pub event: HookEvent,
    pub delivered: Vec<Option<String>>,
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
        offset: usize,
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
    pub chief: Option<String>,
    pub(crate) peer: Option<ProcessIdentity>,
    pub reply: Sender<ControlOutcome>,
}

pub struct Channels {
    pub hooks: crate::host::Sender<IncomingHook>,
    pub control: crate::host::Sender<ControlMessage>,
    pub usage: Sender<crate::usage::Report>,
    pub governor: Arc<crate::governor::Governor>,
}

impl Hub {
    pub fn register_chief(
        &mut self,
        pane: &str,
        root: Option<String>,
        key: String,
        preparing: bool,
        process: ProcessIdentity,
        executable: PathBuf,
    ) {
        self.chief_hooks.insert(
            pane.into(),
            ChiefHook {
                key,
                process,
                executable: executable.canonicalize().unwrap_or(executable),
                root,
                launch_root: None,
                preparing,
                started: false,
                instructions: None,
                instructions_delivered: false,
                queued: 0,
                replying: 0,
            },
        );
    }

    pub fn retire_chief(&mut self, pane: &str, root: Option<&str>) {
        self.retired_chief_panes.insert(pane.into());
        let hook = self.chief_hooks.remove(pane);
        if let Some(root) = hook.and_then(|h| h.root).or_else(|| root.map(str::to_owned)) {
            self.retired_chief_roots.insert(root);
        }
    }

    pub fn retired_chief_pane(&self, pane: &str) -> bool {
        self.retired_chief_panes.contains(pane)
    }

    pub fn chief_hook_processed(&mut self, pane: &str) {
        if let Some(hook) = self.chief_hooks.get_mut(pane) {
            hook.queued = hook.queued.saturating_sub(1);
        }
    }

    #[cfg(test)]
    pub fn chief_registered(&self, pane: &str) -> bool {
        self.chief_hooks.contains_key(pane)
    }

    #[cfg(test)]
    pub fn queued_chief_hooks(&self, pane: &str) -> usize {
        self.chief_hooks.get(pane).map_or(0, |hook| hook.queued)
    }

    fn chief_hook_replied(&mut self, pane: &str) {
        if let Some(hook) = self.chief_hooks.get_mut(pane) {
            hook.replying = hook.replying.saturating_sub(1);
        }
    }

    /// Admission and reply completion share this lock with retirement.
    pub fn chief_hooks_drained(&self, old: Option<&str>, new: &str) -> bool {
        [old, Some(new)]
            .into_iter()
            .flatten()
            .all(|pane| self.chief_hooks.get(pane).is_none_or(|h| h.queued == 0 && h.replying == 0))
    }

    pub fn promote_chief(&mut self, old: Option<(&str, Option<&str>)>, new: &str) {
        if let Some((pane, root)) = old {
            self.retire_chief(pane, root);
        }
        if let Some(hook) = self.chief_hooks.get_mut(new) {
            hook.preparing = false;
        }
    }

    pub fn active_chief(&self, pane: &str) -> bool {
        self.chief_hooks.get(pane).is_some_and(|hook| !hook.preparing && hook.process.is_current())
    }

    pub fn set_chief_instructions(&mut self, pane: &str, instructions: String) {
        if let Some(hook) = self.chief_hooks.get_mut(pane) {
            hook.instructions = Some(instructions);
            hook.instructions_delivered = false;
        }
    }

    fn chief_caller(&self, peer: Option<ProcessIdentity>) -> Option<String> {
        let peer = peer.filter(|peer| peer.is_current())?;
        let mut process = peer;
        for _ in 0..64 {
            if let Some((pane, _)) = self.chief_hooks.iter().find(|(_, hook)| hook.process == process) {
                return (process.is_current() && peer.is_current()).then(|| pane.clone());
            }
            process = process.parent()?;
        }
        None
    }

    /// Returns whether an accepted chief hook belongs to the replacement still starting up.
    /// Ordinary task hooks follow the existing pane and conversation routing.
    fn admit_hook(&mut self, request: &HookRequest, event: &HookEvent, peer: Option<ProcessIdentity>) -> Option<bool> {
        let pane = request.pane.as_deref();
        if let Some(hook) = pane.and_then(|pane| self.chief_hooks.get_mut(pane)) {
            // Generated hooks exec from the provider, so a shell command or nested CLI is at least
            // one generation too far away. The inherited key alone cannot prove root origin.
            if request.chief_key.as_ref() != Some(&hook.key)
                || !peer.is_some_and(|pid| {
                    (pid == hook.process
                        || pid.parent() == Some(hook.process)
                            && process_executable(pid.pid).as_ref() == Some(&hook.executable))
                        && pid.is_current()
                        && hook.process.is_current()
                })
                || !hook.started && event.name != "SessionStart"
                || !chief_root_event(hook.root.as_deref(), event, hook.started && !hook.preparing)
            {
                return None;
            }
            if let Some(previous) = hook.root.replace(event.session_id.clone())
                && previous != event.session_id
            {
                self.retired_chief_roots.insert(previous);
            }
            if !hook.started {
                hook.launch_root = Some(event.session_id.clone());
            }
            hook.started = true;
            hook.queued += 1;
            hook.replying += 1;
            return Some(hook.preparing);
        }
        if request.chief_key.is_some()
            || pane.is_some_and(|pane| self.retired_chief_pane(pane))
            || self.retired_chief_roots.contains(&event.session_id)
            || self.chief_hooks.values().any(|hook| hook.root.as_deref() == Some(event.session_id.as_str()))
        {
            return None;
        }
        Some(false)
    }

    /// The stdout for an admitted hook. A replacement gets its own instructions while it prepares.
    fn reply_for_hook(
        &mut self,
        event: &HookEvent,
        chief_pane: Option<&str>,
        preparing: bool,
        recipient: Option<&str>,
    ) -> (HookReply, HookBatch) {
        let mut parts = Vec::new();
        let mut delivered = HookBatch::default();
        if !preparing {
            if let Some(warning) = event.heavy_check_warning() {
                parts.push(warning.to_string());
            }
            if event.name == "SessionStart" && event.thread == HookThread::Main && !self.session_context.is_empty() {
                parts.push(self.session_context.clone());
            }
            if event.name == "UserPromptSubmit"
                && event.thread == HookThread::Main
                && event.takes_context()
                && let Some(line) = self.prompt_context.remove(&event.session_id)
            {
                parts.push(format!("{PROMPT_CONTEXT_PREFIX} {line}"));
            }
            if event.can_deliver() {
                delivered.messages = self.outbox.remove(&event.session_id).unwrap_or_default();
                if let Some(pane) = recipient
                    && let Some(queue) = self.attachments.get_mut(&event.session_id)
                {
                    delivered.attachments =
                        queue.extract_if(.., |a| a.pane == pane).filter(|a| a.delivery.start_write()).collect();
                    if queue.is_empty() {
                        self.attachments.remove(&event.session_id);
                    }
                }
            }
        }
        if matches!(event.name.as_str(), "SessionStart" | "UserPromptSubmit" | "PostToolUse")
            && event.takes_context()
            && let Some(hook) = chief_pane.and_then(|pane| self.chief_hooks.get_mut(pane))
            && (!hook.instructions_delivered || event.name == "SessionStart")
            && let Some(instructions) = &hook.instructions
        {
            parts.push(instructions.clone());
            hook.instructions_delivered = true;
        }
        for message in delivered.messages.iter().chain(delivered.attachments.iter().map(|a| &a.message)) {
            parts.push(format!("{}\n{message}", self.external_prefix));
        }
        let stdout = (!parts.is_empty()).then(|| event.context_output(&parts.join("\n\n"))).flatten();
        (HookReply { stdout }, delivered)
    }

    fn finish_delivery(&mut self, conversation: &str, mut batch: HookBatch, sent: bool) -> Vec<Option<String>> {
        if sent {
            let mut ids = vec![None; batch.messages.len()];
            ids.extend(batch.attachments.into_iter().map(|item| {
                item.delivery.received();
                Some(item.delivery.id.clone())
            }));
            return ids;
        }
        if !batch.messages.is_empty() {
            let next = self.outbox.entry(conversation.into()).or_default();
            batch.messages.append(next);
            *next = batch.messages;
        }
        if !batch.attachments.is_empty() {
            for item in &batch.attachments {
                item.delivery.retry();
            }
            let next = self.attachments.entry(conversation.into()).or_default();
            batch.attachments.append(next);
            *next = batch.attachments;
        }
        Vec::new()
    }

    #[cfg(test)]
    pub(crate) fn reply(&mut self, event: &HookEvent, recipient: Option<&str>) -> (HookReply, HookBatch) {
        self.reply_for_hook(event, None, false, recipient)
    }
}

pub fn chief_root_event(root: Option<&str>, event: &HookEvent, can_transition: bool) -> bool {
    event.thread == HookThread::Main
        && match root {
            Some(root) => {
                root == event.session_id
                    || can_transition
                        && event.name == "SessionStart"
                        && matches!(event.start_source.as_deref(), Some("clear" | "resume"))
                        && safe_chief_root_id(&event.session_id)
            }
            None => event.name == "SessionStart" && safe_chief_root_id(&event.session_id),
        }
}

fn peer_pid(stream: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "macos")]
    let (level, name, mut credential) = (libc::SOL_LOCAL, libc::LOCAL_PEERPID, 0 as libc::pid_t);
    #[cfg(target_os = "linux")]
    // SAFETY: ucred consists solely of integer fields.
    let (level, name, mut credential) =
        (libc::SOL_SOCKET, libc::SO_PEERCRED, unsafe { std::mem::zeroed::<libc::ucred>() });
    let size = std::mem::size_of_val(&credential) as libc::socklen_t;
    let mut len = size;
    // SAFETY: getsockopt may write at most len bytes into credential.
    if unsafe { libc::getsockopt(stream.as_raw_fd(), level, name, (&raw mut credential).cast(), &mut len) } != 0
        || len != size
    {
        return None;
    }
    #[cfg(target_os = "linux")]
    let credential = credential.pid;
    u32::try_from(credential).ok().filter(|pid| *pid > 0)
}

fn connected_peer(stream: &UnixStream) -> Option<ProcessIdentity> {
    #[cfg(target_os = "linux")]
    let peer_fd = {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut fd: libc::c_int = -1;
        let mut len = std::mem::size_of_val(&fd) as libc::socklen_t;
        // SAFETY: SO_PEERPIDFD writes a new descriptor to the supplied int.
        let status = unsafe {
            libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERPIDFD, (&raw mut fd).cast(), &mut len)
        };
        if status == 0 {
            // SAFETY: a successful getsockopt transfers ownership to this process.
            Some(unsafe { OwnedFd::from_raw_fd(fd) })
        } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOPROTOOPT) {
            None
        } else {
            return None;
        }
    };
    let peer = ProcessIdentity::of(peer_pid(stream)?)?;
    #[cfg(target_os = "linux")]
    if let Some(fd) = peer_fd {
        // The PID alone can already have been recycled if the connecting process exited.
        let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: poll points at one valid pollfd, and a zero timeout never blocks.
        if unsafe { libc::poll(&raw mut poll, 1, 0) } != 0 {
            return None;
        }
    }
    Some(peer)
}

fn process_executable(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut bytes = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: proc_pidpath may write up to the supplied size.
        let len = unsafe { libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
        if len <= 0 {
            return None;
        }
        let path = std::ffi::CStr::from_bytes_until_nul(&bytes).ok()?;
        Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())).canonicalize().ok()
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
}

/// The start time is stable across exec and changes if the kernel recycles the PID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pid: u32,
    started: (u64, u64),
}

impl ProcessIdentity {
    pub(crate) fn of(pid: u32) -> Option<Self> {
        let (_, started) = process_info(pid)?;
        Some(Self { pid, started })
    }

    fn is_current(self) -> bool {
        Self::of(self.pid) == Some(self)
    }

    fn parent(self) -> Option<Self> {
        let (parent, started) = process_info(self.pid)?;
        if started != self.started || parent <= 1 || parent == self.pid {
            return None;
        }
        let parent = Self::of(parent)?;
        // Require both ends of this relationship to still be the processes we read.
        (process_info(self.pid) == Some((parent.pid, started)) && parent.is_current()).then_some(parent)
    }

    pub(crate) fn ancestry(self) -> impl Iterator<Item = Self> {
        std::iter::successors(self.is_current().then_some(self), |process| process.parent()).take(64)
    }
}

fn process_info(pid: u32) -> Option<(u32, (u64, u64))> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: proc_bsdinfo is an integer and byte buffer accepted by proc_pidinfo.
        let mut info = unsafe { std::mem::zeroed::<libc::proc_bsdinfo>() };
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let read = unsafe { libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        (read == size).then_some((info.pbi_ppid, (info.pbi_start_tvsec, info.pbi_start_tvusec)))
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The process name may contain spaces and parentheses.
        let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
        let parent = fields.nth(1)?.parse().ok()?;
        let started = fields.nth(17)?.parse().ok()?;
        Some((parent, (started, 0)))
    }
}

pub fn safe_chief_root_id(id: &str) -> bool {
    id.len() <= 128
        && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

/// Serializes stale-socket recovery and owns the socket name until runtime shutdown.
/// The lock file stays in place: unlinking it would let two processes lock different inodes.
#[derive(Debug)]
pub struct SocketGuard {
    lock: std::fs::File,
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
        // A concurrent fork can briefly hold a copy of this descriptor before exec.
        // Explicit unlock releases ownership even if that copy has not closed yet.
        unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) };
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
    let guard = SocketGuard { lock, path: path.to_path_buf(), device: meta.dev(), inode: meta.ino() };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, guard))
}

/// Binds the socket, refusing to steal it from another running ATOMICS.
pub fn serve(path: &Path, hub: Arc<Mutex<Hub>>, channels: Channels, window: Window) -> Result<SocketGuard> {
    let (listener, guard) = bind(path)?;
    std::thread::Builder::new().name("atomics-hooks".into()).spawn(move || {
        let channels = Arc::new(channels);
        for stream in listener.incoming().flatten() {
            // Take the connection's process snapshot before another thread can delay admission.
            let peer = connected_peer(&stream);
            let (hub, channels, window) = (hub.clone(), channels.clone(), window.clone());
            std::thread::spawn(move || {
                if let Err(err) = handle_with_wait_duration(stream, peer, &hub, &channels, &window, Duration::from_secs)
                {
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

#[cfg(test)]
fn handle(stream: UnixStream, hub: &Mutex<Hub>, channels: &Channels, window: &Window) -> Result<()> {
    let peer = connected_peer(&stream);
    handle_with_wait_duration(stream, peer, hub, channels, window, Duration::from_secs)
}

// Keep timeout selection identical when tests scale seconds down to milliseconds.
fn handle_with_wait_duration(
    stream: UnixStream,
    peer: Option<ProcessIdentity>,
    hub: &Mutex<Hub>,
    channels: &Channels,
    window: &Window,
    wait_duration: impl Fn(u64) -> Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut value = read_request(BufReader::new(&stream))?;
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
    if value["cmd"] == "vm_stop_guard" {
        if let Err(error) = channels.governor.begin_restart(true) {
            writeln!(&stream, "{error}")?;
            return Ok(());
        }
        // A dropped ssh connection, a failed check, or the end of the VM releases this socket.
        let held = (|| -> std::io::Result<()> {
            stream.set_read_timeout(None)?;
            writeln!(&stream, "admissions-paused")?;
            std::io::Read::read(&mut &stream, &mut [0])?;
            Ok(())
        })();
        channels.governor.cancel_restart();
        held?;
        return Ok(());
    }
    if value.get("cmd").and_then(Value::as_str) == Some("queue") {
        if let Err(error) = channels.governor.serve(&stream, &value) {
            writeln!(&stream, "{}", serde_json::json!({ "error": error.to_string() }))?;
        }
        return Ok(());
    }
    if value.get("cmd").is_some() {
        let claimed = value.as_object_mut().and_then(|fields| fields.remove("chief_scope"));
        let (chief, launch_root) = {
            let hub = hub.lock().unwrap();
            let pane = (!hub.chief_hooks.is_empty()).then(|| hub.chief_caller(peer)).flatten();
            let root = pane.as_ref().and_then(|id| hub.chief_hooks.get(id)).and_then(|hook| hook.launch_root.clone());
            (pane, root)
        };
        // A scoped provider without per-call metadata can look up only the first conversation
        // on its own launch. After a clear the host rejects that stale conversation.
        if value["cmd"] == "attachments" && value["conversation"].is_null() && claimed.is_some() {
            value["conversation"] = serde_json::json!(launch_root);
        }
        let response = match serde_json::from_value::<ControlRequest>(value) {
            Ok(_) if claimed.as_ref().is_some_and(|claim| claim.as_str() != chief.as_deref() || chief.is_none()) => {
                ControlResponse::error("The tool call is not from its chief launch")
            }
            Ok(ControlRequest::QueueStatus) => ControlResponse::ok(channels.governor.status()),
            Ok(ControlRequest::FolderTrust { .. }) if peer.is_none() => {
                ControlResponse::error("Could not verify the local folder-access caller")
            }
            Ok(request) => control(request, chief, peer, channels, window, &stream),
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
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut state = hub.lock().unwrap();
    let Some(preparing) = state.admit_hook(&request, &event, peer) else { return Ok(()) };
    let chief_pane = request.chief_key.as_ref().and(request.pane.clone());
    let approval_reply = (event.name == "PermissionRequest").then(|| Arc::new(ApprovalReply::default()));
    let conversation = event.session_id.clone();
    let mut message = Some(HookMessage {
        pane: request.pane,
        ancestors: request.ancestors,
        event: event.clone(),
        delivered: Vec::new(),
        approval_reply: approval_reply.clone(),
    });
    let awaiting_recipient = !preparing && event.can_deliver() && state.attachments.contains_key(&conversation);
    let result = (|| -> Result<()> {
        // Dispatch chief hooks in admission order, even when a reply must wait for persistence.
        let recipient = if chief_pane.is_some() || awaiting_recipient || event.is_trace_call() {
            let (tx, rx) = std::sync::mpsc::channel();
            let (ack, saved) = std::sync::mpsc::channel();
            if channels
                .hooks
                .send(IncomingHook {
                    message: message.take().unwrap(),
                    recipient: awaiting_recipient.then_some(tx),
                    trace_ack: event.is_trace_call().then_some(ack),
                })
                .is_err()
            {
                if let Some(pane) = &chief_pane {
                    state.chief_hook_processed(pane);
                }
                bail!("Hook runtime stopped");
            }
            window.request_repaint();
            if awaiting_recipient || event.is_trace_call() {
                drop(state);
                let recipient = rx.recv_timeout(Duration::from_secs(2)).ok();
                if let Ok(result) = saved.recv() {
                    result?;
                }
                state = hub.lock().unwrap();
                recipient
            } else {
                None
            }
        } else {
            None
        };
        let (mut reply, batch) = state.reply_for_hook(&event, chief_pane.as_deref(), preparing, recipient.as_deref());
        drop(state);
        if batch.len() > 0 {
            let result = writeln!(&stream, "{}", serde_json::to_string(&reply)?);
            let delivered = hub.lock().unwrap().finish_delivery(&conversation, batch, result.is_ok());
            if let Some(mut message) = message {
                message.delivered = delivered;
                let _ = channels.hooks.send(IncomingHook { message, recipient: None, trace_ack: None });
            }
            window.request_repaint();
            return Ok(result?);
        }
        if let Some(message) = message {
            channels
                .hooks
                .send(IncomingHook { message, recipient: None, trace_ack: None })
                .map_err(|_| anyhow::anyhow!("Hook runtime stopped"))?;
            window.request_repaint();
        }
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
        writeln!(&stream, "{}", serde_json::to_string(&reply)?)?;
        Ok(())
    })();
    if let Some(pane) = chief_pane {
        hub.lock().unwrap().chief_hook_replied(&pane);
        window.request_repaint();
    }
    result
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

fn control(
    request: ControlRequest,
    chief: Option<String>,
    peer: Option<ProcessIdentity>,
    channels: &Channels,
    window: &Window,
    stream: &UnixStream,
) -> ControlResponse {
    let (reply, outcome) = std::sync::mpsc::channel();
    if channels.control.send(ControlMessage { request, chief, peer, reply }).is_err() {
        return ControlResponse::error("ATOMICS is shutting down");
    }
    window.request_repaint();
    match outcome.recv_timeout(Duration::from_secs(300)) {
        Ok(ControlOutcome::Done(response)) => response,
        Ok(ControlOutcome::Diff { root, base_ref, patch, path, offset, session }) => {
            match atomics_core::git::diff_file_page(
                &root,
                &base_ref,
                None,
                path.as_deref(),
                offset,
                atomics_core::control::MAX_DIFF_FILES,
            ) {
                Ok((diff, next)) => {
                    let mut summary = describe_diff_page(&diff, patch, path.as_deref(), 0);
                    if let Some(next) = next {
                        summary.push_str(&format!("Next file page: offset={next}\n"));
                    }
                    ControlResponse::ok(serde_json::json!({
                        "session": session,
                        "root": root,
                        "diff": summary,
                    }))
                }
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

/// Guest side of the VM stop guard. EOF on the updater's pipe releases admission.
pub fn hold_admissions() -> Result<()> {
    let mut stream = UnixStream::connect(atomics_core::paths::socket_path())?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.write_all(b"{\"cmd\":\"vm_stop_guard\"}\n")?;
    let answer = read_line_bounded(BufReader::new(&stream), 1024)?;
    if answer != b"admissions-paused\n" {
        bail!("host could not pause admissions: {}", String::from_utf8_lossy(&answer).trim());
    }
    stream.set_read_timeout(None)?;
    let release = stream.try_clone()?;
    std::thread::Builder::new().name("vm-stop-guard".into()).spawn(move || {
        let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0]);
        let _ = release.shutdown(std::net::Shutdown::Write);
    })?;
    println!("admissions-paused");
    // Also exit if the host disappears while the updater still holds stdin open.
    std::io::Read::read(&mut stream, &mut [0])?;
    Ok(())
}

/// Sends one control request to a running ATOMICS (for `atomics ctl` and `atomics mcp`).
pub fn request(request: &ControlRequest) -> Result<ControlResponse> {
    request_with_scope(request, None)
}

pub fn request_with_scope(request: &ControlRequest, chief: Option<&str>) -> Result<ControlResponse> {
    request_at_with_scope(&atomics_core::paths::socket_path(), request, chief).map_err(|error| match error {
        RequestError::NotRunning(message) => anyhow::anyhow!(message),
        RequestError::Failed(error) => error,
    })
}

pub fn request_at(path: &Path, request: &ControlRequest) -> Result<ControlResponse, RequestError> {
    request_at_with_scope(path, request, None)
}

fn request_at_with_scope(
    path: &Path,
    request: &ControlRequest,
    chief: Option<&str>,
) -> Result<ControlResponse, RequestError> {
    let stream = UnixStream::connect(path)
        .map_err(|e| RequestError::NotRunning(format!("ATOMICS is not running ({}: {e})", path.display())))?;
    // A sandbox command may wait its timeout for a slot, then run for as long.
    let timeout = match request {
        ControlRequest::Coordination { .. } => 3,
        ControlRequest::SecretRemoved { .. } => 5,
        ControlRequest::Exec { timeout_secs, .. } => 2 * crate::sandbox::exec::timeout(*timeout_secs).as_secs() + 120,
        _ => 320,
    };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    let mut writer = &stream;
    let mut payload = serde_json::to_value(request)?;
    if let Some(chief) = chief {
        payload["chief_scope"] = serde_json::json!(chief);
    }
    writer.write_all(serde_json::to_string(&payload)?.as_bytes())?;
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
                owner: session.clone(),
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
    fn chief_host_can_reopen_its_socket_with_a_copied_lock_descriptor() {
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("chief.sock");
        let (_listener, owner) = bind(&socket).unwrap();
        let inherited = owner.lock.try_clone().unwrap();
        drop(owner);
        let (_new_listener, _new_owner) = bind(&socket).unwrap();
        drop(inherited);
    }

    #[test]
    fn chief_hook_and_scope_reject_another_process_with_the_registered_pid() {
        let mut hub = Hub::default();
        let peer = ProcessIdentity::of(std::process::id()).unwrap();
        let replaced = ProcessIdentity { started: (peer.started.0.wrapping_add(1), peer.started.1), ..peer };
        hub.register_chief("chief", None, "fixture-key".into(), false, replaced, std::env::current_exe().unwrap());
        let payload = serde_json::json!({"hook_event_name": "SessionStart", "session_id": "fixture-thread"});
        let event = HookEvent::from_payload(&payload).unwrap();
        let request = HookRequest {
            pane: Some("chief".into()),
            ancestors: Vec::new(),
            chief_key: Some("fixture-key".into()),
            payload,
        };
        assert_eq!(hub.admit_hook(&request, &event, Some(peer)), None);
        assert_eq!(hub.chief_caller(Some(peer)), None);
        assert!(!hub.active_chief("chief"));
    }

    #[test]
    fn chief_socket_worker_uses_the_accept_snapshot() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let captured = connected_peer(&server).expect("peer identity at accept");
        assert_eq!(captured, ProcessIdentity::of(std::process::id()).unwrap());
        let stale = ProcessIdentity { started: (captured.started.0.wrapping_add(1), captured.started.1), ..captured };
        let mut hub = Hub::default();
        hub.register_chief("chief", None, "fixture-key".into(), false, captured, std::env::current_exe().unwrap());
        let (hooks, received) = std::sync::mpsc::channel();
        let channels = Channels {
            hooks: hooks.into(),
            control: std::sync::mpsc::channel().0.into(),
            usage: std::sync::mpsc::channel().0,
            governor: Arc::new(crate::governor::Governor::new(Default::default(), Default::default())),
        };
        writeln!(
            client,
            "{}",
            json!({"pane":"chief", "chief_key":"fixture-key", "payload":{
                "hook_event_name":"SessionStart", "session_id":"fixture-thread"
            }})
        )
        .unwrap();
        handle_with_wait_duration(
            server,
            Some(stale),
            &Mutex::new(hub),
            &channels,
            &Window::default(),
            Duration::from_secs,
        )
        .unwrap();
        assert!(received.try_recv().is_err(), "the worker resampled a stale peer");
    }

    #[test]
    fn chief_socket_rejects_foreign_and_retired_hooks_before_replying() {
        let (hooks, received) = std::sync::mpsc::channel();
        let channels = Channels {
            hooks: hooks.into(),
            control: std::sync::mpsc::channel().0.into(),
            usage: std::sync::mpsc::channel().0,
            governor: Arc::new(crate::governor::Governor::new(Default::default(), Default::default())),
        };
        let mut hub = Hub { session_context: "host context".into(), ..Hub::default() };
        hub.register_chief(
            "old",
            None,
            "fixture-key".into(),
            false,
            ProcessIdentity::of(std::process::id()).unwrap(),
            std::env::current_exe().unwrap(),
        );
        hub.register_chief(
            "new",
            Some("s1".into()),
            "replacement-key".into(),
            true,
            ProcessIdentity::of(std::process::id()).unwrap(),
            std::env::current_exe().unwrap(),
        );
        let hub = Mutex::new(hub);
        let send = |pane: Option<&str>, key: Option<&str>, mut event: Value| {
            let (server, mut client) = UnixStream::pair().unwrap();
            event["session_id"] = json!("s1");
            writeln!(client, "{}", json!({ "pane":pane, "chief_key":key, "payload":event })).unwrap();
            handle(server, &hub, &channels, &Window::default()).unwrap();
            let mut line = String::new();
            BufReader::new(client).read_line(&mut line).unwrap();
            line
        };
        let start = json!({"hook_event_name":"SessionStart"});
        assert!(send(Some("old"), None, start.clone()).is_empty());
        assert!(send(Some("old"), Some("fixture-key"), start.clone()).contains("host context"));
        assert!(!hub.lock().unwrap().chief_hooks_drained(Some("old"), "new"));
        assert_eq!(received.try_recv().unwrap().message.event.name, "SessionStart");
        hub.lock().unwrap().chief_hook_processed("old");
        assert!(hub.lock().unwrap().chief_hooks_drained(Some("old"), "new"));
        hub.lock().unwrap().outbox.insert("s1".into(), vec!["queued".into()]);
        assert!(send(None, None, start.clone()).is_empty());
        assert!(send(Some("unknown"), None, start.clone()).is_empty());
        assert!(
            send(
                Some("old"),
                Some("fixture-key"),
                json!({
                    "hook_event_name":"PostToolUse", "agent_id":"spawned",
                })
            )
            .is_empty()
        );
        hub.lock().unwrap().promote_chief(Some(("old", Some("s1"))), "new");
        assert!(send(Some("old"), Some("fixture-key"), start).is_empty());
        assert!(received.try_recv().is_err());
        assert_eq!(hub.lock().unwrap().outbox["s1"], ["queued"]);
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
        let pending = received.recv_timeout(Duration::from_secs(2)).unwrap().message.approval_reply.unwrap();
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
                    None,
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
            let pending = received.recv_timeout(Duration::from_secs(2)).unwrap().message.approval_reply.unwrap();
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
                let (reply, delivered) = hub.reply(&HookEvent { thread, ..event(name) }, None);
                assert!(reply.stdout.is_none(), "{thread:?} {name}");
                assert_eq!(delivered.len(), 0, "{thread:?} {name}");
            }
        }
        let (reply, delivered) = hub.reply(&event("SessionStart"), None);
        assert_eq!(delivered.len(), 1);
        let stdout = reply.stdout.unwrap();
        assert!(stdout.contains("runs inside ATOMICS") && stdout.contains("stop and run the tests"));
    }

    #[test]
    fn attachment_failed_hook_write_retries_the_same_delivery() {
        let mut hub = Hub::default();
        hub.outbox.insert("s1".into(), vec!["first".into()]);
        let (_, messages) = hub.reply(&event("Stop"), None);
        hub.outbox.insert("s1".into(), vec!["second".into()]);
        let ids = hub.finish_delivery("s1", messages, false);
        assert!(ids.is_empty());
        let (retry, _) = hub.reply(&event("PostToolUse"), None);
        let text = retry.stdout.unwrap();
        assert!(text.find("first").unwrap() < text.find("second").unwrap());
    }

    #[test]
    fn atomics_own_prompt_context_comes_once_with_the_next_prompt_of_the_main_thread() {
        let mut hub = Hub { external_prefix: "Message from the user:".into(), ..Hub::default() };
        hub.prompt_context.insert("s1".into(), "The user is in simple mode.".into());
        let reply = |hub: &mut Hub, name: &str, thread| hub.reply(&HookEvent { thread, ..event(name) }, None).0.stdout;
        assert_eq!(reply(&mut hub, "PostToolUse", HookThread::Main), None, "not before the prompt");
        assert_eq!(reply(&mut hub, "UserPromptSubmit", HookThread::Spawned), None, "nor to a subagent's");
        let out: Value = serde_json::from_str(&reply(&mut hub, "UserPromptSubmit", HookThread::Main).unwrap()).unwrap();
        let context = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert_eq!(context, "From ATOMICS, not the user: The user is in simple mode.");
        assert!(!context.contains("Message from the user"));
        assert_eq!(reply(&mut hub, "UserPromptSubmit", HookThread::Main), None, "once");
    }

    /// Cursor reads `additional_context`, and a permission hook's output outside its schema may block the call:
    /// the heavy-check warning on its `preToolUse` comes in its shape, never Claude's. Copilot reads a flat
    /// `additionalContext`, and nothing at its agentStop, so a queued message waits for an event that carries it.
    #[test]
    fn copilot_and_cursor_replies_come_in_their_own_shape() {
        let mut hub = Hub {
            session_context: "You run in ATOMICS.".into(),
            external_prefix: "From ATOMICS:".into(),
            ..Hub::default()
        };
        let cursor = |name: &str, input: Value| {
            HookEvent::from_payload(&json!({"hook_event_name": name, "conversation_id": "s1",
                "cursor_version": "2026.11.22-e9042b2", "tool_name": "Shell", "tool_input": input}))
            .unwrap()
        };
        let build = cursor("preToolUse", json!({"command": "cargo test --workspace"}));
        assert!(build.heavy_check_warning().is_some());
        let out: Value = serde_json::from_str(&hub.reply(&build, None).0.stdout.unwrap()).unwrap();
        assert!(out.get("hookSpecificOutput").is_none() && out.get("permission").is_none(), "{out}");
        assert!(out["additional_context"].as_str().unwrap().starts_with("Heavy check outside the queue"));
        let start: Value =
            serde_json::from_str(&hub.reply(&cursor("sessionStart", json!({})), None).0.stdout.unwrap()).unwrap();
        assert_eq!(start, json!({"additional_context": "You run in ATOMICS."}));

        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);
        let (reply, delivered) = hub.reply(&cursor("stop", json!({})), None);
        assert!(reply.stdout.is_none() && delivered.messages.is_empty(), "Cursor's stop carries no context");
        let copilot = |name: &str| {
            HookEvent::from_payload(&json!({"hook_event_name": name, "session_id": "s1", "cwd": "/w",
                "atomics_provider": "copilot"}))
            .unwrap()
        };
        let (reply, delivered) = hub.reply(&copilot("Stop"), None);
        assert!(reply.stdout.is_none() && delivered.messages.is_empty(), "Copilot's agentStop takes no context");
        let (reply, delivered) = hub.reply(&copilot("PostToolUse"), None);
        assert_eq!(delivered.messages.len(), 1);
        let out: Value = serde_json::from_str(&reply.stdout.unwrap()).unwrap();
        assert_eq!(out, json!({"additionalContext": "From ATOMICS:\nstop and run the tests"}));
    }

    #[test]
    fn queued_messages_wait_for_a_deliverable_event() {
        let mut hub = Hub { external_prefix: "From ATOMICS:".into(), ..Hub::default() };
        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);

        let (reply, delivered) = hub.reply(&event("Notification"), None);
        assert!(reply.stdout.is_none());
        assert_eq!(delivered.len(), 0);

        let (reply, delivered) = hub.reply(&event("PostToolUse"), None);
        assert_eq!(delivered.len(), 1);
        let out: Value = serde_json::from_str(&reply.stdout.unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "From ATOMICS:\nstop and run the tests");
        assert!(hub.outbox.is_empty());
    }

    #[test]
    fn session_start_gets_context() {
        let mut hub = Hub { session_context: "You run in ATOMICS.".into(), ..Hub::default() };
        let (reply, _) = hub.reply(&event("SessionStart"), None);
        assert!(reply.stdout.unwrap().contains("You run in ATOMICS."));
        assert!(hub.reply(&event("Stop"), None).0.stdout.is_none());
    }
}
