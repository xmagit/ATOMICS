//! The host boundary for the shared ticket reader.
use super::*;

impl State {
    pub(in crate::app) fn ticket_ui_info(&self) -> Option<atomics_core::ticket_ui::TicketsInfo> {
        use atomics_core::ticket_ui::TicketsInfo;
        let config = self.ticket_config()?;
        let cleaner = self.cleaner();
        let mut states = config.states.clone();
        for state in
            [&mut states.planned, &mut states.running, &mut states.review, &mut states.done, &mut states.blocked]
                .into_iter()
                .flatten()
        {
            *state = cleaner.field(state);
        }
        states.stages =
            states.stages.into_iter().map(|(name, state)| (cleaner.field(&name), cleaner.field(&state))).collect();
        Some(TicketsInfo {
            system: cleaner.field(&config.system),
            identity: self.ticket_identity_label().map(|label| cleaner.field(&label)),
            search: config.operation("search").is_some(),
            semantic_search: config.operation("semantic_search").is_some(),
            views: if config.has_views() {
                config.views.iter().map(|v| cleaner.field(&v.label)).collect()
            } else {
                Vec::new()
            },
            default_view: config.default_view(),
            filters: if config.has_views() {
                config.filters.iter().map(|f| cleaner.field(&f.label)).collect()
            } else {
                Vec::new()
            },
            mode: config.mode,
            states,
            replies: self.tickets.client_replies.clone(),
        })
    }

    pub(in crate::app) fn ticket_tree(&self, plan: &Plan) -> Option<atomics_core::ticket_ui::TicketTree> {
        self.ticket_config()?;
        Some(self.ticket_tree_with(plan, &self.cleaner()))
    }

    pub(in crate::app) fn ticket_tree_with(
        &self,
        plan: &Plan,
        cleaner: &ticket::Cleaner,
    ) -> atomics_core::ticket_ui::TicketTree {
        let clean = |ticket: &TicketLink| cleaner.link(ticket.clone());
        atomics_core::ticket_ui::TicketTree {
            plan: plan.id.clone(),
            ticket: plan.ticket.as_ref().map(clean),
            tasks: plan.tasks.iter().filter_map(|task| task.ticket.as_ref().map(clean)).collect(),
            unassigned: self
                .tickets
                .boards
                .get(&plan.id)
                .into_iter()
                .flatten()
                .filter(|link| {
                    !plan.tasks.iter().any(|task| task.ticket.as_ref().is_some_and(|own| own.same_ticket(link)))
                })
                .map(clean)
                .collect(),
        }
    }

    pub(in crate::app) fn ticket_ui_command(
        &mut self,
        request: String,
        action: atomics_core::ticket_ui::TicketAction,
    ) -> Result<(), String> {
        let result = self.run_ticket_ui_command(request.clone(), action, None);
        if let Err(error) = &result {
            self.ticket_ui_result(request, Err(error.clone()));
        }
        result
    }

    pub(in crate::app) fn private_ticket_lookup(
        &mut self,
        request: String,
        action: atomics_core::ticket_ui::TicketAction,
        reply: Sender<ControlOutcome>,
    ) {
        let batch = self.open_batch(reply, Answer::Read(None));
        if let Err(error) = self.run_ticket_ui_command(request, action, Some(batch)) {
            self.settle_job(Some(batch), Some(error));
        }
    }

    fn run_ticket_ui_command(
        &mut self,
        request: String,
        action: atomics_core::ticket_ui::TicketAction,
        batch: Option<u64>,
    ) -> Result<(), String> {
        use atomics_core::ticket_ui::TicketAction;
        if batch.is_some() && matches!(action, TicketAction::Plan { .. } | TicketAction::Link { .. }) {
            return Err("This connection may only look up tickets".into());
        }
        let config = self.ticket_config().ok_or_else(|| self.no_ticket_system())?;
        let op = match action {
            TicketAction::Search { query, keyword, view, filters, cursor } => {
                let query = query.trim().to_string();
                let name = if query.is_empty() {
                    "browse"
                } else if config.operation("semantic_search").is_some()
                    && (!keyword || config.operation("search").is_none())
                {
                    "semantic_search"
                } else {
                    "search"
                };
                let operation = config
                    .operation(name)
                    .ok_or_else(|| format!("No {name} operation is mapped in [tickets.operations]"))?;
                let mut extra = if query.is_empty() {
                    config.views.get(view).ok_or("No such ticket view")?.arguments.clone()
                } else {
                    serde_json::Map::new()
                };
                for index in filters {
                    let filter = config.filters.get(index).ok_or("No such ticket filter")?;
                    if config.filter_applies(filter, name) {
                        extra.extend(filter.arguments.clone());
                    }
                }
                if let Some(cursor) = cursor {
                    if operation.paging.cursor.is_empty() {
                        return Err("This ticket operation has no next page".into());
                    }
                    extra.insert(operation.paging.cursor.clone(), serde_json::Value::String(cursor));
                }
                TicketOp::Page { name, query: (!query.is_empty()).then_some(query), extra }
            }
            TicketAction::Read { ticket } => TicketOp::ReadCard(ticket),
            TicketAction::Children { ticket } => {
                if config.operation("children").is_none() {
                    self.finish_ticket_lookup(request, Ok(TicketOutput::Cards(Vec::new())), batch);
                    return Ok(());
                }
                TicketOp::Children(ticket)
            }
            TicketAction::Related { ticket } => {
                if config.operation("related").is_none() {
                    self.finish_ticket_lookup(request, Ok(TicketOutput::Cards(Vec::new())), batch);
                    return Ok(());
                }
                TicketOp::Related(ticket)
            }
            TicketAction::Plan { ticket, mode, extra } => {
                if let Some(error) = config.mismatch(&ticket) {
                    return Err(error);
                }
                let result = self.plan_ticket(ticket, mode, &extra)?;
                use super::super::tickets_panel::PlanStart;
                let mut reply = atomics_core::ticket_ui::TicketReply::new(request);
                match result {
                    PlanStart::Started(plan) | PlanStart::Existing(plan) => reply.plan = Some(plan),
                    PlanStart::SignIn => reply.error = Some(format!("Sign in to {} first", config.system)),
                }
                self.keep_ticket_ui_reply(reply);
                return Ok(());
            }
            TicketAction::Link { plan, task, ticket } => {
                let index = self.plans.iter().position(|p| p.id == plan && p.is_open()).ok_or("No such open plan")?;
                if let Some(task) = &task
                    && !self.plans[index].tasks.iter().any(|t| &t.id == task)
                {
                    return Err(format!("No task {task}"));
                }
                if let Some(error) = config.mismatch(&ticket) {
                    return Err(error);
                }
                let (tx, rx) = std::sync::mpsc::channel();
                self.link_ticket(index, task, Some(ticket), None, false, tx);
                if let Ok(ControlOutcome::Done(response)) = rx.try_recv()
                    && !response.ok
                {
                    return Err(response.error.unwrap_or_else(|| "Could not link ticket".into()));
                }
                // The link is persisted synchronously; the existing batch continues refreshing it.
                self.keep_ticket_ui_reply(atomics_core::ticket_ui::TicketReply::new(request));
                return Ok(());
            }
        };
        if self.tickets.pending >= 64 {
            return Err("Too many ticket requests; wait for the current reads".into());
        }
        let tag = Tag {
            key: (String::new(), None),
            ticket: String::new(),
            system: config.system.clone(),
            kind: Kind::Client(request, config.clone()),
            batch,
        };
        self.send_ticket(&config, op, tag);
        Ok(())
    }

    pub(super) fn ticket_ui_result(&mut self, request: String, result: Result<TicketOutput, String>) {
        self.finish_ticket_lookup(request, result, None);
    }

    pub(super) fn finish_ticket_lookup(
        &mut self,
        request: String,
        result: Result<TicketOutput, String>,
        batch: Option<u64>,
    ) {
        let mut reply = atomics_core::ticket_ui::TicketReply::new(request);
        match result {
            Ok(TicketOutput::Page(cards, next, fallback)) => {
                reply.cards = cards;
                reply.next = next;
                reply.fallback = fallback;
            }
            Ok(TicketOutput::Card(card)) => reply.cards = vec![*card],
            Ok(TicketOutput::Cards(cards)) => reply.cards = cards,
            Ok(_) => reply.error = Some("Unexpected ticket response".into()),
            Err(error) => reply.error = Some(self.scrub(&error)),
        }
        let cleaner = self.cleaner();
        reply.cards = reply.cards.into_iter().map(|card| cleaner.card(card)).collect();
        reply.fallback = reply.fallback.map(|why| cleaner.field(&why));

        // Leave room for the rest of HostInfo in every snapshot part and host_upsert frame.
        if serde_json::to_vec(&reply).map_or(usize::MAX, |bytes| bytes.len()) > 96 * 1024 {
            reply.cards.clear();
            reply.next = None;
            reply.error = Some(
                "This ticket response is too large to display. Narrow the search or open it in the browser.".into(),
            );
        }
        if let Some(id) = batch {
            if let Some(batch) = self.tickets.batches.get_mut(&id) {
                batch.answer = Answer::Read(Some(json!({ "ticket_reply": reply })));
            }
            self.settle_job(Some(id), None);
        } else {
            self.keep_ticket_ui_reply(reply);
        }
    }

    fn keep_ticket_ui_reply(&mut self, reply: atomics_core::ticket_ui::TicketReply) {
        let replies = &mut self.tickets.client_replies;
        replies.retain(|old| old.request != reply.request);
        replies.push(reply);
        while replies.len() > 32 || serde_json::to_vec(&replies).map_or(usize::MAX, |bytes| bytes.len()) > 144 * 1024 {
            replies.remove(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{session, with_state};
    use atomics_core::ticket_ui::TicketAction;
    use atomics_core::tickets::{Comment, TicketCard};

    fn configure(state: &mut State) {
        state.config.tickets = TicketConfig {
            system: "tracker".into(),
            server: atomics_core::tickets::McpServer { command: vec!["/bin/false".into()], ..Default::default() },
            ..Default::default()
        };
        state.apply_ticket_config();
    }

    #[test]
    fn client_tickets_bound_replies_and_ignore_old_workspace_failures() {
        with_state(
            "app::tickets::client::tests::client_tickets_bound_replies_and_ignore_old_workspace_failures",
            |mut state, _| {
                configure(&mut state);
                let card = TicketCard {
                    body: Some("text ".repeat(16000)),
                    comments: (0..80).map(|_| Comment { text: "comment ".repeat(400), ..Default::default() }).collect(),
                    ..Default::default()
                };
                state.ticket_ui_result("large".into(), Ok(TicketOutput::Card(Box::new(card))));
                let reply = &state.tickets.client_replies[0];
                assert!(reply.cards.is_empty() && reply.error.as_ref().unwrap().contains("too large"));
                for index in 0..50 {
                    state.ticket_ui_result(format!("request-{index}"), Ok(TicketOutput::Cards(vec![])));
                }
                assert_eq!(state.tickets.client_replies.len(), 32);
                assert!(serde_json::to_vec(&state.host_info()).unwrap().len() < atomics_core::wire::MAX_FRAME_BYTES);
                let old = state.ticket_config().unwrap();
                state.config.tickets.system = "another-tracker".into();
                state.apply_ticket_config();
                state.finish_ticket(
                    Tag {
                        key: (String::new(), None),
                        ticket: String::new(),
                        system: old.system.clone(),
                        kind: Kind::Client("old-workspace".into(), old),
                        batch: None,
                    },
                    Err(format!("{}: refused", crate::mcp_client::SIGN_IN_REQUIRED)),
                );
                assert!(state.tickets.client_replies.is_empty());
                assert!(!state.tickets.panel.signin, "an old workspace cannot ask the current one to sign in");
            },
        );
    }

    #[test]
    fn private_ticket_lookup_keeps_cards_off_the_board_and_refuses_writes() {
        with_state(
            "app::tickets::client::tests::private_ticket_lookup_keeps_cards_off_the_board_and_refuses_writes",
            |mut state, _| {
                configure(&mut state);
                let (tx, rx) = std::sync::mpsc::channel();
                state.private_ticket_lookup(
                    "children".into(),
                    TicketAction::Children { ticket: Default::default() },
                    tx,
                );
                let ControlOutcome::Done(response) = rx.try_recv().unwrap() else { panic!("reply") };
                assert!(response.ok, "{response:?}");
                assert_eq!(response.result["ticket_reply"]["request"], "children");
                assert!(state.tickets.client_replies.is_empty());
                let (tx, rx) = std::sync::mpsc::channel();
                state.private_ticket_lookup(
                    "link".into(),
                    TicketAction::Link { ticket: Default::default(), plan: "any".into(), task: None },
                    tx,
                );
                let ControlOutcome::Done(response) = rx.try_recv().unwrap() else { panic!("reply") };
                assert!(!response.ok);
                assert!(state.tickets.batches.is_empty());
            },
        );
    }

    #[test]
    fn client_tickets_plan_once_without_selecting_another_clients_plan_or_chief() {
        with_state(
            "app::tickets::client::tests::client_tickets_plan_once_without_selecting_another_clients_plan_or_chief",
            |mut state, _| {
                configure(&mut state);
                let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &state.launch_dir);
                chief.chief = true;
                state.sessions.push(chief);
                let hints = state.backend().view_hints();
                let chief_open = state.chief_open;
                let plans_before = state.plans.len();
                let action = TicketAction::Plan {
                    ticket: TicketLink {
                        id: "1".into(),
                        key: Some("ENG-123".into()),
                        title: Some("Parser work".into()),
                        ..Default::default()
                    },
                    mode: TicketMode::Ticket,
                    extra: "Keep the API stable".into(),
                };
                for request in ["first", "second"] {
                    state.ticket_ui_command(request.into(), action.clone()).unwrap();
                }
                assert_eq!(state.plans.len(), plans_before + 1);
                assert_eq!(state.backend().view_hints(), hints);
                assert_eq!(state.chief_open, chief_open);
                let replies = &state.tickets.client_replies;
                assert_eq!(replies[0].plan, replies[1].plan);
                assert!(replies[0].plan.is_some());
                let messages = &state.hub.lock().unwrap().outbox["chief"];
                assert_eq!(messages.len(), 1);
                assert!(messages[0].contains("Keep the API stable"));
            },
        );
    }
}
