//! Host authorization and persistence around the pure trace reducer.
use super::*;
use atomics_core::trace::{self, TraceLog};

const MAX_TRACE_JOBS: usize = 64;

/// Chief sends, tool events and ticket mirrors can wait for the worker's current credentials.
pub(super) enum TraceAction {
    Load { id: String, instructions: Option<String> },
    Handoff(String),
    Send(String),
    Capture(Box<Capture>),
    Respond { reference: Option<String>, limit: Option<usize>, reply: Sender<ControlOutcome> },
}

pub(super) struct Capture {
    pub(super) pane: String,
    agent: ChiefAgent,
    event: atomics_core::hook::HookEvent,
    plan: Option<String>,
    mirror: Option<String>,
    order: u64,
    saved: Option<ipc::TraceAck>,
}

/// Unix microseconds give a fresh host an order beyond earlier captures. The counter
/// also orders calls from different plans or conversations in the same clock tick.
pub(super) fn next_order() -> u64 {
    static LAST: Mutex<u64> = Mutex::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
    let mut last = LAST.lock().unwrap();
    *last = now.max(*last + 1);
    *last
}

type TraceReader = (Option<usize>, Sender<ControlOutcome>);

pub(super) struct TraceLoad {
    pane: String,
    conversation: String,
    result: Arc<OnceLock<(TraceLog, bool)>>,
    live: TraceLog,
    readers: Vec<TraceReader>,
    removed: HashSet<String>,
}

impl Drop for TraceLoad {
    fn drop(&mut self) {
        for (_, reply) in self.readers.drain(..) {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error("Chief conversation changed")));
        }
    }
}

impl State {
    pub(super) fn trace_data(&self) -> atomics_core::trace_feed::TraceData {
        use atomics_core::trace_feed::{MAX_STATES, TraceData};
        let session = self.chief_session_id().map(str::to_string);
        let entries = session
            .as_ref()
            .and_then(|id| self.traces.get(id))
            .map(|log| log.entries.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let cleaner = self.chief_cleaner(&self.cleaner());
        let entries = entries.into_iter().filter_map(|entry| entry.cleaned(&cleaner)).collect::<Vec<_>>();
        let mut states = std::collections::BTreeMap::new();
        if let Some(config) = self.ticket_config() {
            for name in entries.iter().flat_map(|e| &e.cards).filter_map(|c| c.link.state.as_ref()) {
                if states.len() < MAX_STATES
                    && name.len() <= 300
                    && let Some(status) = config.states.status_of(name)
                {
                    states.insert(name.clone(), status);
                }
            }
        }
        TraceData { session, entries, states }
    }

    pub(super) fn queue_trace(&mut self, config: Arc<atomics_core::tickets::TicketConfig>, action: TraceAction) {
        let seq = self.tickets.next_trace_job;
        self.tickets.next_trace_job += 1;
        if self.tickets.trace_jobs.len() >= MAX_TRACE_JOBS {
            self.unverified_trace(action);
            return;
        }
        self.tickets.trace_jobs.insert(seq, (config.clone(), action));
        self.send_trace_job(&config, seq);
    }

    fn unverified_trace(&mut self, action: TraceAction) {
        match action {
            TraceAction::Load { id, instructions: Some(_) } => self.chief_instructions_withheld(&id),
            TraceAction::Respond { reply, .. } => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error("Could not read trace credentials")));
            }
            TraceAction::Handoff(id) => {
                self.handoff_masks_ready(
                    &id,
                    Err("Could not read handoff credentials. Select the model again.".into()),
                );
            }
            TraceAction::Send(id) => {
                if let Some(index) = self.find(&id) {
                    self.sessions[index].chief_send_masks = ChiefSendMasks::Failed;
                    self.toast(
                        "The chief message is held because ticket credentials could not be read. Send again to retry.",
                    );
                }
            }
            TraceAction::Capture(capture) => {
                let mut capture = *capture;
                if conversation_turn(&capture.event) {
                    let redacted = |_| "[Turn withheld because credential masks could not be read]".into();
                    capture.event = atomics_core::hook::HookEvent {
                        name: capture.event.name,
                        session_id: capture.event.session_id,
                        prompt: capture.event.prompt.map(redacted),
                        last_assistant_message: capture.event.last_assistant_message.map(redacted),
                        ..Default::default()
                    };
                    capture.mirror = None;
                    self.ingest_chief_trace(capture, None, &self.chief_cleaner(&self.cleaner()));
                } else if let Some(saved) = capture.saved {
                    let _ = saved.send(Err(std::io::Error::other("Could not read trace credentials")));
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish_trace(&mut self, seq: u64, result: Result<crate::tickets::TicketOutput, String>) {
        let Some((config, action)) = self.tickets.trace_jobs.remove(&seq) else { return };
        let Ok(crate::tickets::TicketOutput::TraceCleaner(cleaner)) = result else {
            self.unverified_trace(action);
            return;
        };
        self.remember_chief_masks(cleaner.secrets());
        let cleaner = self.chief_cleaner(&cleaner);
        self.apply_trace(action, config, &cleaner);
    }

    fn apply_trace(
        &mut self,
        action: TraceAction,
        config: Arc<atomics_core::tickets::TicketConfig>,
        cleaner: &atomics_core::tickets::Cleaner,
    ) {
        match action {
            TraceAction::Load { id, instructions } => {
                if let Some(text) = instructions {
                    if self.ticket_config().as_deref() == Some(config.as_ref()) {
                        self.hub.lock().unwrap().set_chief_instructions(&id, cleaner.text(text.trim_start()));
                    } else {
                        self.chief_instructions_withheld(&id);
                    }
                }
                if !self.is_chief_replacement(&id) {
                    self.load_chief_trace_with(&id, cleaner);
                }
            }
            TraceAction::Handoff(id) => {
                self.handoff_masks_ready(&id, Ok(config));
            }
            TraceAction::Send(id) => {
                let Some(index) = self.find(&id) else { return };
                if self.chief_send_config(index).as_deref() == Some(config.as_ref()) {
                    self.sessions[index].chief_send_masks = ChiefSendMasks::Ready(config);
                    self.release_held(index);
                } else {
                    self.unverified_trace(TraceAction::Send(id));
                }
            }
            TraceAction::Capture(capture) => {
                self.ingest_chief_trace(*capture, Some(&config), cleaner);
            }
            TraceAction::Respond { reference, limit, reply } => {
                self.respond_to_trace(reference.as_deref(), limit, cleaner, reply);
            }
        }
    }

    pub(super) fn load_chief_trace(&mut self, id: &str, instructions: Option<String>) {
        if !self.sessions.iter().any(|session| session.id == id && session.chief)
            || instructions.is_none() && self.traces.contains_key(id)
        {
            return;
        }
        if let Some(config) = self.ticket_config().filter(|config| config.server.uses_oauth()) {
            if self
                .tickets
                .trace_jobs
                .values()
                .any(|(_, action)| matches!(action, TraceAction::Load { id: pending, .. } if pending == id))
            {
                return;
            }
            self.queue_trace(config, TraceAction::Load { id: id.into(), instructions });
            return;
        }
        if instructions.is_some() {
            self.chief_instructions_withheld(id);
        }
        if !self.is_chief_replacement(id) {
            self.load_chief_trace_with(id, &self.chief_cleaner(&self.cleaner()));
        }
    }

    fn chief_instructions_withheld(&mut self, id: &str) {
        if self.sessions.iter().any(|session| session.id == id && session.current_chief()) {
            self.toast("Custom chief instructions are withheld because ticket credentials could not be verified.");
        }
    }

    fn load_chief_trace_with(&mut self, id: &str, cleaner: &atomics_core::tickets::Cleaner) -> bool {
        if self.traces.contains_key(id) {
            return true;
        }
        let Some(chief) = self.sessions.iter().find(|s| s.id == id && s.chief) else { return false };
        let Some(conversation) = chief.claude_id.clone() else {
            self.traces.insert(id.into(), TraceLog::default());
            return true;
        };
        if self.trace_load.as_ref().is_some_and(|load| load.pane == id && load.conversation == conversation) {
            return false;
        }
        let result = Arc::new(OnceLock::new());
        let wake = self.wake.clone();
        self.trace_load = Some(TraceLoad {
            pane: id.into(),
            conversation: conversation.clone(),
            result: result.clone(),
            live: TraceLog::default(),
            readers: Vec::new(),
            removed: HashSet::new(),
        });
        let plans: Vec<_> = self.plans.iter().filter(|p| !p.id.is_empty()).map(|p| p.id.clone()).collect();
        let home = paths::atomics_home();
        let cleaner = cleaner.clone();
        self.trace_writes.push(Box::new(move || {
            let mut log = TraceLog::default();
            let mut failed = false;
            for plan in std::iter::once(None).chain(plans.iter().map(|id| Some(id.as_str()))) {
                match trace::disk::load_scope(&home, plan, &cleaner, &conversation) {
                    Ok(loaded) => log.merge(loaded),
                    Err(_) => failed = true,
                }
            }
            let _ = result.set((log, failed));
            wake.notify();
        }));
        false
    }

    pub(crate) fn finish_trace_load(&mut self) -> bool {
        let Some((mut log, failed)) = self.trace_load.as_ref().and_then(|load| load.result.get()).cloned() else {
            return false;
        };
        let mut load = self.trace_load.take().expect("completed load");
        if !self
            .sessions
            .iter()
            .any(|s| s.chief && s.id == load.pane && s.claude_id.as_ref() == Some(&load.conversation))
        {
            return false;
        }
        for plan in &load.removed {
            log.remove_plan(plan);
        }
        log.merge(std::mem::take(&mut load.live));
        self.traces.insert(load.pane.clone(), log);
        if failed {
            self.toast("Could not reload chief trace");
        }
        for (limit, reply) in load.readers.drain(..) {
            self.send_trace(&load.pane, limit, reply);
        }
        true
    }

    pub(super) fn capture_chief_trace(
        &mut self,
        index: usize,
        event: &atomics_core::hook::HookEvent,
        saved: Option<ipc::TraceAck>,
    ) {
        // Called only after on_hook's session-id, sandbox and stale-event checks.
        let tool = event.is_trace_call();
        let config = self.ticket_config();
        let turn = conversation_turn(event);
        let activity = event.thread == atomics_core::hook::HookThread::Main
            && config.as_ref().is_some_and(|c| c.remote.enabled)
            && matches!(event.name.as_str(), "Notification" | "PermissionRequest");
        if !self.sessions[index].chief
            || !(tool || turn || activity)
            || self.sessions[index].claude_id.as_deref() != Some(&event.session_id)
        {
            return;
        }
        let id = self.sessions[index].id.clone();
        if event.name == "UserPromptSubmit" {
            // Steering pasted for a plan is that plan's turn; never the plan the UI shows.
            self.tickets.remote.chief_plan = self.remote_turn_plan(event.prompt.as_deref().unwrap_or_default());
        }
        let mirror = self.tickets.remote.chief_plan.clone();
        let plan = event
            .tool_input
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| self.plans.iter().find(|plan| plan.id == id && plan.is_open()))
            .or_else(|| {
                self.tickets
                    .remote
                    .chief_plan
                    .as_ref()
                    .and_then(|id| self.plans.iter().find(|p| &p.id == id && p.is_open()))
            })
            .or_else(|| self.plans.get(self.selected_plan).filter(|plan| plan.is_open() && !plan.id.is_empty()))
            .map(|plan| plan.id.clone());
        let capture = Capture {
            pane: id,
            agent: super::chief_provider::session_provider(&self.sessions[index]),
            event: event.clone(),
            plan,
            mirror,
            order: next_order(),
            saved,
        };
        if let Some(config) = config.as_ref().filter(|config| config.server.uses_oauth()) {
            self.queue_trace(config.clone(), TraceAction::Capture(Box::new(capture)));
            return;
        }
        self.ingest_chief_trace(capture, config.as_deref(), &self.chief_cleaner(&self.cleaner()));
    }

    pub(super) fn live_chief_trace(&mut self, pane: &str, conversation: &str) -> Option<&mut TraceLog> {
        self.traces.get_mut(pane).or_else(|| {
            self.trace_load
                .as_mut()
                .filter(|load| load.pane == pane && load.conversation == conversation)
                .map(|load| &mut load.live)
        })
    }

    fn update_chief_turn_display(
        &mut self,
        id: &str,
        event: &atomics_core::hook::HookEvent,
        cleaner: &atomics_core::tickets::Cleaner,
    ) {
        if let Some(session) =
            self.sessions.iter_mut().find(|s| s.id == id && s.claude_id.as_deref() == Some(&event.session_id))
        {
            if event.name == "UserPromptSubmit" {
                session.last_prompt = event.prompt.as_ref().map(|prompt| cleaner.redacted(prompt));
            } else {
                session.last_message = event.last_assistant_message.as_ref().map(|reply| cleaner.redacted(reply));
            }
        }
    }

    fn ingest_chief_trace(
        &mut self,
        capture: Capture,
        config: Option<&atomics_core::tickets::TicketConfig>,
        cleaner: &atomics_core::tickets::Cleaner,
    ) {
        let Capture { pane, agent, event, plan, mirror, order, saved } = capture;
        let turn = conversation_turn(&event);
        if turn {
            self.record_chief_turn(agent, &event, cleaner);
            self.update_chief_turn_display(&pane, &event, cleaner);
        }
        let current =
            self.sessions.iter().any(|s| s.chief && s.id == pane && s.claude_id.as_deref() == Some(&event.session_id));
        if current {
            self.load_chief_trace_with(&pane, cleaner);
        }
        let mut detached = TraceLog::default();
        let log = if current { self.live_chief_trace(&pane, &event.session_id) } else { None }.unwrap_or(&mut detached);
        log.reserve_order(order);
        let entry = log
            .capture_for_plan(&event, config, cleaner, order / 1000, plan.as_deref())
            .map(|entry| entry.into_owned());
        if let Some(entry) = &entry {
            let home = paths::atomics_home();
            let entry = entry.clone();
            let history = self
                .trace_load
                .as_ref()
                .filter(|load| {
                    load.pane == pane
                        && load.conversation == event.session_id
                        && !turn
                        && entry.phase == trace::TracePhase::Post
                        && entry.elapsed_ms.is_none()
                })
                .map(|load| (load.result.clone(), load.removed.clone()));
            let store = self.attachment_service();
            let actions = self.action_tx.clone();
            self.trace_writes.push(Box::new(move || {
                // The archive worker has finished this conversation's read, even if the
                // pane has since switched. Preserve the start's plan when filing the Post.
                let entry = match history {
                    Some((history, removed)) => {
                        entry.with_start_from(&history.get().expect("ordered archive read").0, &removed)
                    }
                    None => entry,
                };
                let result = trace::disk::destination(&home, entry.plan_id.as_deref()).and_then(|path| {
                    if trace::disk::append(&path, &entry)? {
                        store.inbox.prune_trace(&path);
                    }
                    Ok(())
                });
                if result.is_err() {
                    let _ = actions.send(Box::new(|s| s.toast("Could not save chief trace")));
                }
                if let Some(saved) = saved {
                    let _ = saved.send(result);
                }
            }));
        }
        // A credential job from an old profile must not mirror into a new connection.
        if current && config == self.ticket_config().as_deref() {
            self.mirror_hook(&event, mirror.as_deref(), cleaner, entry.filter(|_| !turn));
        }
    }

    pub(super) fn remove_plan_trace(&mut self, plan: &str) {
        self.cancel_plan_attachments(plan);
        if let Some(load) = &mut self.trace_load {
            load.removed.insert(plan.into());
            load.live.remove_plan(plan);
        }
        let keep = |_: &u64, (_, action): &mut (_, TraceAction)| {
            let TraceAction::Capture(capture) = action else { return true };
            let Capture { event, plan: pending, mirror, .. } = capture.as_mut();
            if pending.as_deref() != Some(plan) {
                return true;
            }
            if !conversation_turn(event) {
                return false;
            }
            *pending = None;
            *mirror = None;
            true
        };
        self.tickets.trace_jobs.retain(keep);
        for log in self.traces.values_mut() {
            log.remove_plan(plan);
        }
        let home = paths::atomics_home();
        let store = self.attachment_service();
        let plan = plan.to_string();
        let actions = self.action_tx.clone();
        self.trace_writes.push(Box::new(move || {
            let removed = store
                .inbox
                .remove_plan(&plan)
                .and_then(|()| trace::disk::remove(&home, &plan).map_err(|e| e.to_string()));
            if removed.is_err() {
                let _ = actions.send(Box::new(|s| s.toast("Could not remove plan trace")));
            }
        }));
    }

    pub(super) fn request_trace(
        &mut self,
        reference: Option<String>,
        limit: Option<usize>,
        reply: Sender<ControlOutcome>,
    ) {
        if let Some(config) = self.ticket_config().filter(|config| config.server.uses_oauth()) {
            self.queue_trace(config, TraceAction::Respond { reference, limit, reply });
        } else {
            self.respond_to_trace(reference.as_deref(), limit, &self.chief_cleaner(&self.cleaner()), reply);
        }
    }

    fn respond_to_trace(
        &mut self,
        reference: Option<&str>,
        limit: Option<usize>,
        cleaner: &atomics_core::tickets::Cleaner,
        reply: Sender<ControlOutcome>,
    ) {
        let index = match reference {
            Some(reference) => self.find(reference),
            None => self.sessions.iter().position(|session| session.chief),
        };
        let Some(index) = index else {
            let response = if reference.is_some() {
                ControlResponse::error("No matching chief session")
            } else {
                ControlResponse::ok(json!({"entries": []}))
            };
            let _ = reply.send(ControlOutcome::Done(response));
            return;
        };
        if !self.sessions[index].chief {
            let _ =
                reply.send(ControlOutcome::Done(ControlResponse::error("Trace is available for chief sessions only")));
            return;
        }
        let id = self.sessions[index].id.clone();
        if self.load_chief_trace_with(&id, cleaner) {
            self.send_trace(&id, limit, reply);
        } else if let Some(load) = &mut self.trace_load
            && load.readers.len() < MAX_TRACE_JOBS
        {
            load.readers.push((limit, reply));
        } else {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error("Trace is busy; try again")));
        }
    }

    fn send_trace(&self, id: &str, limit: Option<usize>, reply: Sender<ControlOutcome>) {
        let cleaner = self.chief_cleaner(&self.cleaner());
        let entries: Vec<_> = self.traces[id]
            .recent(limit.unwrap_or(20))
            .into_iter()
            .cloned()
            .filter_map(|entry| entry.cleaned(&cleaner))
            .collect();
        let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(json!({"session": id, "entries": entries}))));
    }
}

fn conversation_turn(event: &atomics_core::hook::HookEvent) -> bool {
    event.thread == atomics_core::hook::HookThread::Main && matches!(event.name.as_str(), "UserPromptSubmit" | "Stop")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{request, session, with_app};

    #[test]
    fn cold_trace_completion_keeps_the_plan_recorded_at_start() {
        with_app("app::trace::tests::cold_trace_completion_keeps_the_plan_recorded_at_start", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            app.runtime.plans = ["original", "selected"].map(|id| Plan { id: id.into(), ..Default::default() }).into();
            let mut event = atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"PreToolUse", "session_id":"chief", "tool_name":"mcp__example__lookup",
                "tool_use_id":"paired-call"
            }))
            .unwrap();
            app.runtime.selected_plan = 0;
            app.runtime.capture_chief_trace(0, &event, None);
            crate::app::tests::settle_trace(app.runtime);
            app.runtime.take_op_flush()();
            app.runtime.traces.clear();
            app.runtime.selected_plan = 1;
            event.name = "PostToolUse".into();
            app.runtime.capture_chief_trace(0, &event, None);
            crate::app::tests::settle_trace(app.runtime);
            let path = trace::disk::destination(&paths::atomics_home(), Some("original")).unwrap();
            assert_eq!(
                trace::disk::load(&path, &Default::default()).unwrap().entries[0].phase,
                trace::TracePhase::Post
            );
            assert!(!trace::disk::destination(&paths::atomics_home(), Some("selected")).unwrap().exists());
        });
    }

    #[test]
    fn cold_trace_captures_survive_a_conversation_switch() {
        with_app("app::trace::tests::cold_trace_captures_survive_a_conversation_switch", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            for i in 0..65 {
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PreToolUse", "session_id":"chief", "tool_name":"mcp__example__lookup",
                    "tool_use_id": format!("call-{i}")
                }))
                .unwrap();
                app.runtime.capture_chief_trace(0, &event, None);
            }
            app.runtime.sessions[0].claude_id = Some("next".into());
            app.runtime.traces.clear();
            app.runtime.load_chief_trace("chief", None);
            crate::app::tests::settle_trace(app.runtime);
            assert!(app.runtime.trace_data().entries.is_empty());
            let path = trace::disk::path(&paths::atomics_home(), None).unwrap();
            assert_eq!(
                trace::disk::load_for_session(&path, &Default::default(), Some("chief")).unwrap().entries.len(),
                65
            );
        });
    }

    #[test]
    fn p5_review_unknown_chief_identity_never_guesses_legacy_ownership() {
        with_app("app::trace::tests::p5_review_unknown_chief_identity_never_guesses_legacy_ownership", |app, _| {
            let root = app.runtime.launch_dir.clone();
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
            chief.chief = true;
            chief.claude_id = None;
            app.runtime.sessions = vec![chief];
            let event = atomics_core::hook::HookEvent {
                name: "PostToolUse".into(),
                session_id: "chief-conversation".into(),
                tool_name: Some("mcp__atomics__atomics_wait".into()),
                ..Default::default()
            };
            let mut log = TraceLog::default();
            let current = log.ingest(&event, None, &Default::default(), 1).unwrap().clone();
            let mut legacy = current.clone();
            legacy.seq = 42;
            legacy.session_id.clear();
            let path = trace::disk::path(&paths::atomics_home(), None).unwrap();
            trace::disk::append(&path, &legacy).unwrap();
            trace::disk::append(&path, &current).unwrap();
            assert_eq!(request(app, json!({"cmd":"trace_get"})).result["entries"], json!([]));
            app.runtime.on_hook(HookMessage {
                pane: Some("chief".into()),
                ancestors: vec![],
                delivered: Vec::new(),
                approval_reply: None,
                event: atomics_core::hook::HookEvent {
                    name: "SessionStart".into(),
                    session_id: "chief-conversation".into(),
                    ..Default::default()
                },
            });
            let entries = request(app, json!({"cmd":"trace_get"})).result["entries"].as_array().unwrap().clone();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0]["session_id"], current.session_id);
            assert_eq!(trace::disk::load(&path, &Default::default()).unwrap().entries.len(), 2);
        });
    }

    #[test]
    fn chief_waiting_for_plan_approval_has_a_reason_and_clear_scopes_its_trace() {
        with_app(
            "app::trace::tests::chief_waiting_for_plan_approval_has_a_reason_and_clear_scopes_its_trace",
            |app, _| {
                use atomics_ui::Backend;
                let root = app.runtime.launch_dir.clone();
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Working, &root);
                chief.chief = true;
                app.runtime.sessions = vec![chief];
                app.runtime.plans = vec![Plan {
                    id: "approval".into(),
                    goal: "Ship the parser".into(),
                    tasks: vec![Task { id: "t1".into(), ..Default::default() }],
                    ..Default::default()
                }];
                let hook = |name: &str, conversation: &str| HookMessage {
                    pane: Some("chief".into()),
                    ancestors: Vec::new(),
                    delivered: Vec::new(),
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name": name, "session_id": conversation,
                        "source": "clear", "tool_name": "mcp__atomics__atomics_wait", "tool_use_id": "wait-approval",
                        "tool_input": {"statuses":["plan"]}
                    }))
                    .unwrap(),
                };
                app.runtime.on_hook(hook("PreToolUse", "chief"));
                crate::app::tests::settle_trace(app.runtime);
                let row = app.runtime.backend().session_row(0).unwrap();
                assert_eq!(row.status, AgentStatus::NeedsInput);
                assert_eq!(row.status_label(), "waiting for you: approve Ship the parser");
                assert!(row.needs_user());
                assert_eq!(app.runtime.describe(0)["status_hint"], json!(row.status_hint));
                app.runtime.plans[0].approved = true;
                assert_eq!(app.runtime.backend().session_row(0).unwrap().status, AgentStatus::Working);
                app.runtime.plans[0].approved = false;
                app.runtime.on_hook(hook("PostToolUse", "chief"));
                assert_eq!(app.runtime.backend().session_row(0).unwrap().status, AgentStatus::Working);
                app.runtime.plans[0].status = PlanStatus::Completed;
                app.runtime.on_hook(hook("SessionStart", "new-conversation"));
                assert!(app.runtime.trace_data().entries.is_empty());
                app.runtime.take_op_flush()();
                app.runtime.traces.clear();
                assert_eq!(request(app, json!({"cmd":"trace_get"})).result["entries"], json!([]));
                app.runtime.on_hook(hook("PreToolUse", "new-conversation"));
                crate::app::tests::settle_trace(app.runtime);
                assert_eq!(app.runtime.trace_data().entries.len(), 1);
                app.runtime.take_op_flush()();
                app.runtime.traces.clear();
                assert_eq!(request(app, json!({"cmd":"trace_get"})).result["entries"].as_array().unwrap().len(), 1);
            },
        );
    }

    #[test]
    fn trace_credential_backlog_is_bounded_and_refuses_extra_readers() {
        with_app("app::trace::tests::trace_credential_backlog_is_bounded_and_refuses_extra_readers", |app, _| {
            let config = Arc::new(atomics_core::tickets::TicketConfig::default());
            for seq in 0..MAX_TRACE_JOBS as u64 {
                app.runtime
                    .tickets
                    .trace_jobs
                    .insert(seq, (config.clone(), TraceAction::Load { id: seq.to_string(), instructions: None }));
            }
            let (reply, response) = channel();
            app.runtime.queue_trace(config.clone(), TraceAction::Respond { reference: None, limit: None, reply });
            let ControlOutcome::Done(response) = response.recv().unwrap() else { panic!("expected a refusal") };
            assert!(!response.ok);
            app.runtime.queue_trace(config, TraceAction::Load { id: "new-chief".into(), instructions: None });
            assert_eq!(app.runtime.tickets.trace_jobs.len(), MAX_TRACE_JOBS);
        });
    }

    #[test]
    fn a_refreshed_token_is_redacted_before_the_trace_is_shown_or_saved() {
        with_app("app::trace::tests::a_refreshed_token_is_redacted_before_the_trace_is_shown_or_saved", |app, _| {
            use crate::mcp_client::{FakeOAuthServer, McpClient};
            let fake = FakeOAuthServer::start();
            fake.state.lock().unwrap().expires_in = 30;
            let (dir, oauth) = crate::mcp_client::oauth_test_machine();
            let server = fake.server();
            oauth
                .sign_in_via(&server, Duration::from_secs(10), &|url| {
                    let url = url.to_string();
                    std::thread::spawn(move || crate::mcp_client::oauth_test_browse(&url));
                    Ok(())
                })
                .unwrap();
            app.runtime.config.tickets = toml::from_str("system = \"echo\"").unwrap();
            app.runtime.config.tickets.server = server.clone();
            app.runtime.apply_ticket_config();
            app.runtime.tickets.sign.override_oauth(oauth.clone());
            let root = app.runtime.launch_dir.clone();
            app.runtime.sessions = vec![session("chief", AgentKind::Claude, AgentStatus::Idle, &root)];
            app.runtime.sessions[0].chief = true;
            let capture = |app: &mut Desktop<'_>, token: &str, id: &str| {
                let event = atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"PostToolUse", "session_id":"chief", "tool_name":"mcp__example__read",
                    "tool_use_id":id, "tool_input":{"query":token}, "tool_response":{"text":token}
                }))
                .unwrap();
                app.runtime.capture_chief_trace(0, &event, None);
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                while !app.runtime.tickets.trace_jobs.is_empty() {
                    app.runtime.ticket_results();
                    crate::app::tests::settle_trace(app.runtime);
                    assert!(Instant::now() < deadline, "trace worker timed out");
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            let old = fake.state.lock().unwrap().access.iter().next().unwrap().clone();
            capture(app, &old, "before");
            // Connecting refreshes the nearly expired token through the real OAuth path.
            fake.state.lock().unwrap().expires_in = 3600;
            let client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
            let new = client.oauth_token().unwrap();
            assert_ne!(old, new);
            capture(app, new, "after");
            let entries = serde_json::to_string(&app.runtime.traces["chief"].entries).unwrap();
            assert!(entries.contains("***") && !entries.contains(new) && !entries.contains(&old), "{entries}");
            app.runtime.take_op_flush()();
            let path = trace::disk::path(&paths::atomics_home(), None).unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            assert!(!saved.contains(new) && !saved.contains(&old), "{saved}");
            app.runtime.traces.clear();
            let (reply, received) = channel();
            app.runtime.request_trace(None, None, reply);
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
            let response = loop {
                app.runtime.ticket_results();
                crate::app::tests::settle_trace(app.runtime);
                if let Ok(ControlOutcome::Done(response)) = received.try_recv() {
                    break response;
                }
                assert!(Instant::now() < deadline, "trace response timed out");
                std::thread::sleep(Duration::from_millis(5));
            };
            assert!(response.ok);
            assert_eq!(response.result["entries"].as_array().unwrap().len(), 2);
            assert!(!response.result.to_string().contains(new));
            // Even a warm cache must not permit capture when its fingerprint cannot be checked.
            std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
            capture(app, new, "unreadable-keychain");
            assert_eq!(app.runtime.traces["chief"].entries.len(), 2);
            assert_eq!(std::fs::read_to_string(path).unwrap(), saved);
        });
    }

    #[test]
    fn trace_captures_only_chief_and_reloads_the_same_cleaned_entries() {
        with_app("app::trace::tests::trace_captures_only_chief_and_reloads_the_same_cleaned_entries", |app, _| {
            let root = app.runtime.launch_dir.clone();
            app.runtime.sessions = vec![
                session("worker", AgentKind::Claude, AgentStatus::Idle, &root),
                session("chief", AgentKind::Claude, AgentStatus::Idle, &root),
            ];
            app.runtime.sessions[1].chief = true;
            let hook = |pane: &str, session_id: &str, name: &str| {
                HookMessage {
                pane: Some(pane.into()), ancestors: Vec::new(), delivered: Vec::new(), approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":session_id, "tool_name":"mcp__atomics__atomics_ticket_read",
                    "tool_use_id":"toolu_example", "tool_input":{"ticket":"DEMO-1"},
                    "tool_response":{"structuredContent":{"card":{"system":"example","id":"1","key":"DEMO-1","title":"A ticket"}}}
                })).unwrap(),
            }
            };
            app.runtime.on_hook(hook("worker", "worker", "PostToolUse"));
            assert!(app.runtime.traces.is_empty());
            assert!(!paths::atomics_home().join("trace").exists());
            app.runtime.on_hook(hook("chief", "stale-session", "PreToolUse"));
            assert!(app.runtime.traces.is_empty());
            app.runtime.on_hook(hook("chief", "chief", "PreToolUse"));
            app.runtime.on_hook(hook("chief", "chief", "PostToolUse"));
            let response = request(app, json!({"cmd":"trace_get", "limit":20}));
            assert!(response.ok);
            assert_eq!(response.result["entries"].as_array().unwrap().len(), 1);
            assert_eq!(response.result["entries"][0]["cards"][0]["key"], "DEMO-1");
            assert_eq!(response.result["entries"][0]["state"], "ok");
            assert!(!request(app, json!({"cmd":"trace_get", "session":"worker"})).ok);
            app.runtime.take_op_flush()();
            app.runtime.traces.clear();
            assert_eq!(request(app, json!({"cmd":"trace_get", "limit":20})).result, response.result);
            assert_eq!(request(app, json!({"cmd":"trace_get", "limit":0})).result["entries"], json!([]));
            app.runtime.plans.push(Plan { id: "trace-plan".into(), ..Default::default() });
            app.runtime.selected_plan = app.runtime.plans.len() - 1;
            let mut planned_call = hook("chief", "chief", "PostToolUse");
            planned_call.event.tool_use_id = Some("another-call".into());
            app.runtime.on_hook(planned_call);
            app.runtime.take_op_flush()();
            let path = trace::disk::path(&paths::atomics_home(), Some("trace-plan")).unwrap();
            assert!(path.exists());
            app.runtime.remove_plan_trace("trace-plan");
            app.runtime.take_op_flush()();
            assert!(!path.exists());
            assert!(
                app.runtime.traces["chief"].entries.iter().all(|entry| entry.plan_id.as_deref() != Some("trace-plan"))
            );
        });
    }
    #[test]
    fn chief_waiting_for_plan_approval_has_a_reason_and_ignores_foreign_trace() {
        with_app(
            "app::trace::tests::chief_waiting_for_plan_approval_has_a_reason_and_ignores_foreign_trace",
            |app, _| {
                use atomics_ui::Backend;
                let root = app.runtime.launch_dir.clone();
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Working, &root);
                chief.chief = true;
                app.runtime.sessions = vec![chief];
                app.runtime.plans = vec![Plan {
                    id: "approval".into(),
                    goal: "Ship the private-parser-name".into(),
                    tasks: vec![Task { id: "t1".into(), ..Default::default() }],
                    ..Default::default()
                }];
                app.runtime.remember_chief_masks(&["private-parser-name".into()]);
                let hook = |name: &str, conversation: &str| HookMessage {
                    pane: Some("chief".into()),
                    ancestors: Vec::new(),
                    delivered: Vec::new(),
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name": name, "session_id": conversation,
                        "tool_name": "mcp__atomics__atomics_wait", "tool_use_id": "wait-approval",
                        "tool_input": {"statuses":["plan"]}
                    }))
                    .unwrap(),
                };
                app.runtime.on_hook(hook("PreToolUse", "chief"));
                crate::app::tests::settle_trace(app.runtime);
                let row = app.runtime.backend().session_row(0).unwrap();
                assert_eq!(row.status, AgentStatus::NeedsInput);
                assert_eq!(row.status_label(), "waiting for you: approve Ship the ***");
                let out = atomics_core::projection::Projections::default();
                app.runtime.publish_projections(&out, &mut Default::default(), &Default::default());
                let projected = serde_json::to_string(&out.sessions.load().data).unwrap();
                assert!(!projected.contains("private-parser-name"));
                assert!(row.needs_user());
                assert_eq!(app.runtime.describe(0)["status_hint"], json!(row.status_hint));
                app.runtime.plans[0].approved = true;
                assert_eq!(app.runtime.backend().session_row(0).unwrap().status, AgentStatus::Working);
                app.runtime.plans[0].approved = false;
                app.runtime.on_hook(hook("PostToolUse", "chief"));
                assert_eq!(app.runtime.backend().session_row(0).unwrap().status, AgentStatus::Working);
                app.runtime.plans[0].status = PlanStatus::Completed;
                app.runtime.on_hook(hook("SessionStart", "new-conversation"));
                app.runtime.on_hook(hook("PreToolUse", "new-conversation"));
                assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some("chief"));
                assert_eq!(app.runtime.trace_data().entries.len(), 1);
                app.runtime.take_op_flush()();
                app.runtime.traces.clear();
                assert_eq!(request(app, json!({"cmd":"trace_get"})).result["entries"].as_array().unwrap().len(), 1);
            },
        );
    }

    #[test]
    fn chief_refreshed_tokens_are_redacted_before_saving_turns_and_later_export() {
        with_app(
            "app::trace::tests::chief_refreshed_tokens_are_redacted_before_saving_turns_and_later_export",
            |app, _| {
                use crate::mcp_client::{FakeOAuthServer, McpClient};
                let fake = FakeOAuthServer::start();
                fake.state.lock().unwrap().expires_in = 30;
                let (dir, oauth) = crate::mcp_client::oauth_test_machine();
                let server = fake.server();
                oauth
                    .sign_in_via(&server, Duration::from_secs(10), &|url| {
                        let url = url.to_string();
                        std::thread::spawn(move || crate::mcp_client::oauth_test_browse(&url));
                        Ok(())
                    })
                    .unwrap();
                app.runtime.config.tickets = toml::from_str("system = \"echo\"").unwrap();
                app.runtime.config.tickets.server = server.clone();
                app.runtime.apply_ticket_config();
                app.runtime.tickets.sign.override_oauth(oauth.clone());
                let root = app.runtime.launch_dir.clone();
                app.runtime.sessions = vec![session("chief", AgentKind::Claude, AgentStatus::Idle, &root)];
                app.runtime.sessions[0].chief = true;
                let capture = |app: &mut Desktop<'_>, token: &str, event: &str, text: &str| {
                    let body = format!("{text} {token}");
                    let event = atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":event, "session_id":"chief", "prompt":body, "last_assistant_message":body,
                    }))
                    .unwrap();
                    app.runtime.capture_chief_trace(0, &event, None);
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                    while !app.runtime.tickets.trace_jobs.is_empty() {
                        app.runtime.ticket_results();
                        crate::app::tests::settle_trace(app.runtime);
                        app.runtime.take_op_flush()();
                        assert!(Instant::now() < deadline, "trace worker timed out");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                };
                let old = fake.state.lock().unwrap().access.iter().next().unwrap().clone();
                capture(app, &old, "UserPromptSubmit", "First prompt");
                // Connecting refreshes the nearly expired token through the real OAuth path.
                fake.state.lock().unwrap().expires_in = 3600;
                let client = McpClient::connect_with(&server, Duration::from_secs(5), |_| None, Some(&oauth)).unwrap();
                let new = client.oauth_token().unwrap();
                assert_ne!(old, new);
                capture(app, new, "Stop", "First reply");
                let history = paths::atomics_home().join("chief-transcripts/claude/chief.txt");
                let initial_history = std::fs::read_to_string(&history).unwrap();
                assert!(!initial_history.contains(new) && !initial_history.contains(&old));
                std::fs::write(dir.path().join("fail-keychain"), "").unwrap();
                capture(app, new, "UserPromptSubmit", "Pending prompt");
                capture(app, new, "Stop", "Pending reply");
                let withheld = std::fs::read_to_string(&history).unwrap();
                assert!(
                    withheld.starts_with(&initial_history) && withheld.contains("credential masks could not be read")
                );
                assert_eq!(app.runtime.traces["chief"].entries.len(), 4);
                std::fs::remove_file(dir.path().join("fail-keychain")).unwrap();
                let entries = serde_json::to_string(&app.runtime.traces["chief"].entries).unwrap();
                assert!(entries.contains("***") && !entries.contains(new) && !entries.contains(&old), "{entries}");
                let path = trace::disk::path(&paths::atomics_home(), None).unwrap();
                let saved = std::fs::read_to_string(&path).unwrap();
                assert!(!saved.contains(new) && !saved.contains(&old), "{saved}");
                app.runtime.take_op_flush()();
                app.runtime.traces.clear();
                let (reply, received) = channel();
                app.runtime.request_trace(None, None, reply);
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
                let response = loop {
                    app.runtime.ticket_results();
                    crate::app::tests::settle_trace(app.runtime);
                    app.runtime.take_op_flush()();
                    if let Ok(ControlOutcome::Done(response)) = received.try_recv() {
                        break response;
                    }
                    assert!(Instant::now() < deadline, "trace response timed out");
                    std::thread::sleep(Duration::from_millis(5));
                };
                assert!(response.ok);
                let exported = response.result.to_string();
                assert_eq!(response.result["entries"].as_array().unwrap().len(), 4);
                assert!(exported.contains("credential masks could not be read"));
                assert!(!exported.contains("Pending prompt") && !exported.contains("Pending reply"));
                assert!(!exported.contains(new) && !exported.contains(&old));
                let transcript = std::fs::read_to_string(history).unwrap();
                assert!(transcript.contains("credential masks could not be read"));
                for output in [
                    transcript,
                    std::fs::read_to_string(path).unwrap(),
                    serde_json::to_string(&app.runtime.store.chief_history).unwrap(),
                ] {
                    assert!(!output.contains(new) && !output.contains(&old));
                }
            },
        );
    }
}
