//! Host authorization and persistence around the pure trace reducer.
use super::*;
use atomics_core::trace::{self, TRANSCRIPT_ASSISTANT_MESSAGE, TraceLog};

mod rollout;
pub(super) use rollout::ChiefTranscript;

#[cfg(test)]
thread_local! {
    static PROJECTION_CLEANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn clean_for_projection(
    entry: &trace::TraceEntry,
    cleaner: &atomics_core::tickets::Cleaner,
) -> Option<trace::TraceEntry> {
    #[cfg(test)]
    PROJECTION_CLEANS.set(PROJECTION_CLEANS.get() + 1);
    entry.clone().cleaned(cleaner)
}

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
    chat: Option<String>,
    agent: ChiefAgent,
    event: atomics_core::hook::HookEvent,
    plan: Option<String>,
    mirror: Option<String>,
    order: u64,
    saved: Option<ipc::TraceAck>,
    reply_in_rollout: bool,
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

use atomics_core::tickets::{Cleaner, StateMap};
use atomics_core::trace_feed::{MAX_STATES, TraceData};

pub(super) struct TraceSnapshot {
    session: Option<String>,
    log: Option<Arc<TraceLog>>,
    states: Option<StateMap>,
    mask_epoch: u64,
}

#[derive(Default)]
pub(super) struct TraceProjection {
    session: Option<String>,
    log: Option<std::sync::Weak<TraceLog>>,
    states: Option<StateMap>,
    masks: Option<Arc<Cleaner>>,
    mask_epoch: u64,
    entries: HashMap<u64, (trace::TraceEntry, Option<trace::TraceEntry>)>,
}

impl TraceProjection {
    /// Reuse cleaned entries across appends, evictions and updates to a running call.
    pub(super) fn update(&mut self, snap: TraceSnapshot, masks: Arc<Cleaner>) -> Option<TraceData> {
        let same_masks = self.masks.as_ref().is_some_and(|old| Arc::ptr_eq(old, &masks));
        let same_log = match (&self.log, &snap.log) {
            (Some(old), Some(log)) => std::ptr::eq(old.as_ptr(), Arc::as_ptr(log)),
            (None, None) => true,
            _ => false,
        };
        let same_session = self.session == snap.session;
        if same_masks && same_session && self.states == snap.states && self.mask_epoch == snap.mask_epoch {
            if same_log {
                return None;
            }
            // A hook can reserve an order without leaving an entry. A mutable borrow of the log
            // still changes its copy-on-write identity in that case.
            if let Some(log) = &snap.log
                && log.entries.len() == self.entries.len()
                && log.entries.iter().all(|entry| self.entries.get(&entry.seq).is_some_and(|(old, _)| old == entry))
            {
                self.log = Some(Arc::downgrade(log));
                return None;
            }
        }
        let mut previous = std::mem::take(&mut self.entries);
        if !same_masks || !same_session {
            previous.clear();
        }
        self.session.clone_from(&snap.session);
        self.log = snap.log.as_ref().map(Arc::downgrade);
        self.states = snap.states;
        self.mask_epoch = snap.mask_epoch;
        let entries: Vec<_> = snap
            .log
            .iter()
            .flat_map(|log| &log.entries)
            .filter_map(|entry| {
                // A completion retains its pre-event sequence. Check the contents as well.
                let cached = previous
                    .remove(&entry.seq)
                    .filter(|(original, _)| original == entry)
                    .unwrap_or_else(|| (entry.clone(), clean_for_projection(entry, &masks)));
                let cleaned = cached.1.clone();
                self.entries.insert(entry.seq, cached);
                cleaned
            })
            .collect();
        self.masks = Some(masks);
        let mut states = std::collections::BTreeMap::new();
        if let Some(config) = &self.states {
            for name in entries.iter().flat_map(|e| &e.cards).filter_map(|c| c.link.state.as_ref()) {
                if states.len() < MAX_STATES
                    && name.len() <= 300
                    && let Some(status) = config.status_of(name)
                {
                    states.insert(name.clone(), status);
                }
            }
        }
        Some(TraceData { session: snap.session, entries, states })
    }
}

impl State {
    pub(crate) fn attach_trace_projection(&mut self, out: Arc<atomics_core::projection::Projections>) {
        self.trace_projections = Some(out);
    }

    pub(super) fn clear_trace_projection(&self) {
        if let Some(out) = &self.trace_projections {
            out.trace.publish(TraceData::default());
        }
        *self.past_turns.borrow_mut() = Default::default();
    }

    pub(super) fn trace_snapshot(&self) -> TraceSnapshot {
        let session = self.chief_session_id().map(str::to_string);
        let log = session.as_ref().and_then(|id| self.traces.get(id)).cloned();
        TraceSnapshot {
            session,
            log,
            states: self.ticket_config().map(|config| config.states.clone()),
            mask_epoch: self.chief_mask_epoch,
        }
    }

    pub(super) fn trace_data(&self) -> TraceData {
        if let Some(out) = &self.trace_projections {
            let trace = out.trace.load();
            return if trace.data.session.as_deref() == self.chief_session_id() {
                trace.data.clone()
            } else {
                TraceData::default()
            };
        }
        // Tests can exercise the window model without a runtime publisher.
        #[cfg(test)]
        return TraceProjection::default().update(self.trace_snapshot(), self.chief_view_cleaner()).unwrap_or_default();
        #[cfg(not(test))]
        TraceData::default()
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
            self.traces.insert(id.into(), Arc::default());
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
        self.traces.insert(load.pane.clone(), Arc::new(log));
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
        let mut reply_in_rollout = false;
        let session = &self.sessions[index];
        let provider = super::chief_provider::session_provider(session);
        let mut followed_hook = None;
        if session.chief
            && session.sandbox.is_none()
            && matches!(provider, ChiefAgent::Codex | ChiefAgent::Claude)
            && event.thread == atomics_core::hook::HookThread::Main
            && session.claude_id.as_deref() == Some(&event.session_id)
        {
            if matches!(event.name.as_str(), "SessionStart" | "UserPromptSubmit")
                && self
                    .chief_transcript
                    .as_ref()
                    .is_none_or(|tail| tail.pane != session.id || tail.conversation != event.session_id)
                && let Some(path) = &event.transcript_path
            {
                let resume = event.name != "SessionStart" || event.start_source.as_deref() == Some("resume");
                self.chief_transcript = ChiefTranscript::new(&session.id, &event.session_id, provider, path, resume);
            }
            // Records already written belong before this hook, including a steering prompt that
            // may change the chat or plan scope, and the final reply that Stop repeats.
            self.poll_chief_rollout();
            if let Some(tail) = &mut self.chief_transcript {
                if tail.follows_claude() && event.chief_update.is_some() {
                    // A local transcript supplies every written block. The hook sample is a fallback for remote files.
                    let mut hook = event.clone();
                    hook.chief_update = None;
                    followed_hook = Some(hook);
                }
                match event.name.as_str() {
                    "UserPromptSubmit" => tail.prompt(),
                    "Stop" => reply_in_rollout = tail.stop(event.last_assistant_message.as_deref()),
                    _ => {}
                }
            }
        }
        self.capture_chief_event(index, followed_hook.as_ref().unwrap_or(event), saved, reply_in_rollout);
    }

    pub(super) fn poll_chief_rollout(&mut self) -> bool {
        let Some(tail) = &mut self.chief_transcript else { return false };
        let Some(index) = self.sessions.iter().position(|session| {
            session.chief && session.id == tail.pane && session.claude_id.as_deref() == Some(&tail.conversation)
        }) else {
            self.chief_transcript = None;
            return false;
        };
        let messages = tail.read();
        let changed = !messages.is_empty();
        let conversation = tail.conversation.clone();
        for text in messages {
            let event = atomics_core::hook::HookEvent {
                name: TRANSCRIPT_ASSISTANT_MESSAGE.into(),
                session_id: conversation.clone(),
                last_assistant_message: Some(text),
                ..Default::default()
            };
            self.capture_chief_event(index, &event, None, false);
        }
        changed
    }

    fn capture_chief_event(
        &mut self,
        index: usize,
        event: &atomics_core::hook::HookEvent,
        saved: Option<ipc::TraceAck>,
        reply_in_rollout: bool,
    ) {
        let tool = event.is_trace_call();
        let config = self.ticket_config();
        let turn = conversation_turn(event);
        let activity = event.thread == atomics_core::hook::HookThread::Main
            && config.as_ref().is_some_and(|c| c.remote.enabled)
            && matches!(event.name.as_str(), "Notification" | "PermissionRequest");
        if !self.sessions[index].chief
            || !(tool
                || turn
                || activity
                || (event.thread == atomics_core::hook::HookThread::Main && event.chief_update.is_some()))
            || self.sessions[index].claude_id.as_deref() != Some(&event.session_id)
        {
            return;
        }
        let id = self.sessions[index].id.clone();
        if event.name == "UserPromptSubmit" {
            // Steering pasted for a plan is that plan's turn; never the plan the UI shows.
            let prompt = event.prompt.as_deref().unwrap_or_default();
            self.sessions[index].simple_turn_plan = prompt
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("[Plan ")?.strip_suffix(']'))
                .and_then(|id| self.plans.iter().find(|p| p.id == id))
                .map(|plan| (event.session_id.clone(), plan.id.clone()));
            self.sessions[index].simple_chat =
                trace::chat_scope(prompt).map(|chat| (event.session_id.clone(), chat.to_owned()));
            let mirror = self.remote_turn_plan(prompt);
            self.tickets.remote.chief_plan = mirror
                .filter(|id| self.sessions[index].simple_turn_plan.as_ref().is_none_or(|(_, selected)| id == selected));
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
            .or_else(|| {
                let (conversation, board) = self.sessions[index].simple_turn_plan.as_ref()?;
                self.plans.iter().find(|p| conversation == &event.session_id && &p.id == board)
            })
            .or_else(|| {
                if self.sessions[index].simple_chat.as_ref().is_some_and(|(session, _)| session == &event.session_id) {
                    return None;
                }
                self.plans.get(self.selected_plan).filter(|plan| plan.is_open() && !plan.id.is_empty())
            })
            .map(|plan| plan.id.clone());
        if event.name == "UserPromptSubmit"
            && let Some(id) = &plan
        {
            self.sessions[index].simple_turn_plan = Some((event.session_id.clone(), id.clone()));
        }
        let capture = Capture {
            chat: self.sessions[index]
                .simple_chat
                .as_ref()
                .filter(|(root, _)| root == &event.session_id)
                .map(|(_, chat)| chat.clone()),
            pane: id,
            agent: super::chief_provider::session_provider(&self.sessions[index]),
            event: event.clone(),
            plan,
            mirror,
            order: next_order(),
            saved,
            reply_in_rollout,
        };
        if let Some(config) = config.as_ref().filter(|config| config.server.uses_oauth()) {
            self.queue_trace(config.clone(), TraceAction::Capture(Box::new(capture)));
            return;
        }
        self.ingest_chief_trace(capture, config.as_deref(), &self.chief_cleaner(&self.cleaner()));
    }

    pub(super) fn live_chief_trace(&mut self, pane: &str, conversation: &str) -> Option<&mut TraceLog> {
        self.traces.get_mut(pane).map(Arc::make_mut).or_else(|| {
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
        let Capture { pane, agent, event, plan, mirror, order, saved, chat, reply_in_rollout } = capture;
        let turn = conversation_turn(&event);

        let current =
            self.sessions.iter().any(|s| s.chief && s.id == pane && s.claude_id.as_deref() == Some(&event.session_id));
        if current {
            self.load_chief_trace_with(&pane, cleaner);
        }
        let mut detached = TraceLog::default();
        let log = if current { self.live_chief_trace(&pane, &event.session_id) } else { None }.unwrap_or(&mut detached);
        log.reserve_order(order);
        let progress =
            event.chief_update.as_ref().filter(|_| event.thread == atomics_core::hook::HookThread::Main).map(|text| {
                atomics_core::hook::HookEvent {
                    name: "AssistantProgress".into(),
                    session_id: event.session_id.clone(),
                    last_assistant_message: Some(text.clone()),
                    ..Default::default()
                }
            });
        let progress = progress
            .as_ref()
            .and_then(|msg| log.capture_in_chat(msg, None, cleaner, order / 1000, plan.as_deref(), chat.as_deref()))
            .map(|entry| entry.into_owned());
        let entry = if reply_in_rollout {
            None
        } else {
            log.capture_in_chat(&event, config, cleaner, order / 1000, plan.as_deref(), chat.as_deref())
                .map(|entry| entry.into_owned())
        };
        if turn && event.name != TRANSCRIPT_ASSISTANT_MESSAGE {
            self.record_chief_turn(agent, &event, cleaner, chat.as_deref());
            self.update_chief_turn_display(&pane, &event, cleaner);
        }
        for (item, saved) in
            progress.as_ref().map(|item| (item, None)).into_iter().chain(entry.as_ref().map(|item| (item, saved)))
        {
            let home = paths::atomics_home();
            let entry = item.clone();
            let closed = entry
                .plan_id
                .as_ref()
                .and_then(|id| self.plans.iter().find(|p| &p.id == id && !p.is_open()))
                .map(|p| (p.id.clone(), p.status));
            if let Some((id, _)) = &closed
                && self.past_turns.get_mut().plan == *id
            {
                *self.past_turns.get_mut() = Default::default();
            }
            let cleaner = cleaner.clone();
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
                let result = if let Some((id, status)) = closed {
                    if status == atomics_core::plan::PlanStatus::Completed && entry.turn.is_some() {
                        trace::disk::save_completed_turns(&home, &id, &cleaner, Some(&entry))
                    } else {
                        Ok(())
                    }
                } else {
                    trace::disk::destination(&home, entry.plan_id.as_deref()).and_then(|path| {
                        if trace::disk::append(&path, &entry)? {
                            store.inbox.prune_trace(&path);
                        }
                        Ok(())
                    })
                };
                if result.is_err() {
                    let _ = actions.send(Box::new(|s| s.toast("Could not save chief trace")));
                }
                if let Some(saved) = saved {
                    let _ = saved.send(result);
                }
            }));
        }
        if let Some(created) = entry
            .as_ref()
            .filter(|e| e.tool == "atomics_plan_set" && e.state == trace::TraceState::Ok)
            .and_then(|e| e.plan.as_ref()?.id.as_ref())
            && current
            && let Some(session) = self.sessions.iter_mut().find(|s| s.id == pane)
        {
            session.simple_turn_plan = Some((event.session_id.clone(), created.clone()));
            if let Some(chat) = entry.as_ref().and_then(|e| e.chat_id.as_ref())
                && let Some(saved) = self.store.chief_history.iter_mut().find(|c| &c.conversation == chat)
            {
                saved.plan = Some(created.clone());
                if let Some(board) = self.plans.iter_mut().find(|p| &p.id == created && p.first_message.is_none()) {
                    board.first_message = Some(saved.title.clone());
                    if let Err(error) = board.save_later_in(&paths::atomics_home()) {
                        self.toast(format!("Could not save the chat title: {error}"));
                    }
                }
                let _ = self.store.save_later(&paths::store_file());
            }
        }
        // A credential job from an old profile must not mirror into a new connection.
        if current && event.name != TRANSCRIPT_ASSISTANT_MESSAGE && config == self.ticket_config().as_deref() {
            self.mirror_hook(&event, mirror.as_deref(), cleaner, entry.filter(|_| !turn));
        }
    }

    pub(super) fn close_plan_trace(&mut self, plan: &str, status: PlanStatus) {
        let completed = status == PlanStatus::Completed;
        if self.past_turns.get_mut().plan == plan {
            *self.past_turns.get_mut() = Default::default();
        }
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
            if !completed {
                *pending = None;
            }
            *mirror = None;
            true
        };
        self.tickets.trace_jobs.retain(keep);
        for log in self.traces.values_mut() {
            Arc::make_mut(log).remove_plan(plan);
        }
        let home = paths::atomics_home();
        let store = self.attachment_service();
        let plan = plan.to_string();
        let cleaner = completed.then(|| self.chief_cleaner(&self.cleaner()));
        let actions = self.action_tx.clone();
        self.trace_writes.push(Box::new(move || {
            let removed = store.inbox.remove_plan(&plan).and_then(|()| {
                match cleaner {
                    Some(cleaner) => trace::disk::save_completed_turns(&home, &plan, &cleaner, None),
                    None => trace::disk::remove(&home, &plan),
                }
                .map_err(|e| e.to_string())
            });
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
    event.thread == atomics_core::hook::HookThread::Main
        && matches!(event.name.as_str(), "UserPromptSubmit" | "Stop" | TRANSCRIPT_ASSISTANT_MESSAGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{request, session, with_app};

    #[test]
    fn trace_projection_only_cleans_changed_entries_and_applies_late_masks() {
        with_app("app::trace::tests::trace_projection_only_cleans_changed_entries_and_applies_late_masks", |app, _| {
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir);
            chief.chief = true;
            app.runtime.sessions = vec![chief];
            let mut log = TraceLog::default();
            let event = |i: usize, phase: &str| {
                atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name": phase, "session_id": "chief", "tool_name": "mcp__example__lookup",
                    "tool_use_id": format!("call-{i}"), "tool_input": {"query": "private migration label"}
                }))
                .unwrap()
            };
            for i in 0..trace::MAX_ENTRIES {
                log.ingest(&event(i, "PreToolUse"), None, &Default::default(), i as u64);
            }
            app.runtime.traces.insert("chief".into(), Arc::new(log));
            let out = Arc::new(atomics_core::projection::Projections::default());
            app.runtime.attach_trace_projection(out.clone());
            let bodies = Default::default();
            let mut cache = Default::default();
            PROJECTION_CLEANS.set(0);
            app.runtime.publish_projections(&out, &mut cache, &bodies);
            let first = out.trace.load();
            assert_eq!(first.data.entries.len(), trace::MAX_ENTRIES);
            assert_eq!(PROJECTION_CLEANS.get(), trace::MAX_ENTRIES);
            assert!(first.data.entries[0].summary.contains("private migration label"));
            for _ in 0..20 {
                app.runtime.publish_projections(&out, &mut cache, &bodies);
            }
            assert!(Arc::ptr_eq(&first, &out.trace.load()));
            assert_eq!(PROJECTION_CLEANS.get(), trace::MAX_ENTRIES);

            app.runtime.live_chief_trace("chief", "chief").unwrap().reserve_order(201);
            app.runtime.publish_projections(&out, &mut cache, &bodies);
            assert!(Arc::ptr_eq(&first, &out.trace.load()));
            assert_eq!(PROJECTION_CLEANS.get(), trace::MAX_ENTRIES);

            let log = app.runtime.live_chief_trace("chief", "chief").unwrap();
            log.ingest(&event(0, "PostToolUse"), None, &Default::default(), 201);
            app.runtime.publish_projections(&out, &mut cache, &bodies);
            let completed = out.trace.load();
            assert_eq!(completed.data.entries[0].seq, first.data.entries[0].seq);
            assert_eq!(completed.data.entries[0].state, trace::TraceState::Ok);
            assert_eq!(PROJECTION_CLEANS.get(), trace::MAX_ENTRIES + 1);

            app.runtime.remember_chief_masks(&["private migration label".into()]);
            assert!(out.trace.load().data.entries.is_empty());
            assert!(app.runtime.trace_data().entries.is_empty());
            app.runtime.publish_projections(&out, &mut cache, &bodies);
            let masked = out.trace.load();
            assert!(masked.data.entries.iter().all(|entry| !entry.summary.contains("private migration label")));
            assert_eq!(PROJECTION_CLEANS.get(), trace::MAX_ENTRIES * 2 + 1);
        });
    }

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
            let chat = "chat-0123456789abcdef0123456789abcdef";
            chief.simple_chat = Some(("chief".into(), chat.into()));
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
            let saved = trace::disk::load_for_session(&path, &Default::default(), Some("chief")).unwrap();
            assert_eq!(saved.entries.len(), 65);
            assert!(saved.entries.iter().all(|entry| entry.chat_id.as_deref() == Some(chat)));
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
    fn new_plan_from_plain_chat_keeps_its_first_message_and_ignores_an_unrelated_open_plan() {
        with_app(
            "app::trace::tests::new_plan_from_plain_chat_keeps_its_first_message_and_ignores_an_unrelated_open_plan",
            |app, _| {
                let root = app.runtime.launch_dir.clone();
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
                chief.chief = true;
                chief.claude_id = None;
                app.runtime.sessions = vec![chief];
                let hook = |value: serde_json::Value| HookMessage {
                    pane: Some("chief".into()),
                    ancestors: Vec::new(),
                    delivered: Vec::new(),
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent::from_payload(&value).unwrap(),
                };
                app.runtime.plans = vec![Plan { id: "other".into(), ..Plan::default() }];
                let chat = "chat-0123456789abcdef0123456789abcdef";
                app.runtime.on_hook(hook(json!({"hook_event_name":"SessionStart", "session_id":"talk"})));
                app.runtime.on_hook(hook(json!({"hook_event_name":"UserPromptSubmit","session_id":"talk","prompt":format!("[Chat {chat}]\nCan we release today?")})));
                app.runtime.on_hook(hook(
                    json!({"hook_event_name":"Stop","session_id":"talk","last_assistant_message":"We can plan it."}),
                ));
                crate::app::tests::settle_trace(app.runtime);
                assert!(
                    app.runtime
                        .trace_data()
                        .entries
                        .iter()
                        .filter(|e| e.turn.is_some())
                        .all(|e| e.chat_id.as_deref() == Some(chat) && e.plan_id.is_none())
                );
                let stored = app.runtime.store.chief_history.iter().find(|c| c.conversation == chat).unwrap();
                assert_eq!(stored.title, "Can we release today?");
                assert_eq!(stored.turns.len(), 2);
                app.runtime.plans.push(Plan { id: "release".into(), ..Plan::default() });
                app.runtime.on_hook(hook(json!({"hook_event_name":"PostToolUse","session_id":"talk","tool_name":"mcp__atomics__atomics_plan_set","tool_use_id":"new","tool_input":{"goal":"Release","tasks":[]},"tool_response":{"content":[{"type":"text","text":"{\"plan\":\"release\"}"}]}})));
                let saved = app.runtime.store.chief_history.iter().find(|c| c.conversation == chat).unwrap();
                assert_eq!(saved.plan.as_deref(), Some("release"));
                assert_eq!(app.runtime.plans.last().unwrap().first_message.as_deref(), Some("Can we release today?"));
            },
        );
    }

    #[test]
    fn claude_chief_shows_each_public_message_between_hooks() {
        with_app("app::trace::tests::claude_chief_shows_each_public_message_between_hooks", |app, _| {
            use std::io::Write;
            let state = &mut *app.runtime;
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &state.launch_dir);
            chief.chief = true;
            chief.claude_id = Some("guide".into());
            state.sessions = vec![chief];

            let dir = crate::setup::agent_claude_dir().join("projects/test");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("guide.jsonl");
            let mut transcript = std::fs::File::create(&path).unwrap();
            let hook = |name, field: &str, text| HookMessage {
                pane: Some("chief".into()),
                ancestors: Vec::new(),
                delivered: Vec::new(),
                approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name,"session_id":"guide","transcript_path":path,field:text
                }))
                .unwrap(),
            };
            state.on_hook(hook("SessionStart", "source", "resume"));
            state.on_hook(hook("UserPromptSubmit", "prompt", "Check the board."));
            for (text, sidechain) in [("Reading the board.", false), ("Child message", true), ("Finished.", false)] {
                writeln!(
                    transcript,
                    "{}",
                    json!({
                        "type":"assistant", "sessionId":"guide","isSidechain":sidechain,
                        "message":{"content":[{"type":"thinking","thinking":"hidden"},{"type":"text","text":text}]}
                    })
                )
                .unwrap();
            }
            state.runtime_tick();
            state.on_hook(hook("PostToolUse", "atomics_chief_update", "Finished."));
            state.on_hook(hook("Stop", "last_assistant_message", "Finished."));
            crate::app::tests::settle_trace(state);
            let turns: Vec<_> = state.trace_data().entries.into_iter().filter_map(|e| e.turn.map(|t| t.text)).collect();
            assert_eq!(turns, ["Check the board.", "Reading the board.", "Finished."]);
        });
    }

    #[test]
    fn codex_chief_streams_rollout_messages_in_order_and_only_once() {
        with_app("app::trace::tests::codex_chief_streams_rollout_messages_in_order_and_only_once", |app, _| {
            use std::io::Write;

            let state = &mut *app.runtime;
            let mut chief = session("chief", AgentKind::Generic, AgentStatus::Idle, &state.launch_dir);
            chief.chief = true;
            chief.claude_id = None;
            state.sessions = vec![chief];
            state.remember_chief_masks(&["orchid-fixture-secret".into()]);

            let dir = crate::setup::chief_codex_home().join("sessions/2026/11/19");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("rollout-2026-11-19T12-00-00-guide.jsonl");
            let mut rollout = std::fs::File::create(&path).unwrap();
            let agent = |id, message, phase| {
                json!({
                    "type":"event_msg", "payload":{"type":"item_completed", "item":{
                        "type":"AgentMessage", "id":id, "content":[{"type":"Text", "text":message}], "phase":phase
                    }}
                })
            };
            let user = |message| {
                json!({"type":"event_msg", "payload":{"type":"item_completed", "item":{
                    "type":"UserMessage", "content":[{"type":"text", "text":message}]
                }}})
            };
            let hook = |name: &str, field: &str, value: &str| HookMessage {
                pane: Some("chief".into()),
                ancestors: Vec::new(),
                delivered: Vec::new(),
                approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":"guide", "transcript_path":path, field:value
                }))
                .unwrap(),
            };
            let turns = |state: &mut State| {
                crate::app::tests::settle_trace(state);
                state.trace_data().entries.into_iter().filter_map(|e| e.turn.map(|t| t.text)).collect::<Vec<_>>()
            };

            writeln!(rollout, "{}", agent("old", "Already in the resumed history.", "final_answer")).unwrap();
            state.on_hook(hook("SessionStart", "source", "resume"));
            state.on_hook(hook("UserPromptSubmit", "prompt", "Check the pages."));
            writeln!(rollout, "{}", user("Check the pages.")).unwrap();
            writeln!(rollout, "{}", agent("first", "Checking orchid-fixture-secret.", "commentary")).unwrap();
            writeln!(rollout, "{}", agent("second", "Checking orchid-fixture-secret.", "commentary")).unwrap();
            writeln!(
                rollout,
                "{}",
                json!({"type":"event_msg", "payload":{"type":"agent_reasoning", "text":"Keep private."}})
            )
            .unwrap();
            state.runtime_tick();
            let masked = state.chief_cleaner(&state.cleaner()).redacted("Checking orchid-fixture-secret.");
            assert_eq!(turns(state), ["Check the pages.", &masked, &masked]);
            assert!(!masked.contains("orchid-fixture-secret"));

            // Catch up before applying the steering hook even if the next poll has not run yet.
            writeln!(rollout, "{}", agent("third", "The first page passed.", "commentary")).unwrap();
            state.on_hook(hook("UserPromptSubmit", "prompt", "Check the index too."));
            writeln!(rollout, "{}", user("Check the index too.")).unwrap();
            let partial = agent("fourth", "The index is open.", "commentary").to_string();
            write!(rollout, "{}", &partial[..20]).unwrap();
            state.runtime_tick();
            assert_eq!(
                turns(state),
                ["Check the pages.", &masked, &masked, "The first page passed.", "Check the index too."]
            );
            writeln!(rollout, "{}", &partial[20..]).unwrap();
            state.runtime_tick();
            assert_eq!(turns(state).last().unwrap(), "The index is open.");

            writeln!(rollout, "{}", agent("last", "All pages passed.", "final_answer")).unwrap();
            state.on_hook(hook("Stop", "last_assistant_message", "All pages passed."));
            state.runtime_tick();
            assert_eq!(
                turns(state),
                [
                    "Check the pages.",
                    &masked,
                    &masked,
                    "The first page passed.",
                    "Check the index too.",
                    "The index is open.",
                    "All pages passed."
                ]
            );
        });
    }

    #[test]
    fn the_chiefs_prompts_and_answers_join_its_trace_as_its_conversation() {
        with_app("app::trace::tests::the_chiefs_prompts_and_answers_join_its_trace_as_its_conversation", |app, _| {
            use atomics_core::trace::TurnRole;
            let root = app.runtime.launch_dir.clone();
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
            chief.chief = true;
            chief.claude_id = None;
            let agent = session("agent", AgentKind::Claude, AgentStatus::Idle, &root);
            app.runtime.sessions = vec![chief, agent];
            let hook = |pane: &str, value: serde_json::Value| HookMessage {
                pane: Some(pane.into()),
                ancestors: Vec::new(),
                delivered: Vec::new(),
                approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&value).unwrap(),
            };
            app.runtime.on_hook(hook("chief", json!({"hook_event_name":"SessionStart", "session_id":"talk"})));
            app.runtime.on_hook(hook(
                "chief",
                json!({"hook_event_name":"UserPromptSubmit", "session_id":"talk",
                "prompt":"Plan the week"}),
            ));
            // A subagent's prompt, and an agent's, are not the chief's conversation.
            app.runtime.on_hook(hook(
                "chief",
                json!({"hook_event_name":"UserPromptSubmit", "session_id":"talk",
                "agent_id":"a1", "prompt":"look around"}),
            ));
            app.runtime.on_hook(hook(
                "agent",
                json!({"hook_event_name":"UserPromptSubmit", "session_id":"other",
                "prompt":"fix the parser"}),
            ));
            app.runtime.on_hook(hook(
                "chief",
                json!({"hook_event_name":"Stop", "session_id":"talk",
                "last_assistant_message":"On it."}),
            ));
            crate::app::tests::settle_trace(app.runtime);
            let data = app.runtime.trace_data();
            let turns: Vec<_> = data.entries.iter().filter_map(|e| e.turn.clone()).map(|t| (t.role, t.text)).collect();
            assert_eq!(turns, [(TurnRole::User, "Plan the week".to_string()), (TurnRole::Chief, "On it.".to_string())]);
            assert!(app.lane_entries().is_empty(), "the full window's lane shows the chief's calls only");
            app.runtime.plans = ["on-the-host", "chosen-on-phone"]
                .map(|id| atomics_core::plan::Plan { id: id.into(), ..Default::default() })
                .into();
            app.runtime.selected_plan = 0;
            app.runtime.tickets.remote.plans.entry("on-the-host".into()).or_default().conversation =
                Some("other-thread".into());
            for (event, field, text) in [
                ("UserPromptSubmit", "prompt", "[Plan chosen-on-phone]\nHow is the migration going?"),
                ("Stop", "last_assistant_message", "The checks passed."),
            ] {
                app.runtime.on_hook(hook("chief", json!({"hook_event_name":event, "session_id":"talk", field:text})));
            }
            app.runtime.plans[1].status = atomics_core::plan::PlanStatus::Completed;
            app.runtime.cleanup_closed_plan(1);
            app.runtime.on_hook(hook("chief", json!({"hook_event_name":"Stop", "session_id":"talk", "last_assistant_message":"Delivered the integrated changes."})));
            app.runtime.take_op_flush()();
            let saved = atomics_core::trace::disk::recent_turns(
                &paths::atomics_home(),
                "chosen-on-phone",
                &app.runtime.cleaner(),
            );
            assert!(
                saved
                    .iter()
                    .filter_map(|e| e.turn.as_ref())
                    .any(|turn| turn.text == "Delivered the integrated changes.")
            );
            assert!(saved.iter().all(|e| e.plan_id.as_deref() == Some("chosen-on-phone")));
            app.runtime.on_hook(hook("chief", json!({"hook_event_name":"UserPromptSubmit", "session_id":"talk", "prompt":"[Plan chosen-on-phone]\nWhat passed?"})));
            app.runtime.on_hook(hook("chief", json!({"hook_event_name":"Stop", "session_id":"talk", "last_assistant_message":"All five pages passed."})));
            app.runtime.take_op_flush()();
            let file = atomics_core::trace::disk::path(&paths::atomics_home(), Some("chosen-on-phone")).unwrap();
            let completed = atomics_core::trace::disk::load(&file, &app.runtime.cleaner()).unwrap();
            assert_eq!(completed.entries.len(), 4);
            assert_eq!(completed.entries.back().unwrap().turn.as_ref().unwrap().text, "All five pages passed.");
            let trace = app.runtime.trace_data();
            assert!(trace.entries.iter().rev().take(2).all(|e| e.plan_id.as_deref() == Some("chosen-on-phone")));
            assert!(
                app.runtime.tickets.remote.chief_plan.is_none(),
                "describing a plan in chat cannot open a ticket mirror"
            );
        });
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
            app.runtime.close_plan_trace("trace-plan", PlanStatus::Archived);
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
