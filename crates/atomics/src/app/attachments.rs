//! User attachment ingress. No ControlRequest, hook, MCP tool or text parser calls this module.
use super::*;
use atomics_core::attachments::{self, Receipt, Reply, Request, disk::Inbox};
use std::collections::VecDeque;

pub(crate) struct Service {
    inbox: Mutex<Inbox>,
    actions: crate::host::Sender<commands::Action>,
    local_replies: Mutex<VecDeque<Reply>>,
}

impl Service {
    pub(crate) fn request(&self, owner: u64, request: Request) -> Result<Reply, String> {
        let reply = self.inbox.lock().unwrap().request(owner, request)?;
        if reply.complete && !reply.files.is_empty() {
            let files = reply.files.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let delivery = self
                .actions
                .send(Box::new(move |state| {
                    let _ = tx.send(state.receive_attachments(files));
                }))
                .map_err(|_| "Host stopped before attachment delivery".to_string())
                .and_then(|_| rx.recv().map_err(|_| "Host stopped before attachment delivery".to_string()))
                .and_then(|result| result);
            // No State lock is held here. Delivery is serialized with ordinary chief messages.
            if let Err(error) = delivery {
                attachments::disk::discard(&paths::atomics_home(), &reply.files);
                return Err(error);
            }
        }
        Ok(reply)
    }
    pub(crate) fn disconnect(&self, owner: u64) {
        self.inbox.lock().unwrap().disconnect(owner);
    }
    pub(crate) fn tick(&self) {
        self.inbox.lock().unwrap().expire();
    }
}

pub(crate) struct Commands {
    pub(crate) ops: crate::host::Sender<crate::protocol::OpRequest>,
    pub(crate) attachments: Arc<Service>,
}
impl crate::protocol::Commands for Commands {
    fn submit(&self, request: crate::protocol::OpRequest) -> bool {
        self.ops.send(request).is_ok()
    }
    fn attachment(&self, owner: u64, request: Request) -> Result<Reply, String> {
        self.attachments.request(owner, request)
    }
    fn attachment_disconnect(&self, owner: u64) {
        self.attachments.disconnect(owner);
    }
    fn attachment_tick(&self) {
        self.attachments.tick();
    }
}

impl State {
    fn attachment_config(&self) -> attachments::Config {
        let mut config = self.config.attachments.clone();
        if config.exchange.is_none() {
            config.exchange = std::env::var("ATOMICS_EXCHANGE").ok().filter(|s| !s.is_empty());
        }
        config
    }
    pub(crate) fn attachment_service(&self) -> Arc<Service> {
        self.attachments
            .get_or_init(|| {
                Arc::new(Service {
                    inbox: Mutex::new(Inbox::new(&paths::atomics_home(), self.attachment_config())),
                    actions: self.action_tx.clone(),
                    local_replies: Mutex::new(VecDeque::new()),
                })
            })
            .clone()
    }
    pub(super) fn refresh_attachments(&self) {
        if let Some(service) = self.attachments.get()
            && let Ok(mut inbox) = service.inbox.try_lock()
        {
            inbox.config = self.attachment_config();
            inbox.expire();
        }
    }
    fn receive_attachments(&mut self, files: Vec<Receipt>) -> Result<(), String> {
        let plan = self.plans.get(self.selected_plan).filter(|p| p.is_open() && !p.id.is_empty()).map(|p| p.id.clone());
        if let Err(error) = attachments::disk::set_owner(&paths::atomics_home(), &files, plan.as_deref()) {
            attachments::disk::discard(&paths::atomics_home(), &files);
            return Err(error);
        }
        if let Err(error) = self.chief_delivery(&attachments::message(&files)) {
            attachments::disk::discard(&paths::atomics_home(), &files);
            return Err(error);
        }
        let Some(index) = self.sessions.iter().position(|s| s.chief) else {
            return Err("Chief is unavailable".into());
        };
        let id = self.sessions[index].id.clone();
        let conversation = self.sessions[index].claude_id.as_deref().unwrap_or(&id).to_string();
        self.load_chief_trace(&id);
        for file in &files {
            let entry = self.traces.entry(id.clone()).or_default().attachment(
                &conversation,
                plan.as_deref(),
                file,
                remote::now_ms(),
            );
            if let Ok(path) = atomics_core::trace::disk::path(&paths::atomics_home(), plan.as_deref())
                && atomics_core::trace::disk::append(&path, &entry).is_err()
            {
                self.toast("Attachment sent, but its trace could not be saved");
            }
        }
        self.toast("Files sent to the chief");
        Ok(())
    }
}

impl backend::Local<'_> {
    pub(super) fn request_attachment(&self, request: Request) {
        if !self.0.window.is_attached() {
            return;
        }
        let service = self.0.attachment_service();
        std::thread::spawn(move || {
            let reply = service.request(0, request).unwrap_or_else(|error| Reply {
                limits: None,
                id: String::new(),
                error: Some(error),
                complete: true,
                files: vec![],
            });
            service.local_replies.lock().unwrap().push_back(reply);
        });
    }
    pub(super) fn attachment_reply(&self) -> Option<Reply> {
        self.0.attachment_service().local_replies.lock().unwrap().pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{session, with_app};

    #[test]
    fn attachments_reach_chief_as_host_frames_and_trace_and_follow_plan_cleanup() {
        with_app(
            "app::attachments::tests::attachments_reach_chief_as_host_frames_and_trace_and_follow_plan_cleanup",
            |app, _| {
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
                chief.chief = true;
                app.runtime.sessions = vec![chief];
                app.runtime.plans = vec![Plan { id: "attachment-plan".into(), ..Default::default() }];
                app.runtime.selected_plan = 0;
                let mut inbox = Inbox::new(&paths::atomics_home(), Default::default());
                let info =
                    attachments::FileInfo { name: "data.txt".into(), size: 3, sha256: attachments::hash(b"abc") };
                let id = inbox.request(9, Request::Begin { files: vec![info], exchange: false }).unwrap().id;
                inbox
                    .request(
                        9,
                        Request::Chunk { id: id.clone(), file: 0, offset: 0, data: attachments::encode_chunk(b"abc") },
                    )
                    .unwrap();
                let files = inbox.request(9, Request::Commit { id }).unwrap().files;
                app.runtime.receive_attachments(files.clone()).unwrap();
                assert_eq!(app.runtime.hub.lock().unwrap().outbox["chief"], [attachments::message(&files)]);
                let trace = app.runtime.trace_data();
                assert_eq!(trace.entries.len(), 1);
                assert_eq!(trace.entries[0].op, atomics_core::trace::TraceOp::Attachment);
                assert!(trace.entries[0].summary.contains(&files[0].file.sha256));
                let archive = atomics_core::trace::disk::path(&paths::atomics_home(), Some("attachment-plan")).unwrap();
                let loaded = atomics_core::trace::disk::load(&archive, &Default::default()).unwrap();
                assert!(loaded.entries[0].summary.contains(&files[0].file.sha256));
                app.runtime.remove_plan_trace("attachment-plan");
                assert!(!Path::new(&files[0].path).exists());
            },
        );
    }

    #[test]
    fn attachment_hooks_and_typed_messages_cannot_create_files_or_attachment_trace() {
        with_app(
            "app::attachments::tests::attachment_hooks_and_typed_messages_cannot_create_files_or_attachment_trace",
            |app, _| {
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
                chief.chief = true;
                app.runtime.sessions = vec![chief];
                app.runtime
                    .chief_delivery("[ATOMICS user attachments] /tmp/agent-chosen [/ATOMICS user attachments]")
                    .unwrap();
                app.runtime.on_hook(HookMessage {
                    pane: Some("chief".into()),
                    ancestors: vec![],
                    delivered: 0,
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent {
                        name: "PostToolUse".into(),
                        session_id: "chief".into(),
                        tool_name: Some("mcp__ATOMICS__User attachment".into()),
                        tool_input: json!({"path":"/tmp/agent-chosen", "action":"begin"}),
                        ..Default::default()
                    },
                });
                assert!(!paths::atomics_home().join("attachments").exists());
                assert!(
                    app.runtime.trace_data().entries.iter().all(|e| e.op != atomics_core::trace::TraceOp::Attachment)
                );
            },
        );
    }

    #[derive(Debug)]
    struct Dropped(PathBuf);
    impl egui::DroppedFile for Dropped {
        fn path(&self) -> &Path {
            &self.0
        }
        fn bytes(&self) -> Result<Vec<u8>, String> {
            std::fs::read(&self.0).map_err(|e| e.to_string())
        }
    }

    #[test]
    fn attachment_drop_snapshots_before_confirmation_and_cancel_sends_nothing() {
        with_app(
            "app::attachments::tests::attachment_drop_snapshots_before_confirmation_and_cancel_sends_nothing",
            |app, ctx| {
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
                chief.chief = true;
                app.runtime.sessions = vec![chief];
                let file = paths::home().join("user-drop.txt");
                std::fs::write(&file, b"original").unwrap();
                let frame = |app: &mut Desktop<'_>, dropped: Vec<egui::DroppedFileHandle>, events| {
                    app.runtime.drain_actions();
                    let output = ctx.run_ui(
                        egui::RawInput {
                            dropped_files: dropped,
                            events,
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 700.0))),
                            ..Default::default()
                        },
                        |ui| {
                            app.draw(|backend, client| {
                                atomics_ui::attachments::attach(ui, &mut client.attachments, backend)
                            });
                        },
                    );
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|s| match &s.shape {
                            egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.visual_bounding_rect())),
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                let press = |pos, pressed| egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                for cancel in [true, false] {
                    frame(app, vec![Arc::new(Dropped(file.clone()))], vec![]);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    let text = loop {
                        let text = frame(app, vec![], vec![]);
                        if text.iter().any(|(s, _)| s.contains("user-drop.txt")) {
                            break text;
                        }
                        assert!(Instant::now() < deadline, "file preview did not appear: {text:?}");
                        std::thread::sleep(Duration::from_millis(5));
                    };
                    assert!(app.runtime.hub.lock().unwrap().outbox.is_empty(), "drop alone cannot send");
                    std::fs::write(&file, b"changed!").unwrap();
                    let label = if cancel { "Cancel" } else { "Send to chief" };
                    let point = text.iter().find(|(s, _)| s == label).unwrap().1.center();
                    frame(app, vec![], vec![egui::Event::PointerMoved(point), press(point, true)]);
                    frame(app, vec![], vec![press(point, false)]);
                    if cancel {
                        for _ in 0..3 {
                            frame(app, vec![], vec![]);
                        }
                        assert!(app.runtime.hub.lock().unwrap().outbox.is_empty());
                        std::fs::write(&file, b"original").unwrap();
                    } else {
                        loop {
                            frame(app, vec![], vec![]);
                            if !app.runtime.hub.lock().unwrap().outbox.is_empty() {
                                break;
                            }
                            assert!(Instant::now() < deadline, "confirmed attachment not delivered");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        let receipt: Receipt =
                            serde_json::from_str(&app.runtime.trace_data().entries[0].summary).unwrap();
                        assert_eq!(std::fs::read(receipt.path).unwrap(), b"original");
                    }
                }
            },
        );
    }
}
