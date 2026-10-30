//! The app side of the hook bridge: a unix socket that answers `imacs hook` at once
//! from shared state, so a hidden or busy window never slows an agent down.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use imacs_core::control::{ControlRequest, ControlResponse, describe_diff};
use imacs_core::hook::{HookEvent, HookReply, HookRequest, context_output};
use serde_json::Value;

#[derive(Default)]
pub struct Hub {
    pub chief_running: bool,
    /// Messages waiting for sessions that run outside IMACS, by Claude session id.
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
        self.wait_with_timeout(|| timeout)
    }

    fn wait_with_timeout(&self, timeout: impl Fn() -> Duration) -> HookReply {
        let started = Instant::now();
        let mut state = self.state.lock().unwrap();
        while matches!(*state, ApprovalState::Waiting) {
            let remaining = timeout().saturating_sub(started.elapsed());
            if remaining.is_zero() {
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
        if event.name == "SessionStart" && !self.session_context.is_empty() {
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

/// Binds the socket, refusing to steal it from another running IMACS.
pub fn serve(path: &Path, hub: Arc<Mutex<Hub>>, channels: Channels, ctx: egui::Context) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another IMACS is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let path: PathBuf = path.to_path_buf();
    std::thread::Builder::new().name("imacs-hooks".into()).spawn(move || {
        let channels = Arc::new(channels);
        for stream in listener.incoming().flatten() {
            let (hub, channels, ctx) = (hub.clone(), channels.clone(), ctx.clone());
            std::thread::spawn(move || {
                if let Err(err) = handle(stream, &hub, &channels, &ctx) {
                    eprintln!("IMACS: hook connection failed: {err:#}");
                }
            });
        }
        let _ = std::fs::remove_file(path);
    })?;
    Ok(())
}

fn handle(stream: UnixStream, hub: &Mutex<Hub>, channels: &Channels, ctx: &egui::Context) -> Result<()> {
    handle_with_wait_duration(stream, hub, channels, ctx, Duration::from_secs)
}

// Keep timeout selection identical when tests scale seconds down to milliseconds.
fn handle_with_wait_duration(
    stream: UnixStream,
    hub: &Mutex<Hub>,
    channels: &Channels,
    ctx: &egui::Context,
    wait_duration: impl Fn(u64) -> Duration,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let value: Value = serde_json::from_str(&line)?;
    if let Some(payload) = value.get("statusline") {
        if payload["rate_limits"].is_object() {
            let usage = imacs_core::usage::Usage::claude(payload, crate::usage::now());
            if channels.usage.send(crate::usage::Report::Claude(usage)).is_ok() {
                ctx.request_repaint();
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
            Ok(request) => control(request, channels, ctx, &stream),
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
    ctx.request_repaint();
    if let Some(approval_reply) = approval_reply {
        // After this window, leave Codex's native prompt available to the user.
        reply = if hub.lock().unwrap().chief_running {
            approval_reply.wait_with_timeout(|| {
                let seconds =
                    if hub.lock().unwrap().chief_running { imacs_core::hook::PERMISSION_WAIT_SECONDS } else { 10 };
                wait_duration(seconds)
            })
        } else {
            approval_reply.wait(wait_duration(10))
        };
    }
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(&reply)?.as_bytes())?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn control(request: ControlRequest, channels: &Channels, ctx: &egui::Context, stream: &UnixStream) -> ControlResponse {
    let (reply, outcome) = std::sync::mpsc::channel();
    if channels.control.send(ControlMessage { request, reply }).is_err() {
        return ControlResponse::error("IMACS is shutting down");
    }
    ctx.request_repaint();
    match outcome.recv_timeout(Duration::from_secs(300)) {
        Ok(ControlOutcome::Done(response)) => response,
        Ok(ControlOutcome::Diff { root, base_ref, patch, session }) => {
            match imacs_core::git::diff_against_base(&root, &base_ref, None) {
                Ok(diff) => ControlResponse::ok(serde_json::json!({
                    "session": session,
                    "root": root,
                    "diff": describe_diff(&diff, patch),
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
        Err(_) => ControlResponse::error("IMACS did not answer in time"),
    }
}

/// Sends one control request to a running IMACS (for `imacs ctl` and `imacs mcp`).
pub fn request(request: &ControlRequest) -> Result<ControlResponse> {
    let path = imacs_core::paths::socket_path();
    let stream =
        UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("IMACS is not running ({}: {e})", path.display()))?;
    // A sandbox command may wait its timeout for a slot, then run for as long.
    let timeout = match request {
        ControlRequest::Exec { timeout_secs, .. } => 2 * crate::sandbox::exec::timeout(*timeout_secs).as_secs() + 120,
        _ => 320,
    };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(request)?.as_bytes())?;
    writer.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Read;

    #[test]
    fn statusline_socket_routes_account_usage_without_a_session_or_lifecycle_event() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (hooks, hook_rx) = std::sync::mpsc::channel();
        let (control, _) = std::sync::mpsc::channel();
        let (usage, usage_rx) = std::sync::mpsc::channel();
        let governor = Arc::new(crate::governor::Governor::new(Default::default(), None));
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
            &egui::Context::default(),
        )
        .unwrap();
        let crate::usage::Report::Claude(usage) = usage_rx.try_recv().unwrap() else { panic!("claude report") };
        assert_eq!(usage.session.unwrap().remaining, 78.0);
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
        let governor = Arc::new(crate::governor::Governor::new(Default::default(), None));
        writeln!(client, "{}", json!({"statusline":{"session_id":"new-session"}})).unwrap();
        handle(
            server,
            &Mutex::new(Hub::default()),
            &Channels { hooks: hooks.into(), control: control.into(), usage, governor },
            &egui::Context::default(),
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
        std::fs::write(dir.path().join("sandboxes/imacs-s1"), "{}").unwrap();
        let workdir = dir.path().canonicalize().unwrap();
        let (mut client, server) = UnixStream::pair().unwrap();
        let (hooks, _) = std::sync::mpsc::channel();
        let (control, requests) = std::sync::mpsc::channel::<ControlMessage>();
        let (finished, then_ran) = std::sync::mpsc::channel();
        let governor = Arc::new(crate::governor::Governor::new(imacs_core::governor::ResourceConfig::default(), None));
        let slots = governor.clone();
        // The app: answers with the job, and hears when it has run.
        let app = std::thread::spawn(move || {
            let message = requests.recv().unwrap();
            let ControlRequest::Exec { session, command, .. } = message.request else { panic!("an exec request") };
            let job = crate::sandbox::exec::Exec {
                sbx,
                sandbox: "imacs-s1".into(),
                workdir,
                command,
                timeout: Duration::from_secs(60),
                class: imacs_core::governor::SlotClass::Test,
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
            handle(server, &Mutex::new(Hub::default()), &channels, &egui::Context::default()).unwrap();
        });
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).unwrap();
        let response: ControlResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok, "{response:?}");
        assert_eq!(response.result["session"], "s1");
        assert_eq!(response.result["sandbox"], "imacs-s1");
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
                let governor = Arc::new(crate::governor::Governor::new(Default::default(), None));
                handle_with_wait_duration(
                    server,
                    &server_hub,
                    &Channels {
                        hooks: hooks.into(),
                        control: control.into(),
                        governor,
                        usage: std::sync::mpsc::channel().0,
                    },
                    &egui::Context::default(),
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
    fn queued_messages_wait_for_a_deliverable_event() {
        let mut hub = Hub { external_prefix: "From IMACS:".into(), ..Hub::default() };
        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);

        let (reply, delivered) = hub.reply(&event("Notification"));
        assert!(reply.stdout.is_none());
        assert_eq!(delivered, 0);

        let (reply, delivered) = hub.reply(&event("PostToolUse"));
        assert_eq!(delivered, 1);
        let out: Value = serde_json::from_str(&reply.stdout.unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "From IMACS:\nstop and run the tests");
        assert!(hub.outbox.is_empty());
    }

    #[test]
    fn session_start_gets_context() {
        let mut hub = Hub { session_context: "You run in IMACS.".into(), ..Hub::default() };
        let (reply, _) = hub.reply(&event("SessionStart"));
        assert!(reply.stdout.unwrap().contains("You run in IMACS."));
        assert!(hub.reply(&event("Stop")).0.stdout.is_none());
    }
}
