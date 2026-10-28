//! The app side of the hook bridge: a unix socket that answers `cox hook` at once
//! from shared state, so a hidden or busy window never slows an agent down.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use cox_core::control::{ControlRequest, ControlResponse, describe_diff};
use cox_core::hook::{HookEvent, HookReply, HookRequest, context_output};
use serde_json::Value;

#[derive(Default)]
pub struct Hub {
    /// Messages waiting for sessions that run outside cox, by Claude session id.
    pub outbox: HashMap<String, Vec<String>>,
    pub session_context: String,
    pub external_prefix: String,
}

pub struct HookMessage {
    pub pane: Option<String>,
    pub event: HookEvent,
    pub delivered: usize,
}

/// What the app answers to a control request. Diffs are computed on the socket
/// thread so the UI never waits on git.
pub enum ControlOutcome {
    Done(ControlResponse),
    Diff { root: PathBuf, base_ref: String, patch: bool, session: Value },
}

pub struct ControlMessage {
    pub request: ControlRequest,
    pub reply: Sender<ControlOutcome>,
}

pub struct Channels {
    pub hooks: Sender<HookMessage>,
    pub control: Sender<ControlMessage>,
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

/// Binds the socket, refusing to steal it from another running cox.
pub fn serve(path: &Path, hub: Arc<Mutex<Hub>>, channels: Channels, ctx: egui::Context) -> Result<()> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            bail!("another cox is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    let path: PathBuf = path.to_path_buf();
    std::thread::Builder::new().name("cox-hooks".into()).spawn(move || {
        let channels = Arc::new(channels);
        for stream in listener.incoming().flatten() {
            let (hub, channels, ctx) = (hub.clone(), channels.clone(), ctx.clone());
            std::thread::spawn(move || {
                if let Err(err) = handle(stream, &hub, &channels, &ctx) {
                    eprintln!("cox: hook connection failed: {err:#}");
                }
            });
        }
        let _ = std::fs::remove_file(path);
    })?;
    Ok(())
}

fn handle(stream: UnixStream, hub: &Mutex<Hub>, channels: &Channels, ctx: &egui::Context) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let value: Value = serde_json::from_str(&line)?;
    if value.get("cmd").is_some() {
        let response = match serde_json::from_value::<ControlRequest>(value) {
            Ok(request) => control(request, &channels.control, ctx),
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
    let (reply, delivered) = hub.lock().unwrap().reply(&event);
    let mut writer = &stream;
    writer.write_all(serde_json::to_string(&reply)?.as_bytes())?;
    writer.write_all(b"\n")?;
    let _ = channels.hooks.send(HookMessage { pane: request.pane, event, delivered });
    ctx.request_repaint();
    Ok(())
}

fn control(request: ControlRequest, app: &Sender<ControlMessage>, ctx: &egui::Context) -> ControlResponse {
    let (reply, outcome) = std::sync::mpsc::channel();
    if app.send(ControlMessage { request, reply }).is_err() {
        return ControlResponse::error("cox is shutting down");
    }
    ctx.request_repaint();
    match outcome.recv_timeout(Duration::from_secs(300)) {
        Ok(ControlOutcome::Done(response)) => response,
        Ok(ControlOutcome::Diff { root, base_ref, patch, session }) => {
            match cox_core::git::diff_against_base(&root, &base_ref) {
                Ok(diff) => ControlResponse::ok(serde_json::json!({
                    "session": session,
                    "root": root,
                    "diff": describe_diff(&diff, patch),
                })),
                Err(err) => ControlResponse::error(format!("{err:#}")),
            }
        }
        Err(_) => ControlResponse::error("cox did not answer in time"),
    }
}

/// Sends one control request to a running cox (for `cox ctl` and `cox mcp`).
pub fn request(request: &ControlRequest) -> Result<ControlResponse> {
    let path = cox_core::paths::socket_path();
    let stream =
        UnixStream::connect(&path).map_err(|e| anyhow::anyhow!("cox is not running ({}: {e})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(320)))?;
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

    fn event(name: &str) -> HookEvent {
        HookEvent::from_payload(&json!({ "hook_event_name": name, "session_id": "s1", "cwd": "/w" })).unwrap()
    }

    #[test]
    fn queued_messages_wait_for_a_deliverable_event() {
        let mut hub = Hub { external_prefix: "From cox:".into(), ..Hub::default() };
        hub.outbox.insert("s1".into(), vec!["stop and run the tests".into()]);

        let (reply, delivered) = hub.reply(&event("Notification"));
        assert!(reply.stdout.is_none());
        assert_eq!(delivered, 0);

        let (reply, delivered) = hub.reply(&event("PostToolUse"));
        assert_eq!(delivered, 1);
        let out: Value = serde_json::from_str(&reply.stdout.unwrap()).unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "From cox:\nstop and run the tests");
        assert!(hub.outbox.is_empty());
    }

    #[test]
    fn session_start_gets_context() {
        let mut hub = Hub { session_context: "You run in cox.".into(), ..Hub::default() };
        let (reply, _) = hub.reply(&event("SessionStart"));
        assert!(reply.stdout.unwrap().contains("You run in cox."));
        assert!(hub.reply(&event("Stop")).0.stdout.is_none());
    }
}
