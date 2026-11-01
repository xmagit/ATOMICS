//! Remote control from a ticket-linked conversation (docs/tickets.md, "Remote control"):
//! opening each open plan's conversation, polling it at the §3.7 cadence, and delivering
//! what verified steering humans write to the chief with recorded intent. The sidecar is
//! only ever written on the ticket worker; this side decides when, and does the pasting.
use super::*;
use crate::tickets::{RemoteOutcome, RemoteOutput, TicketOp, TicketOutput};
use atomics_core::tickets::{self as ticket, RemoteConfig, RemoteStatus, Steerer, SteeringItem, TicketConfig};

/// A chief turn or a steering message this recent keeps a plan on the fast cadence.
pub(super) const ACTIVE_WINDOW: Duration = Duration::from_secs(600);
/// How often the server's steering list is read again.
pub(super) const STEERERS_EVERY: Duration = Duration::from_secs(600);
/// The wait after a 429, a 5xx or a lost connection.
pub(super) const THROTTLED: Duration = Duration::from_secs(300);
/// The wait before opening again after a failed open.
const REOPEN_AFTER: Duration = Duration::from_secs(60);
const MAX_NOTICES: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteJob {
    Open,
    Steerers,
    Poll,
    Settle,
}

#[derive(Default)]
pub(super) struct Remote {
    pub(super) plans: HashMap<String, RemotePlan>,
    /// The chief's latest turn, from its UserPromptSubmit and Stop hooks.
    pub(super) chief_turn: Option<Instant>,
    /// Plans whose interrupted deliveries this process has already handled.
    recovered: HashSet<String>,
    /// Bumped when remote restarts, so results of older jobs are recognised.
    pub(super) epoch: u64,
    /// What was last said about remote's state, so each change is said once.
    shown: Option<RemoteStatus>,
}

impl Remote {
    pub(super) fn reset(&mut self) {
        self.plans.clear();
        self.epoch += 1;
        self.shown = None;
    }
}

#[derive(Default)]
pub(super) struct RemotePlan {
    pub(super) conversation: Option<String>,
    /// The steering list the conversation was opened with.
    owners: Vec<String>,
    /// The steering humans the server accepted.
    pub(super) steerers: Vec<Steerer>,
    opening: bool,
    open_after: Option<Instant>,
    polling: bool,
    pub(super) next_poll: Option<Instant>,
    steerers_at: Option<Instant>,
    pub(super) last_steering: Option<Instant>,
    /// The last approval scope requested on this connection.
    requested_digest: Option<String>,
    /// An approval request is open: keeps the fast cadence.
    pub(super) approval_open: bool,
    /// Items not sent to the chief, with the reason.
    pub(super) notices: Vec<String>,
    error: Option<String>,
}

impl RemotePlan {
    /// The configured list, narrowed to the humans the server accepted when it named any.
    pub(super) fn steering(&self) -> Vec<String> {
        if self.steerers.is_empty() {
            return self.owners.clone();
        }
        self.owners.iter().filter(|id| self.steerers.iter().any(|s| s.id == **id)).cloned().collect()
    }
}

/// §3.7: `poll_secs` while the plan is active, else `idle_poll_secs`.
pub(super) fn poll_interval(
    config: &RemoteConfig,
    now: Instant,
    last_activity: Option<Instant>,
    approval_open: bool,
) -> Duration {
    let active = approval_open || last_activity.is_some_and(|at| now.saturating_duration_since(at) < ACTIVE_WINDOW);
    Duration::from_secs(if active { config.poll_secs } else { config.idle_poll_secs }.max(1))
}

impl State {
    /// The sign-in chip: both identities, for example "Example agent (agent) · steering: Example human".
    pub(super) fn ticket_identity_label(&self) -> Option<String> {
        let whoami = self.tickets.whoami.as_ref()?;
        let mut names: Vec<String> = Vec::new();
        for plan in self.tickets.remote.plans.values() {
            for steerer in &plan.steerers {
                let name = if whoami.owner_id.as_deref() == Some(steerer.id.as_str()) {
                    format!("{} (owner)", steerer.name)
                } else {
                    steerer.name.clone()
                };
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        if names.is_empty() {
            return Some(whoami.label());
        }
        Some(format!("{} ({}) · steering: {}", whoami.name, whoami.kind, names.join(", ")))
    }

    /// Runs on the runtime tick: opens, re-reads steering lists and polls, each on the ticket worker.
    pub(super) fn remote_tick(&mut self) {
        let now = Instant::now();
        let Some(config) = self.ticket_config() else {
            self.tickets.remote.plans.clear();
            return;
        };
        let status = config.remote_status(self.tickets.whoami.as_ref());
        if config.remote.enabled && self.tickets.remote.shown.as_ref() != Some(&status) {
            self.tickets.remote.shown = Some(status.clone());
            match &status {
                RemoteStatus::Off(why) if self.tickets.whoami.is_some() => {
                    self.toast(format!("Remote control is off: {why}"))
                }
                RemoteStatus::Error(why) => self.toast(format!("Remote control refused: {why}")),
                _ => {}
            }
        }
        let RemoteStatus::On(owners) = status else {
            self.tickets.remote.plans.clear();
            return;
        };
        let Some(whoami) = self.tickets.whoami.clone() else { return };
        // Polling stops when a plan completes or is archived: it simply leaves this list.
        let followed: Vec<(String, ticket::TicketLink, String)> = self
            .plans
            .iter()
            .filter(|plan| plan.is_open())
            .filter_map(|plan| {
                let link = plan.ticket.clone().filter(|link| !link.id.is_empty() && config.accepts(link))?;
                Some((plan.id.clone(), link, plan.goal.clone()))
            })
            .collect();
        self.tickets.remote.plans.retain(|id, _| followed.iter().any(|(plan, ..)| plan == id));
        let home = paths::atomics_home();
        let epoch = self.tickets.remote.epoch;
        let mut jobs = Vec::new();
        for (plan, link, goal) in followed {
            let remote = &mut self.tickets.remote;
            let entry = remote.plans.entry(plan.clone()).or_default();
            if entry.opening {
                continue;
            }
            if entry.conversation.is_none() || entry.owners != owners {
                if entry.open_after.is_some_and(|at| now < at) && entry.owners == owners {
                    continue;
                }
                entry.opening = true;
                entry.owners = owners.clone();
                // Marked once an open succeeds, so a failed first open still recovers later.
                let recover = !remote.recovered.contains(&plan);
                let op = TicketOp::RemoteOpen {
                    home: home.clone(),
                    plan: plan.clone(),
                    link,
                    title: goal,
                    owners: owners.clone(),
                    recover,
                };
                jobs.push((plan, op, RemoteJob::Open));
                continue;
            }
            let conversation = entry.conversation.clone().expect("open");
            if entry.steerers_at.is_none_or(|at| now.saturating_duration_since(at) >= STEERERS_EVERY) {
                entry.steerers_at = Some(now);
                jobs.push((plan.clone(), TicketOp::RemoteSteerers(conversation), RemoteJob::Steerers));
            }
            if !entry.polling && entry.next_poll.is_none_or(|at| now >= at) {
                entry.polling = true;
                let op = TicketOp::RemotePoll {
                    home: home.clone(),
                    plan: plan.clone(),
                    whoami: Box::new(whoami.clone()),
                    owners: entry.steering(),
                };
                jobs.push((plan, op, RemoteJob::Poll));
            }
        }
        for (plan, op, job) in jobs {
            let tag = super::tickets::remote_tag(&plan, &config, job, epoch);
            self.send_ticket(&config, op, tag);
        }
        for index in 0..self.plans.len() {
            self.remote_request_approval(index, false);
        }
    }

    /// Every post goes through the worker's durable outbox, including approval replies.
    fn remote_post(&mut self, index: usize, kind: &str, body: &str, activity: Option<&str>, turn: &str) {
        let Some(config) = self.ticket_config() else { return };
        if !matches!(config.remote_status(self.tickets.whoami.as_ref()), RemoteStatus::On(_)) {
            return;
        }
        let plan = self.plans[index].id.clone();
        let Some(conversation) = self.tickets.remote.plans.get(&plan).and_then(|p| p.conversation.as_deref()) else {
            return;
        };
        let vars = ticket::Vars::new(&config.system)
            .with("conversation", Some(conversation))
            .with("kind", Some(kind))
            .with("body", Some(body))
            .with("activity", Some(activity.unwrap_or("")))
            .with("turn", Some(turn));
        let write =
            atomics_core::remote::PendingWrite::new("conversation_post", uuid::Uuid::new_v4().to_string(), vars);
        self.send_ticket(
            &config,
            TicketOp::Outbox { home: paths::atomics_home(), plan: plan.clone(), write: Some(Box::new(write)) },
            super::tickets::outbox_tag(&plan, &config),
        );
    }

    /// Called on plan_set, brief edits and opening/reopening remote. The digest also
    /// catches approval resets made before the conversation finished opening.
    pub(super) fn remote_request_approval(&mut self, index: usize, force: bool) {
        let plan = &self.plans[index];
        if !plan.is_open() || plan.approved {
            return;
        }
        let Some(entry) = self.tickets.remote.plans.get_mut(&plan.id) else { return };
        if entry.conversation.is_none() {
            return;
        }
        let digest = plan.digest();
        if !force && entry.requested_digest.as_ref() == Some(&digest) {
            return;
        }
        entry.requested_digest = Some(digest.clone());
        entry.approval_open = true;
        entry.next_poll = None;
        let turn = format!("approval:{}:{digest}", plan.id);
        let bodies: Vec<_> = plan
            .tasks
            .iter()
            .map(|task| {
                // The worker redacts before bounding this to the server's 2000 characters.
                format!(
                    "{} · {}\nagent {} · repo {} · args {}\n{}",
                    task.id,
                    task.title,
                    task.agent,
                    task.repo.as_deref().unwrap_or(""),
                    task.args.join(" "),
                    task.brief
                )
            })
            .collect();
        let approval =
            format!("Plan {} needs approval: {} tasks. Digest {digest}.\n/approve {digest}", plan.id, plan.tasks.len());
        let Some(config) = self.ticket_config() else { return };
        for body in bodies {
            self.remote_post(index, &config.remote.kinds.plan, &body, None, &turn);
        }
        self.remote_post(
            index,
            &config.remote.kinds.approval,
            &approval,
            Some(&config.remote.activity.approval),
            &turn,
        );
    }

    pub(super) fn remote_approved(&mut self, index: usize, author: Option<&str>) {
        if let Some(entry) = self.tickets.remote.plans.get_mut(&self.plans[index].id) {
            entry.approval_open = false;
            entry.requested_digest = None;
        }
        let Some(config) = self.ticket_config() else { return };
        let body = author
            .map(|name| format!("Approved by {name} from the conversation; the chief is dispatching"))
            .unwrap_or_else(|| "Approved in ATOMICS".into());
        // Working clears the outstanding approval activity, including for board and ctl approvals.
        self.remote_post(
            index,
            &config.remote.kinds.status,
            &body,
            Some(&config.remote.activity.working),
            &format!("approved:{}:{}", self.plans[index].id, self.plans[index].digest()),
        );
    }

    fn remote_approve(&mut self, index: usize, item: &SteeringItem, config: &TicketConfig) {
        let command = ticket::steering_command(&item.text).expect("approval command");
        let digest = self.plans[index].digest();
        let supplied = command.strip_prefix("/approve ").unwrap_or("").trim();
        let turn = format!("steering:{}:{}", self.plans[index].id, item.id);
        if supplied.len() != 12 || !supplied.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            self.remote_post(
                index,
                &config.remote.kinds.status,
                "Use /approve followed by the plan's 12 character digest.",
                None,
                &turn,
            );
        } else if supplied != digest {
            self.remote_post(
                index,
                &config.remote.kinds.status,
                &format!("This plan changed; the new digest is {digest}"),
                None,
                &turn,
            );
            self.remote_request_approval(index, true);
        } else if self.plans[index].approved {
            self.remote_post(index, &config.remote.kinds.status, "Already approved", None, &turn);
        } else {
            self.approve_plan_by(index, item.author.as_deref().or(item.author_id.as_deref()));
        }
    }

    pub(super) fn finish_remote(
        &mut self,
        plan: String,
        job: RemoteJob,
        epoch: u64,
        config: Arc<TicketConfig>,
        result: Result<TicketOutput, String>,
    ) {
        let now = Instant::now();
        let current = epoch == self.tickets.remote.epoch && self.tickets.remote.plans.contains_key(&plan);
        let result = match result {
            Ok(TicketOutput::Remote(output)) => Ok(output),
            Ok(_) => Err("unexpected ticket worker output".to_string()),
            Err(error) => Err(error),
        };
        match (job, result) {
            (RemoteJob::Open, result) => {
                if !current {
                    return;
                }
                let entry = self.tickets.remote.plans.get_mut(&plan).expect("current");
                entry.opening = false;
                match result {
                    Ok(RemoteOutput::Opened { conversation, messages }) => {
                        let refused: Vec<String> = if conversation.steerers.is_empty() {
                            Vec::new()
                        } else {
                            entry
                                .owners
                                .iter()
                                .filter(|id| !conversation.steerers.iter().any(|s| s.id == **id))
                                .cloned()
                                .collect()
                        };
                        if entry.conversation.as_ref() != Some(&conversation.id) {
                            entry.requested_digest = None;
                        }
                        entry.conversation = Some(conversation.id);
                        self.tickets.remote.recovered.insert(plan.clone());
                        let entry = self.tickets.remote.plans.get_mut(&plan).expect("current");
                        entry.steerers = conversation.steerers;
                        entry.steerers_at = None;
                        entry.next_poll = None;
                        entry.open_after = None;
                        if !refused.is_empty() {
                            self.toast(format!(
                                "The ticket system refused steering for {}; the conversation uses its list",
                                refused.join(", ")
                            ));
                        }
                        for message in messages {
                            self.toast(message);
                        }
                        if let Some(index) = self.plans.iter().position(|p| p.id == plan) {
                            self.remote_request_approval(index, false);
                        }
                        // Register the sidecar so its writes are retried.
                        let home = paths::atomics_home();
                        self.send_ticket(
                            &config,
                            TicketOp::Outbox { home, plan: plan.clone(), write: None },
                            super::tickets::outbox_tag(&plan, &config),
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        entry.open_after = Some(now + REOPEN_AFTER);
                        self.toast(format!("Remote conversation for {plan}: {error}"));
                    }
                }
            }
            (RemoteJob::Steerers, Ok(RemoteOutput::Steerers(steerers))) => {
                if current && !steerers.is_empty() {
                    self.tickets.remote.plans.get_mut(&plan).expect("current").steerers = steerers;
                }
            }
            (RemoteJob::Steerers, _) => {}
            (RemoteJob::Poll, result) => {
                let deliver = match result {
                    Ok(RemoteOutput::Polled { deliver, refused }) => {
                        if current {
                            for (item, reason) in refused {
                                // Rejected commands get a reply in this plan's conversation; they never
                                // enter the approval handler or the chief's delivery path.
                                if ticket::steering_command(&item.text).is_some()
                                    && self.tickets.remote.plans[&plan].conversation.as_deref()
                                        == Some(&item.conversation)
                                    && let Some(index) = self.plans.iter().position(|p| p.id == plan)
                                {
                                    self.remote_post(
                                        index,
                                        &config.remote.kinds.status,
                                        &format!("Approval refused: {reason}"),
                                        None,
                                        &format!("refused:{plan}:{}", item.id),
                                    );
                                }
                                let author = item.author.or(item.author_id).unwrap_or_else(|| "someone".into());
                                let notice = format!("Not sent to the chief: a message from {author}: {reason}");
                                let entry = self.tickets.remote.plans.get_mut(&plan).expect("current");
                                entry.notices.push(notice.clone());
                                let excess = entry.notices.len().saturating_sub(MAX_NOTICES);
                                entry.notices.drain(..excess);
                                self.toast(notice);
                            }
                        }
                        deliver
                    }
                    Ok(RemoteOutput::PollFailed { error, throttled }) => {
                        self.remote_poll_failed(&plan, current, error, throttled);
                        Vec::new()
                    }
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        self.remote_poll_failed(&plan, current, error, false);
                        Vec::new()
                    }
                };
                if current {
                    let chief_turn = self.tickets.remote.chief_turn;
                    let entry = self.tickets.remote.plans.get_mut(&plan).expect("current");
                    entry.polling = false;
                    if !deliver.is_empty() {
                        entry.last_steering = Some(now);
                    }
                    let interval =
                        poll_interval(&config.remote, now, entry.last_steering.max(chief_turn), entry.approval_open);
                    // A throttled read keeps its longer wait.
                    entry.next_poll = Some(entry.next_poll.filter(|at| *at > now + interval).unwrap_or(now + interval));
                }
                self.remote_deliver(&plan, &config, deliver, current);
            }
            (RemoteJob::Settle, Ok(RemoteOutput::Settled(messages))) => {
                for message in messages {
                    self.toast(message);
                }
            }
            (RemoteJob::Settle, Err(error)) => self.toast(format!("Remote conversation for {plan}: {error}")),
            (RemoteJob::Settle, Ok(_)) => {}
        }
    }

    fn remote_poll_failed(&mut self, plan: &str, current: bool, error: String, throttled: bool) {
        if !current {
            return;
        }
        let entry = self.tickets.remote.plans.get_mut(plan).expect("current");
        if throttled {
            entry.next_poll = Some(Instant::now() + THROTTLED);
        }
        if entry.error.as_ref() != Some(&error) {
            entry.error = Some(error.clone());
            self.toast(format!("Reading the remote conversation for {plan} failed: {error}"));
        }
    }

    /// Delivers each item in order; an item handed out after remote stopped is settled
    /// as never tried, which clears its recorded intent without pasting it.
    fn remote_deliver(&mut self, plan: &str, config: &Arc<TicketConfig>, items: Vec<SteeringItem>, current: bool) {
        let label = self.plans.iter().find(|p| p.id == plan).and_then(|p| p.ticket.clone());
        let mut unavailable = !current || label.is_none() || !self.plans.iter().any(|p| p.id == plan && p.is_open());
        let label = label.map(|link| link.label().to_string()).unwrap_or_default();
        for item in items {
            let outcome = if unavailable {
                RemoteOutcome::Deferred
            } else if ticket::steering_command(&item.text).is_some() {
                // Commands go to their handler, never to the chief (even if the chief is unavailable).
                let index = self.plans.iter().position(|p| p.id == plan).expect("current plan");
                self.remote_approve(index, &item, config);
                RemoteOutcome::Command
            } else {
                let text = ticket::steering_message(&item, &config.system, &label);
                match self.chief_delivery(&text) {
                    Ok(Delivery::Held) => RemoteOutcome::Held,
                    Ok(_) => RemoteOutcome::Delivered,
                    Err(_) => {
                        // Later items wait behind this one, in order.
                        unavailable = true;
                        RemoteOutcome::Unavailable
                    }
                }
            };
            let op = TicketOp::RemoteSettle {
                home: paths::atomics_home(),
                plan: plan.to_string(),
                item: Box::new(item),
                outcome,
            };
            let tag = super::tickets::remote_tag(plan, config, RemoteJob::Settle, self.tickets.remote.epoch);
            self.send_ticket(config, op, tag);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, with_app};
    use super::*;
    use crate::tickets::{DELIVERED_TEXT, HELD_TEXT, RESTARTED_TEXT};
    use atomics_core::remote::RemoteState;
    use serde_json::{Value, json};

    const TICKET: &str = "00000000-0000-4000-8000-000000000001";

    fn fake(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn set(path: &Path, edit: impl FnOnce(&mut Value)) {
        let mut state = fake(path);
        edit(&mut state);
        std::fs::write(path, state.to_string()).unwrap();
    }

    fn calls(path: &Path, tool: &str) -> usize {
        fake(path)["calls"].as_array().unwrap().iter().filter(|call| call["tool"] == tool).count()
    }

    fn steering(id: &str, revision: u64, author: &str, kind: &str, text: &str) -> Value {
        json!({"id":id, "revision":revision, "author_id":author, "author_kind":kind, "author":format!("{author} name"), "text":text})
    }

    /// Ticket jobs until none are left, then one remote tick, `rounds` times.
    fn settle(app: &mut Desktop<'_>, rounds: usize) {
        for _ in 0..rounds {
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
            loop {
                app.runtime.ticket_results();
                if app.runtime.tickets.pending == 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "ticket jobs did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
            app.runtime.remote_tick();
        }
        while app.runtime.tickets.pending > 0 {
            app.runtime.ticket_results();
            std::thread::sleep(Duration::from_millis(5));
        }
        // Outbox posts are uncounted because the worker also reports background retries.
        // A counted read after them is a FIFO barrier, so assertions observe their results.
        if let Some(config) = app.runtime.ticket_config()
            && let Some((plan, conversation)) =
                app.runtime.tickets.remote.plans.iter().find_map(|(id, p)| Some((id.clone(), p.conversation.clone()?)))
        {
            let tag = super::super::tickets::remote_tag(
                &plan,
                &config,
                RemoteJob::Steerers,
                app.runtime.tickets.remote.epoch,
            );
            app.runtime.send_ticket(&config, TicketOp::RemoteSteerers(conversation), tag);
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
            while app.runtime.tickets.pending > 0 {
                app.runtime.ticket_results();
                assert!(Instant::now() < deadline, "outbox barrier did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// A chief whose messages queue in the hub, a plan on the fake ticket, and remote on.
    fn remote_app(app: &mut Desktop<'_>, dir: &Path, edit: impl FnOnce(&mut Value)) -> PathBuf {
        let path = crate::mcp_client::tests::fake_state(dir, json!([]));
        set(&path, |state| {
            state["steering"] = json!([steering("before", 1, "human-1", "Human", "written before remote")]);
            edit(state);
        });
        let config = crate::tickets::tests::conversation_profile("", &path);
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir.clone());
        chief.chief = true;
        app.runtime.sessions = vec![chief];
        app.runtime.plans.push(Plan {
            id: "remote-plan".into(),
            goal: "Remote goal".into(),
            approved: true,
            ticket: Some(ticket::TicketLink {
                system: config.system.clone(),
                id: TICKET.into(),
                key: Some("DEMO-1".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
        settle(app, 3);
        assert_eq!(
            app.runtime.tickets.remote.plans.get("remote-plan").and_then(|p| p.conversation.as_deref()),
            Some("conversation-1"),
            "toast: {:?}; identity: {:?}; calls: {:?}",
            app.runtime.toast,
            app.runtime.tickets.whoami,
            fake(&path)
        );
        path
    }

    fn sent(app: &Desktop<'_>) -> Vec<String> {
        app.runtime.hub.lock().unwrap().outbox.get("chief").cloned().unwrap_or_default()
    }

    fn poll_now(app: &mut Desktop<'_>) {
        app.runtime.tickets.remote.plans.get_mut("remote-plan").unwrap().next_poll = None;
        settle(app, 2);
    }

    fn posts(path: &Path) -> Vec<String> {
        fake(path)["posts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["text"].as_str().map(str::to_string))
            .collect()
    }

    fn acks(path: &Path) -> Vec<Value> {
        fake(path)["acks"].as_array().into_iter().flatten().map(|a| a["ids"].clone()).collect()
    }

    fn approval_app(app: &mut Desktop<'_>, dir: &Path) -> (PathBuf, usize) {
        let path = remote_app(app, dir, |_| {});
        let index = app.runtime.plans.iter().position(|p| p.id == "remote-plan").unwrap();
        let response = super::super::tests::request(
            app,
            json!({
                "cmd":"plan_set", "plan":"remote-plan", "goal":"Approval goal",
                "tasks":[{"id":"t1", "title":"Implement", "brief":"Opening\nDetails", "agent":"codex",
                    "repo":"ATOMICS", "args":["--fast"], "acceptance":["passes"]}]
            }),
        );
        assert!(response.ok, "{:?}", response);
        settle(app, 2);
        (path, index)
    }

    fn approval_events(app: &Desktop<'_>) -> Vec<Value> {
        app.runtime.events.iter().filter(|event| event.change == Some("approved")).map(StatusEvent::to_json).collect()
    }

    #[test]
    fn approval_request_contract_and_scope_updates() {
        with_app("app::remote::tests::approval_request_contract_and_scope_updates", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let (path, index) = approval_app(app, dir.path());
            let check = |app: &Desktop<'_>, path: &Path| {
                let state = fake(path);
                let posts = state["posts"].as_array().unwrap();
                let request = posts.last().unwrap();
                let digest = app.runtime.plans[index].digest();
                assert_eq!(request["kind"], "approval");
                let text = request["text"].as_str().unwrap();
                let last = text.lines().last().unwrap();
                assert_eq!(last, format!("/approve {digest}"));
                assert_eq!(last.len(), 21);
                assert!(last[9..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
                assert_eq!(request["activity"], "approval");
                assert_eq!(request["turn"], format!("approval:remote-plan:{digest}"));
                let tasks = &app.runtime.plans[index].tasks;
                let task_posts = &posts[posts.len() - tasks.len() - 1..posts.len() - 1];
                for (post, task) in task_posts.iter().zip(tasks) {
                    assert_eq!(post["kind"], "plan");
                    let text = post["text"].as_str().unwrap();
                    assert!(text.starts_with(&format!("{} · {}\nagent {} · repo ", task.id, task.title, task.agent)));
                    assert!(text.chars().count() <= 2000);
                }
                assert!(app.runtime.tickets.remote.plans["remote-plan"].approval_open);
            };
            check(app, &path);
            assert!(posts(&path)[0].starts_with("t1 · Implement\nagent codex · repo ATOMICS · args --fast\nOpening"));
            let before = posts(&path).len();
            settle(app, 3);
            assert_eq!(posts(&path).len(), before, "one request for an unchanged scope");
            let old = app.runtime.plans[index].digest();
            app.runtime.edit_task_brief(index, "t1", "é".repeat(4000));
            settle(app, 2);
            assert_ne!(app.runtime.plans[index].digest(), old);
            check(app, &path);
            assert_eq!(posts(&path).len(), before * 2);
            app.runtime.approve_plan(index);
            settle(app, 1);
            let response = super::super::tests::request(
                app,
                json!({
                    "cmd":"plan_set", "plan":"remote-plan", "goal":"Changed after approval",
                    "tasks":[{"id":"t1", "title":"Implement", "brief":"changed", "agent":"codex"}]
                }),
            );
            assert!(response.ok);
            assert!(!app.runtime.plans[index].approved);
            settle(app, 2);
            check(app, &path);
        });
    }

    #[test]
    fn approval_from_steering_human_is_once_and_matches_board_event() {
        with_app("app::remote::tests::approval_from_steering_human_is_once_and_matches_board_event", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let (path, index) = approval_app(app, dir.path());
            let text = format!("/approve {}\nThanks", app.runtime.plans[index].digest());
            set(&path, |state| {
                state["steering"].as_array_mut().unwrap().extend([
                    steering("approve-2", 2, "human-1", "Human", &text),
                    steering("approve-3", 3, "human-1", "human", &text),
                ]);
            });
            // Approval commands work even when no chief can receive ordinary steering.
            app.runtime.sessions.clear();
            poll_now(app);
            assert!(app.runtime.plans[index].approved);
            let events = approval_events(app);
            assert_eq!(events.len(), 1);
            assert!(sent(app).is_empty());
            assert!(app.runtime.sessions.is_empty(), "approval does not start or dispatch an agent");
            assert!(
                app.runtime.plans[index].tasks.iter().all(|t| t.status == TaskStatus::Planned && t.session.is_none())
            );
            assert_eq!(acks(&path), [json!(["approve-2"]), json!(["approve-3"])]);
            assert!(
                posts(&path)
                    .iter()
                    .any(|p| p == "Approved by human-1 name from the conversation; the chief is dispatching")
            );
            assert_eq!(posts(&path).last().unwrap(), "Already approved");
            let state = fake(&path);
            let confirmed = state["posts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["text"].as_str().unwrap().starts_with("Approved by "))
                .unwrap();
            assert_eq!(confirmed["activity"], "Working");
            assert!(!app.runtime.tickets.remote.plans["remote-plan"].approval_open);
            poll_now(app);
            assert_eq!(approval_events(app).len(), 1);
            assert_eq!(acks(&path).len(), 2);
            // The exact same plan, approved through the board's method, emits the same event fields.
            app.runtime.events.clear();
            app.runtime.plans[index].approved = false;
            app.runtime.approve_plan(index);
            let board = approval_events(app);
            for key in ["session", "title", "status", "plan", "task", "change", "approval"] {
                assert_eq!(events[0][key], board[0][key], "{key}");
            }
        });
    }

    #[test]
    fn approval_stale_non_owner_agent_and_malformed_commands_reply_without_changes() {
        with_app(
            "app::remote::tests::approval_stale_non_owner_agent_and_malformed_commands_reply_without_changes",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let (path, index) = approval_app(app, dir.path());
                let before = app.runtime.plans[index].clone();
                let text = format!("/approve {}", before.digest());
                let stale = if before.digest() == "000000000000" { "111111111111" } else { "000000000000" };
                set(&path, |state| {
                    state["steering"].as_array_mut().unwrap().extend([
                        steering("reject-2", 2, "human-2", "Human", &text),
                        steering("reject-3", 3, "human-1", "Agent", &text),
                        steering("stale-4", 4, "human-1", "Human", &format!("/approve {stale}")),
                        steering("bad-5", 5, "human-1", "Human", "/approve"),
                    ]);
                });
                poll_now(app);
                assert_eq!(app.runtime.plans[index], before);
                assert!(approval_events(app).is_empty());
                assert!(sent(app).is_empty());
                let replies = posts(&path);
                assert!(
                    replies.iter().any(|p| p.contains("Approval refused:") && p.contains("not on the steering list"))
                );
                assert!(replies.iter().any(|p| p.contains("Approval refused:") && p.contains("not a human")));
                let stale_reply = replies
                    .iter()
                    .position(|p| p == &format!("This plan changed; the new digest is {}", before.digest()))
                    .unwrap();
                assert!(replies[stale_reply + 1].starts_with("t1 · Implement"));
                assert!(replies[stale_reply + before.tasks.len() + 1].ends_with(&text));
                assert!(replies.last().unwrap().starts_with("Use /approve"));
                assert_eq!(acks(&path), [json!(["stale-4"]), json!(["bad-5"])]);
                assert!(app.runtime.tickets.remote.plans["remote-plan"].approval_open);
                let count = replies.len();
                poll_now(app);
                assert_eq!(posts(&path).len(), count, "rejected and stale messages are settled once");
            },
        );
    }

    #[test]
    fn approval_board_and_ctl_clear_conversation_activity() {
        with_app("app::remote::tests::approval_board_and_ctl_clear_conversation_activity", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let (path, index) = approval_app(app, dir.path());
            for ctl in [false, true] {
                app.runtime.plans[index].approved = false;
                app.runtime.remote_request_approval(index, true);
                settle(app, 1);
                if ctl {
                    assert!(super::super::tests::request(app, json!({"cmd":"plan_approve", "plan":"remote-plan"})).ok);
                } else {
                    app.runtime.approve_plan(index);
                }
                settle(app, 1);
                let state = fake(&path);
                let last = state["posts"].as_array().unwrap().last().unwrap();
                assert_eq!(last["text"], "Approved in ATOMICS");
                assert_eq!(last["activity"], "Working");
                assert!(!app.runtime.tickets.remote.plans["remote-plan"].approval_open);
                let count = posts(&path).len();
                app.runtime.approve_plan(index);
                settle(app, 1);
                assert_eq!(posts(&path).len(), count, "repeated local approval posts nothing");
            }
        });
    }

    #[test]
    fn remote_steering_human_reaches_the_chief_with_its_label_and_is_acknowledged_after() {
        with_app(
            "app::remote::tests::remote_steering_human_reaches_the_chief_with_its_label_and_is_acknowledged_after",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let path = remote_app(app, dir.path(), |_| {});
                assert!(app.runtime.ticket_identity_label().unwrap().contains("steering: Example human 1 (owner)"));
                assert_eq!(fake(&path)["opens"][0]["owners"], json!(["human-1"]), "the owner is the default list");
                assert!(sent(app).is_empty(), "nothing from before the opening cursor");
                set(&path, |state| {
                    state["steering"].as_array_mut().unwrap().push(steering("s-2", 2, "human-1", "Human", "status?"));
                });
                poll_now(app);
                assert_eq!(
                    sent(app),
                    [
                        "[From human-1 name, a verified steering human, via the meaning-example conversation on DEMO-1, message 2.]\nstatus?"
                    ]
                );
                assert_eq!(acks(&path), [json!(["s-2"])]);
                assert_eq!(posts(&path), [DELIVERED_TEXT]);
                let calls: Vec<String> = fake(&path)["calls"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["tool"].as_str().unwrap().to_string())
                    .collect();
                let read = calls.iter().rposition(|t| t == "conversation_read").unwrap();
                assert!(
                    calls.iter().rposition(|t| t == "conversation_ack").unwrap() > read,
                    "acknowledged after the read that delivered it"
                );
                let state = RemoteState::load(&paths::atomics_home(), "remote-plan").unwrap();
                assert!(state.delivering.is_empty() && state.seen.contains(&"s-2".to_string()));
                poll_now(app);
                assert_eq!(sent(app).len(), 1, "never delivered twice");
            },
        );
    }

    #[test]
    fn remote_refuses_non_owners_agents_itself_and_old_items_and_accepts_both_human_spellings() {
        with_app(
            "app::remote::tests::remote_refuses_non_owners_agents_itself_and_old_items_and_accepts_both_human_spellings",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let path = remote_app(app, dir.path(), |_| {});
                set(&path, |state| {
                    state["steering"].as_array_mut().unwrap().extend([
                        steering("s-2", 2, "human-2", "Human", "status?"),
                        // A personal agent whose owner is on the list, and an agent using the owner's id.
                        steering("s-3", 3, "agent-7", "Agent", "status?"),
                        steering("s-4", 4, "human-1", "Agent", "status?"),
                        steering("s-5", 5, "agent-1", "Human", "status?"),
                        steering("s-6", 6, "human-1", "Human", "first"),
                        steering("s-7", 7, "human-1", "human", "second"),
                    ]);
                });
                poll_now(app);
                let delivered = sent(app);
                assert_eq!(delivered.len(), 2, "{delivered:?}");
                assert!(delivered[0].ends_with("\nfirst") && delivered[1].ends_with("\nsecond"));
                assert!(
                    delivered
                        .iter()
                        .all(|text| text.starts_with("[From human-1 name, a verified steering human, via "))
                );
                assert_eq!(acks(&path), [json!(["s-6"]), json!(["s-7"])]);
                let notices = &app.runtime.tickets.remote.plans["remote-plan"].notices;
                assert_eq!(notices.len(), 4, "{notices:?}");
                assert!(notices.iter().all(|n| n.starts_with("Not sent to the chief")));
                assert!(notices[0].contains("not on the steering list"));
                assert!(notices[1].contains("not a human") && notices[2].contains("not a human"));
                assert!(notices[3].contains("own identity"));
            },
        );
    }

    #[test]
    fn remote_strips_escapes_and_holds_during_a_permission_prompt() {
        with_app("app::remote::tests::remote_strips_escapes_and_holds_during_a_permission_prompt", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            let path = remote_app(app, dir.path(), |_| {});
            set(&path, |state| {
                state["steering"].as_array_mut().unwrap().push(steering(
                    "s-2",
                    2,
                    "human-1",
                    "Human",
                    "\u{1b}[31mred\u{1b}[0m\u{7}",
                ));
            });
            poll_now(app);
            let delivered = sent(app);
            assert!(delivered[0].ends_with("\nred") && !delivered[0].contains('\u{1b}'), "{delivered:?}");
            app.runtime.sessions[0].approval_hold = true;
            set(&path, |state| {
                state["steering"].as_array_mut().unwrap().push(steering("s-3", 3, "human-1", "Human", "while asking"));
            });
            poll_now(app);
            assert_eq!(sent(app).len(), 1, "nothing is typed into a pending prompt");
            assert_eq!(app.runtime.sessions[0].held.len(), 1);
            assert!(app.runtime.sessions[0].held[0].ends_with("\nwhile asking"));
            assert_eq!(acks(&path), [json!(["s-2"]), json!(["s-3"])]);
            assert_eq!(posts(&path), [DELIVERED_TEXT, HELD_TEXT]);
        });
    }

    #[test]
    fn remote_restart_after_recorded_intent_never_pastes_again() {
        with_app("app::remote::tests::remote_restart_after_recorded_intent_never_pastes_again", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            // The previous process recorded intent for s-2 and crashed before acknowledging it.
            let crashed = RemoteState {
                conversation: Some("conversation-1".into()),
                cursor: Some(json!(1)),
                delivering: std::collections::BTreeMap::from([("s-2".to_string(), 1)]),
                ..Default::default()
            };
            crashed.save(&paths::atomics_home(), "remote-plan").unwrap();
            let path = remote_app(app, dir.path(), |state| {
                state["steering"].as_array_mut().unwrap().push(steering("s-2", 2, "human-1", "Human", "status?"));
            });
            poll_now(app);
            assert!(sent(app).is_empty(), "{:?}", sent(app));
            assert_eq!(acks(&path), [json!(["s-2"])]);
            assert_eq!(posts(&path), [RESTARTED_TEXT]);
            let state = RemoteState::load(&paths::atomics_home(), "remote-plan").unwrap();
            assert!(state.delivering.is_empty() && state.seen.contains(&"s-2".to_string()));
            // Reopening in the same process (a config reload) is not a restart.
            app.runtime.config.tickets.workflow.push_str(" Reloaded.");
            app.runtime.apply_ticket_config();
            settle(app, 3);
            assert_eq!(posts(&path), [RESTARTED_TEXT]);
        });
    }

    #[test]
    fn remote_polling_slows_after_ten_idle_minutes_and_stops_when_the_plan_completes() {
        let config = RemoteConfig::default();
        let now = Instant::now() + Duration::from_secs(3600);
        assert_eq!(poll_interval(&config, now, None, false), Duration::from_secs(60));
        assert_eq!(poll_interval(&config, now, Some(now - Duration::from_secs(9 * 60)), false), Duration::from_secs(5));
        assert_eq!(
            poll_interval(&config, now, Some(now - Duration::from_secs(10 * 60)), false),
            Duration::from_secs(60)
        );
        assert_eq!(poll_interval(&config, now, None, true), Duration::from_secs(5));
        with_app(
            "app::remote::tests::remote_polling_slows_after_ten_idle_minutes_and_stops_when_the_plan_completes",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                let path = remote_app(app, dir.path(), |_| {});
                let wait = |app: &Desktop<'_>| {
                    app.runtime.tickets.remote.plans["remote-plan"]
                        .next_poll
                        .unwrap()
                        .saturating_duration_since(Instant::now())
                };
                assert!(wait(app) > Duration::from_secs(50), "idle from the start: {:?}", wait(app));
                app.runtime.tickets.remote.chief_turn = Some(Instant::now());
                poll_now(app);
                assert!(wait(app) <= Duration::from_secs(5), "a chief turn makes the plan active");
                let eleven = Instant::now().checked_sub(Duration::from_secs(11 * 60));
                app.runtime.tickets.remote.chief_turn = eleven;
                app.runtime.tickets.remote.plans.get_mut("remote-plan").unwrap().last_steering = eleven;
                poll_now(app);
                assert!(wait(app) > Duration::from_secs(50), "{:?}", wait(app));
                let reads = calls(&path, "conversation_read");
                app.runtime.plans.iter_mut().find(|p| p.id == "remote-plan").unwrap().status = PlanStatus::Completed;
                settle(app, 1);
                assert!(!app.runtime.tickets.remote.plans.contains_key("remote-plan"));
                for plan in app.runtime.tickets.remote.plans.values_mut() {
                    plan.next_poll = None;
                }
                settle(app, 2);
                assert_eq!(calls(&path, "conversation_read"), reads, "polling stopped");
            },
        );
    }
}
