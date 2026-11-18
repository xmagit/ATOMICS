//! `atomics://open` links, once parsed (see `crate::open_link`): they focus the window and show
//! a plan, or the plan for a ticket, or draft one for the chief to read. They never do more.

use super::*;
use crate::open_link::{Incoming, OpenLink};
use crate::tickets::{TicketOp, TicketOutput};
use atomics_core::tickets::{TicketLink, is_plain_key};

/// How long a link waits for the ticket state before it is dropped with a toast.
const LINK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl State {
    /// Handles a link, toasting why when it cannot be opened.
    pub(super) fn open_link(&mut self, incoming: Incoming) -> Result<(), String> {
        self.window.raise();
        let result =
            incoming.map_err(|reason| format!("Ignored a link: {reason}")).and_then(|link| self.show_link(link));
        if let Err(message) = &result {
            self.toast(message.clone());
        }
        result
    }

    fn show_link(&mut self, link: OpenLink) -> Result<(), String> {
        if let Some(id) = link.plan {
            let Ok(index) = plan::resolve_plan(&self.plans, Some(&id)) else {
                return Err(format!("No open plan {}", id.chars().take(40).collect::<String>()));
            };
            self.show_plan_at(index);
            return Ok(());
        }
        let Some(reference) = link.ticket else { return Ok(()) };
        let Some(config) = self.ticket_config() else { return Err(self.no_ticket_system()) };
        let system = config.system.trim();
        if let Some(wanted) = link.system.as_deref().filter(|wanted| !wanted.eq_ignore_ascii_case(system)) {
            let wanted: String = wanted.chars().take(40).collect();
            return Err(format!("Sign in to {wanted} to open this link; ATOMICS is set up for {system}"));
        }
        if self.plan_needs_sign_in(&config) {
            return Err(format!("Sign in to {system} first"));
        }
        // What the link names is only trusted once the read confirms it: a bare key, or the key or
        // id an address held where the profile's template has one.
        let ticket = if reference.contains("://") {
            config.link_from_url(&reference).ok_or_else(|| format!("That address is not a {system} ticket"))?
        } else if !is_plain_key(&reference) {
            return Err(format!("That is not a {system} ticket key"));
        } else {
            TicketLink {
                system: config.system.clone(),
                id: reference.clone(),
                key: Some(reference),
                ..Default::default()
            }
        };
        let confirm = ticket.key.clone().filter(|key| !key.is_empty()).unwrap_or_else(|| ticket.id.clone());
        let linked = self.plans.iter().position(|plan| {
            let names = |t: &TicketLink| {
                t.same_ticket(&ticket) || ticket.url.as_deref().is_some_and(|url| t.url.as_deref() == Some(url))
            };
            plan.is_open()
                && (plan.ticket.as_ref().is_some_and(names)
                    || plan.tasks.iter().any(|task| task.ticket.as_ref().is_some_and(names)))
        });
        if let Some(index) = linked {
            self.show_plan_at(index);
            return Ok(());
        }
        // Nothing plans it yet: read it for its title, then offer a plan as an unsent draft.
        let panel = &mut self.tickets.panel;
        panel.seq += 1;
        let seq = panel.seq;
        panel.open_link = Some((seq, Some(confirm)));
        let tag = self.panel_tag(&config, super::tickets_panel::Job::Link, seq);
        self.send_ticket(&config, TicketOp::ReadCard(ticket), tag);
        Ok(())
    }

    /// Keeps a link until the ticket state is known; a newer one replaces an older.
    pub(super) fn hold_link(&mut self, incoming: Incoming) {
        self.pending_link = Some(incoming);
        self.pending_link_since = None;
    }

    /// Opens the held link once the ticket state is known, or gives up on it with a toast after
    /// `LINK_WAIT`, when the state never comes (a sign-in check that hangs). Whether anything changed.
    pub(super) fn release_pending_link(&mut self) -> bool {
        if self.pending_link.is_none() {
            self.pending_link_since = None;
            return false;
        }
        let since = *self.pending_link_since.get_or_insert_with(Instant::now);
        if self.ticket_state_known() {
            self.pending_link_since = None;
            if let Some(link) = self.pending_link.take() {
                let _ = self.open_link(link);
            }
        } else if since.elapsed() >= LINK_WAIT {
            self.pending_link = None;
            self.pending_link_since = None;
            self.toast("Could not open the link: the ticket system did not answer in time");
        } else {
            return false;
        }
        true
    }

    /// Whether the ticket config is applied and, for a server that signs in, its sign-in state read.
    /// A link waits for both: before them, "not configured" and "sign in" would be guesses.
    pub(super) fn ticket_state_known(&self) -> bool {
        self.tickets.workspace_file.is_some() && (self.sign_server().is_none() || self.tickets.sign.state().is_some())
    }

    /// A link to a plan opens its tab with its board beside the agents.
    fn show_plan_at(&mut self, index: usize) {
        self.selected_plan = index;
        self.show_plan_fleet();
        self.panel = PanelMode::Plan;
        self.diff_open = true;
    }

    /// The answer to the read a link started: the draft "Plan KEY: title" for the chief.
    pub(super) fn link_card_read(&mut self, result: Result<TicketOutput, (bool, String)>, confirm: Option<String>) {
        match result {
            Ok(TicketOutput::Card(card))
                if confirm.is_some_and(|wanted| {
                    let names = |text: &str| text.eq_ignore_ascii_case(&wanted);
                    !(names(&card.link.id) || card.link.key.as_deref().is_some_and(names))
                }) =>
            {
                self.toast("No such ticket");
            }
            Ok(TicketOutput::Card(card)) => {
                let cleaner = self.cleaner();
                let key = cleaner.field(card.link.label());
                let title = cleaner.field(card.link.title.as_deref().unwrap_or_default());
                let line = |text: String| text.split_whitespace().collect::<Vec<_>>().join(" ");
                let draft = match line(title).as_str() {
                    "" => format!("Plan {}", line(key)),
                    title => format!("Plan {}: {title}", line(key)),
                };
                self.draft_for_chief(draft);
            }
            Ok(_) => self.toast("Could not read that ticket"),
            Err((true, _)) => {
                let system = self.ticket_config().map(|config| config.system.clone()).unwrap_or_default();
                self.toast(format!("Sign in to {system} first"));
            }
            Err((false, error)) => self.toast(format!("Could not read that ticket: {error}")),
        }
    }

    /// Opens the chief with `text` typed into its prompt and not sent: the user reads it and presses Enter.
    fn draft_for_chief(&mut self, text: String) {
        if !self.sessions.iter().any(|s| s.chief && s.status != AgentStatus::Exited)
            && let Err(error) = self.start_chief(None)
        {
            self.toast(error);
            return;
        }
        self.chief_draft = Some(text);
        self.show_chief();
        self.flush_chief_draft();
    }

    /// Types the draft into the chief's prompt once the chief is up to read it.
    pub(super) fn flush_chief_draft(&mut self) {
        let Some(text) = &self.chief_draft else { return };
        let ready = self.sessions.iter().find(|s| s.chief && s.status != AgentStatus::Starting);
        if let Some(terminal) = ready.and_then(|s| s.terminal.as_ref()).filter(|t| !t.exited()) {
            terminal.draft(text);
            self.chief_draft = None;
        }
    }
}
