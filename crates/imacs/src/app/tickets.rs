//! Tickets in the app: which links to read and when, what to push as tasks move, and
//! the chief's ticket tools. IMACS's own task status stays authoritative: it pushes a
//! state when its status changes, and reports changes made in the ticket system to
//! the chief as `ticket` events instead of acting on them.
use super::*;
use crate::tickets::{TicketJob, TicketOp, TicketOutput};
use imacs_core::tickets::{self as ticket, Author, TicketConfig, TicketDraft, TicketLink, TicketMode};

/// A plan's link (task `None`) or a task's.
pub(super) type LinkKey = (String, Option<String>);

#[derive(Clone)]
pub(super) struct Tag {
    key: LinkKey,
    /// The link's id when the job was queued: results for a link replaced since are dropped.
    ticket: String,
    /// The configured system the job was sent to: results never land on another system's link.
    system: String,
    kind: Kind,
    batch: Option<u64>,
}

#[derive(Clone, PartialEq)]
enum Kind {
    Read,
    Refresh,
    List,
    State(String),
    Comment,
    /// Creating a ticket for a slot, at the slot's generation when requested.
    Create(u64),
}

type Done = (Tag, Result<TicketOutput, String>);

pub(super) struct Tickets {
    /// The effective `[tickets]`: the workspace file's, else the main config's.
    pub(super) config: Option<Arc<TicketConfig>>,
    /// The workspace file and its content's fingerprint when last read.
    workspace_file: Option<(Option<PathBuf>, Option<u64>)>,
    workspace_config: Option<TicketConfig>,
    workspace_workflow: Option<imacs_core::workflow::Workflow>,
    /// The workspace file's `[isolation]`, merged over the main and host config's as it is read.
    workspace_isolation: Option<imacs_core::isolation::IsolationOverrides>,
    jobs: Option<std::sync::mpsc::Sender<TicketJob<Tag>>>,
    done_tx: crate::host::Sender<Done>,
    done_rx: Receiver<Done>,
    /// Jobs sent and not yet answered.
    pub(super) pending: usize,
    refreshing: HashSet<LinkKey>,
    /// The state each link is being moved to right now.
    pushing: HashMap<LinkKey, String>,
    /// State pushes that failed, retried on the next heartbeat or sync.
    failed: HashMap<LinkKey, String>,
    /// The tickets each board-mode plan's board lists.
    pub(super) boards: HashMap<String, Vec<TicketLink>>,
    batches: HashMap<u64, Batch>,
    next_batch: u64,
    /// How often each slot's link was set by request, so a late create result never
    /// replaces a link set after it was asked for.
    generations: HashMap<LinkKey, u64>,
}

/// A control request answered once all of its ticket jobs are done.
struct Batch {
    pending: usize,
    errors: Vec<String>,
    reply: Sender<ControlOutcome>,
    answer: Answer,
}

enum Answer {
    Read(Option<serde_json::Value>),
    Sync(Vec<String>),
    Link(LinkKey),
    Comment,
}

impl Tickets {
    pub(super) fn new(wake: &crate::host::Wake) -> Tickets {
        let (done_tx, done_rx) = wake.channel();
        Tickets {
            config: None,
            workspace_file: None,
            workspace_config: None,
            workspace_workflow: None,
            workspace_isolation: None,
            jobs: None,
            done_tx,
            done_rx,
            pending: 0,
            refreshing: HashSet::new(),
            pushing: HashMap::new(),
            failed: HashMap::new(),
            boards: HashMap::new(),
            batches: HashMap::new(),
            next_batch: 0,
            generations: HashMap::new(),
        }
    }
}

impl State {
    /// Re-reads the workspace file's `[tickets]` when it changed, and reads every
    /// link again when the effective config changes.
    pub(super) fn apply_ticket_config(&mut self) {
        let file = self.workspace.as_deref().and_then(|name| Workspace::file(name).ok());
        let stamp = file.as_deref().and_then(file_fingerprint);
        if self.tickets.workspace_file.as_ref() != Some(&(file.clone(), stamp)) {
            self.tickets.workspace_file = Some((file, stamp));
            let loaded = match self.workspace.clone().filter(|_| stamp.is_some()) {
                Some(name) => Workspace::load(&name).map(Some).map_err(|err| format!("{err:#}")),
                None => Ok(None),
            };
            match loaded {
                Ok(Some(workspace)) => {
                    match workspace.tickets() {
                        Ok(config) => self.tickets.workspace_config = config,
                        Err(err) => self.toast(format!("Workspace tickets: {}", err.message())),
                    }
                    let workflow = workspace.workflow().map_err(|err| err.message().to_string()).and_then(|config| {
                        if let Some(config) = &config {
                            config.validate()?;
                        }
                        Ok(config)
                    });
                    match workflow {
                        Ok(config) => self.tickets.workspace_workflow = config,
                        Err(err) => self.toast(format!("Workspace workflow: {err}")),
                    }
                    let isolation = workspace.isolation().and_then(|overrides| {
                        overrides.map(|o| self.config.isolation.with_overrides(&o).map(|_| o)).transpose()
                    });
                    match isolation {
                        Ok(table) => self.tickets.workspace_isolation = table,
                        Err(err) => self.toast(format!("Workspace isolation: {}", err.message())),
                    }
                }
                Ok(None) => {
                    self.tickets.workspace_config = None;
                    self.tickets.workspace_workflow = None;
                    self.tickets.workspace_isolation = None;
                }
                // Broken settings keep their last good values.
                Err(err) => self.toast(format!("Workspace tickets/workflow: {err}")),
            }
        }

        let effective = self.effective_tickets();
        let effective = effective.is_configured().then(|| Arc::new(effective));
        if effective != self.tickets.config {
            self.tickets.config = effective;
            self.tickets.failed.clear();
            self.refresh_tickets(None, None);
        }
    }

    /// Tickets follow the fleet heartbeat: read again, and failed pushes get another try.
    pub(super) fn ticket_heartbeat(&mut self) {
        self.tickets.failed.clear();
        self.refresh_tickets(None, None);
    }

    /// The board's sync button: read one plan's tickets again and retry its failed pushes.
    pub(super) fn resync_tickets(&mut self, plan: &str) {
        self.tickets.failed.retain(|key, _| key.0 != plan);
        self.refresh_tickets(Some(plan), None);
        self.push_ticket_states(None);
    }

    pub(super) fn workflow(&self) -> &imacs_core::workflow::Workflow {
        self.tickets.workspace_workflow.as_ref().unwrap_or(&self.config.workflow)
    }

    /// The `[isolation]` in effect: the main and host config's, with the workspace file's merged in.
    pub(super) fn effective_isolation(&self) -> imacs_core::isolation::IsolationConfig {
        self.tickets
            .workspace_isolation
            .as_ref()
            .and_then(|overrides| self.config.isolation.with_overrides(overrides).ok())
            .unwrap_or_else(|| self.config.isolation.clone())
    }

    /// The `[tickets]` in effect, configured or not: the workspace file's, else the main config's.
    pub(super) fn effective_tickets(&self) -> TicketConfig {
        self.tickets.workspace_config.clone().unwrap_or_else(|| self.config.tickets.clone())
    }

    pub(super) fn ticket_config(&self) -> Option<Arc<TicketConfig>> {
        self.tickets.config.clone()
    }

    pub(super) fn chief_author(&self) -> Author {
        Author::Chief(ticket::model_label("claude", &self.config.chief.args))
    }

    /// The chief's prompt with the configured ticket workflow added, then the user's own
    /// `[chief] prompt` from the main file, this host's and the workspace's, read as the chief starts.
    pub(super) fn chief_prompt(&self) -> String {
        let projects: Vec<String> = self.repos.iter().map(|r| paths::display(r)).collect();
        let mut prompt = self
            .config
            .chief
            .system_prompt
            .replace("{projects}", &projects.join(", "))
            .replace("{base_ref}", &self.config.base_ref);
        prompt.push_str(&format!("\n\nWorkflow: done = {:?}; ordered stages: {}. Acceptance still unblocks dependencies. Report lifecycle stages with imacs_plan_stage (plan, optional task, stage, optional url/note); external bridges use imacs_stage_event. Completion and the ticket done mapping wait for the configured milestone. ci_failed never proves success.\n", self.workflow().done, self.workflow().stages.join(", ")));
        if let Some(config) = self.ticket_config() {
            prompt.push_str("\n\n");
            prompt.push_str(&config.prompt_section());
        }
        let sections = imacs_core::selfconfig::prompt_sections(&self.config_layers(None));
        if !sections.is_empty() {
            prompt.push_str("\n\nThe user's own instructions, from [chief] prompt in the IMACS config:");
            for (_, text) in sections {
                prompt.push_str("\n\n");
                prompt.push_str(&text);
            }
        }
        for root in &self.repos {
            let workflow = imacs_core::repo_workflow::RepoWorkflow::read(root);
            prompt.push_str(&workflow.chief_section(&paths::display(root)));
        }
        prompt
    }

    fn link(&self, key: &LinkKey) -> Option<&TicketLink> {
        let plan = self.plans.iter().find(|p| p.id == key.0)?;
        match &key.1 {
            None => plan.ticket.as_ref(),
            Some(task) => plan.tasks.iter().find(|t| &t.id == task)?.ticket.as_ref(),
        }
    }

    fn link_slot(&mut self, key: &LinkKey) -> Option<&mut Option<TicketLink>> {
        let plan = self.plans.iter_mut().find(|p| p.id == key.0)?;
        match &key.1 {
            None => Some(&mut plan.ticket),
            Some(task) => Some(&mut plan.tasks.iter_mut().find(|t| &t.id == task)?.ticket),
        }
    }

    /// A request set or cleared a slot's link: results of earlier requests for it are stale.
    pub(super) fn relinked(&mut self, key: LinkKey) -> u64 {
        let generation = self.tickets.generations.entry(key).or_default();
        *generation += 1;
        *generation
    }

    fn save_plan_id(&self, id: &str) {
        if let Some(index) = self.plans.iter().position(|p| p.id == id) {
            self.save_plan(index);
        }
    }

    fn send_ticket(&mut self, config: &Arc<TicketConfig>, op: TicketOp, tag: Tag) {
        if let Some(batch) = tag.batch.and_then(|id| self.tickets.batches.get_mut(&id)) {
            batch.pending += 1;
        }
        let addressed = match &op {
            TicketOp::Get(link)
            | TicketOp::Read(link)
            | TicketOp::List(link)
            | TicketOp::UpdateState(link, _)
            | TicketOp::Comment(link, _) => Some(link),
            TicketOp::Create { .. } => None,
        };
        // Callers already leave other systems' links alone; nothing may reach them anyway.
        if let Some(mismatch) = addressed.and_then(|link| config.mismatch(link)) {
            self.finish_ticket(tag, Err(mismatch));
            return;
        }
        let done = self.tickets.done_tx.clone();
        let jobs = self.tickets.jobs.get_or_insert_with(|| crate::tickets::spawn(done));
        self.tickets.pending += 1;
        if let Err(unsent) = jobs.send(TicketJob { config: config.clone(), op, tag }) {
            // A worker that stopped is started again for the next job; this one fails.
            self.tickets.jobs = None;
            self.tickets.pending -= 1;
            self.finish_ticket(unsent.0.tag, Err("the ticket worker stopped".into()));
        }
    }

    /// Stops the worker, which closes the ticket system's stdio server.
    pub(super) fn stop_tickets(&mut self) {
        self.tickets.jobs = None;
    }

    /// Reads the linked tickets of one plan, or of every open plan, again. In board
    /// mode a plan's board is listed once instead of reading its tickets one by one.
    pub(super) fn refresh_tickets(&mut self, plan: Option<&str>, batch: Option<u64>) {
        let Some(config) = self.ticket_config() else { return };
        let plans: Vec<usize> = (0..self.plans.len())
            .filter(|&i| !self.plans[i].id.is_empty())
            .filter(|&i| plan.map_or(self.plans[i].is_open(), |id| self.plans[i].id == id))
            .collect();
        for index in plans {
            let id = self.plans[index].id.clone();
            let board = self.plans[index].ticket.clone().filter(|link| self.accepted(&config, link, batch));
            let listed = config.mode == TicketMode::Board && config.operation("list").is_some() && board.is_some();
            match board {
                Some(board) if listed => self.queue_read(&config, (id.clone(), None), TicketOp::List(board), batch),
                Some(link) if config.mode == TicketMode::Ticket => {
                    self.queue_read(&config, (id.clone(), None), TicketOp::Get(link), batch)
                }
                _ => {}
            }
            if !listed {
                let links: Vec<(String, TicketLink)> =
                    self.plans[index].tasks.iter().filter_map(|t| Some((t.id.clone(), t.ticket.clone()?))).collect();
                for (task, link) in links {
                    if self.accepted(&config, &link, batch) {
                        self.queue_read(&config, (id.clone(), Some(task)), TicketOp::Get(link), batch);
                    }
                }
            }
        }
    }

    fn queue_read(&mut self, config: &Arc<TicketConfig>, key: LinkKey, op: TicketOp, batch: Option<u64>) {
        let (kind, ticket, operation) = match &op {
            TicketOp::List(link) => (Kind::List, link.id.clone(), "list"),
            TicketOp::Get(link) => (Kind::Refresh, link.id.clone(), "get"),
            _ => return,
        };
        // Another read of the same link is already on its way, unless a request waits for this one.
        if config.operation(operation).is_none() || (!self.tickets.refreshing.insert(key.clone()) && batch.is_none()) {
            return;
        }
        self.send_ticket(config, op, Tag { key, ticket, kind, batch, system: config.system.clone() });
    }

    /// Takes finished ticket jobs; returns whether any arrived.
    pub(super) fn ticket_results(&mut self) -> bool {
        let mut changed = false;
        while let Ok((tag, result)) = self.tickets.done_rx.try_recv() {
            changed = true;
            self.tickets.pending = self.tickets.pending.saturating_sub(1);
            self.finish_ticket(tag, result);
        }
        changed
    }

    fn finish_ticket(&mut self, tag: Tag, result: Result<TicketOutput, String>) {
        match &tag.kind {
            Kind::Refresh | Kind::List => {
                self.tickets.refreshing.remove(&tag.key);
            }
            Kind::State(_) => {
                self.tickets.pushing.remove(&tag.key);
            }
            Kind::Read | Kind::Comment | Kind::Create(_) => {}
        }
        let error = match (tag.kind.clone(), result) {
            (Kind::Read, Ok(TicketOutput::Read(value))) => {
                if let Some(batch) = tag.batch.and_then(|id| self.tickets.batches.get_mut(&id)) {
                    batch.answer = Answer::Read(Some(value));
                }
                None
            }
            (Kind::Refresh, Ok(TicketOutput::Ticket(fetched))) => {
                self.take_ticket(&tag, &fetched, true);
                None
            }
            (Kind::List, Ok(TicketOutput::Tickets(items))) => {
                self.take_board(&tag, items);
                None
            }
            (kind @ (Kind::State(_) | Kind::Comment), Ok(TicketOutput::Pushed(fresh))) => {
                if let Kind::State(state) = &kind {
                    self.tickets.failed.remove(&tag.key);
                    if let Some(link) = self.live_link(&tag, true) {
                        link.pushed_state = Some(state.clone());
                    }
                }
                match fresh {
                    // Only the read after IMACS's own state update acknowledges that update; a
                    // read after a comment may show someone else's change like any other read.
                    Some(Ok(fetched)) => self.take_ticket(&tag, &fetched, matches!(kind, Kind::Comment)),
                    Some(Err(err)) => self.ticket_failed(&tag, format!("reading it after the update failed: {err}")),
                    None if !self.tickets.failed.contains_key(&tag.key) => {
                        if let Some(link) = self.live_link(&tag, true) {
                            link.error = None;
                        }
                        self.save_plan_id(&tag.key.0);
                    }
                    None => self.save_plan_id(&tag.key.0),
                }
                None
            }
            (Kind::Create(generation), Ok(TicketOutput::Ticket(created))) => {
                if self.tickets.generations.get(&tag.key) != Some(&generation) {
                    Some(format!("{} was created, but a newer link superseded it", created.label()))
                } else {
                    // The ticket exists in the system the request went to, whatever the config says now.
                    let created =
                        TicketLink { system: tag.system.clone(), synced_at: Some(imacs_core::store::now()), ..created };
                    if let Some(slot) = self.link_slot(&tag.key) {
                        *slot = Some(created);
                        self.save_plan_id(&tag.key.0);
                    }
                    None
                }
            }
            (kind, Err(err)) => {
                let what = match &kind {
                    Kind::Read | Kind::Refresh | Kind::List => "reading the ticket failed".to_string(),
                    Kind::State(state) => {
                        self.tickets.failed.insert(tag.key.clone(), state.clone());
                        format!("setting the state to {state} failed")
                    }
                    Kind::Comment => "commenting failed".into(),
                    Kind::Create(_) => "creating the ticket failed".into(),
                };
                let message = format!("{what}: {err}");
                if !matches!(kind, Kind::Create(_)) {
                    self.ticket_failed(&tag, message.clone());
                }
                Some(message)
            }
            (_, Ok(_)) => Some("unexpected ticket result".into()),
        };
        self.settle_job(tag.batch, error);
    }

    /// The cleaner for the configured credentials. The worker already cleans what a job
    /// brings back with that job's own secrets; this one covers what arrives here, such as
    /// webhook fields and links the chief gives, and errors made here.
    pub(super) fn cleaner(&self) -> ticket::Cleaner {
        self.ticket_config().map(|config| ticket::Cleaner::of(&config, crate::setup::known_env_var)).unwrap_or_default()
    }

    /// Where ticket errors are stored on a link or returned to a caller.
    fn scrub(&self, error: &str) -> String {
        self.cleaner().error(error)
    }

    /// The link a job's result belongs to: still the same ticket, and of the system the
    /// job was sent to. With `pin`, a link made for no system takes that system now.
    fn live_link(&mut self, tag: &Tag, pin: bool) -> Option<&mut TicketLink> {
        let link = self.link_slot(&tag.key)?.as_mut()?;
        if link.id != tag.ticket || !(link.system.is_empty() || link.system == tag.system) {
            return None;
        }
        if pin && link.system.is_empty() {
            link.system = tag.system.clone();
        }
        Some(link)
    }

    /// Whether operations may address a link; a skipped link is named in the batch's errors.
    fn accepted(&mut self, config: &TicketConfig, link: &TicketLink, batch: Option<u64>) -> bool {
        match config.mismatch(link) {
            None => true,
            Some(mismatch) => {
                if let Some(entry) = batch.and_then(|id| self.tickets.batches.get_mut(&id)) {
                    entry.errors.push(format!("{mismatch}; left alone"));
                }
                false
            }
        }
    }

    fn ticket_failed(&mut self, tag: &Tag, error: String) {
        let error = self.scrub(&error);
        if let Some(link) = self.live_link(tag, false)
            && link.error.as_deref() != Some(&error)
        {
            link.error = Some(error);
            self.save_plan_id(&tag.key.0);
        }
    }

    /// Takes a ticket as read. The read after IMACS's own state update never counts as
    /// someone else changing it.
    fn take_ticket(&mut self, tag: &Tag, fetched: &TicketLink, external: bool) {
        let keep_error = self.tickets.failed.contains_key(&tag.key);
        // A webhook may echo IMACS's own push before the push itself has returned.
        let pushing = self.tickets.pushing.get(&tag.key).cloned();
        let Some(link) = self.live_link(tag, true) else {
            return;
        };
        let before = link.state.clone();
        link.update_from(fetched);
        link.synced_at = Some(imacs_core::store::now());
        if !keep_error {
            link.error = None;
        }
        let moved = external
            && before.is_some()
            && link.state.is_some()
            && !same_state(link.state.as_deref(), before.as_deref())
            && !same_state(link.state.as_deref(), link.pushed_state.as_deref())
            && !same_state(link.state.as_deref(), pushing.as_deref());
        let link = link.clone();
        self.save_plan_id(&tag.key.0);
        if moved {
            self.ticket_moved(&tag.key, &link, before);
        }
    }

    fn take_board(&mut self, tag: &Tag, items: Vec<TicketLink>) {
        let Some(index) = self.plans.iter().position(|p| p.id == tag.key.0) else { return };
        if let Some(board) = self.live_link(tag, true) {
            board.synced_at = Some(imacs_core::store::now());
            board.error = None;
        }
        let links: Vec<(String, TicketLink)> =
            self.plans[index].tasks.iter().filter_map(|t| Some((t.id.clone(), t.ticket.clone()?))).collect();
        for (task, link) in links {
            let key = (tag.key.0.clone(), Some(task));
            if !(link.system.is_empty() || link.system == tag.system) {
                continue;
            }
            let tag = Tag {
                key: key.clone(),
                ticket: link.id.clone(),
                kind: Kind::Refresh,
                batch: tag.batch,
                system: tag.system.clone(),
            };
            match items.iter().find(|item| link.same_ticket(item)) {
                Some(item) => self.take_ticket(&tag, item, true),
                // Not on the board (any more): read it on its own.
                None => {
                    if let Some(config) = self.ticket_config() {
                        self.queue_read(&config, key, TicketOp::Get(link), tag.batch);
                    }
                }
            }
        }
        self.tickets.boards.insert(tag.key.0.clone(), items);
        self.save_plan(index);
    }

    /// Someone changed a linked ticket: tell the chief, which decides what the plan does.
    fn ticket_moved(&mut self, key: &LinkKey, link: &TicketLink, from: Option<String>) {
        let config = self.ticket_config();
        let means = config.as_ref().and_then(|c| c.states.status_of(link.state.as_deref().unwrap_or_default()));
        let expected =
            self.plans.iter().find(|p| p.id == key.0).is_some_and(|p| p.ticket_progress_expected(key.1.as_deref()));
        let session = key.1.as_ref().and_then(|task| {
            self.plans.iter().find(|p| p.id == key.0)?.tasks.iter().find(|t| &t.id == task)?.session.clone()
        });
        self.event_seq += 1;
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: session.unwrap_or_default(),
            title: format!("Ticket {}", link.label()),
            status: AgentStatus::Idle,
            at: imacs_core::store::now(),
            plan: Some(key.0.clone()),
            task: key.1.clone(),
            report: None,
            approval: None,
            findings: None,
            config: None,
            ticket: Some(json!({
                "system": link.system, "id": link.id, "key": link.label(), "title": link.title, "url": link.url,
                "from": from, "to": link.state, "means": means.map(TaskStatus::label), "expected": expected,
            })),
            change: None,
        });
        self.serve_waiters();
        let system = config.map(|c| c.system.clone()).unwrap_or_default();
        self.toast(format!(
            "{} moved to {} in {system}",
            link.label(),
            link.state.as_deref().unwrap_or("an unknown state")
        ));
    }

    /// Pushes ticket states that IMACS's statuses moved away from. A ticket linked to
    /// a task that has not started is left alone, and a failed push waits for the next
    /// heartbeat or `imacs_ticket_sync` instead of repeating every tick. A sync passes
    /// its batch, so it answers only once the pushes and the reads after them are done.
    pub(super) fn push_ticket_states(&mut self, batch: Option<u64>) {
        let Some(config) = self.ticket_config() else { return };
        if config.operation("update_state").is_none() {
            return;
        }
        let mut pushes = Vec::new();
        for plan in self.plans.iter().filter(|p| !p.id.is_empty() && p.status != PlanStatus::Archived) {
            let workflow = self.plan_workflow(plan);
            let mut links = Vec::new();
            for task in &plan.tasks {
                let Some(link) = &task.ticket else { continue };
                let latest = plan.latest_stage(task);
                let reached = workflow.reached(&task.stages) || workflow.reached(&plan.stages);
                let desired = if task.status == TaskStatus::Done {
                    config.states.after_acceptance(&workflow, latest, reached)
                } else if let Some(stage) =
                    latest.filter(|s| s.stage == "integrated" && task.status == TaskStatus::Review)
                {
                    config.states.stage(&stage.stage)
                } else {
                    config.states.get(task.status)
                };
                links.push(((plan.id.clone(), Some(task.id.clone())), task.status, link, desired));
            }
            if config.mode == TicketMode::Ticket
                && let Some(link) = &plan.ticket
            {
                let status = ticket::plan_status(plan);
                let desired = if status == TaskStatus::Done {
                    config.states.after_acceptance(&workflow, plan.stages.last(), plan.workflow_done(&workflow))
                } else {
                    config.states.get(status)
                };
                links.push(((plan.id.clone(), None), status, link, desired));
            }
            for (key, status, link, desired) in links.into_iter().filter(|(_, _, link, _)| config.accepts(link)) {
                let Some(desired) = desired else { continue };
                if link.pushed_state.as_deref() == Some(desired)
                    || (link.pushed_state.is_none() && status == TaskStatus::Planned)
                    || self.tickets.pushing.contains_key(&key)
                    || self.tickets.failed.get(&key).is_some_and(|failed| failed == desired)
                {
                    continue;
                }
                pushes.push((key, link.clone(), desired.to_string()));
            }
        }
        for (key, link, state) in pushes {
            self.tickets.pushing.insert(key.clone(), state.clone());
            let tag = Tag {
                key,
                ticket: link.id.clone(),
                kind: Kind::State(state.clone()),
                batch,
                system: config.system.clone(),
            };
            self.send_ticket(&config, TicketOp::UpdateState(link, state), tag);
        }
    }

    /// Where a comment about a task goes: its own ticket, else the plan's ticket in ticket mode.
    fn comment_target(&self, index: usize, task: Option<&str>) -> Option<(LinkKey, TicketLink)> {
        let plan = &self.plans[index];
        if let Some(task) = task.and_then(|id| plan.tasks.iter().find(|t| t.id == id))
            && let Some(link) = &task.ticket
        {
            return Some(((plan.id.clone(), Some(task.id.clone())), link.clone()));
        }
        let config = self.ticket_config()?;
        let link = plan.ticket.clone().filter(|_| config.mode == TicketMode::Ticket)?;
        Some(((plan.id.clone(), None), link))
    }

    fn comment_job(
        &self,
        index: usize,
        task: Option<&str>,
        author: &Author,
    ) -> Result<(Arc<TicketConfig>, LinkKey, TicketLink), String> {
        let config =
            self.ticket_config().ok_or("No ticket system is configured ([tickets] in the workspace or config).")?;
        if !config.posts(author) {
            return Err(format!(
                "Ticket comments by {} are off in [tickets] comments.",
                match author {
                    Author::Agent(_) => "agents",
                    _ => "IMACS and the chief",
                }
            ));
        }
        if config.operation("comment").is_none() {
            return Err("operations.comment is not configured in [tickets].".into());
        }
        let (key, link) = self.comment_target(index, task).ok_or_else(|| match task {
            Some(task) => format!("Task {task} has no ticket, and the plan has no ticket to comment on instead."),
            None => "The plan has no ticket to comment on (in board mode, comment on a task's ticket).".into(),
        })?;
        if let Some(mismatch) = config.mismatch(&link) {
            return Err(format!("{mismatch}; IMACS leaves it alone."));
        }
        Ok((config, key, link))
    }

    /// Posts a lifecycle comment when the config wants one; silently skips otherwise.
    pub(super) fn lifecycle_comment(&mut self, index: usize, task: Option<&str>, author: &Author, body: String) {
        if let Ok((config, key, link)) = self.comment_job(index, task, author) {
            let tag =
                Tag { key, ticket: link.id.clone(), kind: Kind::Comment, batch: None, system: config.system.clone() };
            self.send_ticket(&config, TicketOp::Comment(link, body), tag);
        }
    }

    pub(super) fn ticket_dispatched(&mut self, index: usize, id: &str) {
        let Some(task) = self.plans[index].tasks.iter().find(|t| t.id == id).cloned() else { return };
        let author = self.chief_author();
        let body = ticket::dispatch_comment(&author, &task, &ticket::model_label(&task.agent, &task.args));
        self.lifecycle_comment(index, Some(id), &author, body);
    }

    pub(super) fn ticket_reported(&mut self, index: usize, id: &str, status: &str, summary: &str) {
        let Some(task) = self.plans[index].tasks.iter().find(|t| t.id == id).cloned() else { return };
        let author = Author::Agent(ticket::model_label(&task.agent, &task.args));
        let body = ticket::report_comment(&author, &task, status, summary);
        self.lifecycle_comment(index, Some(id), &author, body);
    }

    /// Accepting or blocking a task outside a report comments with the note that came with it.
    pub(super) fn ticket_status_changed(
        &mut self,
        index: usize,
        id: &str,
        old: TaskStatus,
        new: TaskStatus,
        note: Option<&str>,
        author: Author,
    ) {
        if old == new || !matches!(new, TaskStatus::Done | TaskStatus::Blocked) {
            return;
        }
        let Some(task) = self.plans[index].tasks.iter().find(|t| t.id == id).cloned() else { return };
        let body = ticket::status_comment(&author, &task, new, note);
        self.lifecycle_comment(index, Some(id), &author, body);
    }

    pub(super) fn ticket_plan_completed(&mut self, index: usize) {
        let body = ticket::completed_comment(&Author::Imacs, &self.plans[index]);
        self.lifecycle_comment(index, None, &Author::Imacs, body);
    }

    /// An external system or webhook bridge says a ticket or board changed.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ticket_event(
        &mut self,
        system: Option<&str>,
        id: Option<&str>,
        key: Option<&str>,
        board: Option<&str>,
        fields: TicketLink,
    ) -> serde_json::Value {
        // Webhook fields come from outside like server results and are cleaned the same way.
        let fields = self.cleaner().link(fields);
        let mut matched: Vec<LinkKey> = Vec::new();
        let mut boards: Vec<String> = Vec::new();
        let config = self.ticket_config();
        let board_mode = config.as_ref().is_some_and(|c| c.mode == TicketMode::Board);
        // An event naming no system comes from the configured one: other systems' links stay as they are.
        let ours = |link: &&TicketLink| system.is_some() || config.as_ref().is_none_or(|c| c.accepts(link));
        for plan in self.plans.iter().filter(|p| !p.id.is_empty() && p.status != PlanStatus::Archived) {
            if let Some(link) = plan.ticket.as_ref().filter(ours) {
                if board_mode && (link.matches(system, board, board) || link.matches(system, id, key)) {
                    boards.push(plan.id.clone());
                } else if link.matches(system, id, key) {
                    matched.push((plan.id.clone(), None));
                }
            }
            for task in &plan.tasks {
                if task.ticket.as_ref().filter(ours).is_some_and(|link| link.matches(system, id, key)) {
                    matched.push((plan.id.clone(), Some(task.id.clone())));
                }
            }
        }
        for link_key in &matched {
            let Some(link) = self.link(link_key).cloned() else { continue };
            let tag = Tag {
                key: link_key.clone(),
                ticket: link.id.clone(),
                kind: Kind::Refresh,
                batch: None,
                system: link.system.clone(),
            };
            self.take_ticket(&tag, &fields, true);
            if !boards.contains(&link_key.0)
                && let Some(config) = self.ticket_config()
                && let Some(link) = self.link(link_key).cloned().filter(|link| config.accepts(link))
            {
                self.queue_read(&config, link_key.clone(), TicketOp::Get(link), None);
            }
        }
        for plan in &boards {
            self.refresh_tickets(Some(plan), None);
        }
        json!({
            "matched": matched.iter().map(|(plan, task)| json!({ "plan": plan, "task": task })).collect::<Vec<_>>(),
            "boards": boards,
            "refreshing": self.ticket_config().is_some() && !(matched.is_empty() && boards.is_empty()),
        })
    }

    fn open_batch(&mut self, reply: Sender<ControlOutcome>, answer: Answer) -> u64 {
        self.tickets.next_batch += 1;
        let id = self.tickets.next_batch;
        self.tickets.batches.insert(id, Batch { pending: 0, errors: Vec::new(), reply, answer });
        id
    }

    /// One job of a batch finished (`batch` already counted it), or with `None` error
    /// and no job, checks whether a batch whose jobs were all queued is complete.
    fn settle_job(&mut self, batch: Option<u64>, error: Option<String>) {
        let Some(id) = batch else { return };
        let error = error.map(|error| self.scrub(&error));
        let Some(entry) = self.tickets.batches.get_mut(&id) else { return };
        entry.pending = entry.pending.saturating_sub(1);
        entry.errors.extend(error);
        if entry.pending == 0 {
            self.answer_batch(id);
        }
    }

    fn settle_batch(&mut self, id: u64) {
        if self.tickets.batches.get(&id).is_some_and(|b| b.pending == 0) {
            self.answer_batch(id);
        }
    }

    fn answer_batch(&mut self, id: u64) {
        let Some(batch) = self.tickets.batches.remove(&id) else { return };
        let response = match batch.answer {
            Answer::Read(Some(value)) if batch.errors.is_empty() => ControlResponse::ok(value),
            Answer::Read(_) => ControlResponse::error(batch.errors.join("; ")),
            Answer::Sync(plans) => ControlResponse::ok(json!({
                "plans": plans.iter().filter_map(|plan| self.ticket_summary(plan)).collect::<Vec<_>>(),
                "errors": batch.errors,
            })),
            Answer::Link(key) => match self.link(&key) {
                None if !batch.errors.is_empty() => ControlResponse::error(batch.errors.join("; ")),
                link => {
                    let mut result = json!({ "plan": key.0, "task": key.1, "ticket": link, "errors": batch.errors });
                    if key.1.is_none()
                        && let Some(board) = self.ticket_summary(&key.0).and_then(|s| s.get("board").cloned())
                    {
                        result["board"] = board;
                    }
                    ControlResponse::ok(result)
                }
            },
            Answer::Comment if batch.errors.is_empty() => ControlResponse::ok(json!({ "posted": true })),
            Answer::Comment => ControlResponse::error(batch.errors.join("; ")),
        };
        let _ = batch.reply.send(ControlOutcome::Done(response));
    }

    /// A plan's links, and in board mode which of the board's tickets have a task.
    pub(super) fn ticket_summary(&self, id: &str) -> Option<serde_json::Value> {
        let plan = self.plans.iter().find(|p| p.id == id)?;
        let mut summary = json!({
            "plan": plan.id,
            "goal": plan.goal,
            "ticket": plan.ticket,
            "tasks": plan.tasks.iter().map(|t| json!({
                "id": t.id, "title": t.title, "status": t.status, "ticket": t.ticket
            })).collect::<Vec<_>>(),
        });
        if let Some(items) = self.tickets.boards.get(id) {
            summary["board"] = json!(
                items
                    .iter()
                    .map(|item| json!({
                        "ticket": item,
                        "task": plan.tasks.iter().find(|t| t.ticket.as_ref().is_some_and(|l| l.same_ticket(item))).map(|t| &t.id),
                    }))
                    .collect::<Vec<_>>()
            );
        }
        Some(summary)
    }

    /// A link given by the chief: cleaned like a ticket read from the server, and given
    /// the configured system when it names none.
    pub(super) fn stamp_system(&self, link: Option<TicketLink>) -> Option<TicketLink> {
        link.map(|link| self.cleaner().link(link)).map(|mut link| {
            if link.system.is_empty()
                && let Some(config) = self.ticket_config()
            {
                link.system = config.system.clone();
            }
            link
        })
    }

    pub(super) fn link_ticket(
        &mut self,
        index: usize,
        task: Option<String>,
        given: Option<TicketLink>,
        create: Option<TicketDraft>,
        unlink: bool,
        reply: Sender<ControlOutcome>,
    ) {
        let fail = |reply: Sender<ControlOutcome>, error: String| {
            let _ = reply.send(ControlOutcome::Done(ControlResponse::error(error)));
        };
        let key: LinkKey = (self.plans[index].id.clone(), task.clone());
        if let Some(task) = &task
            && !self.plans[index].tasks.iter().any(|t| &t.id == task)
        {
            return fail(reply, format!("no task {task}"));
        }
        // Each branch advances the slot's generation only once its request is valid, so a
        // rejected request never cancels a pending create.
        if unlink {
            self.relinked(key.clone());
            if let Some(slot) = self.link_slot(&key) {
                *slot = None;
            }
            if key.1.is_none() {
                self.tickets.boards.remove(&key.0);
            }
            self.save_plan(index);
            let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(
                json!({ "plan": key.0, "task": key.1, "ticket": null }),
            )));
            return;
        }
        let config = self.ticket_config();
        match (given, create) {
            (Some(link), _) => {
                if link.id.trim().is_empty() {
                    return fail(reply, "ticket.id is required".into());
                }
                self.relinked(key.clone());
                let old = self.link(&key).cloned();
                let link = TicketLink::relink(self.stamp_system(Some(link)), old.as_ref());
                if let Some(slot) = self.link_slot(&key) {
                    *slot = link;
                }
                self.save_plan(index);
                let batch = self.open_batch(reply, Answer::Link(key.clone()));
                if let (Some(config), Some(link)) = (config, self.link(&key).cloned())
                    && self.accepted(&config, &link, Some(batch))
                {
                    let op = match &key.1 {
                        None if config.mode == TicketMode::Board => TicketOp::List(link),
                        _ => TicketOp::Get(link),
                    };
                    self.queue_read(&config, key, op, Some(batch));
                }
                self.settle_batch(batch);
            }
            (None, Some(draft)) => {
                let Some(config) = config.filter(|c| c.operation("create").is_some()) else {
                    return fail(reply, "operations.create is not configured in [tickets].".into());
                };
                if draft.title.trim().is_empty() {
                    return fail(reply, "create.title is required".into());
                }
                let generation = self.relinked(key.clone());
                let board = self.plans[index].ticket.clone().filter(|board| config.accepts(board));
                let batch = self.open_batch(reply, Answer::Link(key.clone()));
                let op = TicketOp::Create { board, title: draft.title, body: draft.body };
                self.send_ticket(
                    &config,
                    op,
                    Tag {
                        key,
                        ticket: String::new(),
                        kind: Kind::Create(generation),
                        batch: Some(batch),
                        system: config.system.clone(),
                    },
                );
            }
            (None, None) => fail(reply, "Give ticket (with its id), create (with a title), or unlink.".into()),
        }
    }

    /// `imacs_ticket_sync`: read everything again now, retry failed pushes, and answer
    /// with each plan's links and board.
    pub(super) fn sync_tickets(&mut self, plan: Option<String>, reply: Sender<ControlOutcome>) {
        let plans: Vec<String> = match &plan {
            Some(id) if self.plans.iter().any(|p| &p.id == id) => vec![id.clone()],
            Some(id) => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error(format!("no plan {id}"))));
                return;
            }
            None => self.plans.iter().filter(|p| p.is_open()).map(|p| p.id.clone()).collect(),
        };
        self.tickets.failed.retain(|key, _| !plans.contains(&key.0));
        let batch = self.open_batch(reply, Answer::Sync(plans.clone()));
        if self.ticket_config().is_none()
            && let Some(entry) = self.tickets.batches.get_mut(&batch)
        {
            entry.errors.push("No ticket system is configured ([tickets] in the workspace or config).".into());
        }
        for id in &plans {
            self.refresh_tickets(Some(id), Some(batch));
        }
        self.push_ticket_states(Some(batch));
        self.settle_batch(batch);
    }

    pub(super) fn comment_ticket(
        &mut self,
        index: usize,
        task: Option<String>,
        text: String,
        reply: Sender<ControlOutcome>,
    ) {
        let author = self.chief_author();
        match self.comment_job(index, task.as_deref(), &author) {
            Err(error) => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error(error)));
            }
            Ok(_) if text.trim().is_empty() => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error("text is empty")));
            }
            Ok((config, key, link)) => {
                let batch = self.open_batch(reply, Answer::Comment);
                let tag = Tag {
                    key,
                    ticket: link.id.clone(),
                    kind: Kind::Comment,
                    batch: Some(batch),
                    system: config.system.clone(),
                };
                self.send_ticket(&config, TicketOp::Comment(link, ticket::chief_comment(&author, &text)), tag);
            }
        }
    }

    /// Session identity comes from the agent's IMACS MCP process, never tool arguments.
    /// This path permits only get and comment; it never syncs a board or retries pushes.
    pub(super) fn agent_ticket(
        &mut self,
        pane: Option<&str>,
        plan_link: bool,
        comment: Option<String>,
        reply: Sender<ControlOutcome>,
    ) {
        let target = (|| {
            let config = self.ticket_config().ok_or("No ticket system is configured.")?;
            if config.comments != ticket::CommentPolicy::Agents {
                return Err("Agent ticket access requires [tickets] comments = agents.");
            }
            let pane = pane.filter(|id| !id.is_empty()).ok_or("No agent session identity.")?;
            if !self.sessions.iter().any(|s| s.id == pane && !s.chief) {
                return Err("No task agent matches this session.");
            }
            let mut tasks =
                self.plans.iter().filter(|p| p.is_open()).flat_map(|p| {
                    p.tasks.iter().filter(move |t| t.session.as_deref() == Some(pane)).map(move |t| (p, t))
                });
            let (plan, task) = tasks.next().ok_or("This session has no task in an open plan.")?;
            if tasks.next().is_some() {
                return Err("This session belongs to more than one task.");
            }
            let link = if plan_link { plan.ticket.as_ref() } else { task.ticket.as_ref() }
                .ok_or("The requested task or plan has no linked ticket.")?;
            if !link.system.is_empty() && link.system != config.system {
                return Err("The linked ticket belongs to another ticket system.");
            }
            let operation = if comment.is_some() { "comment" } else { "get" };
            if config.operation(operation).is_none() {
                return Err("The requested ticket operation is not configured.");
            }
            let key = (plan.id.clone(), (!plan_link).then(|| task.id.clone()));
            let author = Author::Agent(ticket::model_label(&task.agent, &task.args));
            Ok((
                config,
                key,
                link.clone(),
                ticket::agent_comment(&author, task, comment.as_deref().unwrap_or_default()),
            ))
        })();
        let (config, key, link, body) = match target {
            Ok(target) => target,
            Err(error) => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error(error)));
                return;
            }
        };
        let (op, kind, answer) = match comment {
            Some(text) if text.trim().is_empty() => {
                let _ = reply.send(ControlOutcome::Done(ControlResponse::error("text is empty")));
                return;
            }
            Some(_) => (TicketOp::Comment(link.clone(), body), Kind::Comment, Answer::Comment),
            None => (TicketOp::Read(link.clone()), Kind::Read, Answer::Read(None)),
        };
        let batch = self.open_batch(reply, answer);
        let system = config.system.clone();
        self.send_ticket(&config, op, Tag { key, ticket: link.id, kind, batch: Some(batch), system });
    }
}

fn same_state(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.trim().eq_ignore_ascii_case(b.trim()),
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, session, with_app};
    use super::*;
    use crate::mcp_client::tests::{fake_server, fake_state};
    use serde_json::Value;

    /// A board system whose tools nest their results under other names.
    const CARDS: &str = r#"
        system = "cards"
        mode = "board"
        states = { running = "Doing", review = "Check", done = "Shipped", blocked = "Stuck" }
        [operations.get]
        tool = "card_get"
        arguments = { card = "{id}" }
        result = { id = "card.ref", key = "card.code", url = "card.href", title = "card.name", state = "card.column.title", updated_at = "card.touched" }
        [operations.list]
        tool = "card_list"
        arguments = { board = "{board}" }
        items = "cards"
        result = { id = "ref", key = "code", url = "href", title = "name", state = "column.title", updated_at = "touched" }
        [operations.update_state]
        tool = "card_move"
        arguments = { card = "{id}", column = "{state}" }
        [operations.comment]
        tool = "card_note"
        arguments = { card = "{id}", text = "{body}" }
        [operations.create]
        tool = "card_add"
        arguments = { board = "{board}", name = "{title}", text = "{body}" }
        result = { id = "card.ref", key = "card.code", url = "card.href", title = "card.name", state = "card.column.title" }
    "#;

    /// An issue system answering with flat JSON text, addressed by key.
    const ISSUES: &str = r#"
        system = "issues"
        mode = "ticket"
        comments = "agents"
        workflow = "One issue per plan."
        states = { running = "In Progress", review = "In Review", done = "Done", blocked = "Blocked" }
        [operations.get]
        tool = "issue_read"
        arguments = { id = "{key}" }
        result = { id = "uuid", key = "id", url = "url", title = "title", state = "status", updated_at = "updatedAt" }
        [operations.list]
        tool = "issue_search"
        arguments = { project = "{board}" }
        items = "issues"
        result = { id = "uuid", key = "id", url = "url", title = "title", state = "status", updated_at = "updatedAt" }
        [operations.update_state]
        tool = "issue_save"
        arguments = { id = "{key}", state = "{state}" }
        [operations.comment]
        tool = "issue_comment"
        arguments = { issueId = "{key}", body = "{body}" }
    "#;

    fn configure(app: &mut Desktop<'_>, text: &str, state: &Path, edit: impl FnOnce(&mut TicketConfig)) {
        let mut config: TicketConfig = toml::from_str(text).unwrap();
        config.server.command = vec![fake_server().display().to_string(), state.display().to_string()];
        edit(&mut config);
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
    }

    fn ticket_request(app: &mut Desktop<'_>, value: Value) -> ControlResponse {
        let (tx, rx) = channel();
        app.runtime.control(&app.runtime.ctx.clone(), serde_json::from_value(value).unwrap(), tx);
        settle(app);
        match rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(2))).unwrap() {
            ControlOutcome::Done(response) => response,
            _ => panic!("unexpected deferred response"),
        }
    }

    #[test]
    fn shipped_profiles_work_against_the_fake_server() {
        with_app("app::tickets::tests::shipped_profiles_work_against_the_fake_server", |app, _| {
            use crate::mcp_client::tests::example_profile;
            for name in ["RELAY", "LINEAR"] {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "ENG");
                let config = example_profile(name, &state);
                let board = config.mode == TicketMode::Board;
                app.runtime.plans.clear();
                app.runtime.config.tickets = config.clone();
                app.runtime.apply_ticket_config();
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":name,
                    "ticket":{"id": if board {"b1"} else {"ENG-1"}},
                    "tasks":[{"id":"a", "title":"Parser", "ticket": if board {json!({"id":"ENG-1"})} else {Value::Null}}]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                settle(app);
                let fetched = if board { link(app, 0, 0) } else { app.runtime.plans[0].ticket.clone().unwrap() };
                assert_eq!(fetched.state.as_deref(), Some("Todo"));
                assert_eq!(fetched.label(), "ENG-1");
                assert_eq!(app.runtime.tickets.boards.contains_key(&plan), board);
                assert!(
                    ticket_request(
                        app,
                        json!({"cmd":"ticket_comment", "plan":plan, "task":"a", "text":"Implementation details"})
                    )
                    .ok
                );
                assert_eq!(comments(&state, "ENG-1").len(), 1);
                for status in ["running", "blocked", "review", "done"] {
                    assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":status})).ok);
                    settle(app);
                    assert_eq!(ticket(&state, "ENG-1")["state"], "In Progress");
                }
                for status in [TaskStatus::Review, TaskStatus::Done, TaskStatus::Blocked] {
                    assert!(config.states.get(status).is_none());
                }
                let created = ticket_request(
                    app,
                    json!({"cmd":"ticket_link", "plan":plan, "task":"review", "create":{"title":"Review", "body":"Review the implementation"}}),
                );
                assert!(created.ok, "{:?}", created.error);
                assert!(created.result["ticket"]["id"].as_str().unwrap().starts_with(if board { "n" } else { "NEW-" }));
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"review", "status":"done"})).ok);
                settle(app);
                assert_eq!(ticket(&state, "ENG-1")["state"], "In Progress");
            }
        });
    }

    #[test]
    fn lifecycle_stages_gate_completion_and_reach_tickets_through_mcp() {
        with_app("app::tickets::tests::lifecycle_stages_gate_completion_and_reach_tickets_through_mcp", |app, ctx| {
            for gate in ["pr_opened", "ci_passed"] {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "ENG");
                app.runtime.plans.clear();
                configure(app, ISSUES, &state, |config| {
                    config.states.stages.insert("pr_opened".into(), "In Review".into());
                    config.states.stages.insert("ci_failed".into(), "Blocked".into());
                });
                app.runtime.config.workflow.done = gate.into();
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":gate,
                    "ticket":{"id":"ENG-1"}, "tasks":[{"id":"a", "title":"Implement"}]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                settle(app);
                assert!(!request(app, json!({"cmd":"plan_stage", "plan":plan, "stage":"pr_opened"})).ok);
                for id in ["a", "review"] {
                    assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":id, "status":"done"})).ok);
                }
                settle(app);
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
                assert_ne!(ticket(&state, "ENG-1")["state"], "Done");
                assert!(!comments(&state, "ENG-1").iter().any(|c| c.contains("Plan complete:")));
                assert!(!request(app, json!({"cmd":"plan_close", "plan":plan, "status":"completed"})).ok);
                let args = json!({"plan":plan,"stage":"pr_opened","url":"https://github.com/example/repo/pull/42"});
                let command = crate::mcp::request_for_tool("imacs_plan_stage", args.clone()).unwrap();
                let reply = request(app, serde_json::to_value(command).unwrap());
                assert!(reply.ok, "{:?}", reply.error);
                settle(app);
                assert!(
                    comments(&state, "ENG-1")
                        .iter()
                        .any(|c| c.contains("PR opened: https://github.com/example/repo/pull/42"))
                );
                let output = board_frame(app, ctx, vec![]);
                assert!(texts(&output).iter().any(|(t, _)| t == "PR opened"));
                output.drop_without_applying_deltas();
                let saved = plan::load_plans(&paths::imacs_home()).unwrap().0;
                assert_eq!(
                    saved.iter().find(|p| p.id == plan).unwrap().stages.last().unwrap().url.as_deref(),
                    Some("https://github.com/example/repo/pull/42")
                );
                let count_before = comments(&state, "ENG-1").len();
                let duplicate = request(
                    app,
                    json!({"cmd":"plan_stage", "plan":plan, "stage":"pr_opened", "url":"https://github.com/example/repo/pull/42"}),
                );
                assert_eq!(duplicate.result["recorded"], false);
                settle(app);
                assert_eq!(comments(&state, "ENG-1").len(), count_before);
                if gate == "ci_passed" {
                    assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
                    assert_eq!(ticket(&state, "ENG-1")["state"], "In Review");
                    let failed = crate::mcp::request_for_tool(
                        "imacs_stage_event",
                        json!({"system":"issues", "key":"ENG-1", "stage":"ci_failed"}),
                    )
                    .unwrap();
                    assert!(request(app, serde_json::to_value(failed).unwrap()).ok);
                    settle(app);
                    assert_eq!(ticket(&state, "ENG-1")["state"], "Blocked");
                    assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
                    let passed = request(
                        app,
                        json!({"cmd":"stage_event", "plan":plan, "stage":"ci_passed", "url":"https://ci.example/runs/12"}),
                    );
                    assert!(passed.ok, "{:?}", passed.error);
                    settle(app);
                    assert!(
                        comments(&state, "ENG-1").iter().any(|c| c.contains("CI passed: https://ci.example/runs/12"))
                    );
                }
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
                assert_eq!(ticket(&state, "ENG-1")["state"], "Done");
                assert_eq!(comments(&state, "ENG-1").iter().filter(|c| c.contains("Plan complete:")).count(), 1);
                configure(app, ISSUES, &state, |config| config.comments = ticket::CommentPolicy::Off);
                let before = comments(&state, "ENG-1").len();
                assert!(request(app, json!({"cmd":"stage_event", "plan":plan, "stage":"deployed"})).ok);
                settle(app);
                assert_eq!(comments(&state, "ENG-1").len(), before);
                assert!(!request(app, json!({"cmd":"stage_event", "plan":plan, "stage":"unknown"})).ok);
            }
        });
    }

    #[test]
    fn task_stage_events_are_scoped_and_custom_stages_are_config_only() {
        with_app("app::tickets::tests::task_stage_events_are_scoped_and_custom_stages_are_config_only", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |config| {
                config.states.stages.insert("security_approved".into(), "Audited".into());
                config.states.stages.insert("deployed".into(), "Live".into());
                config.states.stages.insert("ci_failed".into(), "Stuck".into());
                config.states.done = None;
            });
            app.runtime.config.workflow.done = "security_approved".into();
            app.runtime.config.workflow.stages.insert(4, "security_approved".into());
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Custom", "tasks":[
                    {"id":"a", "title":"A", "ticket":{"id":"CARD-1"}}, {"id":"b", "title":"B", "ticket":{"id":"CARD-2"}}
                ]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            for id in ["a", "b", "review"] {
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":id, "status":"done"})).ok);
            }
            let event = json!({"cmd":"stage_event", "system":"cards", "key":"CARD-1", "stage":"security_approved", "note":"Audit passed"});
            assert!(request(app, event).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Audited");
            assert_eq!(ticket(&state, "CARD-2")["state"], "Todo");
            assert_eq!(app.runtime.plans[index].status, PlanStatus::Open);
            assert!(comments(&state, "CARD-1").iter().any(|c| c.contains("Audit passed")));
            assert!(request(app, json!({"cmd":"plan_stage", "plan":plan, "task":"b", "stage":"security_approved"})).ok);
            settle(app);
            assert_eq!(app.runtime.plans[index].status, PlanStatus::Completed);
            assert_eq!(ticket(&state, "CARD-2")["state"], "Audited");
            assert!(request(app, json!({"cmd":"plan_stage", "plan":plan, "stage":"deployed"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Live", "plan stages supersede older task stages");
            assert_eq!(ticket(&state, "CARD-2")["state"], "Live");
            assert!(request(app, json!({"cmd":"stage_event", "plan":plan, "task":"a", "stage":"ci_failed"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Stuck", "a newer task event supersedes the plan event");
            assert_eq!(ticket(&state, "CARD-2")["state"], "Live");
            assert!(!request(app, json!({"cmd":"stage_event", "stage":"merged"})).ok);
            assert!(!request(app, json!({"cmd":"stage_event", "plan":plan, "task":"missing", "stage":"merged"})).ok);
        });
    }

    #[test]
    fn agent_ticket_access_is_scoped_to_its_session_and_comment_policy() {
        with_app("app::tickets::tests::agent_ticket_access_is_scoped_to_its_session_and_comment_policy", |app, _| {
            use crate::mcp_client::tests::example_profile;
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "ENG");
            edit_fake(&state, |fake| {
                fake["tickets"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id":"b1", "key":"BOARD", "title":"Board details", "description":"Plan context"}));
                fake["tickets"][0]["description"] = json!("Task implementation details");
            });
            app.runtime.config.tickets = example_profile("RELAY", &state);
            app.runtime.apply_ticket_config();
            let plan = request(app, json!({"cmd":"plan_set", "goal":"Own plan", "ticket":{"id":"b1"}, "tasks":[
                {"id":"a", "title":"Own task", "agent":"codex", "args":["-m","gpt-6.1-sol","-c","model_reasoning_effort=high"], "ticket":{"id":"1"}},
                {"id":"b", "title":"Other task", "ticket":{"id":"2"}}
            ]})).result["plan"].as_str().unwrap().to_string();
            settle(app);
            let own = app.runtime.selected_plan;
            app.runtime.plans[own].tasks[0].session = Some("agent-a".into());
            app.runtime.plans[own].tasks[1].session = Some("agent-b".into());
            for id in ["agent-a", "agent-b", "outsider"] {
                app.runtime.sessions.push(session(id, AgentKind::Generic, AgentStatus::Working, dir.path()));
            }
            // Selection and task ids in another plan must not affect session ownership.
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Other plan", "ticket":{"id":"2"}, "tasks":[{"id":"a", "ticket":{"id":"2"}}]}),
            );
            settle(app);
            let other = app.runtime.selected_plan;
            let before = tools(&state).len();
            let read = ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a"}));
            assert!(read.ok);
            assert_eq!(read.result["ticket"]["description"], "Task implementation details");
            let board = ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a", "plan":true}));
            assert!(board.ok);
            assert_eq!(board.result["ticket"]["description"], "Plan context");
            let posted = ticket_request(
                app,
                json!({"cmd":"agent_ticket_comment", "pane":"agent-a", "text":"A design question"}),
            );
            assert!(posted.ok);
            assert_eq!(
                comments(&state, "ENG-1"),
                ["This is GPT-6.1 Sol (high), the IMACS agent on task a (Own task).\n\nA design question"]
            );
            assert!(comments(&state, "ENG-2").is_empty());
            assert!(tools(&state)[before..].iter().all(|name| name == "get_ticket" || name == "add_comment"));
            let before = tools(&state).len();
            for pane in [Value::Null, json!(""), json!("agent"), json!("missing"), json!("outsider")] {
                for cmd in ["agent_ticket_get", "agent_ticket_comment"] {
                    assert!(!ticket_request(app, json!({"cmd":cmd,"pane":pane,"text":"Denied"})).ok);
                }
            }
            assert_eq!(tools(&state).len(), before, "denied identities must not reach the server");
            for policy in [ticket::CommentPolicy::Chief, ticket::CommentPolicy::Off] {
                app.runtime.config.tickets.comments = policy;
                app.runtime.apply_ticket_config();
                settle(app);
                let calls = tools(&state).len();
                for cmd in ["agent_ticket_get", "agent_ticket_comment"] {
                    assert!(!ticket_request(app, json!({"cmd":cmd,"pane":"agent-a","text":"Denied"})).ok);
                }
                assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get","pane":"agent-a","plan":true})).ok);
                assert_eq!(tools(&state).len(), calls);
            }
            app.runtime.config.tickets.comments = ticket::CommentPolicy::Agents;
            app.runtime.config.tickets.mode = TicketMode::Ticket;
            app.runtime.apply_ticket_config();
            settle(app);
            let before = tools(&state).len();
            app.runtime.plans[own].tasks[0].ticket = None;
            assert!(
                !ticket_request(app, json!({"cmd":"agent_ticket_comment", "pane":"agent-a", "text":"No fallback"})).ok
            );
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a"})).ok);
            assert_eq!(tools(&state).len(), before);
            assert!(ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a", "plan":true})).ok);
            app.runtime.plans[own].tasks[0].ticket =
                Some(TicketLink { system: "different".into(), id: "2".into(), ..TicketLink::default() });
            let before = tools(&state).len();
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a"})).ok);
            app.runtime.plans[other].tasks[0].session = Some("agent-a".into());
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a", "plan":true})).ok);
            app.runtime.plans[other].tasks[0].session = None;
            app.runtime.sessions.iter_mut().find(|s| s.id == "agent-a").unwrap().chief = true;
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a", "plan":true})).ok);
            assert_eq!(tools(&state).len(), before);
            // The existing report path still posts exactly once, independently of explicit comments.
            app.runtime.sessions.iter_mut().find(|s| s.id == "agent-a").unwrap().chief = false;
            app.runtime.plans[own].tasks[0].ticket =
                Some(TicketLink { system: "relay".into(), id: "1".into(), ..TicketLink::default() });
            app.runtime.report(Some("agent-a"), "done", "Finished once");
            settle(app);
            assert_eq!(comments(&state, "ENG-1").iter().filter(|c| c.contains("Finished once")).count(), 1);
            assert_eq!(app.runtime.plans[own].id, plan);
            let before = tools(&state).len();
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_comment", "pane":"agent-a", "text":"  "})).ok);
            app.runtime.plans[own].status = PlanStatus::Archived;
            assert!(!ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a", "plan":true})).ok);
            assert_eq!(tools(&state).len(), before);
        });
    }

    /// Runs ticket jobs, and the pushes they lead to, until none are left.
    fn settle(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        loop {
            app.runtime.ticket_results();
            app.runtime.push_ticket_states(None);
            if app.runtime.tickets.pending == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "ticket jobs did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn fake(state: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(state).unwrap()).unwrap()
    }

    fn tools(state: &Path) -> Vec<String> {
        fake(state)["calls"].as_array().unwrap().iter().map(|c| c["tool"].as_str().unwrap().to_string()).collect()
    }

    fn count(state: &Path, tool: &str) -> usize {
        tools(state).iter().filter(|t| *t == tool).count()
    }

    fn ticket(state: &Path, key: &str) -> Value {
        fake(state)["tickets"].as_array().unwrap().iter().find(|t| t["key"] == key).unwrap().clone()
    }

    fn comments(state: &Path, key: &str) -> Vec<String> {
        ticket(state, key)["comments"]
            .as_array()
            .map_or_else(Vec::new, |c| c.iter().map(|c| c.as_str().unwrap().to_string()).collect())
    }

    /// Changes the fake system behind IMACS's back.
    fn edit_fake(state: &Path, edit: impl FnOnce(&mut Value)) {
        let mut value = fake(state);
        edit(&mut value);
        std::fs::write(state, value.to_string()).unwrap();
    }

    fn set_field(state: &Path, key: &str, field: &str, value: &str) {
        edit_fake(state, |fake| {
            let ticket = fake["tickets"].as_array_mut().unwrap().iter_mut().find(|t| t["key"] == key).unwrap();
            ticket[field] = json!(value);
        });
    }

    fn link(app: &Desktop<'_>, index: usize, task: usize) -> TicketLink {
        app.runtime.plans[index].tasks[task].ticket.clone().unwrap()
    }

    fn wait_for(app: &mut Desktop<'_>, ctx: &egui::Context, statuses: &[&str]) -> Receiver<ControlOutcome> {
        let (tx, rx) = channel();
        let request = ControlRequest::Wait {
            // Ticket events reach waiters watching other sessions too.
            sessions: vec!["agent-a".into()],
            statuses: statuses.iter().map(|s| s.to_string()).collect(),
            after: Some(app.runtime.event_seq),
            timeout_secs: Some(30),
        };
        app.runtime.control(ctx, request, tx);
        rx
    }

    fn tickets_fixture(dir: &Path, prefix: &str) -> PathBuf {
        fake_state(
            dir,
            json!([
                { "id": "1", "key": format!("{prefix}-1"), "board": "b1", "title": "Write the parser", "state": "Todo",
                  "url": format!("https://tickets.example/{prefix}-1"), "rev": 1 },
                { "id": "2", "key": format!("{prefix}-2"), "board": "b1", "title": "Document it", "state": "Todo",
                  "url": format!("https://tickets.example/{prefix}-2"), "rev": 1 },
                { "id": "3", "key": format!("{prefix}-3"), "board": "b1", "title": "Not planned yet", "state": "Todo",
                  "url": format!("https://tickets.example/{prefix}-3"), "rev": 1 },
            ]),
        )
    }

    #[test]
    fn board_mode_keeps_tickets_and_tasks_in_step_in_both_directions() {
        with_app("app::tickets::tests::board_mode_keeps_tickets_and_tasks_in_step_in_both_directions", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |_| {});
            let (repo_dir, repo) = repository();
            app.runtime.config.worktree_path = format!("{}/{{slug}}", repo_dir.path().display());
            let created = request(
                app,
                json!({"cmd":"plan_set", "goal":"Parser", "ticket":{"id":"b1", "title":"Parser board"}, "tasks":[
                    {"id":"a", "title":"Write the parser", "brief":"Use recursive descent.", "acceptance":["tests pass"],
                     "repo":repo, "agent":"codex", "args":["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high"],
                     "ticket":{"id":"1"}},
                    {"id":"b", "title":"Document it", "repo":repo, "ticket":{"id":"CARD-2"}}]}),
            );
            let plan = created.result["plan"].as_str().unwrap().to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            // Loading the plan reads the board once and fills in every task's ticket.
            let a = link(app, index, 0);
            assert_eq!(
                (a.system.as_str(), a.label(), a.state.as_deref(), a.title.as_deref()),
                ("cards", "CARD-1", Some("Todo"), Some("Write the parser"))
            );
            assert_eq!(a.url.as_deref(), Some("https://tickets.example/CARD-1"));
            assert_eq!(link(app, index, 1).state.as_deref(), Some("Todo"));
            assert!(a.synced_at.is_some() && a.error.is_none());
            assert_eq!(tools(&state), ["card_list"], "one call reads the board; planned tasks are not pushed");
            let full = request(app, json!({"cmd":"plan_get", "plan":plan}));
            assert_eq!(full.result["tasks"][0]["ticket"]["key"], "CARD-1");
            assert_eq!(full.result["ticket"]["id"], "b1");
            assert_eq!(full.result["ticket_board"][0]["task"], "a");
            assert_eq!(full.result["ticket_board"][2]["ticket"]["key"], "CARD-3");
            assert_eq!(full.result["ticket_board"][2]["task"], Value::Null, "tickets without a task stand out");

            // Dispatching posts the brief, signed by the chief, and moves the ticket to running.
            app.runtime.plans[index].approved = true;
            app.runtime.dispatch(index, Some("a")).unwrap();
            app.runtime
                .worktree_rx
                .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10)))
                .unwrap()
                .1
                .unwrap();
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Doing");
            let dispatched = &comments(&state, "CARD-1")[0];
            assert!(
                dispatched.starts_with(
                    "This is Claude Opus 5.5, the IMACS chief.\nDispatched task a (Write the parser) to GPT-6.1 Sol (high) on branch imacs/"
                ),
                "{dispatched}"
            );
            assert!(dispatched.contains("Implementation brief:\nUse recursive descent.\n\nDone when:\n- tests pass"));
            let a = link(app, index, 0);
            assert_eq!((a.state.as_deref(), a.pushed_state.as_deref()), (Some("Doing"), Some("Doing")));
            assert!(a.updated_at.as_deref().is_some_and(|u| u.starts_with("rev-")), "read again after each push");

            // An agent's report is signed with its model and moves the ticket to review.
            app.runtime.sessions.push(session("agent-a", AgentKind::Generic, AgentStatus::Working, &repo));
            app.runtime.plans[index].tasks[0].session = Some("agent-a".into());
            app.runtime.report(Some("agent-a"), "done", "The parser handles every case.");
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Check");
            assert_eq!(
                comments(&state, "CARD-1")[1],
                "This is GPT-6.1 Sol (high), the IMACS agent on task a (Write the parser).\nReported done:\n\nThe parser handles every case."
            );

            // Accepting posts the chief's note and moves the ticket to done.
            let note = "Verified the parser tests.";
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done", "note":note})).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Shipped");
            assert_eq!(
                comments(&state, "CARD-1")[2],
                format!("This is Claude Opus 5.5, the IMACS chief.\nTask a (Write the parser) accepted.\n\n{note}")
            );

            // A blocked report moves the other ticket to blocked.
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"b", "status":"running"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-2")["state"], "Doing");
            assert!(comments(&state, "CARD-2").is_empty(), "only dispatch, reports, acceptance and blocks comment");
            app.runtime.sessions.push(session("agent-b", AgentKind::Generic, AgentStatus::Working, &repo));
            app.runtime.plans[index].tasks[1].session = Some("agent-b".into());
            app.runtime.report(Some("agent-b"), "blocked", "Which format?");
            settle(app);
            assert_eq!(ticket(&state, "CARD-2")["state"], "Stuck");
            assert!(comments(&state, "CARD-2")[0].contains("Reported blocked:\n\nWhich format?"));

            // Someone moves it on in the ticket system: the chief hears about it on the
            // next heartbeat, and IMACS neither pushes back nor changes the task itself.
            let moves = count(&state, "card_move");
            let waiter = wait_for(app, ctx, &["ticket"]);
            set_field(&state, "CARD-2", "state", "Doing");
            app.runtime.ticket_heartbeat();
            settle(app);
            let ControlOutcome::Done(waited) =
                waiter.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!("wait did not finish")
            };
            let event = &waited.result["events"][0];
            assert_eq!(
                (event["status"].as_str(), event["plan"].as_str(), event["task"].as_str()),
                (Some("ticket"), Some(plan.as_str()), Some("b"))
            );
            assert_eq!(event["session"], "agent-b");
            assert_eq!(
                event["ticket"],
                json!({"system":"cards", "id":"CARD-2", "key":"CARD-2", "title":"Document it",
                       "url":"https://tickets.example/CARD-2", "from":"Stuck", "to":"Doing", "means":"running", "expected":false})
            );
            assert_eq!(app.runtime.plans[index].tasks[1].status, TaskStatus::Blocked);
            assert_eq!(count(&state, "card_move"), moves, "a state set in the ticket system is never overwritten");
            app.runtime.ticket_heartbeat();
            settle(app);
            assert_eq!(
                app.runtime.events.iter().filter(|e| e.ticket.is_some()).count(),
                1,
                "an unchanged ticket is no news"
            );

            // A webhook bridge says a ticket changed: applied at once, then read again.
            set_field(&state, "CARD-1", "title", "Write the parser, v2");
            let reads = count(&state, "card_get");
            let event = request(
                app,
                json!({"cmd":"ticket_event", "system":"cards", "key":"CARD-1", "title":"Write the parser, v2"}),
            );
            assert_eq!(event.result["matched"], json!([{"plan":plan, "task":"a"}]));
            assert_eq!(link(app, index, 0).title.as_deref(), Some("Write the parser, v2"));
            settle(app);
            assert_eq!(count(&state, "card_get"), reads + 1);
            // A webhook echoing IMACS's own push, even before the push returns, is no news.
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"b", "status":"review"})).ok);
            app.runtime.push_ticket_states(None);
            let events = app.runtime.events.iter().filter(|e| e.ticket.is_some()).count();
            let echo = request(app, json!({"cmd":"ticket_event", "key":"CARD-2", "state":"Check"}));
            assert_eq!(link(app, index, 1).state.as_deref(), Some("Check"));
            assert_eq!(echo.result["matched"], json!([{"plan":plan, "task":"b"}]));
            settle(app);
            assert_eq!(app.runtime.events.iter().filter(|e| e.ticket.is_some()).count(), events);
            let other =
                request(app, json!({"cmd":"ticket_event", "system":"elsewhere", "key":"CARD-1", "state":"Gone"}));
            assert_eq!(other.result["matched"], json!([]));
            assert_eq!(link(app, index, 0).state.as_deref(), Some("Shipped"));
            let board = request(app, json!({"cmd":"ticket_event", "board":"b1"}));
            assert_eq!(board.result["boards"], json!([plan]));
            settle(app);
            assert_eq!(tools(&state).last().map(String::as_str), Some("card_list"));
            let saved = Plan::load(&paths::imacs_home().join("plans").join(format!("{plan}.json"))).unwrap();
            assert_eq!(
                saved.tasks[0].ticket, app.runtime.plans[index].tasks[0].ticket,
                "links are saved with the plan"
            );
        });
    }

    #[test]
    fn ticket_mode_reports_the_whole_plan_on_its_one_ticket() {
        with_app("app::tickets::tests::ticket_mode_reports_the_whole_plan_on_its_one_ticket", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = fake_state(
                dir.path(),
                json!([{ "id": "u1", "key": "ISS-1", "board": "cli", "title": "Ship the CLI", "state": "Backlog",
                         "url": "https://issues.example/ISS-1", "rev": 1 }]),
            );
            configure(app, ISSUES, &state, |_| {});
            let (repo_dir, repo) = repository();
            app.runtime.config.worktree_path = format!("{}/{{slug}}", repo_dir.path().display());
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"CLI", "ticket":{"id":"ISS-1"},
                       "tasks":[{"id":"a", "title":"Add --json", "brief":"Serde.", "repo":repo}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            let link = app.runtime.plans[index].ticket.clone().unwrap();
            assert_eq!((link.id.as_str(), link.label(), link.state.as_deref()), ("ISS-1", "ISS-1", Some("Backlog")));
            assert_eq!(link.title.as_deref(), Some("Ship the CLI"));
            assert_eq!(tools(&state), ["issue_read"]);

            app.runtime.plans[index].approved = true;
            app.runtime.dispatch(index, Some("a")).unwrap();
            app.runtime
                .worktree_rx
                .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(10)))
                .unwrap()
                .1
                .unwrap();
            settle(app);
            assert_eq!(ticket(&state, "ISS-1")["state"], "In Progress");
            assert!(comments(&state, "ISS-1")[0].contains("Dispatched task a (Add --json) to Claude"));
            app.runtime.sessions.push(session("agent-a", AgentKind::Claude, AgentStatus::Working, &repo));
            app.runtime.plans[index].tasks[0].session = Some("agent-a".into());
            app.runtime.report(Some("agent-a"), "done", "Added it.");
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "ISS-1")["state"], "In Progress", "the final review is still to come");
            let posted = comments(&state, "ISS-1");
            assert!(posted[1].starts_with("This is Claude, the IMACS agent on task a (Add --json).\nReported done:"));
            assert!(posted[2].contains("Task a (Add --json) accepted."));
            assert_eq!(count(&state, "issue_save"), 1);

            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"review", "status":"done"})).ok);
            assert!(request(app, json!({"cmd":"plan_close", "plan":plan, "status":"completed"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "ISS-1")["state"], "Done");
            assert_eq!(
                comments(&state, "ISS-1").last().unwrap(),
                &format!(
                    "Posted by IMACS.\nPlan complete: CLI (2 of 2 tasks done, integrated on {}).",
                    app.runtime.plans[index].integration_branch()
                )
            );
            assert!(app.runtime.chief_prompt().contains("Mode ticket: one ticket maps to one plan."));
            assert!(app.runtime.chief_prompt().ends_with("Workflow: One issue per plan."));
        });
    }

    /// The same plan and steps against two unrelated tool dialects: only the config differs.
    #[test]
    fn swapping_ticket_systems_needs_only_config() {
        with_app("app::tickets::tests::swapping_ticket_systems_needs_only_config", |app, _| {
            for (text, prefix, expected) in [
                (
                    CARDS,
                    "CARD",
                    [
                        "card_list",
                        "card_note",
                        "card_get",
                        "card_move",
                        "card_get",
                        "card_note",
                        "card_get",
                        "card_move",
                        "card_get",
                    ],
                ),
                (
                    ISSUES,
                    "ISS",
                    [
                        "issue_search",
                        "issue_comment",
                        "issue_read",
                        "issue_save",
                        "issue_read",
                        "issue_comment",
                        "issue_read",
                        "issue_save",
                        "issue_read",
                    ],
                ),
            ] {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), prefix);
                configure(app, text, &state, |config| config.mode = TicketMode::Board);
                let config = app.runtime.ticket_config().unwrap();
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":format!("Swap {prefix}"), "ticket":{"id":"b1"},
                           "tasks":[{"id":"a", "title":"Write the parser", "ticket":{"id":"1"}}]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                assert_eq!(link(app, index, 0).label(), format!("{prefix}-1"));
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"blocked", "note":"Needs a grammar"})).ok);
                settle(app);
                assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok);
                settle(app);
                let key = format!("{prefix}-1");
                assert_eq!(ticket(&state, &key)["state"], config.states.done.clone().unwrap());
                assert_eq!(link(app, index, 0).state, config.states.done);
                assert!(
                    comments(&state, &key)[0].ends_with("Task a (Write the parser) is blocked.\n\nNeeds a grammar")
                );
                assert_eq!(tools(&state), expected);
                assert!(request(app, json!({"cmd":"plan_close", "plan":plan, "status":"archived"})).ok);
            }
        });
    }

    #[test]
    fn failures_show_on_the_link_and_never_block_the_plan() {
        with_app("app::tickets::tests::failures_show_on_the_link_and_never_block_the_plan", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            // Startup and ordinary error replies are fixture readiness, not the timeout behavior.
            configure(app, CARDS, &state, |_| {});
            edit_fake(&state, |fake| fake["fail"] = json!(["card_move", "card_note"]));
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Broken", "tasks":[{"id":"a", "title":"Write the parser", "ticket":{"id":"1"}}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
            settle(app);
            assert_eq!(app.runtime.plans[index].tasks[0].status, TaskStatus::Running, "IMACS goes on regardless");
            let error = link(app, index, 0).error.unwrap();
            assert_eq!(error, "setting the state to Doing failed: card_move failed: card_move is broken today");
            for _ in 0..3 {
                app.runtime.sync_plan();
                settle(app);
            }
            assert_eq!(count(&state, "card_move"), 1, "a failed push waits for the heartbeat instead of repeating");
            app.runtime.ticket_heartbeat();
            settle(app);
            assert_eq!(count(&state, "card_move"), 2);
            assert!(link(app, index, 0).error.is_some(), "a successful read does not hide a failed push");

            let (tx, rx) = channel();
            app.runtime.control(
                &app.runtime.ctx.clone(),
                ControlRequest::TicketComment { plan: Some(plan.clone()), task: Some("a".into()), text: "Hi".into() },
                tx,
            );
            settle(app);
            let ControlOutcome::Done(commented) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            assert!(commented.error.unwrap().contains("card_note is broken today"));

            edit_fake(&state, |fake| fake["fail"] = json!([]));
            let (tx, rx) = channel();
            app.runtime.sync_tickets(Some(plan.clone()), tx);
            settle(app);
            let ControlOutcome::Done(synced) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            assert!(synced.ok && synced.result["errors"] == json!([]), "{:?}", synced.result);
            // The reply waits for the retried state update and the read after it.
            let replied = &synced.result["plans"][0]["tasks"][0]["ticket"];
            assert_eq!((replied["state"].as_str(), replied.get("error")), (Some("Doing"), None), "{replied}");
            let healed = link(app, index, 0);
            assert_eq!((healed.state.as_deref(), healed.error.as_deref()), (Some("Doing"), None));

            // A server that stops answering times out, and the next jobs fail at once.
            app.runtime.config.tickets.timeout_secs = 1;
            // Use a fresh worker/connection for the deliberately short behavior deadline.
            app.runtime.tickets.jobs.take();
            app.runtime.apply_ticket_config();
            edit_fake(&state, |fake| fake["hang"] = json!(["card_get"]));
            let started = Instant::now();
            app.runtime.ticket_heartbeat();
            settle(app);
            assert!(link(app, index, 0).error.unwrap().contains("did not answer within 1 s"));
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"review"})).ok);
            settle(app);
            assert!(started.elapsed() < Duration::from_secs(10));
            let error = link(app, index, 0).error.unwrap();
            assert!(error.contains("setting the state to Check failed: cards is unavailable"), "{error}");
            assert_eq!(app.runtime.plans[index].tasks[0].status, TaskStatus::Review);
        });
    }

    #[test]
    fn switching_systems_never_sends_old_links_to_the_new_system() {
        with_app("app::tickets::tests::switching_systems_never_sends_old_links_to_the_new_system", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            for system in ["cards", "issues"] {
                std::fs::create_dir(dir.path().join(system)).unwrap();
            }
            let cards = tickets_fixture(&dir.path().join("cards"), "CARD");
            configure(app, CARDS, &cards, |_| {});
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Switch", "ticket":{"id":"b1"}, "tasks":[
                    {"id":"a", "title":"Parser", "ticket":{"id":"1"}}, {"id":"b", "title":"Docs"}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            app.runtime.plan_view = true;
            app.runtime.panel = PanelMode::Plan;
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
            settle(app);
            assert_eq!(ticket(&cards, "CARD-1")["state"], "Doing");
            assert_eq!(link(app, index, 0).system, "cards");

            // Another system with the same ticket and board ids: any cross-talk would land on them.
            let issues = tickets_fixture(&dir.path().join("issues"), "ISS");
            configure(app, ISSUES, &issues, |config| config.mode = TicketMode::Board);
            settle(app);
            app.runtime.ticket_heartbeat();
            settle(app);
            assert!(
                request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"blocked", "note":"stuck"}))
                    .ok
            );
            app.runtime.sessions.push(session("agent-a", AgentKind::Generic, AgentStatus::Working, dir.path()));
            app.runtime.plans[index].tasks[0].session = Some("agent-a".into());
            app.runtime.report(Some("agent-a"), "progress", "Halfway there.");
            settle(app);
            let (tx, rx) = channel();
            let comment =
                ControlRequest::TicketComment { plan: Some(plan.clone()), task: Some("a".into()), text: "Hi".into() };
            app.runtime.control(ctx, comment, tx);
            settle(app);
            let ControlOutcome::Done(commented) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            assert!(commented.error.unwrap_or_default().contains("belongs to cards"));
            let (tx, rx) = channel();
            app.runtime.sync_tickets(Some(plan.clone()), tx);
            settle(app);
            let ControlOutcome::Done(synced) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            assert!(synced.result["errors"].to_string().contains("belongs to cards"), "{}", synced.result);
            request(app, json!({"cmd":"ticket_event", "key":"CARD-1", "state":"Gone"}));
            settle(app);
            assert_eq!(tools(&issues), Vec::<String>::new(), "nothing reaches the new system");
            assert_eq!(link(app, index, 0).state.as_deref(), Some("Doing"), "events from the new system do not apply");

            let output = board_frame(app, ctx, vec![]);
            let shown = texts(&output);
            assert!(
                shown.iter().any(|(text, _)| text.contains("belongs to cards") && text.contains("issues")),
                "{:?}",
                shown.iter().map(|(t, _)| t).collect::<Vec<_>>()
            );
            output.drop_without_applying_deltas();

            // New links use the new system, and switching back resumes the old ones.
            let (tx, rx) = channel();
            let linked = ControlRequest::TicketLink {
                plan: Some(plan.clone()),
                task: Some("b".into()),
                ticket: Some(TicketLink { id: "2".into(), ..TicketLink::default() }),
                create: None,
                unlink: false,
            };
            app.runtime.control(ctx, linked, tx);
            settle(app);
            rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();
            assert_eq!(link(app, index, 1).system, "issues");
            assert_eq!(tools(&issues), ["issue_read"]);
            configure(app, CARDS, &cards, |_| {});
            settle(app);
            assert_eq!(ticket(&cards, "CARD-1")["state"], "Stuck", "the blocked status reaches the right system later");
        });
    }

    #[test]
    fn a_late_create_never_replaces_a_newer_link_or_takes_another_system() {
        with_app(
            "app::tickets::tests::a_late_create_never_replaces_a_newer_link_or_takes_another_system",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "CARD");
                configure(app, CARDS, &state, |_| {});
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Late", "ticket":{"id":"b1"}, "tasks":[{"id":"a"}, {"id":"b"}]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                let send = |app: &mut Desktop<'_>, task: &str, ticket: Option<&str>| {
                    let (tx, rx) = channel();
                    let request = ControlRequest::TicketLink {
                        plan: Some(plan.clone()),
                        task: Some(task.into()),
                        ticket: ticket.map(|id| TicketLink { id: id.into(), ..TicketLink::default() }),
                        create: ticket
                            .is_none()
                            .then(|| TicketDraft { title: "Created late".into(), body: String::new() }),
                        unlink: false,
                    };
                    app.runtime.control(ctx, request, tx);
                    rx
                };
                // The create is under way when the chief links another ticket to the same task.
                let created = send(app, "a", None);
                let linked = send(app, "a", Some("2"));
                settle(app);
                let ControlOutcome::Done(created) =
                    created.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!()
                };
                linked.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();
                assert_eq!(link(app, index, 0).label(), "CARD-2", "the newer link stays");
                assert!(created.result["errors"].to_string().contains("superseded"), "{:?}", created);

                // A create sent to one system keeps that system when the config changes meanwhile.
                let created = send(app, "b", None);
                std::fs::create_dir(dir.path().join("issues")).unwrap();
                let other = tickets_fixture(&dir.path().join("issues"), "ISS");
                configure(app, ISSUES, &other, |config| config.mode = TicketMode::Board);
                settle(app);
                created.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();
                let b = link(app, index, 1);
                assert!(b.label().starts_with("NEW-"));
                assert_eq!(b.system, "cards");
            },
        );
    }

    #[test]
    fn a_read_after_a_comment_reports_what_someone_else_changed() {
        with_app("app::tickets::tests::a_read_after_a_comment_reports_what_someone_else_changed", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |_| {});
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Watch", "tasks":[{"id":"a", "title":"Parser", "ticket":{"id":"1"}}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
            settle(app);
            assert_eq!(ticket(&state, "CARD-1")["state"], "Doing");
            set_field(&state, "CARD-1", "state", "Stuck");
            let (tx, rx) = channel();
            let comment =
                ControlRequest::TicketComment { plan: Some(plan.clone()), task: Some("a".into()), text: "Note".into() };
            app.runtime.control(ctx, comment, tx);
            settle(app);
            rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();
            let moved: Vec<_> = app.runtime.events.iter().filter_map(|e| e.ticket.clone()).collect();
            assert_eq!(moved.len(), 1, "the read after the comment saw someone else's change");
            assert_eq!((moved[0]["from"].as_str(), moved[0]["to"].as_str()), (Some("Doing"), Some("Stuck")));
            app.runtime.ticket_heartbeat();
            settle(app);
            assert_eq!(app.runtime.events.iter().filter(|e| e.ticket.is_some()).count(), 1, "reported once");
        });
    }

    #[test]
    fn plan_set_supersedes_pending_creates_by_the_ids_tasks_end_up_with() {
        with_app(
            "app::tickets::tests::plan_set_supersedes_pending_creates_by_the_ids_tasks_end_up_with",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "CARD");
                configure(app, CARDS, &state, |_| {});
                let plan =
                    request(app, json!({"cmd":"plan_set", "goal":"Ids", "ticket":{"id":"b1"}, "tasks":[{"id":"t1"}]}))
                        .result["plan"]
                        .as_str()
                        .unwrap()
                        .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                let (tx, created) = channel();
                let create = ControlRequest::TicketLink {
                    plan: Some(plan.clone()),
                    task: Some("t1".into()),
                    ticket: None,
                    create: Some(TicketDraft { title: "Created late".into(), body: String::new() }),
                    unlink: false,
                };
                app.runtime.control(ctx, create, tx);
                // A task without an id becomes t1, and its ticket is newer than the pending create.
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":plan, "goal":"Ids", "tasks":[{"id":" ", "ticket":{"id":"2"}}]})
                    )
                    .ok
                );
                settle(app);
                created.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();
                assert_eq!(app.runtime.plans[index].tasks[0].id, "t1");
                assert_eq!(link(app, index, 0).label(), "CARD-2", "the plan_set link stays");
            },
        );
    }

    #[test]
    fn a_rejected_link_request_leaves_a_pending_create_alone() {
        with_app("app::tickets::tests::a_rejected_link_request_leaves_a_pending_create_alone", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |_| {});
            let plan =
                request(app, json!({"cmd":"plan_set", "goal":"Reject", "ticket":{"id":"b1"}, "tasks":[{"id":"a"}]}))
                    .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            let link_request = |ticket: Option<TicketLink>, create: Option<TicketDraft>| ControlRequest::TicketLink {
                plan: Some(plan.clone()),
                task: Some("a".into()),
                ticket,
                create,
                unlink: false,
            };
            let (tx, created) = channel();
            app.runtime.control(
                ctx,
                link_request(None, Some(TicketDraft { title: "Wanted".into(), body: String::new() })),
                tx,
            );
            for invalid in [
                link_request(Some(TicketLink::default()), None),
                link_request(None, Some(TicketDraft { title: " ".into(), body: String::new() })),
                link_request(None, None),
            ] {
                let (tx, rx) = channel();
                app.runtime.control(ctx, invalid, tx);
                let ControlOutcome::Done(rejected) =
                    rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!()
                };
                assert!(!rejected.ok);
            }
            settle(app);
            let ControlOutcome::Done(created) =
                created.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            assert!(created.ok && created.result["errors"] == json!([]), "{:?}", created);
            assert!(link(app, index, 0).label().starts_with("NEW-"), "the created ticket is attached");
        });
    }

    #[test]
    fn late_results_are_cleaned_with_the_secrets_of_the_job_that_made_them() {
        with_app(
            "app::tickets::tests::late_results_are_cleaned_with_the_secrets_of_the_job_that_made_them",
            |app, ctx| {
                const OLD_KEY: &str = "alpha-key-7c1d9e";
                const OLD_TOKEN: &str = "alpha-token-5b6c7d";
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "CARD");
                let with_key = |key: &'static str| {
                    move |config: &mut TicketConfig| {
                        config.server.env.insert("API_KEY".into(), key.into());
                    }
                };
                configure(app, CARDS, &state, with_key(OLD_KEY));
                edit_fake(&state, |fake| {
                    fake["fail"] = json!(["card_note"]);
                    fake["fail_message"] = json!(format!("denied for {OLD_KEY}"));
                });
                let plan =
                    request(app, json!({"cmd":"plan_set", "goal":"Rotate", "tasks":[{"id":"a", "ticket":{"id":"1"}}]}))
                        .result["plan"]
                        .as_str()
                        .unwrap()
                        .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                // The comment is under way with the old key when the key is rotated.
                let (tx, commented) = channel();
                let comment = ControlRequest::TicketComment {
                    plan: Some(plan.clone()),
                    task: Some("a".into()),
                    text: "Hi".into(),
                };
                app.runtime.control(ctx, comment, tx);
                configure(app, CARDS, &state, with_key("bravo-key-2e8f4a"));
                settle(app);
                let ControlOutcome::Done(commented) =
                    commented.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!()
                };
                let reply = commented.error.unwrap();
                assert!(reply.contains("denied for ***"), "{reply}");

                // A server that dies printing the old token: the backoff message must not carry it later.
                // SAFETY: no ticket job runs while the variable changes, and nothing else reads it.
                unsafe { std::env::set_var("ROTATE_TOKEN", OLD_TOKEN) };
                let dying = |config: &mut TicketConfig| {
                    config.server.command =
                        vec!["/bin/sh".into(), "-c".into(), "echo \"denied for $ROTATE_TOKEN\" >&2; exit 1".into()];
                    config.server.token_env = Some("ROTATE_TOKEN".into());
                };
                configure(app, CARDS, &state, dying);
                settle(app);
                unsafe { std::env::set_var("ROTATE_TOKEN", "bravo-token-9a0b1c") };
                let (tx, synced) = channel();
                app.runtime.sync_tickets(Some(plan.clone()), tx);
                settle(app);
                let ControlOutcome::Done(synced) =
                    synced.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!()
                };
                let errors = synced.result["errors"].to_string();
                assert!(errors.contains("unavailable"), "{errors}");

                let saved =
                    std::fs::read_to_string(paths::imacs_home().join("plans").join(format!("{plan}.json"))).unwrap();
                for text in [&reply, &errors, &link(app, index, 0).error.unwrap_or_default(), &saved] {
                    for secret in [OLD_KEY, OLD_TOKEN] {
                        assert!(!text.contains(secret), "{secret} leaked into {text}");
                    }
                }
            },
        );
    }

    #[test]
    fn tickets_read_from_a_backend_are_cleaned_before_they_are_stored() {
        with_app("app::tickets::tests::tickets_read_from_a_backend_are_cleaned_before_they_are_stored", |app, ctx| {
            const TOKEN: &str = "sk-title-4d5e6f";
            // SAFETY: set before the ticket worker starts; nothing else reads this variable.
            unsafe { std::env::set_var("CARDS_TOKEN", TOKEN) };
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            set_field(&state, "CARD-1", "title", &format!("Rotate {TOKEN} today"));
            set_field(
                &state,
                "CARD-1",
                "url",
                &format!("https://bot:hunter22@cards.example/1?access_token={TOKEN}&view=full"),
            );
            configure(app, CARDS, &state, |config| config.server.token_env = Some("CARDS_TOKEN".into()));
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Clean", "ticket":{"id":"b1"}, "tasks":[
                {"id":"a", "ticket":{"id":"1"}}, {"id":"b", "ticket":{"id":"2"}}, {"id":"c"}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            let a = link(app, index, 0);
            assert_eq!(a.title.as_deref(), Some("Rotate *** today"));
            assert_eq!(a.url.as_deref(), Some("https://cards.example/1?view=full"));

            // Fields from a webhook and links from the chief are cleaned the same way.
            request(
                app,
                json!({"cmd":"ticket_event", "key":"CARD-2",
                "url":"https://u:pw@cards.example/2?token=abc123xyz&tab=1", "title":format!("Echo {TOKEN}")}),
            );
            let b = link(app, index, 1);
            assert_eq!(b.url.as_deref(), Some("https://cards.example/2?tab=1"));
            assert_eq!(b.title.as_deref(), Some("Echo ***"));
            let (tx, rx) = channel();
            let linked = ControlRequest::TicketLink {
                plan: Some(plan.clone()),
                task: Some("c".into()),
                ticket: Some(TicketLink {
                    id: "3".into(),
                    url: Some(format!("https://cards.example/3?api_key={TOKEN}")),
                    ..TicketLink::default()
                }),
                create: None,
                unlink: false,
            };
            app.runtime.control(ctx, linked, tx);
            assert_eq!(link(app, index, 2).url.as_deref(), Some("https://cards.example/3"), "cleaned as given");
            settle(app);
            rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap();

            let saved =
                std::fs::read_to_string(paths::imacs_home().join("plans").join(format!("{plan}.json"))).unwrap();
            let got = request(app, json!({"cmd":"plan_get", "plan":plan})).result.to_string();
            for text in [&saved, &got] {
                for secret in [TOKEN, "hunter22", "pw@", "abc123xyz", "access_token", "api_key"] {
                    assert!(!text.contains(secret), "{secret} reached {text}");
                }
            }
        });
    }

    #[test]
    fn agents_read_long_descriptions_whole() {
        with_app("app::tickets::tests::agents_read_long_descriptions_whole", |app, _| {
            use crate::mcp_client::tests::example_profile;
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "ENG");
            let description = format!("{}LAST LINE: ship it", "Step by step instructions for the agent.\n".repeat(80));
            assert!(description.chars().count() > imacs_core::tickets::MAX_FIELD_CHARS);
            edit_fake(&state, |fake| fake["tickets"][0]["description"] = json!(description));
            app.runtime.config.tickets = example_profile("RELAY", &state);
            app.runtime.apply_ticket_config();
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Long", "tasks":[{"id":"a", "title":"Own", "ticket":{"id":"1"}}]}),
            );
            let index = app.runtime.selected_plan;
            settle(app);
            app.runtime.plans[index].tasks[0].session = Some("agent-a".into());
            app.runtime.sessions.push(session("agent-a", AgentKind::Generic, AgentStatus::Working, dir.path()));
            let read = ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a"}));
            assert!(read.ok, "{read:?}");
            assert_eq!(read.result["ticket"]["description"], description, "agents get the whole description");
        });
    }

    #[test]
    fn split_and_property_name_secrets_never_reach_plans_events_or_agents() {
        with_app(
            "app::tickets::tests::split_and_property_name_secrets_never_reach_plans_events_or_agents",
            |app, ctx| {
                use crate::mcp_client::tests::example_profile;
                const TOKEN: &str = "sk-split-9e8d7c";
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "ENG");
                edit_fake(&state, |fake| {
                    fake["tickets"][0]["title"] = json!("Rotate sk-split\n-9e8d7c now");
                    fake["tickets"][0]["fields"] = json!({ TOKEN: "rejected", "note": "key sk-split-\r\n9e8d7c" });
                });
                let mut config = example_profile("RELAY", &state);
                config.server.env.insert("API_KEY".into(), TOKEN.into());
                app.runtime.config.tickets = config;
                app.runtime.apply_ticket_config();
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Split", "tasks":[{"id":"a", "title":"Own", "ticket":{"id":"1"}}]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                assert_eq!(link(app, index, 0).title.as_deref(), Some("Rotate *** now"));
                // A webhook's state split across lines reaches the link, the wait event and the toast.
                let waiter = wait_for(app, ctx, &["ticket"]);
                request(app, json!({"cmd":"ticket_event", "key":"ENG-1", "state":"Blocked by sk-split-\n 9e8d7c"}));
                let ControlOutcome::Done(event) =
                    waiter.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
                else {
                    panic!()
                };
                assert_eq!(link(app, index, 0).state.as_deref(), Some("Blocked by ***"));
                // The agent's read cleans property names as well as values.
                app.runtime.plans[index].tasks[0].session = Some("agent-a".into());
                app.runtime.sessions.push(session("agent-a", AgentKind::Generic, AgentStatus::Working, dir.path()));
                let read = ticket_request(app, json!({"cmd":"agent_ticket_get", "pane":"agent-a"}));
                assert!(read.ok, "{read:?}");

                let saved =
                    std::fs::read_to_string(paths::imacs_home().join("plans").join(format!("{plan}.json"))).unwrap();
                let got = request(app, json!({"cmd":"plan_get", "plan":plan})).result.to_string();
                let toast = app.runtime.toast.clone().map(|(text, _)| text).unwrap_or_default();
                for text in [saved, got, event.result.to_string(), toast, read.result.to_string()] {
                    assert!(!text.contains("9e8d7c"), "the token's tail reached {text}");
                }
            },
        );
    }

    #[test]
    fn secrets_echoed_by_a_backend_never_reach_plans_or_replies() {
        with_app("app::tickets::tests::secrets_echoed_by_a_backend_never_reach_plans_or_replies", |app, ctx| {
            const TOKEN: &str = "sk-synthetic-7f3a9c";
            // SAFETY: set before the ticket worker starts; nothing else reads this variable.
            unsafe { std::env::set_var("TRACKER_TOKEN", TOKEN) };
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |config| config.server.token_env = Some("TRACKER_TOKEN".into()));
            let echo = format!(
                "rejected Authorization: Bearer {TOKEN} for https://bot:hunter22@cards.example/api?page=2&access_token={TOKEN} {}",
                "padding ".repeat(400)
            );
            edit_fake(&state, |fake| {
                fake["fail"] = json!(["card_move", "card_note"]);
                fake["fail_message"] = json!(echo);
            });
            let plan = request(
                app,
                json!({"cmd":"plan_set", "goal":"Secrets", "tasks":[{"id":"a", "title":"Parser", "ticket":{"id":"1"}}]}),
            )
            .result["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            settle(app);
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"})).ok);
            settle(app);
            let (tx, rx) = channel();
            let comment =
                ControlRequest::TicketComment { plan: Some(plan.clone()), task: Some("a".into()), text: "Hi".into() };
            app.runtime.control(ctx, comment, tx);
            settle(app);
            let ControlOutcome::Done(commented) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            let reply = commented.error.unwrap();

            // A token split across two content blocks, joined with a newline on the way in.
            edit_fake(&state, |fake| fake["fail_message"] = json!(["rejected key sk-synthe", "tic-7f3a9c, try again"]));
            let (tx, rx) = channel();
            let comment =
                ControlRequest::TicketComment { plan: Some(plan.clone()), task: Some("a".into()), text: "Hi".into() };
            app.runtime.control(ctx, comment, tx);
            settle(app);
            let ControlOutcome::Done(split) =
                rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap()
            else {
                panic!()
            };
            let split = split.error.unwrap();
            assert!(!split.contains("7f3a9c") && !split.contains("sk-synthe"), "{split}");
            assert!(split.contains("rejected key ***, try again"), "masked in place: {split}");

            // A stdio server that prints its credentials to stderr before dying.
            let mut dying: TicketConfig = toml::from_str(CARDS).unwrap();
            dying.server.command =
                vec!["/bin/sh".into(), "-c".into(), "echo \"denied: Bearer $TICKET_API_KEY\" >&2; exit 1".into()];
            dying.server.env.insert("TICKET_API_KEY".into(), "sk-stderr-5d1e8b".into());
            app.runtime.config.tickets = dying;
            app.runtime.apply_ticket_config();
            settle(app);
            let error = link(app, index, 0).error.unwrap();
            assert!(error.contains("denied: Bearer ***"), "{error}");

            let saved =
                std::fs::read_to_string(paths::imacs_home().join("plans").join(format!("{plan}.json"))).unwrap();
            let got = request(app, json!({"cmd":"plan_get", "plan":plan})).result.to_string();
            for text in [&saved, &got, &reply, &error] {
                for secret in [TOKEN, "hunter22", "sk-stderr-5d1e8b"] {
                    assert!(!text.contains(secret), "{secret} leaked into {text}");
                }
            }
            assert!(
                reply.contains("Authorization: Bearer ***")
                    && reply.contains("https://***@cards.example/api?page=2&access_token=***"),
                "{reply}"
            );
            assert!(
                reply.chars().count() <= imacs_core::tickets::MAX_ERROR_CHARS + 1,
                "errors are bounded: {}",
                reply.len()
            );
        });
    }

    #[test]
    fn chief_tools_link_create_and_comment_within_the_comment_policy() {
        with_app("app::tickets::tests::chief_tools_link_create_and_comment_within_the_comment_policy", |app, ctx| {
            assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__imacs__imacs_ticket_comment"));
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            let call = |app: &mut Desktop<'_>, value: Value| {
                let (tx, rx) = channel();
                app.runtime.control(ctx, serde_json::from_value(value).unwrap(), tx);
                settle(app);
                match rx.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1))).unwrap() {
                    ControlOutcome::Done(response) => response,
                    _ => panic!("deferred"),
                }
            };
            let plan = request(app, json!({"cmd":"plan_set", "goal":"Tools", "tasks":[{"id":"a"}, {"id":"b"}]})).result
                ["plan"]
                .as_str()
                .unwrap()
                .to_string();
            let index = app.runtime.selected_plan;
            // Without a configured system, links are still kept.
            let manual =
                call(app, json!({"cmd":"ticket_link", "plan":plan, "task":"a", "ticket":{"id":"1", "key":"CARD-1"}}));
            assert_eq!(manual.result["ticket"], json!({"id":"1", "key":"CARD-1"}));
            assert!(!app.runtime.chief_prompt().contains("Tickets:"));

            configure(app, CARDS, &state, |_| {});
            assert!(app.runtime.chief_prompt().contains("Tickets: this workspace tracks its work in cards"));
            settle(app);
            assert_eq!(link(app, index, 0).system, "cards", "a link made for no system takes the system that reads it");
            assert_eq!(link(app, index, 0).title.as_deref(), Some("Write the parser"), "read once configured");
            let board = call(app, json!({"cmd":"ticket_link", "plan":plan, "ticket":{"id":"b1"}}));
            assert_eq!(board.result["ticket"]["system"], "cards");
            assert_eq!(board.result["board"].as_array().unwrap().len(), 3);
            let created = call(
                app,
                json!({"cmd":"ticket_link", "plan":plan, "task":"b", "create":{"title":"Write docs", "body":"The README"}}),
            );
            let key = created.result["ticket"]["key"].as_str().unwrap().to_string();
            assert!(key.starts_with("NEW-"));
            assert_eq!(ticket(&state, &key)["board"], "b1");
            assert_eq!(link(app, index, 1).title.as_deref(), Some("Write docs"));

            let commented =
                call(app, json!({"cmd":"ticket_comment", "plan":plan, "task":"b", "text":"Uses the v2 grammar."}));
            assert!(commented.ok);
            assert_eq!(comments(&state, &key), ["This is Claude Opus 5.5, the IMACS chief.\n\nUses the v2 grammar."]);
            let missing = call(app, json!({"cmd":"ticket_comment", "plan":plan, "text":"No target"}));
            assert!(missing.error.unwrap().contains("board mode"));
            let unlinked = call(app, json!({"cmd":"ticket_link", "plan":plan, "task":"a", "unlink":true}));
            assert_eq!(unlinked.result["ticket"], Value::Null);
            assert!(app.runtime.plans[index].tasks[0].ticket.is_none());
            assert!(!call(app, json!({"cmd":"ticket_link", "plan":plan, "task":"zz", "ticket":{"id":"1"}})).ok);
            assert!(!call(app, json!({"cmd":"ticket_link", "plan":plan, "task":"a"})).ok);

            configure(app, CARDS, &state, |config| config.comments = imacs_core::tickets::CommentPolicy::Off);
            let off = call(app, json!({"cmd":"ticket_comment", "plan":plan, "task":"b", "text":"Again"}));
            assert!(off.error.unwrap().contains("are off"));
            assert!(request(app, json!({"cmd":"plan_update", "plan":plan, "id":"b", "status":"done", "note":"ok"})).ok);
            settle(app);
            assert_eq!(comments(&state, &key).len(), 1, "no comments while they are off");
            assert_eq!(ticket(&state, &key)["state"], "Shipped", "states are still kept in step");
        });
    }

    fn board_frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        let time = ctx.input(|i| i.time) + 0.1;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1800.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| {
                egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
            },
        );
        app.runtime.drain_actions();
        output
    }

    fn texts(output: &egui::FullOutput) -> Vec<(String, Pos2)> {
        output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => {
                    Some((text.galley.text().to_string(), text.pos + text.galley.rect.center().to_vec2()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ticket_progress_is_expected_after_acceptance_stages_or_plan_completion() {
        with_app(
            "app::tickets::tests::ticket_progress_is_expected_after_acceptance_stages_or_plan_completion",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let state = tickets_fixture(dir.path(), "CARD");
                configure(app, CARDS, &state, |_| {});
                app.runtime.sessions.push(session("agent-a", AgentKind::Generic, AgentStatus::Idle, dir.path()));
                let plan = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Drift", "tasks":[
                        {"id":"a", "title":"Parser", "ticket":{"id":"CARD-1"}}
                    ]}),
                )
                .result["plan"]
                    .as_str()
                    .unwrap()
                    .to_string();
                let index = app.runtime.selected_plan;
                settle(app);
                for (scenario, external, expected) in [
                    ("active", "Check", false),
                    ("accepted", "Doing", true),
                    ("stage", "Stuck", true),
                    ("completed", "Doing", true),
                ] {
                    match scenario {
                        "active" => {
                            assert!(
                                request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"running"}))
                                    .ok
                            );
                        }
                        "accepted" => {
                            assert!(
                                request(app, json!({"cmd":"plan_update", "plan":plan, "id":"a", "status":"done"})).ok
                            );
                        }
                        "stage" => {
                            app.runtime.plans[index].tasks[0].status = TaskStatus::Review;
                            assert!(
                                request(
                                    app,
                                    json!({"cmd":"plan_stage", "plan":plan, "task":"a", "stage":"integrated"})
                                )
                                .ok
                            );
                        }
                        "completed" => {
                            app.runtime.plans[index].status = PlanStatus::Completed;
                            app.runtime.plans[index].tasks[0].stages.clear();
                        }
                        _ => unreachable!(),
                    }
                    settle(app);
                    let waiter = wait_for(app, ctx, &["ticket"]);
                    set_field(&state, "CARD-1", "state", external);
                    assert!(
                        request(app, json!({"cmd":"ticket_event", "system":"cards", "key":"CARD-1", "state":external}))
                            .ok
                    );
                    settle(app);
                    let ControlOutcome::Done(result) = waiter
                        .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(1)))
                        .unwrap_or_else(|error| panic!("{scenario}: {error:?}"))
                    else {
                        panic!("wait reply")
                    };
                    assert_eq!(result.result["events"][0]["ticket"]["expected"], expected, "{scenario}: {result:?}");
                    let output = board_frame(app, ctx, vec![]);
                    assert_eq!(
                        texts(&output).iter().any(|(t, _)| t.starts_with("ticket says")),
                        !expected,
                        "{scenario}"
                    );
                    output.drop_without_applying_deltas();
                }
            },
        );
    }

    #[test]
    fn cards_and_the_plan_header_show_live_tickets_and_open_them() {
        with_app("app::tickets::tests::cards_and_the_plan_header_show_live_tickets_and_open_them", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let state = tickets_fixture(dir.path(), "CARD");
            configure(app, CARDS, &state, |_| {});
            request(
                app,
                json!({"cmd":"plan_set", "goal":"Board view", "ticket":{"id":"b1"}, "tasks":[
                    {"id":"a", "title":"Parser task", "ticket":{"id":"1"}},
                    {"id":"b", "title":"Docs task", "ticket":{"id":"2"}}]}),
            );
            let index = app.runtime.selected_plan;
            app.runtime.plan_view = true;
            app.runtime.panel = PanelMode::Plan;
            settle(app);
            set_field(&state, "CARD-1", "state", "Doing");
            app.runtime.ticket_heartbeat();
            settle(app);
            app.runtime.plans[index].tasks[1].status = TaskStatus::Blocked;
            app.runtime.plans[index].tasks[1].ticket.as_mut().unwrap().error =
                Some("setting the state to Stuck failed: card_move is broken today".into());
            board_frame(app, ctx, vec![]).drop_without_applying_deltas();
            let output = board_frame(app, ctx, vec![]);
            let shown = texts(&output);
            let labels: Vec<&str> = shown.iter().map(|(text, _)| text.as_str()).collect();
            for expected in [
                "cards board",
                "b1",
                "3 tickets · 1 without a task",
                "sync",
                "CARD-1 · Doing",
                "CARD-2 · Todo",
                "⚠ setting the state to Stuck failed: card_move is broken today",
            ] {
                assert!(labels.contains(&expected), "missing {expected:?} in {labels:?}");
            }
            // The card notices a ticket whose state means another status than the task's.
            assert_eq!(
                labels.iter().filter(|l| l.starts_with("ticket says")).collect::<Vec<_>>(),
                [&"ticket says running"]
            );
            output.drop_without_applying_deltas();
            app.runtime.plans[index].tasks[0].status = TaskStatus::Running;
            let output = board_frame(app, ctx, vec![]);
            let shown = texts(&output);
            assert!(!shown.iter().any(|(text, _)| text.starts_with("ticket says")));
            let chip = shown.iter().find(|(text, _)| text == "CARD-1 · Doing").unwrap().1;
            output.drop_without_applying_deltas();
            let button = |pressed| egui::Event::PointerButton {
                pos: chip,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            board_frame(app, ctx, vec![egui::Event::PointerMoved(chip), button(true)]).drop_without_applying_deltas();
            let released = board_frame(app, ctx, vec![button(false)]);
            let opened: Vec<String> = released
                .platform_output
                .commands
                .iter()
                .filter_map(|command| match command {
                    egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(opened, ["https://tickets.example/CARD-1"]);
            released.drop_without_applying_deltas();

            // The sync button reads the tickets again.
            let reads = count(&state, "card_list");
            let output = board_frame(app, ctx, vec![]);
            let sync = texts(&output).into_iter().find(|(text, _)| text == "sync").unwrap().1;
            output.drop_without_applying_deltas();
            for pressed in [true, false] {
                let event = egui::Event::PointerButton {
                    pos: sync,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                board_frame(app, ctx, vec![egui::Event::PointerMoved(sync), event]).drop_without_applying_deltas();
            }
            settle(app);
            assert_eq!(count(&state, "card_list"), reads + 1);
        });
    }

    #[test]
    fn workspace_workflow_wins_reloads_and_preserves_the_last_valid_settings() {
        with_app(
            "app::tickets::tests::workspace_workflow_wins_reloads_and_preserves_the_last_valid_settings",
            |app, _| {
                let file = Workspace::file("workflow-team").unwrap();
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, "projects = []\n[workflow]\ndone = \"pr_opened\"\n").unwrap();
                app.runtime.workspace = Some("workflow-team".into());
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.workflow().done, "pr_opened");
                let workspace = Workspace::load("workflow-team").unwrap();
                workspace.save().unwrap();
                assert_eq!(Workspace::load("workflow-team").unwrap().workflow().unwrap().unwrap().done, "pr_opened");
                std::fs::write(&file, "projects = []\n[workflow]\ndone = \"ci_passed\"\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.workflow().done, "ci_passed");
                std::fs::write(&file, "projects = []\n[workflow]\ndone = \"unknown\"\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.workflow().done, "ci_passed");
                std::fs::write(&file, "projects = []\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.workflow().done, "accepted");
                assert!(app.runtime.chief_prompt().contains("imacs_plan_stage"));

                // [isolation] merges over the main config's key by key, and a broken one keeps the last good.
                use imacs_core::isolation::IsolationMode;
                app.runtime.config.isolation.mode = IsolationMode::Sandbox;
                app.runtime.config.isolation.sandbox.memory = Some("8g".into());
                std::fs::write(&file, "projects = []\n[isolation]\nmode = \"bare\"\n[isolation.sandbox]\ncpus = 2\n")
                    .unwrap();
                app.runtime.apply_ticket_config();
                let effective = app.runtime.effective_config().isolation;
                assert_eq!(effective.mode, IsolationMode::Bare);
                assert_eq!((effective.sandbox.memory.as_deref(), effective.sandbox.cpus), (Some("8g"), Some(2)));
                std::fs::write(&file, "projects = []\n[isolation]\nmode = \"open\"\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.effective_isolation().mode, IsolationMode::Bare);
                std::fs::write(&file, "projects = []\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.effective_isolation(), app.runtime.config.isolation);
            },
        );
    }

    #[test]
    fn a_workspace_files_tickets_win_over_the_main_config_and_follow_edits() {
        with_app(
            "app::tickets::tests::a_workspace_files_tickets_win_over_the_main_config_and_follow_edits",
            |app, _| {
                app.runtime.config.tickets =
                    toml::from_str("system = \"main\"\nserver = { url = \"https://main.example/mcp\" }").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.ticket_config().unwrap().system, "main");
                let file = Workspace::file("team").unwrap();
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(
                    &file,
                    "projects = []\n[tickets]\nsystem = \"team\"\nserver = { command = [\"team-mcp\"] }\n",
                )
                .unwrap();
                app.runtime.workspace = Some("team".into());
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.ticket_config().unwrap().system, "team");
                std::fs::write(&file, "projects = []\n[tickets]\nsystem = \"\"\n").unwrap();
                app.runtime.apply_ticket_config();
                assert!(app.runtime.ticket_config().is_none(), "a workspace can turn tickets off");
                std::fs::write(&file, "projects = []\n").unwrap();
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.ticket_config().unwrap().system, "main");

                // An edit within the same modification time (fast saves, coarse clocks) still counts.
                let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
                std::fs::write(
                    &file,
                    "projects = []\n[tickets]\nsystem = \"same-second\"\nserver = { command = [\"x\"] }\n",
                )
                .unwrap();
                std::fs::File::options().write(true).open(&file).unwrap().set_modified(stamp).unwrap();
                assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
                app.runtime.apply_ticket_config();
                assert_eq!(app.runtime.ticket_config().unwrap().system, "same-second");
            },
        );
    }
}
