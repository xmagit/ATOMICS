//! Controller-only ingress and confirmed chief delivery.
use super::*;
use crate::protocol::LiveGrant;
use atomics_core::attachments::{self, Receipt, Reply, Request, disk::Inbox};
use std::collections::VecDeque;

pub(crate) struct Service {
    inbox: Inbox,
    config: Mutex<attachments::Config>,
    actions: crate::host::Sender<commands::Action>,
    local_replies: Mutex<VecDeque<Reply>>,
}

impl Service {
    fn request(&self, owner: u64, request: Request, grant: LiveGrant) -> Result<Reply, String> {
        if !grant() {
            self.inbox.disconnect(owner);
            return Err("Controller permission lost".into());
        }
        let config = self.config.lock().unwrap().clone();
        let mut reply = self.inbox.request_with(owner, request, &config)?;
        if !reply.files.is_empty() {
            let files = reply.files.clone();
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let delivery = self
                .actions
                .send(Box::new(move |state| {
                    let result = if grant() {
                        state.receive_attachments(files)
                    } else {
                        Err("Controller permission lost".into())
                    };
                    let _ = tx.send(result);
                }))
                .map_err(|_| "Host stopped before attachment delivery".to_string())
                .and_then(|_| rx.recv().map_err(|_| "Host stopped before attachment delivery".to_string()))
                .and_then(|result| result);
            match delivery {
                Ok(queued) => reply.queued = queued,
                Err(error) => {
                    attachments::disk::discard(&paths::atomics_home(), &reply.files);
                    return Err(error);
                }
            }
        }
        Ok(reply)
    }

    fn local(&self, files: Vec<atomics_ui::attachments::Prepared>, plan: Option<String>) -> Result<Reply, String> {
        let grant: LiveGrant = Arc::new(|| true);
        let begin = self.request(
            0,
            Request::Begin { files: files.iter().map(|f| f.info.clone()).collect(), plan, exchange: false },
            grant.clone(),
        )?;
        let snapshots = files.into_iter().map(|f| f.bytes).collect::<Vec<_>>();
        if let Err(error) = self.inbox.local_bytes(0, &begin.id, &snapshots) {
            self.inbox.disconnect(0);
            return Err(error);
        }
        self.request(0, Request::Commit { id: begin.id }, grant)
    }

    fn local_reply(&self, result: Result<Reply, String>) {
        let reply = result.unwrap_or_else(|error| Reply {
            queued: false,
            limits: None,
            id: String::new(),
            error: Some(error),
            complete: true,
            files: vec![],
        });
        self.local_replies.lock().unwrap().push_back(reply);
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
    fn attachment(&self, owner: u64, request: Request, grant: LiveGrant) -> Result<Reply, String> {
        self.attachments.request(owner, request, grant)
    }
    fn attachment_disconnect(&self, owner: u64) {
        self.attachments.inbox.disconnect(owner);
    }
}

pub(super) struct PendingReceipt {
    pane: String,
    conversation: Option<String>,
    pub(super) plan: Option<String>,
    files: Vec<Receipt>,
    message: String,
    confirmed: bool,
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
                let config = self.attachment_config();
                let service = Arc::new(Service {
                    inbox: Inbox::new(&paths::atomics_home(), config.clone()),
                    config: Mutex::new(config),
                    actions: self.action_tx.clone(),
                    local_replies: Mutex::new(VecDeque::new()),
                });
                let weak = Arc::downgrade(&service);
                std::thread::spawn(move || {
                    let mut sweep = attachments::disk::ExchangeSweep::default();
                    let mut expiry_cursor = None;
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                        let Some(service) = weak.upgrade() else { break };
                        let config = service.config.lock().unwrap().clone();
                        service.inbox.expire(&config, &mut expiry_cursor);
                        service.inbox.sweep_exchange(&config, &mut sweep);
                    }
                });
                service
            })
            .clone()
    }
    pub(super) fn refresh_attachments(&mut self) {
        if let Some(service) = self.attachments.get() {
            *service.config.lock().unwrap() = self.attachment_config();
        }
        self.pending_receipts.retain(|r| {
            let keep = self.sessions.iter().any(|s| {
                s.id == r.pane
                    && s.status != AgentStatus::Exited
                    && (r.conversation.is_none() || s.claude_id == r.conversation)
            });
            if !keep {
                attachments::disk::discard(&paths::atomics_home(), &r.files);
            }
            keep
        });
        if let Some(pane) = self
            .pending_receipts
            .iter()
            .find(|r| r.confirmed && self.traces.contains_key(&r.pane))
            .map(|r| r.pane.clone())
        {
            self.confirm_attachments(&pane, None, &[]);
        }
    }

    fn receive_attachments(&mut self, files: Vec<Receipt>) -> Result<bool, String> {
        let plan = attachments::disk::accept(&paths::atomics_home(), &files)?;
        if plan.as_ref().is_some_and(|id| !self.plans.iter().any(|p| &p.id == id && p.is_open())) {
            return Err("The attachment’s plan is no longer open".into());
        }
        let message = attachments::message(&files);
        let starting = !self.sessions.iter().any(|s| s.chief && s.can_receive());
        let delivery = self.chief_delivery(&message)?;
        let chief = self.sessions.iter().find(|s| s.chief).ok_or("Chief is unavailable")?;
        let receipt = PendingReceipt {
            pane: chief.id.clone(),
            conversation: chief.claude_id.clone(),
            files,
            plan,
            message,
            confirmed: !starting
                && delivery == Delivery::Sent
                && chief.pending.is_none()
                && chief.status != AgentStatus::Starting,
        };
        if receipt.confirmed && receipt.conversation.is_some() {
            self.record_attachment_delivery(receipt);
            Ok(false)
        } else {
            self.pending_receipts.push(receipt);
            Ok(true)
        }
    }

    /// A trusted delivery event can acknowledge only a batch the host already verified.
    pub(super) fn confirm_attachments(&mut self, pane: &str, prompt: Option<&str>, delivered: &[String]) {
        if self.pending_receipts.is_empty() {
            return;
        }
        let Some(chief) = self.sessions.iter().find(|s| s.chief && s.id == pane) else { return };
        let conversation = chief.claude_id.clone();
        let mut ready = Vec::new();
        for mut receipt in std::mem::take(&mut self.pending_receipts) {
            if receipt.pane == pane && (receipt.conversation.is_none() || receipt.conversation == conversation) {
                receipt.conversation.clone_from(&conversation);
                if prompt.is_some_and(|text| text.contains(&receipt.message)) {
                    receipt.confirmed = true;
                }
                if delivered.iter().any(|text| text.contains(&receipt.message)) {
                    receipt.confirmed = true;
                }
            }
            if receipt.confirmed && receipt.conversation.is_some() {
                ready.push(receipt);
            } else {
                self.pending_receipts.push(receipt);
            }
        }
        for receipt in ready {
            self.record_attachment_delivery(receipt);
        }
    }

    fn record_attachment_delivery(&mut self, receipt: PendingReceipt) {
        let home = paths::atomics_home();
        self.load_chief_trace(&receipt.pane);
        if !self.traces.contains_key(&receipt.pane) {
            self.pending_receipts.push(receipt);
            return;
        }
        let mut saved = true;
        for file in receipt.files {
            if !Path::new(&file.path).is_file() {
                continue;
            }
            let entry = self.traces.entry(receipt.pane.clone()).or_default().attachment(
                receipt.conversation.as_deref().expect("confirmed conversation"),
                receipt.plan.as_deref(),
                &file,
                remote::now_ms(),
            );
            let result = atomics_core::trace::disk::path(&home, receipt.plan.as_deref())
                .and_then(|path| atomics_core::trace::disk::append(&path, &entry));
            saved &= result.is_ok() && attachments::disk::traced(&home, &file).is_ok();
        }
        self.toast(if saved {
            "Files sent to the chief"
        } else {
            "Files sent, but the attachment trace could not be saved"
        });
    }
}

impl backend::Local<'_> {
    pub(super) fn request_attachment(&self, request: Request) {
        if !self.0.window.is_attached() {
            return;
        }
        let service = self.0.attachment_service();
        std::thread::spawn(move || service.local_reply(service.request(0, request, Arc::new(|| true))));
    }
    pub(super) fn send_local_attachments(
        &self,
        files: &[atomics_ui::attachments::Prepared],
        plan: Option<String>,
    ) -> bool {
        if !self.0.window.is_attached() {
            return false;
        }
        let service = self.0.attachment_service();
        let files = files.to_vec();
        std::thread::spawn(move || service.local_reply(service.local(files, plan)));
        true
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
    fn attachment_current_grant_is_required_at_final_delivery() {
        with_app("app::attachments::tests::attachment_current_grant_is_required_at_final_delivery", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            let service = app.runtime.attachment_service();
            let info = attachments::FileInfo { name: "empty".into(), size: 0, sha256: attachments::hash(b"") };
            let id = service
                .request(7, Request::Begin { plan: None, files: vec![info], exchange: false }, Arc::new(|| true))
                .unwrap()
                .id;
            let worker = std::thread::spawn(move || {
                let checks = std::sync::atomic::AtomicUsize::new(0);
                let grant: Arc<dyn Fn() -> bool + Send + Sync> =
                    Arc::new(move || checks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0);
                let _ = &grant;
                service.request(7, Request::Commit { id }, grant)
            });
            while !worker.is_finished() {
                app.runtime.drain_actions();
                std::thread::yield_now();
            }
            assert!(worker.join().unwrap().is_err(), "the live grant ended before host delivery");
            assert!(app.runtime.hub.lock().unwrap().outbox.is_empty());
        });
    }
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
                let inbox = Inbox::new(&paths::atomics_home(), Default::default());
                let info =
                    attachments::FileInfo { name: "data.txt".into(), size: 3, sha256: attachments::hash(b"abc") };
                let id = inbox
                    .request(
                        9,
                        Request::Begin { plan: Some("attachment-plan".into()), files: vec![info], exchange: false },
                    )
                    .unwrap()
                    .id;
                inbox
                    .request(
                        9,
                        Request::Chunk { id: id.clone(), file: 0, offset: 0, data: attachments::encode_chunk(b"abc") },
                    )
                    .unwrap();
                app.runtime.plans.push(Plan { id: "different".into(), ..Default::default() });
                app.runtime.selected_plan = 1;
                let files = inbox.request(9, Request::Commit { id }).unwrap().files;
                assert!(app.runtime.receive_attachments(files.clone()).unwrap());
                let lookup = crate::app::tests::request(app, json!({"cmd":"attachments","id":files[0].id}));
                assert_eq!(lookup.result["attachments"], json!(files));
                assert_eq!(app.runtime.hub.lock().unwrap().outbox["chief"], [attachments::message(&files)]);
                assert!(app.runtime.trace_data().entries.is_empty(), "queued hook delivery must not confirm Sent");
                let delivered = app.runtime.hub.lock().unwrap().outbox.remove("chief").unwrap();
                app.runtime.confirm_attachments("chief", None, &delivered);
                let trace = app.runtime.trace_data();
                assert_eq!(trace.entries.len(), 1);
                assert_eq!(trace.entries[0].op, atomics_core::trace::TraceOp::Attachment);
                assert!(trace.entries[0].summary.contains(&files[0].file.sha256));
                let archive = atomics_core::trace::disk::path(&paths::atomics_home(), Some("attachment-plan")).unwrap();
                let loaded = atomics_core::trace::disk::load(&archive, &Default::default()).unwrap();
                assert!(loaded.entries[0].summary.contains(&files[0].file.sha256));
                app.runtime.remove_plan_trace("different");
                assert!(Path::new(&files[0].path).is_file());
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
                    delivered: Vec::new(),
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

    #[test]
    fn attachment_held_for_first_hook_uses_the_provider_conversation() {
        with_app("app::attachments::tests::attachment_held_for_first_hook_uses_the_provider_conversation", |app, _| {
            let home = paths::atomics_home();
            let inbox = Inbox::new(&home, Default::default());
            let info = attachments::FileInfo { name: "empty".into(), size: 0, sha256: attachments::hash(b"") };
            let id = inbox.request(1, Request::Begin { plan: None, files: vec![info], exchange: false }).unwrap().id;
            let files = inbox.request(1, Request::Commit { id }).unwrap().files;
            let mut chief = session("pane", AgentKind::Claude, AgentStatus::Starting, &app.runtime.launch_dir);
            chief.chief = true;
            chief.approval_hold = true;
            chief.claude_id = Some("provider-conversation".into());
            app.runtime.sessions = vec![chief];
            assert!(app.runtime.receive_attachments(files.clone()).unwrap());
            assert!(app.runtime.trace_data().entries.is_empty());
            assert_eq!(app.runtime.sessions[0].held, [attachments::message(&files)]);
            // A terminal delivery can be acknowledged before the provider reports its conversation.
            app.runtime.pending_receipts[0].conversation = None;
            app.runtime.sessions[0].claude_id = None;
            app.runtime.confirm_attachments("pane", Some(&attachments::message(&files)), &[]);
            assert!(app.runtime.trace_data().entries.is_empty());
            app.runtime.on_hook(HookMessage {
                pane: Some("pane".into()),
                ancestors: vec![],
                delivered: vec![],
                approval_reply: None,
                event: atomics_core::hook::HookEvent {
                    name: "SessionStart".into(),
                    session_id: "provider-conversation".into(),
                    ..Default::default()
                },
            });
            let archive = atomics_core::trace::disk::path(&home, None).unwrap();
            let loaded = atomics_core::trace::disk::load_for_session(
                &archive,
                &Default::default(),
                Some("provider-conversation"),
            )
            .unwrap();
            assert_eq!(loaded.entries.len(), 1);
            assert!(
                atomics_core::trace::disk::load_for_session(&archive, &Default::default(), Some("pane"))
                    .unwrap()
                    .entries
                    .is_empty()
            );
        });
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
                    let moved = file.with_extension("selected");
                    std::fs::rename(&file, &moved).unwrap();
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
                        let delivered = app.runtime.hub.lock().unwrap().outbox.remove("chief").unwrap();
                        app.runtime.confirm_attachments("chief", None, &delivered);
                        let receipt: Receipt =
                            serde_json::from_str(&app.runtime.trace_data().entries[0].summary).unwrap();
                        assert_eq!(std::fs::read(receipt.path).unwrap(), b"original");
                    }
                }
            },
        );
    }
    #[test]
    fn attachment_in_process_desktop_delivers_the_snapshot_directly() {
        with_app("app::attachments::tests::attachment_in_process_desktop_delivers_the_snapshot_directly", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            let bytes: Arc<[u8]> = Arc::from(&b"local bytes"[..]);
            let info = attachments::FileInfo {
                name: "sample.txt".into(),
                size: bytes.len() as u64,
                sha256: attachments::hash(&bytes),
            };
            app.draw(|backend, _| {
                assert!(backend.attach_local(&[atomics_ui::attachments::Prepared { info, bytes }], None))
            });
            let start = Instant::now();
            loop {
                app.runtime.drain_actions();
                if let Some(reply) = backend::Local(app.runtime).attachment_reply() {
                    assert!(reply.complete && reply.error.is_none());
                    assert_eq!(std::fs::read(&reply.files[0].path).unwrap(), b"local bytes");
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::yield_now();
            }
        });
    }
}
