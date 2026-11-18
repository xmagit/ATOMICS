//! Controller-only ingress and confirmed chief delivery.
use super::*;
use crate::protocol::LiveGrant;
use atomics_core::attachments::{self, Receipt, Reply, Request, disk::Inbox};
use std::collections::VecDeque;

pub(crate) struct Service {
    pub(super) inbox: Inbox,
    config: Mutex<attachments::Config>,
    actions: crate::host::Sender<commands::Action>,
    local_replies: Mutex<VecDeque<Reply>>,
    deliveries: Mutex<std::collections::HashMap<u64, Arc<DeliveryTicket>>>,
}

enum Progress {
    Waiting,
    Retry,
    Writing { cancelled: Option<String> },
    Submitted,
    Received,
    Unconfirmed,
    Sent,
    Failed(String),
}

pub(crate) struct DeliveryTicket {
    pub(crate) id: String,
    grant: LiveGrant,
    progress: Mutex<Progress>,
}

impl DeliveryTicket {
    fn new(id: String, grant: LiveGrant) -> Arc<Self> {
        Arc::new(Self { id, grant, progress: Mutex::new(Progress::Waiting) })
    }

    pub(crate) fn live(&self) -> bool {
        match *self.progress.lock().unwrap() {
            Progress::Waiting | Progress::Retry => (self.grant)(),
            Progress::Writing { .. } | Progress::Submitted | Progress::Received => true,
            _ => false,
        }
    }

    // Claim the hook write while the sender is live. Its actual result settles a
    // cancellation racing a write already handed to the socket.
    pub(crate) fn start_write(&self) -> bool {
        let mut progress = self.progress.lock().unwrap();
        if matches!(*progress, Progress::Waiting | Progress::Retry) && (self.grant)() {
            *progress = Progress::Writing { cancelled: None };
            return true;
        }
        false
    }

    fn submit_terminal(&self) -> bool {
        let mut progress = self.progress.lock().unwrap();
        if matches!(*progress, Progress::Waiting) && (self.grant)() {
            *progress = Progress::Submitted;
            return true;
        }
        false
    }

    pub(crate) fn retry(&self) {
        let mut progress = self.progress.lock().unwrap();
        if let Progress::Writing { cancelled } = &mut *progress {
            *progress = cancelled.take().map_or(Progress::Retry, Progress::Failed);
        }
    }

    pub(crate) fn received(&self) {
        let mut progress = self.progress.lock().unwrap();
        if !matches!(*progress, Progress::Failed(_)) {
            *progress = Progress::Received;
        }
    }

    pub(super) fn unconfirmed(&self) {
        *self.progress.lock().unwrap() = Progress::Unconfirmed;
    }

    pub(super) fn fail(&self, reason: &str) -> bool {
        let mut progress = self.progress.lock().unwrap();
        match &mut *progress {
            Progress::Writing { cancelled } => {
                cancelled.get_or_insert_with(|| reason.into());
            }
            Progress::Waiting | Progress::Retry => *progress = Progress::Failed(reason.into()),
            Progress::Failed(_) => {}
            _ => return false,
        }
        true
    }

    fn reply(&self) -> Reply {
        let progress = self.progress.lock().unwrap();
        Reply {
            id: self.id.clone(),
            limits: None,
            files: vec![],
            complete: true,
            queued: matches!(
                *progress,
                Progress::Waiting
                    | Progress::Retry
                    | Progress::Writing { .. }
                    | Progress::Submitted
                    | Progress::Received
            ),
            retrying: matches!(*progress, Progress::Retry),
            error: match &*progress {
                Progress::Failed(reason) => Some(reason.clone()),
                Progress::Unconfirmed => Some("Chief delivery unconfirmed; check its terminal".into()),
                _ => None,
            },
        }
    }
}

impl Service {
    pub(crate) fn recover_existing(&self) -> Result<(), String> {
        self.inbox.recover_existing()
    }

    fn request(&self, owner: u64, request: Request, grant: LiveGrant) -> Result<Reply, String> {
        if !grant() {
            self.inbox.disconnect(owner);
            return Err("Controller permission lost".into());
        }
        let config = self.config.lock().unwrap().clone();
        if let Request::Status { id } | Request::Cancel { id } = &request {
            let deliveries = self.deliveries.lock().unwrap();
            if let Some(ticket) = deliveries.get(&owner).filter(|ticket| ticket.id == *id) {
                if matches!(request, Request::Cancel { .. }) && !ticket.fail("Cancelled") {
                    let mut reply = ticket.reply();
                    if reply.error.is_none() {
                        reply.error = Some("Already sent to the chief".into());
                    }
                    return Ok(reply);
                }
                return Ok(ticket.reply());
            }
            if matches!(request, Request::Status { .. }) {
                return Err("Attachment delivery is no longer available".into());
            }
        }
        if matches!(request, Request::Begin { .. } | Request::Commit { .. })
            && self.deliveries.lock().unwrap().get(&owner).is_some_and(|t| t.live())
        {
            return Err("An attachment is still waiting for delivery".into());
        }
        let Request::Commit { id } = &request else {
            return self.inbox.request_while(owner, request, &config, &*grant);
        };
        let ticket = DeliveryTicket::new(id.clone(), grant);
        {
            let mut deliveries = self.deliveries.lock().unwrap();
            if !ticket.live() {
                return Err("Sender disconnected".into());
            }
            deliveries.insert(owner, ticket.clone());
        }
        let result = self.commit(owner, request, &config, &ticket);
        if let Err(error) = &result {
            ticket.fail(error);
        }
        result
    }

    fn commit(
        &self,
        owner: u64,
        request: Request,
        config: &attachments::Config,
        ticket: &Arc<DeliveryTicket>,
    ) -> Result<Reply, String> {
        let mut reply = self.inbox.request_while(owner, request, config, &|| ticket.live())?;
        let result = (|| {
            let files = reply.files.clone();
            let plan = attachments::disk::accept(&paths::atomics_home(), &files)?;
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            let delivery = ticket.clone();
            self.actions
                .send(Box::new(move |state| {
                    let _ = tx.send(state.route_attachments(files, plan, delivery));
                }))
                .map_err(|_| "Host stopped before attachment delivery")?;
            rx.recv().map_err(|_| "Host stopped before attachment delivery")?
        })();
        match result {
            Ok(queued) => reply.queued = queued,
            Err(error) => {
                attachments::disk::discard(&paths::atomics_home(), &reply.files);
                return Err(error);
            }
        }
        Ok(reply)
    }

    fn local(
        &self,
        files: Vec<atomics_ui::attachments::Prepared>,
        plan: Option<String>,
        grant: LiveGrant,
    ) -> Result<Reply, String> {
        let begin = self.request(
            0,
            Request::Begin { files: files.iter().map(|f| f.info.clone()).collect(), plan, exchange: false },
            grant.clone(),
        )?;
        let snapshots = files.into_iter().map(|f| f.bytes).collect::<Vec<_>>();
        if let Err(error) = self.inbox.local_bytes(0, &begin.id, &snapshots, &*grant) {
            self.inbox.disconnect(0);
            return Err(error);
        }
        self.request(0, Request::Commit { id: begin.id }, grant)
    }

    fn local_reply(&self, result: Result<Reply, String>) {
        let reply = result.unwrap_or_else(|error| Reply {
            retrying: false,
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
        if let Some(ticket) = self.attachments.deliveries.lock().unwrap().remove(&owner) {
            ticket.fail("Sender disconnected");
        }
    }
}

pub(super) struct PendingReceipt {
    pane: String,
    conversation: Option<String>,
    pub(super) plan: Option<String>,
    files: Vec<Receipt>,
    ticket: Arc<DeliveryTicket>,
    dispatched: bool,
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
                    inbox: Inbox::unopened(&paths::atomics_home(), config.clone()),
                    config: Mutex::new(config),
                    actions: self.action_tx.clone(),
                    local_replies: Mutex::new(VecDeque::new()),
                    deliveries: Default::default(),
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
        for mut receipt in std::mem::take(&mut self.pending_receipts) {
            let chief = self.sessions.iter().position(|s| {
                s.chief
                    && s.id == receipt.pane
                    && s.status != AgentStatus::Exited
                    && (receipt.conversation.is_none() || s.claude_id == receipt.conversation)
            });
            let received = matches!(*receipt.ticket.progress.lock().unwrap(), Progress::Received);
            if let Some(index) = chief
                && receipt.conversation.is_none()
            {
                receipt.conversation.clone_from(&self.sessions[index].claude_id);
            }
            if received && receipt.conversation.is_some() {
                self.record_attachment_delivery(receipt, chief.is_some());
                continue;
            }
            if matches!(*receipt.ticket.progress.lock().unwrap(), Progress::Unconfirmed) {
                continue;
            }
            let Some(index) = chief else {
                receipt.ticket.fail("The attachment’s chief conversation ended");
                let transfer_in_flight =
                    matches!(*receipt.ticket.progress.lock().unwrap(), Progress::Writing { .. } | Progress::Received);
                if transfer_in_flight && receipt.conversation.is_some() {
                    self.pending_receipts.push(receipt);
                } else if receipt.ticket.reply().error.is_some() {
                    self.discard_receipt(receipt);
                } else {
                    receipt.ticket.unconfirmed();
                }
                continue;
            };
            if received {
                // The first provider hook supplies the conversation for an early terminal receipt.
                self.pending_receipts.push(receipt);
                continue;
            }
            if !receipt.ticket.live() {
                receipt.ticket.fail("Attachment cancelled or controller permission lost");
                self.discard_receipt(receipt);
                continue;
            }
            let session = &self.sessions[index];
            if !receipt.dispatched
                && session.status != AgentStatus::Starting
                && session.can_receive()
                && !session.approval_hold
                && session.approvals.is_empty()
                && session.pending.is_none()
            {
                if matches!(session.chief_send_masks, ChiefSendMasks::Failed) {
                    receipt.ticket.fail("Could not verify chief credentials; resend the attachment");
                    self.discard_receipt(receipt);
                    continue;
                }
                if !self.prepare_chief_send(index) {
                    self.pending_receipts.push(receipt);
                    continue;
                }
                let message = attachments::message(&receipt.files);
                if self.sessions[index].terminal.is_some() {
                    if !receipt.ticket.submit_terminal() {
                        receipt.ticket.fail("Attachment cancelled or controller permission lost");
                        self.discard_receipt(receipt);
                        continue;
                    }
                    let delivered = self.deliver(index, &message);
                    if let Some(pending) = &mut self.sessions[index].pending {
                        pending.attachment = Some(receipt.ticket.clone());
                    } else if delivered == Delivery::Sent {
                        receipt.ticket.received();
                    } else {
                        *receipt.ticket.progress.lock().unwrap() =
                            Progress::Failed("The chief could not receive the attachment".into());
                    }
                } else if let Some(conversation) = &receipt.conversation {
                    self.hub.lock().unwrap().attachments.entry(conversation.clone()).or_default().push(
                        crate::ipc::HookAttachment {
                            pane: receipt.pane.clone(),
                            delivery: receipt.ticket.clone(),
                            message,
                        },
                    );
                }
                receipt.dispatched = true;
            }
            self.pending_receipts.push(receipt);
        }
    }

    pub(super) fn cancel_plan_attachments(&mut self, plan: &str) {
        for receipt in &self.pending_receipts {
            if receipt.plan.as_deref() == Some(plan) {
                receipt.ticket.fail("The attachment’s plan closed");
            }
        }
        self.refresh_attachments();
    }

    fn discard_receipt(&mut self, receipt: PendingReceipt) {
        if let Some(id) = &receipt.conversation {
            let mut hub = self.hub.lock().unwrap();
            if let Some(queue) = hub.attachments.get_mut(id) {
                queue.retain(|a| a.delivery.id != receipt.ticket.id);
                if queue.is_empty() {
                    hub.attachments.remove(id);
                }
            }
        }
        let home = paths::atomics_home();
        self.trace_writes.push(Box::new(move || attachments::disk::discard(&home, &receipt.files)));
    }

    #[cfg(test)]
    fn receive_attachments(&mut self, files: Vec<Receipt>) -> Result<bool, String> {
        let plan = attachments::disk::accept(&paths::atomics_home(), &files)?;
        let id = files[0].id.rsplit_once('-').unwrap().0.into();
        self.route_attachments(files, plan, DeliveryTicket::new(id, Arc::new(|| true)))
    }

    fn route_attachments(
        &mut self,
        files: Vec<Receipt>,
        plan: Option<String>,
        ticket: Arc<DeliveryTicket>,
    ) -> Result<bool, String> {
        if !ticket.live() {
            return Err("Attachment cancelled or controller permission lost".into());
        }
        if plan.as_ref().is_some_and(|id| !self.plans.iter().any(|p| &p.id == id && p.is_open())) {
            return Err("The attachment’s plan is no longer open".into());
        }
        if !self.sessions.iter().any(|s| s.chief && (s.can_receive() || s.status == AgentStatus::Starting)) {
            self.sessions.retain(|s| !s.chief);
            self.start_chief(None)?;
        }
        let index = self
            .sessions
            .iter()
            .position(|s| s.chief && !self.is_chief_replacement(&s.id))
            .ok_or("Chief is unavailable")?;
        if matches!(self.sessions[index].chief_send_masks, ChiefSendMasks::Failed) {
            self.prepare_chief_send(index);
        }
        let chief = &self.sessions[index];
        self.pending_receipts.push(PendingReceipt {
            pane: chief.id.clone(),
            conversation: chief.claude_id.clone(),
            files,
            plan,
            ticket: ticket.clone(),
            dispatched: false,
        });
        self.refresh_attachments();
        let reply = ticket.reply();
        reply.error.map_or(Ok(reply.queued), Err)
    }

    pub(super) fn confirm_attachments(&mut self, pane: &str, delivered: &[Option<String>]) {
        for receipt in &self.pending_receipts {
            if receipt.pane == pane && delivered.iter().flatten().any(|id| id == &receipt.ticket.id) {
                receipt.ticket.received();
            }
        }
        self.refresh_attachments();
    }

    pub(super) fn attachment_lookup(
        &self,
        caller: Option<&str>,
    ) -> (Option<String>, std::collections::HashSet<String>) {
        let Some(chief) = self.sessions.iter().find(|s| s.chief && s.status != AgentStatus::Exited) else {
            return (None, Default::default());
        };
        if caller.is_none() || caller != chief.claude_id.as_deref() {
            return (None, Default::default());
        }
        let conversation = chief.claude_id.clone();
        let mut pending: std::collections::HashSet<_> = self
            .pending_receipts
            .iter()
            .filter(|r| {
                r.pane == chief.id && r.conversation == conversation && r.dispatched && r.ticket.reply().error.is_none()
            })
            .map(|r| r.ticket.id.clone())
            .collect();
        if let (Some(log), Some(id)) = (self.traces.get(&chief.id), &conversation) {
            for entry in &log.entries {
                if entry.op == atomics_core::trace::TraceOp::Attachment
                    && entry.belongs_to(id)
                    && let Ok(file) = serde_json::from_str::<Receipt>(&entry.summary)
                    && let Some((drop, _)) = file.id.rsplit_once('-')
                {
                    pending.insert(drop.into());
                }
            }
        }
        (conversation, pending)
    }

    fn record_attachment_delivery(&mut self, receipt: PendingReceipt, current: bool) {
        let home = paths::atomics_home();
        if current {
            self.load_chief_trace(&receipt.pane, None);
        }
        let mut detached = atomics_core::trace::TraceLog::default();
        let conversation = receipt.conversation.expect("confirmed conversation");
        let log =
            if current { self.live_chief_trace(&receipt.pane, &conversation) } else { None }.unwrap_or(&mut detached);
        let entries: Vec<_> = receipt
            .files
            .iter()
            .map(|file| {
                let order = super::trace::next_order();
                log.reserve_order(order);
                log.attachment(&conversation, receipt.plan.as_deref(), file, order / 1000)
            })
            .collect();
        let actions = self.action_tx.clone();
        self.trace_writes.push(Box::new(move || {
            let dir = match attachments::disk::journal_delivery(&home, &receipt.files, conversation, &entries) {
                Ok(dir) => dir,
                Err(error) => {
                    *receipt.ticket.progress.lock().unwrap() =
                        Progress::Failed(format!("Could not save delivery receipt: {error}"));
                    let _ = actions.send(Box::new(|s| s.toast("Could not save the attachment receipt")));
                    return;
                }
            };
            *receipt.ticket.progress.lock().unwrap() = Progress::Sent;
            let _ = actions.send(Box::new(|s| s.toast("Files sent to the chief")));
            let saved = attachments::disk::archive_delivery(&home, &dir, receipt.plan.as_deref(), &receipt.files);
            if saved.is_err() {
                let _ = actions.send(Box::new(|s| s.toast("Files sent, but the attachment trace could not be saved")));
            }
        }));
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
        cancel: Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        if !self.0.window.is_attached() {
            return false;
        }
        let service = self.0.attachment_service();
        let files = files.to_vec();
        std::thread::spawn(move || {
            service.local_reply(service.local(
                files,
                plan,
                Arc::new(move || !cancel.load(std::sync::atomic::Ordering::Acquire)),
            ))
        });
        true
    }
    pub(super) fn attachment_reply(&self) -> Option<Reply> {
        self.0.attachments.get()?.local_replies.lock().unwrap().pop_front()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{session, with_app};

    #[test]
    fn attachment_waits_for_first_send_masks_before_dispatch() {
        with_app("app::attachments::tests::attachment_waits_for_first_send_masks_before_dispatch", |app, _| {
            let state = &mut *app.runtime;
            let fake = crate::mcp_client::FakeOAuthServer::start();
            let (_dir, oauth) = crate::mcp_client::oauth_test_machine();
            state.config.tickets = toml::from_str("system = 'echo'").unwrap();
            state.config.tickets.server = fake.server();
            state.apply_ticket_config();
            state.tickets.sign.override_oauth(oauth);
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &state.launch_dir);
            chief.chief = true;
            state.sessions = vec![chief];
            let ticket = DeliveryTicket::new("transfer".into(), Arc::new(|| true));
            assert!(state.route_attachments(Vec::new(), None, ticket).unwrap());
            assert!(!state.pending_receipts[0].dispatched);
            assert!(state.hub.lock().unwrap().attachments.is_empty());
            let seq = *state.tickets.trace_jobs.keys().max().unwrap();
            state.finish_trace(seq, Ok(crate::tickets::TicketOutput::TraceCleaner(Default::default())));
            state.refresh_attachments();
            assert!(state.pending_receipts[0].dispatched);
            assert!(state.hub.lock().unwrap().attachments.contains_key("chief"));
        });
    }

    #[test]
    fn attachment_lookup_and_confirmation_follow_the_host_delivery() {
        with_app("app::attachments::tests::attachment_lookup_and_confirmation_follow_the_host_delivery", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            let service = app.runtime.attachment_service();
            let inbox = &service.inbox;
            let info = attachments::FileInfo { name: "sample".into(), size: 0, sha256: attachments::hash(b"") };
            let id = inbox.request(1, Request::Begin { plan: None, files: vec![info], exchange: false }).unwrap().id;
            let files = inbox.request(1, Request::Commit { id }).unwrap().files;
            assert!(app.runtime.receive_attachments(files.clone()).unwrap());
            let quote = format!("Earlier message: {}", attachments::message(&files));
            app.runtime.on_hook(HookMessage {
                pane: Some("chief".into()),
                ancestors: vec![],
                delivered: vec![],
                approval_reply: None,
                event: atomics_core::hook::HookEvent {
                    name: "UserPromptSubmit".into(),
                    session_id: "chief".into(),
                    prompt: Some(quote),
                    ..Default::default()
                },
            });
            crate::app::tests::settle_trace(app.runtime);
            assert!(app.runtime.trace_data().entries.iter().all(|e| e.op != atomics_core::trace::TraceOp::Attachment));
            app.runtime.sessions[0].claude_id = Some("another-conversation".into());
            let lookup = crate::app::tests::request(
                app,
                json!({"cmd":"attachments", "conversation":"another-conversation", "id":files[0].id}),
            );
            assert_eq!(lookup.result["attachments"], json!([]));
        });
    }

    #[test]
    fn attachment_lookup_from_a_previous_conversation_cannot_read_the_current_drop() {
        with_app(
            "app::attachments::tests::attachment_lookup_from_a_previous_conversation_cannot_read_the_current_drop",
            |app, _| {
                let mut chief = session("pane", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
                chief.chief = true;
                chief.claude_id = Some("second".into());
                app.runtime.sessions = vec![chief];
                let store = app.runtime.attachment_service();
                let info = attachments::FileInfo { name: "file".into(), size: 0, sha256: attachments::hash(b"") };
                let id = store
                    .inbox
                    .request(1, Request::Begin { plan: None, files: vec![info], exchange: false })
                    .unwrap()
                    .id;
                let files = store.inbox.request(1, Request::Commit { id }).unwrap().files;
                assert!(app.runtime.receive_attachments(files.clone()).unwrap());
                let lookup = |app: &mut Desktop<'_>, caller: &str| {
                    crate::app::tests::request(app, json!({"cmd":"attachments","conversation": caller})).result["attachments"].clone()
                };
                assert_eq!(lookup(app, "second"), json!(files));
                assert_eq!(lookup(app, "first"), json!([]));
            },
        );
    }

    #[test]
    fn rejected_hook_leaves_the_attachment_queued() {
        with_app("app::attachments::tests::rejected_hook_leaves_the_attachment_queued", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions =
                vec![chief, session("other", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir)];
            let store = app.runtime.attachment_service();
            let info = attachments::FileInfo { name: "file".into(), size: 0, sha256: attachments::hash(b"") };
            let id =
                store.inbox.request(1, Request::Begin { plan: None, files: vec![info], exchange: false }).unwrap().id;
            let files = store.inbox.request(1, Request::Commit { id }).unwrap().files;
            assert!(app.runtime.receive_attachments(files).unwrap());
            let ticket = app.runtime.pending_receipts[0].ticket.clone();
            use std::io::{BufRead, Write};
            for (pane, accepted) in [("other", false), ("chief", true)] {
                let mut socket = std::os::unix::net::UnixStream::connect(paths::socket_path()).unwrap();
                writeln!(
                    socket,
                    "{}",
                    json!({"pane":pane, "payload":{"hook_event_name":"PostToolUse", "session_id":"chief"}})
                )
                .unwrap();
                let incoming = app.runtime.hook_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                app.runtime.process_hook(incoming);
                let mut line = String::new();
                std::io::BufReader::new(socket).read_line(&mut line).unwrap();
                assert_eq!(line.contains("[ATOMICS user attachments]"), accepted);
                if accepted {
                    app.runtime.refresh_attachments();
                    crate::app::tests::settle_trace(app.runtime);
                    app.runtime.take_op_flush()();
                    assert!(!ticket.reply().queued);
                    break;
                }
                assert!(ticket.reply().queued);
                assert!(app.runtime.trace_data().entries.is_empty());
                assert_eq!(app.runtime.hub.lock().unwrap().attachments["chief"][0].delivery.id, ticket.id);
            }
        });
    }

    #[test]
    fn attachment_plan_trace_survives_a_non_owner_removal() {
        with_app("app::attachments::tests::attachment_plan_trace_survives_a_non_owner_removal", |app, _| {
            let home = paths::atomics_home();
            let _owner = Inbox::new(&home, Default::default());
            let path = atomics_core::trace::disk::path(&home, Some("plan")).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"").unwrap();
            app.runtime.remove_plan_trace("plan");
            app.runtime.take_op_flush()();
            assert!(path.is_file());
        });
    }

    #[test]
    fn attachment_polling_an_idle_window_leaves_storage_unopened() {
        with_app("app::attachments::tests::attachment_polling_an_idle_window_leaves_storage_unopened", |app, _| {
            assert!(backend::Local(app.runtime).attachment_reply().is_none());
            assert!(app.runtime.attachments.get().is_none());
        });
    }

    #[test]
    fn attachment_sender_sees_retry_and_delivery_by_id() {
        with_app("app::attachments::tests::attachment_sender_sees_retry_and_delivery_by_id", |app, _| {
            let service = app.runtime.attachment_service();
            let ticket = DeliveryTicket::new("delivery".into(), Arc::new(|| true));
            service.deliveries.lock().unwrap().insert(7, ticket.clone());
            let status = || Request::Status { id: ticket.id.clone() };
            assert!(service.request(8, status(), Arc::new(|| true)).is_err());
            assert!(ticket.start_write());
            ticket.retry();
            let retry = service.request(7, status(), Arc::new(|| true)).unwrap();
            assert!(retry.queued && retry.retrying);
            ticket.received();
            assert!(ticket.reply().queued);
            *ticket.progress.lock().unwrap() = Progress::Sent;
            let done = service.request(7, status(), Arc::new(|| true)).unwrap();
            assert_eq!(done.id, ticket.id);
            assert!(done.complete && !done.queued && done.error.is_none());
        });
    }

    #[test]
    fn attachment_terminal_preview_waits_for_the_provider_delivery() {
        with_app("app::attachments::tests::attachment_terminal_preview_waits_for_the_provider_delivery", |app, _| {
            let terminal = Terminal::spawn(
                app.runtime.wake.clone(),
                app.runtime.theme.terminal_colors(),
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf 'Earlier: [ATOMICS user attachments]\n› Ask Codex to do anything\n'; exec cat".into(),
                ],
                &app.runtime.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !terminal.screen_text(60).contains("Ask Codex") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut chief = session("chief", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir);
            chief.chief = true;
            chief.terminal = Some(terminal);
            let ticket = DeliveryTicket::new("transfer".into(), Arc::new(|| true));
            chief.pending = Some(Pending {
                attachment: Some(ticket.clone()),
                text: "[ATOMICS user attachments]\nsample".into(),
                sent: Instant::now(),
                attempts: 1,
                was_working: true,
                codex: true,
                screen_before: String::new(),
                paste_seen: false,
                queued: false,
                reply: None,
            });
            app.runtime.sessions = vec![chief];
            app.runtime.verify_deliveries();
            assert!(ticket.reply().queued, "only the tracked provider submission can confirm a terminal attachment");
        });
    }

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
            let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let grant = live.clone();
            let worker = std::thread::spawn(move || {
                service.request(
                    7,
                    Request::Commit { id },
                    Arc::new(move || grant.load(std::sync::atomic::Ordering::Acquire)),
                )
            });
            let deliver =
                app.runtime.action_rx.recv_timeout(Duration::from_secs(5)).expect("verified commit reached State");
            live.store(false, std::sync::atomic::Ordering::Release);
            deliver(app.runtime);
            assert!(worker.join().unwrap().is_err(), "the live grant ended before host delivery");
            assert!(app.runtime.hub.lock().unwrap().attachments.is_empty());
            assert!(app.runtime.pending_receipts.is_empty());
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
                let service = app.runtime.attachment_service();
                let inbox = &service.inbox;
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
                let lookup = crate::app::tests::request(
                    app,
                    json!({"cmd":"attachments","conversation":"chief","id":files[0].id}),
                );
                assert_eq!(lookup.result["attachments"], json!(files));
                assert_eq!(
                    app.runtime.hub.lock().unwrap().attachments["chief"][0].message,
                    attachments::message(&files)
                );
                assert!(app.runtime.trace_data().entries.is_empty(), "queued hook delivery must not confirm Sent");
                let delivered = app
                    .runtime
                    .hub
                    .lock()
                    .unwrap()
                    .attachments
                    .remove("chief")
                    .unwrap()
                    .into_iter()
                    .map(|item| Some(item.delivery.id.clone()))
                    .collect::<Vec<_>>();
                app.runtime.confirm_attachments("chief", &delivered);
                let archive = atomics_core::trace::disk::path(&paths::atomics_home(), Some("attachment-plan")).unwrap();
                assert!(!archive.exists(), "receipt persistence runs after releasing State");
                crate::app::tests::settle_trace(app.runtime);
                let trace = app.runtime.trace_data();
                assert_eq!(trace.entries.len(), 1);
                assert_eq!(trace.entries[0].op, atomics_core::trace::TraceOp::Attachment);
                assert!(trace.entries[0].summary.contains(&files[0].file.sha256));
                app.runtime.take_op_flush()();
                let loaded = atomics_core::trace::disk::load(&archive, &Default::default()).unwrap();
                assert!(loaded.entries[0].summary.contains(&files[0].file.sha256));
                app.runtime.remove_plan_trace("different");
                assert!(Path::new(&files[0].path).is_file());
                app.runtime.remove_plan_trace("attachment-plan");
                app.runtime.take_op_flush()();
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
            let service = app.runtime.attachment_service();
            let inbox = &service.inbox;
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
            assert!(!app.runtime.pending_receipts[0].dispatched);
            // A terminal delivery can be acknowledged before the provider reports its conversation.
            app.runtime.pending_receipts[0].conversation = None;
            app.runtime.sessions[0].claude_id = None;
            app.runtime.pending_receipts[0].ticket.received();
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
            crate::app::tests::settle_trace(app.runtime);
            app.runtime.take_op_flush()();
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
                            if !app.runtime.hub.lock().unwrap().attachments.is_empty() {
                                break;
                            }
                            assert!(Instant::now() < deadline, "confirmed attachment not delivered");
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        let delivered = app
                            .runtime
                            .hub
                            .lock()
                            .unwrap()
                            .attachments
                            .remove("chief")
                            .unwrap()
                            .into_iter()
                            .map(|item| Some(item.delivery.id.clone()))
                            .collect::<Vec<_>>();
                        app.runtime.confirm_attachments("chief", &delivered);
                        crate::app::tests::settle_trace(app.runtime);
                        app.runtime.take_op_flush()();
                        let receipt: Receipt =
                            serde_json::from_str(&app.runtime.trace_data().entries[0].summary).unwrap();
                        assert_eq!(std::fs::read(receipt.path).unwrap(), b"original");
                        loop {
                            let labels = frame(app, vec![], vec![]);
                            if labels.iter().any(|(s, _)| s == "Sent to the chief") {
                                break;
                            }
                            assert!(Instant::now() < deadline, "sender dialog lost its queued delivery");
                            std::thread::sleep(Duration::from_millis(10));
                        }
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
                assert!(backend.attach_local(
                    &[atomics_ui::attachments::Prepared { info, bytes }],
                    None,
                    Default::default()
                ))
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
