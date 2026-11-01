//! The Tickets panel: Find (a query, results with relative score bars, state chips,
//! assignee and snippet) and Read (one ticket as markdown sections, comments, children,
//! related tickets and links). It sits in the diff's slot and uses the diff's keyboard
//! model (`panel_nav`): Shift+↓/↑ step results or sections, PageUp/PageDown/Home/End
//! scroll, Enter opens, Alt+← goes back. Reads run on the ticket worker as panel jobs.
use super::*;
use crate::markdown::{self, Block, Inline};
use crate::panel_nav;
use crate::tickets::{TicketOp, TicketOutput};
use atomics_core::tickets::{TicketCard, TicketConfig, TicketLink, TicketMode};
use egui::text::{LayoutJob, TextWrapping};

const ROW_H: f32 = 58.0;
const ECHO_H: f32 = 22.0;
const LINE_H: f32 = 18.0;
const RECENT: usize = 5;
const COMMENT_LINES: usize = 12;

/// Which panel read an answer belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Job {
    Find,
    Read,
    Children,
    Related,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TicketsView {
    #[default]
    Find,
    Read,
}

/// A list the reading view fills in as its job answers.
#[derive(Clone, Debug)]
pub(super) enum Part {
    /// The profile maps no operation for it.
    Unmapped,
    Loading,
    Loaded(Vec<TicketCard>),
    Failed(String),
}

impl Part {
    fn items(&self) -> &[TicketCard] {
        match self {
            Part::Loaded(items) => items,
            _ => &[],
        }
    }
}

#[derive(Default)]
pub(super) struct Find {
    pub results: Vec<TicketCard>,
    pub selected: Option<usize>,
    pub busy: bool,
    pub error: Option<String>,
    /// The query that made the results, and whether it searched by meaning.
    pub ran: Option<(String, bool)>,
    /// The chief ran the search; the user's own query box is untouched.
    pub from_chief: bool,
    seq: u64,
    reveal: bool,
    /// Scroll position as of the last frame.
    pub offset: f32,
}

pub(super) struct Read {
    pub link: TicketLink,
    pub card: TicketCard,
    pub loading: bool,
    pub error: Option<String>,
    pub children: Part,
    pub related: Part,
    /// The description split at its headings.
    pub parts: Vec<(String, String)>,
    pub selected: Option<usize>,
    pub expanded: HashSet<usize>,
    seq: u64,
    jump: bool,
    /// Scroll position and document height as of the last frame.
    pub offset: f32,
    content_h: f32,
}

/// The places PageUp/PageDown stop at in a ticket, in reading order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Stop {
    Description(usize),
    Comment(usize),
    Child(usize),
    Related(usize),
}

impl Read {
    fn new(link: TicketLink, seed: Option<TicketCard>, seq: u64, config: &TicketConfig) -> Read {
        let mut card = seed.unwrap_or_default();
        card.link = link.clone();
        let part = |op: &str| if config.operation(op).is_some() { Part::Loading } else { Part::Unmapped };
        let mut read = Read {
            link,
            card,
            loading: true,
            error: None,
            children: part("children"),
            related: part("related"),
            parts: Vec::new(),
            selected: None,
            expanded: HashSet::new(),
            seq,
            jump: false,
            offset: 0.0,
            content_h: 0.0,
        };
        read.parts = read.card.body.as_deref().map(markdown::sections).unwrap_or_default();
        read
    }

    pub(super) fn stops(&self) -> Vec<Stop> {
        (0..self.parts.len())
            .map(Stop::Description)
            .chain((0..self.card.comments.len()).map(Stop::Comment))
            .chain((0..self.children.items().len()).map(Stop::Child))
            .chain((0..self.related.items().len()).map(Stop::Related))
            .collect()
    }

    fn stop(&self) -> Option<Stop> {
        self.selected.and_then(|i| self.stops().get(i).copied())
    }
}

#[derive(Default)]
pub(super) struct TicketsPanel {
    pub view: TicketsView,
    pub query: String,
    /// Search by keyword rather than by meaning when both are mapped.
    pub keyword: bool,
    pub find: Find,
    pub read: Option<Read>,
    /// Tickets opened from within a ticket, for Alt+←.
    pub crumbs: Vec<TicketLink>,
    /// The last tickets read, newest first.
    pub recent: Vec<TicketLink>,
    /// The ticket system said it needs a sign-in.
    pub signin: bool,
    pub focus_query: bool,
    /// The Plan with chief dialog, while it is open.
    pub dialog: Option<PlanDialog>,
    /// Plans whose Plan with chief message the chief has received.
    pub planning: HashSet<String>,
    seq: u64,
}

/// The Plan with chief dialog: the ticket, whether to plan it alone, and the user's own words.
pub(super) struct PlanDialog {
    pub link: TicketLink,
    pub ticket_only: bool,
    pub extra: String,
    pub error: Option<String>,
    focused: bool,
}

/// What pressing Plan with chief did.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum PlanStart {
    /// A new plan was made and the chief has the message.
    Started(String),
    /// The ticket already has an open plan; nothing new was made.
    Existing(String),
    /// The ticket system needs a sign-in first; nothing was made or sent.
    SignIn,
}

/// The mode a plan for this ticket gets: forced by the profile, else decided by the children
/// (auto). `None` while the children are still being read.
pub(super) fn plan_mode(config: &TicketConfig, children: &Part, ticket_only: bool) -> Option<TicketMode> {
    if ticket_only {
        return Some(TicketMode::Ticket);
    }
    match (config.mode, children) {
        (TicketMode::Auto, Part::Loading) => None,
        (TicketMode::Auto, Part::Loaded(items)) if !items.is_empty() => Some(TicketMode::Board),
        (TicketMode::Auto, _) => Some(TicketMode::Ticket),
        (mode, _) => Some(mode),
    }
}

/// How opening a ticket changes the breadcrumbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Nav {
    /// From the results or the sidebar: a new trail.
    Fresh,
    /// From a child or related ticket: the current one joins the trail.
    Push,
    /// Back along the trail.
    Pop,
}

pub(super) enum Act {
    Search,
    Open(Box<TicketLink>, Nav),
    Back,
    Browser(String),
    FindView,
    Plan,
}

impl TicketsPanel {
    /// Shift+↓/↑ in Find: the next or previous result.
    pub(super) fn step_result(&mut self, delta: i32) {
        self.find.selected = panel_nav::step_index(self.find.selected, self.find.results.len(), delta);
        self.find.reveal = true;
    }

    /// PageDown/PageUp in Read: the next or previous section.
    fn step_section(&mut self, delta: i32) {
        if let Some(read) = self.read.as_mut() {
            read.selected = panel_nav::step_index(read.selected, read.stops().len(), delta);
            read.jump = true;
        }
    }

    /// The ticket Shift+↓/↑ reads next in Read: the following result when the trail is
    /// just the results, else (↑ only) the ticket before this one on the breadcrumbs.
    fn neighbour(&self, delta: i32) -> Option<(TicketLink, Nav)> {
        let read = self.read.as_ref()?;
        if !self.crumbs.is_empty() {
            return (delta < 0).then(|| (self.crumbs.last().cloned().expect("not empty"), Nav::Pop));
        }
        let at = self.find.results.iter().position(|card| card.link.same_ticket(&read.link))?;
        let next = (at as i32 + delta).clamp(0, self.find.results.len() as i32 - 1) as usize;
        (next != at).then(|| (self.find.results[next].link.clone(), Nav::Fresh))
    }

    /// A navigation key that moves the selection or opens it. Keys it does not use are left to scrolling.
    fn key(&mut self, key: egui::Key) -> Option<Act> {
        use egui::Key;
        match self.view {
            TicketsView::Find => {
                let len = self.find.results.len();
                let selected = &mut self.find.selected;
                match key {
                    Key::ArrowDown => *selected = panel_nav::step_index(*selected, len, 1),
                    Key::ArrowUp => *selected = panel_nav::step_index(*selected, len, -1).map(|i| i.min(len)),
                    Key::Home => *selected = (len > 0).then_some(0),
                    Key::End => *selected = len.checked_sub(1),
                    Key::Enter => {
                        let card = selected.and_then(|i| self.find.results.get(i))?;
                        return Some(Act::Open(Box::new(card.link.clone()), Nav::Fresh));
                    }
                    _ => return None,
                }
                self.find.reveal = true;
                None
            }
            TicketsView::Read => {
                let read = self.read.as_mut()?;
                if key != Key::Enter {
                    return None;
                }
                match read.stop()? {
                    Stop::Child(i) => {
                        read.children.items().get(i).map(|c| Act::Open(Box::new(c.link.clone()), Nav::Push))
                    }
                    Stop::Related(i) => {
                        read.related.items().get(i).map(|c| Act::Open(Box::new(c.link.clone()), Nav::Push))
                    }
                    Stop::Comment(i) => {
                        if !read.expanded.remove(&i) {
                            read.expanded.insert(i);
                        }
                        None
                    }
                    Stop::Description(_) => None,
                }
            }
        }
    }

    /// Where the card of `link` is known from, so a read starts with what the lists showed.
    fn known(&self, link: &TicketLink) -> Option<TicketCard> {
        let read = self.read.as_ref();
        self.find
            .results
            .iter()
            .chain(read.into_iter().flat_map(|r| r.children.items().iter().chain(r.related.items())))
            .find(|card| card.link.same_ticket(link))
            .cloned()
    }
}

/// The query box, whose focus the shortcuts leave alone.
pub(super) fn query_id() -> egui::Id {
    egui::Id::NULL.with("tickets-query")
}

/// How full a score's bar is, relative to the lowest and highest score of the set. One
/// result, or equal scores, fill the bar. The lowest keeps a visible stub.
pub(super) fn relative_score(score: f64, low: f64, high: f64) -> f32 {
    if high <= low { 1.0 } else { (0.1 + 0.9 * (score - low) / (high - low)).clamp(0.0, 1.0) as f32 }
}

pub(super) fn score_range(cards: &[TicketCard]) -> Option<(f64, f64)> {
    let mut scores = cards.iter().filter_map(|c| c.score).filter(|s| s.is_finite());
    let first = scores.next()?;
    Some(scores.fold((first, first), |(low, high), s| (low.min(s), high.max(s))))
}

impl State {
    pub(super) fn toggle_tickets(&mut self) {
        self.ctx.request_repaint();
        if self.panel == PanelMode::Tickets && self.diff_open {
            self.panel = PanelMode::Diff;
        } else {
            self.panel = PanelMode::Tickets;
            self.diff_open = true;
            self.tickets.panel.focus_query = self.tickets.panel.view == TicketsView::Find;
        }
    }

    /// Shows the panel on Find with the query box focused.
    pub(super) fn show_find(&mut self) {
        self.panel = PanelMode::Tickets;
        self.diff_open = true;
        self.tickets.panel.view = TicketsView::Find;
        self.tickets.panel.focus_query = true;
        self.ctx.request_repaint();
    }

    /// Shift+↓/↑ in the panel: the next or previous item, a result in Find and a ticket in Read.
    pub(super) fn tickets_step(&mut self, delta: i32) {
        let panel = &mut self.tickets.panel;
        if panel.view == TicketsView::Find {
            panel.step_result(delta);
        } else if let Some((link, nav)) = panel.neighbour(delta) {
            if nav == Nav::Pop {
                panel.crumbs.pop();
            } else {
                panel.find.selected = panel.find.results.iter().position(|card| card.link.same_ticket(&link));
                panel.find.reveal = true;
            }
            self.open_ticket(link, nav);
        }
    }

    /// Alt+←: the ticket before this one, else the results.
    pub(super) fn tickets_back(&mut self) {
        match self.tickets.panel.crumbs.pop() {
            Some(link) => self.open_ticket(link, Nav::Pop),
            None => self.tickets.panel.view = TicketsView::Find,
        }
    }

    pub(super) fn tickets_search(&mut self) {
        let Some(config) = self.ticket_config() else { return };
        let query = self.tickets.panel.query.trim().to_string();
        if query.is_empty() {
            return;
        }
        let has = |op: &str| config.operation(op).is_some();
        let semantic = has("semantic_search") && (!self.tickets.panel.keyword || !has("search"));
        let panel = &mut self.tickets.panel;
        panel.signin = false;
        panel.seq += 1;
        panel.find = Find { seq: panel.seq, busy: true, ran: Some((query.clone(), semantic)), ..Find::default() };
        if !has("search") && !has("semantic_search") {
            panel.find.busy = false;
            panel.find.error = Some("No search operation is mapped in [tickets.operations]".into());
            return;
        }
        let tag = self.panel_tag(&config, Job::Find, self.tickets.panel.seq);
        self.send_ticket(&config, TicketOp::Search { query, semantic }, tag);
    }

    /// Reads a ticket in the panel: the card, its children and the tickets related to it.
    pub(super) fn open_ticket(&mut self, link: TicketLink, nav: Nav) {
        self.panel = PanelMode::Tickets;
        self.diff_open = true;
        self.ctx.request_repaint();
        let Some(config) = self.ticket_config() else {
            self.toast("Tickets are not configured ([tickets] in the config)");
            return;
        };
        let Some(link) = self.stamp_system(Some(link)) else { return };
        let panel = &mut self.tickets.panel;
        match nav {
            Nav::Fresh => panel.crumbs.clear(),
            Nav::Push => {
                if let Some(read) = panel.read.as_ref() {
                    panel.crumbs.push(read.link.clone());
                }
            }
            Nav::Pop => {}
        }
        panel.recent.retain(|recent| !recent.same_ticket(&link));
        panel.recent.insert(0, link.clone());
        panel.recent.truncate(RECENT);
        panel.signin = false;
        panel.seq += 1;
        let seq = panel.seq;
        panel.view = TicketsView::Read;
        let seed = panel.known(&link);
        panel.read = Some(Read::new(link.clone(), seed, seq, &config));
        let has = |op: &str| config.operation(op).is_some();
        if !has("read") && !has("get") {
            let read = self.tickets.panel.read.as_mut().expect("just set");
            read.loading = false;
            read.error = Some("No read or get operation is mapped in [tickets.operations]".into());
            return;
        }
        let mut jobs = vec![(Job::Read, TicketOp::ReadCard(link.clone()))];
        if has("children") {
            jobs.push((Job::Children, TicketOp::Children(link.clone())));
        }
        if has("related") {
            jobs.push((Job::Related, TicketOp::Related(link)));
        }
        for (job, op) in jobs {
            let tag = self.panel_tag(&config, job, seq);
            self.send_ticket(&config, op, tag);
        }
    }

    /// The ticket Read shows for an agent: its task's, else the plan's.
    pub(super) fn session_ticket(&self, session: &str) -> Option<TicketLink> {
        self.plans
            .iter()
            .filter(|plan| plan.status == PlanStatus::Open)
            .find_map(|plan| {
                plan.tasks
                    .iter()
                    .find(|task| task.session.as_deref() == Some(session))
                    .map(|task| task.ticket.clone().or_else(|| plan.ticket.clone()))
            })
            .flatten()
            .or_else(|| self.plans.get(self.selected_plan).and_then(|plan| plan.ticket.clone()))
    }

    /// Ctrl/Cmd+1…9 with Read open: show the selected agent's ticket.
    pub(super) fn tickets_retarget(&mut self, session: &str) {
        if self.panel != PanelMode::Tickets || self.tickets.panel.view != TicketsView::Read {
            return;
        }
        match self.session_ticket(session) {
            Some(link) => {
                let same = self.tickets.panel.read.as_ref().is_some_and(|read| read.link.same_ticket(&link));
                if !same {
                    self.open_ticket(link, Nav::Fresh);
                }
            }
            None => self.toast("That agent has no ticket"),
        }
    }

    /// Shows results found elsewhere, such as by the chief.
    #[allow(dead_code)] // The chief's ticket tools call this.
    pub(super) fn tickets_found(&mut self, query: &str, cards: Vec<TicketCard>, from_chief: bool) {
        let panel = &mut self.tickets.panel;
        panel.seq += 1;
        panel.view = TicketsView::Find;
        panel.find = Find {
            selected: (!cards.is_empty()).then_some(0),
            results: cards,
            ran: Some((query.to_string(), true)),
            from_chief,
            seq: panel.seq,
            ..Find::default()
        };
        if !from_chief {
            panel.query = query.to_string();
        }
    }

    /// Opens the Plan with chief dialog for the ticket Read shows.
    pub(super) fn open_plan_dialog(&mut self) {
        let Some(link) = self.tickets.panel.read.as_ref().map(|read| read.card.link.clone()) else { return };
        let link = self.cleaner().link(link);
        self.tickets.panel.dialog =
            Some(PlanDialog { link, ticket_only: false, extra: String::new(), error: None, focused: false });
    }

    /// Whether the ticket system needs a sign-in before anything is sent: the sign-in state
    /// (`SignState`, read from the keychain when not yet polled) is not signed in, or the worker
    /// said a sign-in is required.
    fn plan_needs_sign_in(&self, config: &TicketConfig) -> bool {
        use crate::ticket_signin::SignState;
        let state = match self.tickets.sign.state() {
            Some(state) => state.clone(),
            None => SignState::of(&self.tickets.sign.oauth(), &config.server),
        };
        self.tickets.panel.signin || state != SignState::SignedIn
    }

    /// Plan with chief: precreates the plan with the ticket link and the mode decision, starts the
    /// chief if needed and sends it the §6.2 message. Signed out, it shows the sign-in state and
    /// sends nothing. A ticket that already has an open plan gets no second plan.
    pub(super) fn plan_with_chief(
        &mut self,
        link: TicketLink,
        mode: TicketMode,
        extra: &str,
    ) -> Result<PlanStart, String> {
        let Some(config) = self.ticket_config() else { return Err("Tickets are not configured".into()) };
        if self.plan_needs_sign_in(&config) {
            let panel = &mut self.tickets.panel;
            panel.signin = true;
            panel.dialog = None;
            self.panel = PanelMode::Tickets;
            self.diff_open = true;
            self.toast(format!("Sign in to {} first", config.system));
            return Ok(PlanStart::SignIn);
        }
        let Some(link) = self.stamp_system(Some(link)) else { return Err("No ticket to plan".into()) };
        let existing = self
            .plans
            .iter()
            .position(|plan| plan.is_open() && plan.ticket.as_ref().is_some_and(|t| t.same_ticket(&link)));
        let (index, fresh) = match existing {
            Some(index) => (index, false),
            None => (self.precreate_ticket_plan(link.clone(), mode), true),
        };
        let id = self.plans[index].id.clone();
        self.selected_plan = index;
        self.show_plan_fleet();
        // The plan board stays behind the ticket; the chief's window is what opens.
        self.panel = PanelMode::Tickets;
        if !fresh && (!self.plans[index].tasks.is_empty() || self.tickets.panel.planning.contains(&id)) {
            self.show_chief();
            self.toast(format!("{} already has a plan", link.label()));
            return Ok(PlanStart::Existing(id));
        }
        let board = self.plans[index].ticket_mode(config.mode) == TicketMode::Board;
        let own = self.plans[index].ticket.clone().unwrap_or(link);
        let message = atomics_core::tickets::plan_message(&atomics_core::tickets::PlanRequest {
            system: &config.system,
            key: own.label(),
            title: own.title.as_deref().unwrap_or(""),
            url: own.url.as_deref(),
            board,
            plan_id: &id,
            extra,
        });
        self.send_to_chief(&message)?;
        self.tickets.panel.planning.insert(id.clone());
        self.toast(format!("Planning {} with the chief", own.label()));
        Ok(if fresh { PlanStart::Started(id) } else { PlanStart::Existing(id) })
    }

    pub(super) fn panel_result(&mut self, job: Job, seq: u64, result: Result<TicketOutput, String>) {
        let result = result.map_err(|error| (crate::mcp_client::is_sign_in_required(&error), self.scrub(&error)));
        let panel = &mut self.tickets.panel;
        if let Err((true, _)) = &result {
            panel.signin = true;
        }
        match job {
            Job::Find if panel.find.seq == seq => {
                panel.find.busy = false;
                match result {
                    Ok(TicketOutput::Cards(cards)) => {
                        panel.find.selected = (!cards.is_empty()).then_some(0);
                        panel.find.results = cards;
                    }
                    Ok(_) => panel.find.error = Some("unexpected ticket result".into()),
                    Err((true, _)) => {}
                    Err((false, error)) => panel.find.error = Some(error),
                }
            }
            Job::Read | Job::Children | Job::Related => {
                let Some(read) = panel.read.as_mut().filter(|read| read.seq == seq) else { return };
                match (job, result) {
                    (Job::Read, Ok(TicketOutput::Card(card))) => {
                        let old = std::mem::take(&mut read.card);
                        let mut card = *card;
                        card.score = card.score.or(old.score);
                        card.assignee = card.assignee.or(old.assignee);
                        card.kind = card.kind.or(old.kind);
                        if card.link.url.is_none() {
                            card.link.url = old.link.url;
                        }
                        read.parts = card.body.as_deref().map(markdown::sections).unwrap_or_default();
                        read.card = card;
                        read.loading = false;
                    }
                    (Job::Read, Err((signin, error))) => {
                        read.loading = false;
                        read.error = (!signin).then_some(error);
                    }
                    (Job::Children, Ok(TicketOutput::Cards(cards))) => read.children = Part::Loaded(cards),
                    (Job::Related, Ok(TicketOutput::Cards(cards))) => read.related = Part::Loaded(cards),
                    (Job::Children, Err((_, error))) => read.children = Part::Failed(error),
                    (Job::Related, Err((_, error))) => read.related = Part::Failed(error),
                    _ => read.error = Some("unexpected ticket result".into()),
                }
            }
            _ => {}
        }
    }
}

pub(super) fn state_color(config: &TicketConfig, state: &str, p: &Palette) -> Color32 {
    config.states.status_of(state).map_or(p.muted, |status| super::board::task_color(status, p))
}

pub(super) fn fit(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.replace('\n', " "), font, color);
    job.wrap =
        TextWrapping { max_width: width.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// Paints a state chip at `at` and returns the x where the next thing may start.
pub(super) fn chip(
    ui: &egui::Ui,
    painter: &egui::Painter,
    at: Pos2,
    text: &str,
    color: Color32,
    limit: f32,
    p: &Palette,
) -> f32 {
    let galley = fit(ui, text, p.ui(11.0), color, limit - at.x - 10.0);
    let rect = Rect::from_min_size(at, galley.size() + vec2(10.0, 2.0));
    painter.rect_filled(rect, 8.0, lerp(p.raised, color, 0.12));
    painter.rect_stroke(rect, 8.0, Stroke::new(1.0, color.gamma_multiply(0.6)), egui::StrokeKind::Inside);
    painter.galley(at + vec2(5.0, 1.0), galley, color);
    rect.right() + 8.0
}

/// One result: key and title, then score bar, state, assignee and kind, then the snippet.
fn find_row(
    ui: &mut egui::Ui,
    card: &TicketCard,
    selected: bool,
    range: Option<(f64, f64)>,
    config: &TicketConfig,
    p: &Palette,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    if selected {
        painter.rect_filled(rect.shrink2(vec2(0.0, 1.0)), 3.0, lerp(p.raised, p.accent, 0.18));
        painter.rect_filled(Rect::from_min_size(rect.left_top(), vec2(3.0, rect.height())), 0.0, p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect.shrink2(vec2(0.0, 1.0)), 3.0, p.raised);
    }
    let (left, right) = (rect.left() + 10.0, rect.right() - 8.0);
    let key = fit(ui, card.link.label(), p.mono(12.5), p.accent, right - left);
    let key_w = key.size().x;
    painter.galley(pos2(left, rect.top() + 5.0), key, p.accent);
    let title = card.link.title.as_deref().unwrap_or("");
    let galley = fit(ui, title, p.ui(12.5), p.foreground, right - left - key_w - 8.0);
    painter.galley(pos2(left + key_w + 8.0, rect.top() + 5.0), galley, p.foreground);

    let y = rect.top() + 24.0;
    let mut x = left;
    if let (Some(score), Some((low, high))) = (card.score, range) {
        let bar = Rect::from_min_size(pos2(x, y + 4.0), vec2(56.0, 8.0));
        painter.rect_filled(bar, 2.0, p.border);
        let full = relative_score(score, low, high);
        painter.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * full, bar.height())), 2.0, p.accent);
        let text = fit(ui, &format!("{score:.2}"), p.mono(11.0), p.muted, 40.0);
        let width = text.size().x;
        painter.galley(pos2(bar.right() + 5.0, y + 1.0), text, p.muted);
        x = bar.right() + 5.0 + width + 8.0;
    }
    if let Some(state) = &card.link.state {
        x = chip(ui, &painter, pos2(x, y), state, state_color(config, state, p), right, p);
    }
    let assignee = match &card.assignee {
        Some(Some(name)) => Some(name.as_str()),
        Some(None) => Some("unassigned"),
        None => None,
    };
    for text in assignee.into_iter().chain(card.kind.as_deref()) {
        if x >= right {
            break;
        }
        let galley = fit(ui, text, p.ui(11.5), p.muted, right - x);
        let width = galley.size().x;
        painter.galley(pos2(x, y + 1.0), galley, p.muted);
        x += width + 10.0;
    }
    if let Some(snippet) = card.snippet.as_deref().filter(|s| !s.is_empty()) {
        let galley = fit(ui, &snippet.replace("**", ""), p.ui(11.5), p.muted, right - left);
        painter.galley(pos2(left, rect.top() + 40.0), galley, p.muted);
    }
    response
}

impl Desktop<'_> {
    pub(super) fn tickets_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        panel_nav::focusable(ui, rect);
        let p = self.runtime.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.background);
        // The chief's newest trace card, one line, opens the chief.
        let echo = self.runtime.lane_echo();
        let body = if echo.is_some() { rect.with_max_y(rect.bottom() - ECHO_H) } else { rect };
        if let Some(echo) = echo {
            let strip = Rect::from_min_max(pos2(rect.left(), rect.bottom() - ECHO_H), rect.right_bottom());
            let response = ui.interact(strip, egui::Id::NULL.with("tickets-echo"), Sense::click());
            ui.painter().line_segment([strip.left_top(), strip.right_top()], Stroke::new(1.0, p.border));
            let galley =
                fit(ui, &echo, p.ui(11.5), if response.hovered() { p.accent } else { p.muted }, strip.width() - 20.0);
            ui.painter().galley(strip.left_center() + vec2(10.0, -galley.size().y / 2.0), galley, p.muted);
            if response.on_hover_text("Open the chief").clicked() {
                self.runtime.show_chief();
            }
        }
        let mut child =
            ui.new_child(egui::UiBuilder::new().id_salt("tickets-panel").max_rect(body.shrink2(vec2(10.0, 6.0))));
        let ui = &mut child;
        ui.set_clip_rect(ui.clip_rect().intersect(body));
        let config = self.runtime.ticket_config();
        let mut acts = Vec::new();
        let system = config.as_ref().map_or("", |c| c.system.as_str()).to_string();
        let view = self.runtime.tickets.panel.view;
        ui.horizontal(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            ui.label(RichText::new("TICKETS").size(10.5).strong().color(p.muted));
            let title = match (view, &self.runtime.tickets.panel.read) {
                (TicketsView::Read, Some(read)) => read.link.label().to_string(),
                _ => "Find".to_string(),
            };
            ui.label(RichText::new(title).strong().color(p.foreground));
            if !system.is_empty() {
                let oauth_in = self.runtime.sign_server().is_some()
                    && self.runtime.tickets.sign.state() == Some(&crate::ticket_signin::SignState::SignedIn);
                if oauth_in && !self.runtime.tickets.panel.signin {
                    self.signed_in_chip(ui, &p, &system);
                } else {
                    let (dot, text) = if self.runtime.tickets.panel.signin {
                        (p.warning, format!("{system} · not signed in"))
                    } else {
                        (p.success, system.clone())
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(text).size(11.5).color(p.muted));
                        ui.label(RichText::new("●").size(10.0).color(dot));
                    });
                }
            }
        });
        ui.separator();
        let Some(config) = config else {
            ui.add_space(8.0);
            ui.label(
                "Tickets are not configured. Add a [tickets] profile to the config to search and read your tickets.",
            );
            ui.label(RichText::new("docs/tickets.md has example profiles.").color(p.muted));
            return;
        };
        if self.runtime.tickets.panel.signin {
            self.signin_banner(ui, &config, &p);
            return;
        }
        if self.runtime.tickets.sign.job.is_some() {
            self.signin_banner(ui, &config, &p);
            return;
        }
        match view {
            TicketsView::Find => self.find_view(ui, &config, &p, &mut acts),
            TicketsView::Read => self.read_view(ui, &config, &p, &mut acts),
        }
        for act in acts {
            match act {
                Act::Search => self.runtime.tickets_search(),
                Act::Open(link, nav) => self.runtime.open_ticket(*link, nav),
                Act::Back => self.runtime.tickets_back(),
                Act::FindView => self.runtime.show_find(),
                Act::Plan => self.runtime.open_plan_dialog(),
                Act::Browser(url) => {
                    let url = self.runtime.cleaner().url(&url);
                    if markdown::web_url(&url).is_some() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                    }
                }
            }
        }
    }

    /// The Plan with chief dialog: a small modal over the panel, built like the new-session dialog.
    pub(super) fn plan_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.runtime.tickets.panel.dialog.take() else { return };
        let Some(config) = self.runtime.ticket_config() else { return };
        let p = self.runtime.palette.clone();
        let children = self.runtime.tickets.panel.read.as_ref().map_or(Part::Unmapped, |read| read.children.clone());
        let count = children.items().len();
        let mut keep = true;
        let mut go = false;
        egui::Window::new(format!("Plan {} with the chief?", dialog.link.label()))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                if let Some(title) = &dialog.link.title {
                    ui.add(egui::Label::new(RichText::new(title).strong()).wrap());
                }
                let mode = plan_mode(&config, &children, dialog.ticket_only);
                let text = match mode {
                    None => "Reading the ticket's children…".to_string(),
                    Some(TicketMode::Board) => format!(
                        "{count} child ticket{} will become task{} (board).",
                        if count == 1 { "" } else { "s" },
                        if count == 1 { "" } else { "s" }
                    ),
                    Some(_) => "Ticket mode: one plan; progress is reported on this ticket.".to_string(),
                };
                ui.label(RichText::new(text).color(p.muted).size(11.5));
                if config.mode != TicketMode::Ticket && count > 0 {
                    ui.checkbox(&mut dialog.ticket_only, "one plan for this ticket only (ticket mode)");
                }
                ui.add_space(4.0);
                ui.label("Extra instructions for the chief (optional)");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut dialog.extra)
                        .char_limit(atomics_core::tickets::PLAN_EXTRA_MAX)
                        .desired_width(f32::INFINITY),
                );
                if !std::mem::replace(&mut dialog.focused, true) {
                    edit.request_focus();
                }
                ui.label(
                    RichText::new("The chief reads the ticket, proposes a plan and waits for your approval.")
                        .color(p.muted)
                        .size(11.5),
                );
                if let Some(error) = &dialog.error {
                    ui.label(RichText::new(error).color(p.error));
                }
                ui.horizontal(|ui| {
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.add_enabled(mode.is_some(), egui::Button::new("Plan with chief ⏎")).clicked()
                        || (enter && mode.is_some())
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        keep = false;
                    }
                });
            });
        if go && let Some(mode) = plan_mode(&config, &children, dialog.ticket_only) {
            match self.runtime.plan_with_chief(dialog.link.clone(), mode, &dialog.extra) {
                Ok(_) => keep = false,
                Err(error) => dialog.error = Some(error),
            }
        }
        if keep && !self.runtime.tickets.panel.signin {
            self.runtime.tickets.panel.dialog = Some(dialog);
        }
    }

    fn find_view(&mut self, ui: &mut egui::Ui, config: &TicketConfig, p: &Palette, acts: &mut Vec<Act>) {
        use egui::Key;
        let query_id = query_id();
        let both = config.operation("semantic_search").is_some() && config.operation("search").is_some();
        let panel = &mut self.runtime.tickets.panel;
        let in_box = ui.memory(|m| m.has_focus(query_id));
        let plain = |key: Key| panel_nav::consume_bare(ui, key);
        if in_box {
            // Tab toggles the mode, ↓ enters the list, Escape leaves the box.
            if both && plain(Key::Tab) {
                panel.keyword = !panel.keyword;
            }
            if plain(Key::ArrowDown) {
                ui.memory_mut(|m| {
                    m.surrender_focus(query_id);
                    m.request_focus(panel_nav::focus_id());
                });
                if panel.find.selected.is_none() && !panel.find.results.is_empty() {
                    panel.find.selected = Some(0);
                }
            } else if plain(Key::Escape) {
                ui.memory_mut(|m| {
                    m.surrender_focus(query_id);
                    m.request_focus(panel_nav::focus_id());
                });
            }
        }
        ui.horizontal(|ui| {
            let toggle = if both { 150.0 } else { 0.0 };
            let response = ui.add(
                egui::TextEdit::singleline(&mut panel.query)
                    .id(query_id)
                    .hint_text("Search tickets…")
                    .desired_width((ui.available_width() - toggle).max(60.0)),
            );
            if std::mem::take(&mut panel.focus_query) {
                response.request_focus();
            }
            if response.has_focus() {
                // egui drops the focus on Escape and moves it on Tab and the arrows before the box
                // sees them; the box handles all of these itself.
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        query_id,
                        egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                    );
                });
            }
            if response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                acts.push(Act::Search);
            }
            if both {
                if ui.selectable_label(!panel.keyword, "meaning").clicked() {
                    panel.keyword = false;
                }
                if ui.selectable_label(panel.keyword, "keyword").clicked() {
                    panel.keyword = true;
                }
            }
        });
        let find = &panel.find;
        let status = if find.busy {
            "searching…".to_string()
        } else if let Some(error) = &find.error {
            error.clone()
        } else if let Some((query, semantic)) = &find.ran {
            let how = if *semantic { "by meaning" } else { "by keyword" };
            let n = find.results.len();
            let who = if find.from_chief { format!(" · from the chief: “{query}”") } else { String::new() };
            format!("{n} ticket{} {how}{who}", if n == 1 { "" } else { "s" })
        } else {
            "Type a query and press Enter.".to_string()
        };
        let color = if find.error.is_some() { p.error } else { p.muted };
        ui.add(egui::Label::new(RichText::new(status).size(11.5).color(color)).truncate());
        ui.separator();
        // Selection and Enter, then paging.
        for key in panel_nav::take_keys(ui, &[Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End, Key::Enter]) {
            if let Some(act) = panel.key(key) {
                acts.push(act);
            }
        }
        let range = score_range(&panel.find.results);
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("tickets-find"));
        let n = panel.find.results.len();
        let mut jump = None;
        if std::mem::take(&mut panel.find.reveal)
            && let Some(selected) = panel.find.selected
        {
            let current = egui::scroll_area::State::load(ui.ctx(), scroll_id).map_or(0.0, |s| s.clamped_offset().y);
            let (top, bottom) = (selected as f32 * ROW_H, (selected + 1) as f32 * ROW_H);
            let height = ui.available_height();
            jump = Some(if top < current {
                top
            } else if bottom > current + height {
                bottom - height
            } else {
                current
            });
        }
        let jump = panel_nav::keyboard_scroll(ui, scroll_id, ROW_H, 0..n, &panel_nav::SCROLL_KEYS, jump);
        let mut area = egui::ScrollArea::vertical().id_salt("tickets-find").auto_shrink([false, false]);
        if let Some(offset) = jump {
            area = area.vertical_scroll_offset(offset);
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        let output = area.show_rows(ui, ROW_H, n, |ui, rows| {
            for index in rows {
                let card = &panel.find.results[index];
                let response = find_row(ui, card, panel.find.selected == Some(index), range, config, p);
                if response.clicked() {
                    panel.find.selected = Some(index);
                    acts.push(Act::Open(Box::new(card.link.clone()), Nav::Fresh));
                }
            }
        });
        panel.find.offset = output.state.clamped_offset().y;
    }

    fn read_view(&mut self, ui: &mut egui::Ui, config: &TicketConfig, p: &Palette, acts: &mut Vec<Act>) {
        use egui::Key;
        let cleaner = self.runtime.cleaner();
        let plan_key = self.runtime.config.keys.plan_with_chief.clone();
        let panel = &mut self.runtime.tickets.panel;
        let crumbs: Vec<String> = panel.crumbs.iter().map(|l| l.label().to_string()).collect();
        let Some(read) = panel.read.as_mut() else {
            acts.push(Act::FindView);
            return;
        };
        let stops = read.stops();
        ui.horizontal(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            if ui.button("← Back").on_hover_text("Alt+←").clicked() {
                acts.push(Act::Back);
            }
            if let Some(url) = &read.card.link.url
                && ui.button("Open in browser").clicked()
            {
                acts.push(Act::Browser(url.clone()));
            }
            if ui.button("Plan with chief").on_hover_text(shortcut_label(ui.ctx(), &plan_key)).clicked() {
                acts.push(Act::Plan);
            }
            let trail = crumbs.iter().map(String::as_str).chain([read.link.label()]).collect::<Vec<_>>().join(" › ");
            ui.label(RichText::new(trail).size(11.5).color(p.muted));
            if !stops.is_empty() {
                let at = read.selected.map_or("–".to_string(), |i| (i + 1).to_string());
                ui.label(RichText::new(format!("section {at}/{}", stops.len())).size(11.5).color(p.muted));
            }
        });
        let card = &read.card;
        let title = card.link.title.as_deref().unwrap_or("");
        ui.add(egui::Label::new(RichText::new(title).size(16.0).strong().color(p.foreground)).wrap());
        ui.horizontal_wrapped(|ui| {
            if let Some(state) = &card.link.state {
                let color = state_color(config, state, p);
                ui.label(RichText::new(format!("[{state}]")).color(color).size(12.0));
            }
            let assignee = match &card.assignee {
                Some(Some(name)) => Some(name.clone()),
                Some(None) => Some("unassigned".to_string()),
                None => None,
            };
            let facts = [card.kind.clone(), assignee, card.link.updated_at.as_ref().map(|t| format!("updated {t}"))]
                .into_iter()
                .flatten()
                .chain((!read.children.items().is_empty()).then(|| format!("{} children", read.children.items().len())))
                .collect::<Vec<_>>()
                .join(" · ");
            ui.label(RichText::new(facts).size(11.5).color(p.muted));
            for link in &card.links {
                let text = [link.label.as_deref(), link.status.as_deref(), link.ci.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ");
                let response = ui.small_button(text);
                if response.clicked()
                    && let Some(url) = &link.url
                {
                    acts.push(Act::Browser(url.clone()));
                }
            }
        });
        if read.loading {
            ui.label(RichText::new("loading…").size(11.5).color(p.muted));
        }
        if let Some(error) = &read.error {
            ui.label(RichText::new(error).color(p.error));
        }
        ui.separator();
        for key in panel_nav::take_keys(ui, &[Key::Enter]) {
            if let Some(act) = panel.key(key) {
                acts.push(act);
            }
        }
        for key in panel_nav::take_keys(ui, &[Key::PageUp, Key::PageDown]) {
            panel.step_section(if key == Key::PageDown { 1 } else { -1 });
        }
        let read = panel.read.as_mut().expect("checked above");
        let scroll_id = ui.make_persistent_id(egui::IdSalt::new("tickets-read"));
        let rows = (read.content_h / LINE_H).ceil() as usize;
        let jump = panel_nav::keyboard_scroll(ui, scroll_id, LINE_H, 0..rows, &panel_nav::SCROLL_KEYS, None);
        let mut area = egui::ScrollArea::vertical().id_salt("tickets-read").auto_shrink([false, false]);
        if let Some(offset) = jump {
            area = area.vertical_scroll_offset(offset);
        }
        let output = area.show(ui, |ui| {
            let mut stop_at = 0;
            let mut jump_to = read.jump.then_some(read.selected).flatten();
            let mut section = |ui: &mut egui::Ui, index: usize, body: &mut dyn FnMut(&mut egui::Ui)| {
                let selected = read.selected == Some(index);
                if selected && jump_to.take().is_some() {
                    ui.scroll_to_cursor(Some(egui::Align::TOP));
                }
                let frame = egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 0, top: 2, bottom: 2 });
                let response = frame.show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    body(ui);
                });
                if selected {
                    let bar = response.response.rect;
                    ui.painter().rect_filled(Rect::from_min_size(bar.min, vec2(3.0, bar.height())), 0.0, p.accent);
                }
            };
            let _ = &mut stop_at;
            for (i, (_, text)) in read.parts.clone().iter().enumerate() {
                section(ui, i, &mut |ui| markdown_view(ui, text, &cleaner, p, acts));
            }
            if read.parts.is_empty() && read.card.body.is_none() && !read.loading {
                ui.label(RichText::new("No description.").color(p.muted));
            }
            if read.card.body.is_none() && !read.card.fields.is_empty() {
                egui::Grid::new("ticket-fields").num_columns(2).show(ui, |ui| {
                    for (name, value) in &read.card.fields {
                        ui.label(RichText::new(name).color(p.muted).size(11.5));
                        ui.add(egui::Label::new(value).wrap());
                        ui.end_row();
                    }
                });
            }
            let base = read.parts.len();
            if !read.card.comments.is_empty() {
                heading(ui, &format!("Comments ({})", read.card.comments.len()), p);
            }
            for (i, comment) in read.card.comments.clone().iter().enumerate() {
                let expanded = read.expanded.contains(&i);
                section(ui, base + i, &mut |ui| comment_view(ui, comment, expanded, &cleaner, p, acts));
            }
            let base = base + read.card.comments.len();
            related_list(ui, "Children", &read.children, "children", base, &mut section, config, p);
            let base = base + read.children.items().len();
            related_list(ui, "Related", &read.related, "related", base, &mut section, config, p);
            ui.add_space(24.0);
        });
        read.jump = false;
        read.offset = output.state.clamped_offset().y;
        read.content_h = output.content_size.y;
    }
}

fn heading(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).strong().size(12.5).color(p.muted));
    ui.separator();
}

fn comment_view(
    ui: &mut egui::Ui,
    comment: &atomics_core::tickets::Comment,
    expanded: bool,
    cleaner: &atomics_core::tickets::Cleaner,
    p: &Palette,
    acts: &mut Vec<Act>,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(comment.author.as_deref().unwrap_or("someone")).strong().size(12.0));
        if comment.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("agent")) {
            ui.label(RichText::new("agent").size(10.5).color(p.info));
        }
        if let Some(at) = &comment.at {
            ui.label(RichText::new(at).size(11.0).color(p.muted));
        }
    });
    let lines = comment.text.lines().count();
    if expanded || lines <= COMMENT_LINES {
        markdown_view(ui, &comment.text, cleaner, p, acts);
    } else {
        let short = comment.text.lines().take(COMMENT_LINES).collect::<Vec<_>>().join("\n");
        markdown_view(ui, &short, cleaner, p, acts);
        ui.label(
            RichText::new(format!("… {} more lines (Enter expands)", lines - COMMENT_LINES)).color(p.muted).size(11.0),
        );
    }
}

/// Draws one selectable section of a ticket: its index and what goes in it.
type SectionFn<'a> = dyn FnMut(&mut egui::Ui, usize, &mut dyn FnMut(&mut egui::Ui)) + 'a;

#[allow(clippy::too_many_arguments)]
fn related_list(
    ui: &mut egui::Ui,
    title: &str,
    part: &Part,
    op: &str,
    base: usize,
    section: &mut SectionFn,
    config: &TicketConfig,
    p: &Palette,
) {
    match part {
        Part::Unmapped => {
            ui.add_space(8.0);
            ui.label(RichText::new(format!("This server offers no {op} operation.")).size(11.5).color(p.muted));
        }
        Part::Loading => {
            heading(ui, &format!("{title}…"), p);
        }
        Part::Failed(error) => {
            heading(ui, title, p);
            ui.label(RichText::new(error).color(p.error).size(11.5));
        }
        Part::Loaded(items) => {
            if items.is_empty() {
                return;
            }
            let by_meaning = items.iter().any(|c| c.score.is_some());
            let suffix = if by_meaning { " · by meaning" } else { "" };
            heading(ui, &format!("{title} ({}){suffix}", items.len()), p);
            let range = score_range(items);
            for (i, card) in items.iter().enumerate() {
                section(ui, base + i, &mut |ui| {
                    ui.horizontal(|ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        if let (Some(score), Some((low, high))) = (card.score, range) {
                            let (bar, _) = ui.allocate_exact_size(vec2(40.0, 8.0), Sense::hover());
                            ui.painter().rect_filled(bar, 2.0, p.border);
                            let full = relative_score(score, low, high);
                            ui.painter().rect_filled(
                                Rect::from_min_size(bar.min, vec2(bar.width() * full, bar.height())),
                                2.0,
                                p.accent,
                            );
                            ui.label(RichText::new(format!("{score:.2}")).monospace().size(11.0).color(p.muted));
                        }
                        ui.label(RichText::new(card.link.label()).monospace().color(p.accent));
                        if let Some(state) = &card.link.state {
                            ui.label(
                                RichText::new(format!("[{state}]")).size(11.5).color(state_color(config, state, p)),
                            );
                        }
                        ui.label(card.link.title.as_deref().unwrap_or(""));
                    });
                });
            }
        }
    }
}

/// Markdown as egui text. Links open only as clean web addresses.
fn markdown_view(
    ui: &mut egui::Ui,
    source: &str,
    cleaner: &atomics_core::tickets::Cleaner,
    p: &Palette,
    acts: &mut Vec<Act>,
) {
    let inline = |ui: &mut egui::Ui, parts: &[Inline], size: f32, strong: bool, acts: &mut Vec<Act>| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for part in parts {
                match part {
                    Inline::Text(text) => {
                        let mut rich = RichText::new(text).size(size).color(p.foreground);
                        if strong {
                            rich = rich.strong();
                        }
                        ui.label(rich);
                    }
                    Inline::Bold(text) => {
                        ui.label(RichText::new(text).size(size).strong().color(p.foreground));
                    }
                    Inline::Code(text) => {
                        ui.label(RichText::new(text).monospace().size(size - 0.5).background_color(p.raised));
                    }
                    Inline::Link { text, url } => {
                        let url = cleaner.url(url);
                        let response = ui.link(RichText::new(text).size(size));
                        if markdown::web_url(&url).is_some() && response.on_hover_text(&url).clicked() {
                            acts.push(Act::Browser(url));
                        }
                    }
                }
            }
        });
    };
    for block in markdown::parse(source) {
        match block {
            Block::Heading(level, parts) => {
                ui.add_space(6.0);
                inline(ui, &parts, [20.0, 17.0, 15.0, 14.0, 13.0, 13.0][level as usize - 1], true, acts);
            }
            Block::Paragraph(parts) => inline(ui, &parts, 13.0, false, acts),
            Block::Item(depth, marker, parts) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(8.0 + depth as f32 * 14.0);
                    ui.label(RichText::new(marker).color(p.muted));
                    ui.vertical(|ui| inline(ui, &parts, 13.0, false, acts));
                });
            }
            Block::Quote(parts) => {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| inline(ui, &parts, 13.0, false, acts));
                });
            }
            Block::Code(code) => {
                egui::Frame::new().fill(p.raised).corner_radius(4).inner_margin(6).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(code).monospace().size(12.0)).wrap());
                });
            }
            Block::Rule => {
                ui.separator();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::editor::save_png;
    use super::super::tests::{session, with_app};
    use super::*;
    use crate::mcp_client::tests::example_profile;
    use egui::Key;

    fn key(code: Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key: code, physical_key: Some(code), pressed: true, repeat: false, modifiers }
    }

    const SHIFT: egui::Modifiers = egui::Modifiers::SHIFT;
    const NONE: egui::Modifiers = egui::Modifiers::NONE;

    /// One frame in the desktop's order: shortcuts, then the panel. The key releases follow.
    fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        frame_in(app, ctx, events, 1020.0, None)
    }

    fn frame_in(
        app: &mut Desktop<'_>,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        width: f32,
        editor: Option<egui::Id>,
    ) -> egui::FullOutput {
        let releases = events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key, physical_key, modifiers, .. } => Some(egui::Event::Key {
                    key: *key,
                    physical_key: *physical_key,
                    pressed: false,
                    repeat: false,
                    modifiers: *modifiers,
                }),
                _ => None,
            })
            .collect();
        let held = events.iter().find_map(|e| match e {
            egui::Event::Key { modifiers, .. } => Some(*modifiers),
            _ => None,
        });
        let mut last = None;
        for (pass, mut events) in [events, releases].into_iter().enumerate() {
            // egui learns the held modifiers from this event, not from the key events.
            events.insert(
                0,
                egui::Event::ModifiersChanged(if pass == 0 { held.unwrap_or_default() } else { Default::default() }),
            );
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 800.0))),
                    events,
                    ..Default::default()
                },
                |ui| {
                    app.shortcuts(ui.ctx());
                    if let Some(id) = editor {
                        ui.interact(Rect::from_min_size(Pos2::ZERO, vec2(10.0, 10.0)), id, Sense::click());
                    }
                    app.diff_panel(ui, Rect::from_min_max(pos2(0.0, 0.0), pos2(width, 760.0)));
                },
            );
            app.runtime.drain_actions();
            RASTER.with(|raster| raster.borrow_mut().feed(&output));
            output.textures_delta.clear();
            last = Some(output);
        }
        last.unwrap()
    }

    thread_local! {
        static RASTER: std::cell::RefCell<Raster> = std::cell::RefCell::default();
    }

    fn card(number: u32, score: Option<f64>) -> TicketCard {
        TicketCard {
            link: TicketLink {
                system: "demo".into(),
                id: format!("id-{number}"),
                key: Some(format!("DEMO-{number}")),
                title: Some(format!("Ticket number {number}, with a title long enough to need truncating")),
                state: Some("Backlog".into()),
                ..Default::default()
            },
            score,
            snippet: Some("…a **match** in the text…".into()),
            assignee: Some(Some("Demo User".into())),
            kind: Some("task".into()),
            ..Default::default()
        }
    }

    /// A configured system without operations: panel state is injected, nothing is sent.
    fn open_panel(app: &mut Desktop<'_>, ctx: &egui::Context, view: TicketsView) {
        app.runtime.tickets.config = Some(Arc::new(TicketConfig { system: "demo".into(), ..Default::default() }));
        app.runtime.panel = PanelMode::Tickets;
        app.runtime.diff_open = true;
        app.runtime.tickets.panel.view = view;
        frame(app, ctx, vec![]);
        // Opening on Find focuses the query box; the tests that want it ask for it.
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused() {
                m.surrender_focus(id);
            }
        });
    }

    fn results(app: &mut Desktop<'_>, count: u32) {
        let panel = &mut app.runtime.tickets.panel;
        panel.find.results = (1..=count).map(|n| card(n, Some(0.9 - n as f64 * 0.01))).collect();
        panel.find.ran = Some(("anything".into(), true));
    }

    fn focused(ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(diff_view::focus_id()))
    }

    fn long_read(app: &mut Desktop<'_>) {
        let body = (0..3)
            .map(|n| format!("## Part {n}\n{}", (0..40).map(|l| format!("line {l} of part {n}\n")).collect::<String>()))
            .collect::<String>();
        let mut parent = card(1, None);
        parent.body = Some(body);
        parent.comments = (0..2)
            .map(|n| atomics_core::tickets::Comment {
                author: Some(format!("author {n}")),
                text: (0..30).map(|l| format!("comment line {l}\n")).collect(),
                ..Default::default()
            })
            .collect();
        let config = TicketConfig { system: "demo".into(), ..Default::default() };
        let mut read = Read::new(parent.link.clone(), Some(parent.clone()), 1, &config);
        read.parts = markdown::sections(parent.body.as_deref().unwrap());
        read.card = parent;
        read.loading = false;
        read.children = Part::Loaded(vec![card(2, None), card(3, None)]);
        read.related = Part::Loaded(vec![card(4, Some(0.9))]);
        app.runtime.tickets.panel.read = Some(read);
        // A Read panel without a ticket falls back to Find, so the view is set once there is one.
        app.runtime.tickets.panel.view = TicketsView::Read;
    }

    fn read_stop(app: &Desktop<'_>) -> Option<Stop> {
        app.runtime.tickets.panel.read.as_ref().and_then(Read::stop)
    }

    #[test]
    fn relative_bars_scale_between_the_lowest_and_highest_score() {
        assert_eq!(relative_score(0.82, 0.77, 0.82), 1.0);
        assert!((relative_score(0.77, 0.77, 0.82) - 0.1).abs() < 1e-6);
        assert!(relative_score(0.80, 0.77, 0.82) > relative_score(0.78, 0.77, 0.82));
        assert_eq!(relative_score(0.5, 0.5, 0.5), 1.0, "one result, or equal scores, fill the bar");
        assert_eq!(score_range(&[card(1, None)]), None);
        assert_eq!(score_range(&[card(1, Some(0.7)), card(2, Some(0.9)), card(3, None)]), Some((0.7, 0.9)));
    }

    #[test]
    fn shift_arrows_step_results_and_the_diff_keys_page_them() {
        with_app("app::tickets_panel::tests::shift_arrows_step_results_and_the_diff_keys_page_them", |app, ctx| {
            open_panel(app, ctx, TicketsView::Find);
            results(app, 30);
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            assert!(focused(ctx), "Shift+↓ focuses the panel");
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(0));
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(1));
            frame(app, ctx, vec![key(Key::ArrowUp, SHIFT)]);
            frame(app, ctx, vec![key(Key::ArrowUp, SHIFT)]);
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(0), "stops at the first result");
            frame(app, ctx, vec![key(Key::ArrowDown, NONE)]);
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(1), "↓ moves the selection");
            assert_eq!(app.runtime.tickets.panel.find.offset, 0.0);
            frame(app, ctx, vec![key(Key::End, NONE)]);
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(29));
            frame(app, ctx, vec![]);
            assert!(app.runtime.tickets.panel.find.offset > 0.0, "End reveals the last result");
            frame(app, ctx, vec![key(Key::Home, NONE)]);
            frame(app, ctx, vec![]);
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(0));
            assert_eq!(app.runtime.tickets.panel.find.offset, 0.0);
            frame(app, ctx, vec![key(Key::PageDown, NONE)]);
            frame(app, ctx, vec![]);
            assert!(app.runtime.tickets.panel.find.offset > 400.0, "PageDown scrolls a page");
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(0), "paging leaves the selection");
            frame(app, ctx, vec![key(Key::PageUp, NONE)]);
            frame(app, ctx, vec![]);
            assert_eq!(app.runtime.tickets.panel.find.offset, 0.0);
        });
    }

    #[test]
    fn enter_reads_the_selected_result_and_escape_returns_the_terminal() {
        with_app(
            "app::tickets_panel::tests::enter_reads_the_selected_result_and_escape_returns_the_terminal",
            |app, ctx| {
                open_panel(app, ctx, TicketsView::Find);
                results(app, 3);
                app.runtime.active = Some("agent".into());
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                frame(app, ctx, vec![key(Key::Enter, NONE)]);
                let panel = &app.runtime.tickets.panel;
                assert_eq!(panel.view, TicketsView::Read);
                assert_eq!(panel.read.as_ref().unwrap().link.label(), "DEMO-2");
                assert!(panel.crumbs.is_empty());
                frame(app, ctx, vec![key(Key::Escape, NONE)]);
                assert!(!focused(ctx));
                assert_eq!(app.client.focus_terminal.as_deref(), Some("agent"));
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                frame(app, ctx, vec![egui::Event::Text("typing".into())]);
                assert!(!focused(ctx), "typing returns focus to the terminal");
            },
        );
    }

    #[test]
    fn shift_arrows_are_ignored_in_the_query_box_and_in_the_editor() {
        with_app(
            "app::tickets_panel::tests::shift_arrows_are_ignored_in_the_query_box_and_in_the_editor",
            |app, ctx| {
                open_panel(app, ctx, TicketsView::Find);
                results(app, 3);
                frame(app, ctx, vec![]);
                // The query box takes the focus when the panel opens.
                ctx.memory_mut(|m| m.request_focus(query_id()));
                frame(app, ctx, vec![]);
                assert!(ctx.memory(|m| m.has_focus(query_id())));
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                assert_eq!(app.runtime.tickets.panel.find.selected, None, "ignored in a text field");
                assert!(ctx.memory(|m| m.has_focus(query_id())) && !focused(ctx));
                // ↓ in the box enters the list.
                frame(app, ctx, vec![key(Key::ArrowDown, NONE)]);
                assert!(focused(ctx));
                assert_eq!(app.runtime.tickets.panel.find.selected, Some(0));
                // Escape leaves the box; again returns the terminal.
                ctx.memory_mut(|m| {
                    m.surrender_focus(diff_view::focus_id());
                    m.request_focus(query_id());
                });
                frame(app, ctx, vec![]);
                frame(app, ctx, vec![key(Key::Escape, NONE)]);
                assert!(!ctx.memory(|m| m.has_focus(query_id())));
                assert!(focused(ctx));
                frame(app, ctx, vec![key(Key::Escape, NONE)]);
                assert!(!focused(ctx));

                let editor = egui::Id::unique("test-editor");
                app.client.editor_id = Some(editor);
                app.runtime.tickets.panel.find.selected = Some(1);
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                    m.request_focus(editor);
                });
                frame_in(app, ctx, vec![], 1020.0, Some(editor));
                assert!(ctx.memory(|m| m.has_focus(editor)));
                frame_in(app, ctx, vec![key(Key::ArrowDown, SHIFT)], 1020.0, Some(editor));
                assert_eq!(app.runtime.tickets.panel.find.selected, Some(1), "ignored in the editor");
                assert!(ctx.memory(|m| m.has_focus(editor)) && !focused(ctx));
            },
        );
    }

    #[test]
    fn page_keys_step_sections_and_enter_opens_children_and_alt_left_goes_back() {
        with_app(
            "app::tickets_panel::tests::page_keys_step_sections_and_enter_opens_children_and_alt_left_goes_back",
            |app, ctx| {
                open_panel(app, ctx, TicketsView::Read);
                long_read(app);
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                assert!(focused(ctx));
                let expected = [
                    Stop::Description(0),
                    Stop::Description(1),
                    Stop::Description(2),
                    Stop::Comment(0),
                    Stop::Comment(1),
                    Stop::Child(0),
                    Stop::Child(1),
                    Stop::Related(0),
                ];
                assert_eq!(app.runtime.tickets.panel.read.as_ref().unwrap().stops(), expected);
                for stop in expected {
                    frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                    assert_eq!(read_stop(app), Some(stop));
                }
                frame(app, ctx, vec![]);
                assert!(
                    app.runtime.tickets.panel.read.as_ref().unwrap().offset > 0.0,
                    "the last section is scrolled to"
                );
                frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                assert_eq!(read_stop(app), Some(Stop::Related(0)), "stops at the last section");
                frame(app, ctx, vec![key(Key::PageUp, NONE)]);
                assert_eq!(read_stop(app), Some(Stop::Child(1)));
                frame(app, ctx, vec![key(Key::PageUp, NONE)]);
                frame(app, ctx, vec![key(Key::PageUp, NONE)]);
                assert_eq!(read_stop(app), Some(Stop::Comment(1)));
                // Enter expands a comment.
                frame(app, ctx, vec![key(Key::Enter, NONE)]);
                assert!(app.runtime.tickets.panel.read.as_ref().unwrap().expanded.contains(&1));
                // Enter opens a child, and Alt+← goes back along the trail, then to the results.
                frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                assert_eq!(read_stop(app), Some(Stop::Child(1)));
                frame(app, ctx, vec![key(Key::Enter, NONE)]);
                let panel = &app.runtime.tickets.panel;
                assert_eq!(panel.read.as_ref().unwrap().link.label(), "DEMO-3");
                assert_eq!(panel.crumbs.iter().map(|l| l.label()).collect::<Vec<_>>(), ["DEMO-1"]);
                frame(app, ctx, vec![key(Key::ArrowLeft, egui::Modifiers::ALT)]);
                let panel = &app.runtime.tickets.panel;
                assert_eq!(panel.read.as_ref().unwrap().link.label(), "DEMO-1");
                assert!(panel.crumbs.is_empty() && panel.view == TicketsView::Read);
                frame(app, ctx, vec![key(Key::ArrowLeft, egui::Modifiers::ALT)]);
                assert_eq!(app.runtime.tickets.panel.view, TicketsView::Find);
            },
        );
    }

    #[test]
    fn home_end_and_arrows_scroll_a_ticket_and_page_keys_step_sections() {
        with_app(
            "app::tickets_panel::tests::home_end_and_arrows_scroll_a_ticket_and_page_keys_step_sections",
            |app, ctx| {
                open_panel(app, ctx, TicketsView::Read);
                long_read(app);
                // With no results to step through, this only takes the focus.
                frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
                assert!(focused(ctx));
                let offset = |app: &Desktop<'_>| app.runtime.tickets.panel.read.as_ref().unwrap().offset;
                assert_eq!(offset(app), 0.0);
                frame(app, ctx, vec![key(Key::End, NONE)]);
                frame(app, ctx, vec![]);
                let bottom = offset(app);
                assert!(bottom > 50.0, "End goes to the bottom: {bottom}");
                frame(app, ctx, vec![key(Key::ArrowUp, NONE)]);
                frame(app, ctx, vec![]);
                let line = offset(app);
                assert!(line < bottom && bottom - line < 40.0, "↑ scrolls a line: {line} from {bottom}");
                frame(app, ctx, vec![key(Key::ArrowDown, NONE)]);
                frame(app, ctx, vec![]);
                assert!(offset(app) > line, "↓ scrolls back down");
                frame(app, ctx, vec![key(Key::Home, NONE)]);
                frame(app, ctx, vec![]);
                assert_eq!(offset(app), 0.0);
                assert_eq!(read_stop(app), None, "scrolling does not select");
                frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                frame(app, ctx, vec![key(Key::PageDown, NONE)]);
                frame(app, ctx, vec![]);
                assert_eq!(read_stop(app), Some(Stop::Description(1)), "PageDown steps a section");
                assert!(offset(app) > 0.0, "and scrolls to it");
            },
        );
    }

    #[test]
    fn shift_arrows_read_the_next_result_and_walk_the_trail_back() {
        with_app("app::tickets_panel::tests::shift_arrows_read_the_next_result_and_walk_the_trail_back", |app, ctx| {
            open_panel(app, ctx, TicketsView::Find);
            results(app, 3);
            app.runtime.open_ticket(card(1, None).link, Nav::Fresh);
            ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
            frame(app, ctx, vec![]);
            let label = |app: &Desktop<'_>| app.runtime.tickets.panel.read.as_ref().unwrap().link.label().to_string();
            assert_eq!(label(app), "DEMO-1");
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            assert_eq!(label(app), "DEMO-2");
            assert_eq!(app.runtime.tickets.panel.find.selected, Some(1));
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            assert_eq!(label(app), "DEMO-3", "stops at the last result");
            frame(app, ctx, vec![key(Key::ArrowUp, SHIFT)]);
            assert_eq!(label(app), "DEMO-2");
            assert!(app.runtime.tickets.panel.crumbs.is_empty());
            // On a trail, Shift+↑ goes back along the breadcrumbs and Shift+↓ has no next ticket.
            app.runtime.open_ticket(card(9, None).link, Nav::Push);
            assert_eq!(app.runtime.tickets.panel.crumbs.len(), 1);
            frame(app, ctx, vec![key(Key::ArrowDown, SHIFT)]);
            assert_eq!(label(app), "DEMO-9");
            frame(app, ctx, vec![key(Key::ArrowUp, SHIFT)]);
            assert_eq!(label(app), "DEMO-2");
            assert!(app.runtime.tickets.panel.crumbs.is_empty());
        });
    }

    #[test]
    fn agent_chords_retarget_read_and_leave_find_alone() {
        with_app("app::tickets_panel::tests::agent_chords_retarget_read_and_leave_find_alone", |app, ctx| {
            let link = |key: &str| TicketLink {
                system: "demo".into(),
                id: key.into(),
                key: Some(key.into()),
                ..Default::default()
            };
            let dir = crate::test_support::tempdir().unwrap();
            for id in ["first", "second", "chief"] {
                let mut agent = session(id, AgentKind::Generic, AgentStatus::Working, dir.path());
                agent.chief = id == "chief";
                app.runtime.sessions.push(agent);
            }
            app.runtime.ungrouped = false;
            app.runtime.view = None;
            app.runtime.active = Some("first".into());
            let task = |id: &str, session: &str, ticket: Option<TicketLink>| Task {
                id: id.into(),
                session: Some(session.into()),
                ticket,
                ..Default::default()
            };
            app.runtime.plans[0].ticket = Some(link("PLAN-1"));
            app.runtime.plans[0].tasks = vec![
                task("a", "first", Some(link("TASK-1"))),
                task("b", "second", Some(link("TASK-2"))),
                task("c", "x", None),
            ];
            app.runtime.selected_plan = 0;
            assert_eq!(app.runtime.session_ticket("second").unwrap().label(), "TASK-2");
            assert_eq!(
                app.runtime.session_ticket("chief").unwrap().label(),
                "PLAN-1",
                "untasked agents show the plan's"
            );
            assert_eq!(
                app.runtime.session_ticket("x").unwrap().label(),
                "PLAN-1",
                "a task without a ticket shows the plan's"
            );

            let chord = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };
            open_panel(app, ctx, TicketsView::Find);
            results(app, 2);
            frame(app, ctx, vec![key(Key::Num2, chord)]);
            assert_eq!(app.runtime.active.as_deref(), Some("second"));
            assert!(app.runtime.panel == PanelMode::Tickets);
            assert_eq!(app.runtime.tickets.panel.view, TicketsView::Find, "Find stays in Find");
            assert!(app.runtime.tickets.panel.read.is_none());

            app.runtime.tickets.panel.view = TicketsView::Read;
            frame(app, ctx, vec![key(Key::Num1, chord)]);
            assert_eq!(app.runtime.active.as_deref(), Some("first"));
            assert_eq!(app.runtime.tickets.panel.read.as_ref().unwrap().link.label(), "TASK-1");
            frame(app, ctx, vec![key(Key::Num2, chord)]);
            assert_eq!(app.runtime.tickets.panel.read.as_ref().unwrap().link.label(), "TASK-2");
            assert!(focused(ctx));
        });
    }

    #[test]
    fn the_toggle_key_opens_and_closes_the_panel() {
        with_app("app::tickets_panel::tests::the_toggle_key_opens_and_closes_the_panel", |app, ctx| {
            app.runtime.tickets.config = Some(Arc::new(TicketConfig { system: "demo".into(), ..Default::default() }));
            let modifiers =
                if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL | SHIFT };
            assert_eq!(app.runtime.config.keys.toggle_tickets, "cmd+k");
            assert_ne!(app.runtime.config.keys.toggle_tickets, app.runtime.config.keys.next_attention);
            app.runtime.diff_open = false;
            frame(app, ctx, vec![key(Key::K, modifiers)]);
            assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
            frame(app, ctx, vec![]);
            assert!(ctx.memory(|m| m.has_focus(query_id())), "Find opens on the query box");
            frame(app, ctx, vec![key(Key::K, modifiers)]);
            assert!(app.runtime.panel == PanelMode::Diff);
        });
    }

    #[test]
    fn chief_results_are_tagged_and_leave_the_users_query_alone() {
        with_app("app::tickets_panel::tests::chief_results_are_tagged_and_leave_the_users_query_alone", |app, ctx| {
            open_panel(app, ctx, TicketsView::Find);
            app.runtime.tickets.panel.query = "mine".into();
            app.runtime.tickets_found("chief query", vec![card(1, Some(0.8))], true);
            let panel = &app.runtime.tickets.panel;
            assert!(panel.find.from_chief && panel.query == "mine" && panel.find.selected == Some(0));
            let output = frame(app, ctx, vec![]);
            assert!(texts(&output).iter().any(|t| t.contains("from the chief")));
            app.runtime.tickets_found("typed", vec![], false);
            assert_eq!(app.runtime.tickets.panel.query, "typed");
        });
    }

    fn texts(output: &egui::FullOutput) -> Vec<String> {
        output
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn narrow_panels_keep_their_text_apart_and_inside() {
        with_app("app::tickets_panel::tests::narrow_panels_keep_their_text_apart_and_inside", |app, ctx| {
            for view in [TicketsView::Find, TicketsView::Read] {
                for width in [160.0, 240.0, 360.0, 760.0] {
                    open_panel(app, ctx, view);
                    results(app, 4);
                    long_read(app);
                    let output = frame_in(app, ctx, vec![], width, None);
                    // Per row: a wrapped label's bounds also cover the indent of its first row.
                    let shapes: Vec<Rect> = output
                        .shapes
                        .iter()
                        .filter_map(|s| match &s.shape {
                            egui::epaint::Shape::Text(t) => Some((t, s.clip_rect)),
                            _ => None,
                        })
                        .flat_map(|(t, clip)| {
                            t.galley.rows.iter().filter_map(move |placed| {
                                let bounds =
                                    placed.row.visuals.mesh_bounds.translate(placed.pos.to_vec2() + t.pos.to_vec2());
                                let visible = bounds.intersect(clip);
                                (visible.is_positive() && !placed.row.text().trim().is_empty()).then_some(visible)
                            })
                        })
                        .collect();
                    assert!(!shapes.is_empty());
                    let panel = Rect::from_min_max(pos2(0.0, 0.0), pos2(width, 760.0));
                    for (i, a) in shapes.iter().enumerate() {
                        assert!(panel.expand(1.0).contains_rect(*a), "{view:?} at {width}: {a:?} leaves {panel:?}");
                        for b in &shapes[i + 1..] {
                            let overlap = a.intersect(*b);
                            assert!(
                                overlap.width() < 1.0 || overlap.height() < 1.0,
                                "{view:?} at {width}: {a:?} overlaps {b:?}"
                            );
                        }
                    }
                }
            }
        });
    }

    /// The meaning-search example profile against the fake server, with a children operation.
    fn fake_profile(app: &mut Desktop<'_>, dir: &Path) {
        let state = dir.join("fake.json");
        let config = example_profile("meaning-example", &state);
        app.runtime.config.tickets = config;
        app.runtime.apply_ticket_config();
    }

    #[test]
    fn the_panel_finds_and_reads_against_the_fake_server() {
        with_app("app::tickets_panel::tests::the_panel_finds_and_reads_against_the_fake_server", |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            fake_profile(app, dir.path());
            app.runtime.panel = PanelMode::Tickets;
            app.runtime.diff_open = true;
            app.runtime.tickets.panel.query = "personal-agent ticket workspace".into();
            app.runtime.tickets_search();
            settle(app);
            let find = &app.runtime.tickets.panel.find;
            assert!(find.error.is_none(), "{:?}", find.error);
            assert_eq!(find.results.len(), 5, "{:?}", find.results.iter().map(|c| c.link.label()).collect::<Vec<_>>());
            assert!(find.results.iter().all(|c| c.score.is_some() && c.snippet.is_some()));
            assert_eq!(find.ran.as_ref().map(|r| r.1), Some(true));
            ctx.set_pixels_per_point(1.5);
            let output = frame(app, ctx, vec![]);
            let text = texts(&output).join("\n");
            assert!(text.contains("DEMO-1") && text.contains("5 tickets by meaning"), "{text}");
            screenshot(app, ctx, "find");

            let epic = app
                .runtime
                .tickets
                .panel
                .find
                .results
                .iter()
                .find(|c| c.link.label() == "DEMO-1")
                .unwrap()
                .link
                .clone();
            app.runtime.open_ticket(epic, Nav::Fresh);
            settle(app);
            let read = app.runtime.tickets.panel.read.as_ref().unwrap();
            assert!(read.error.is_none(), "{:?}", read.error);
            assert!(read.card.body.as_deref().unwrap().contains("## Goal"));
            assert_eq!(read.card.comments.len(), 1);
            assert_eq!(read.children.items().len(), 4);
            assert!(!read.related.items().is_empty());
            let output = frame(app, ctx, vec![]);
            screenshot(app, ctx, "read");
            let text = texts(&output).join("\n");
            assert!(text.contains("Children (4)") && text.contains("Comments (1)") && text.contains("Goal"), "{text}");
            assert_eq!(app.runtime.tickets.panel.recent[0].label(), "DEMO-1");
        });
    }

    /// A CPU rasterizer for egui output, so a screenshot
    /// needs no window or GPU. `ATOMICS_TICKETS_SCREENSHOTS=<dir>` makes the fake-server test write them.
    #[derive(Default)]
    struct Raster {
        textures: HashMap<egui::TextureId, egui::ColorImage>,
    }

    impl Raster {
        fn feed(&mut self, output: &egui::FullOutput) {
            for (id, deltas) in &output.textures_delta.set {
                for delta in deltas {
                    let egui::epaint::ImageData::Color(image) = &delta.image;
                    match delta.pos {
                        None => {
                            self.textures.insert(*id, (**image).clone());
                        }
                        Some([x, y]) => {
                            if let Some(target) = self.textures.get_mut(id) {
                                for row in 0..image.size[1] {
                                    let from = row * image.size[0];
                                    let to = (y + row) * target.size[0] + x;
                                    target.pixels[to..to + image.size[0]]
                                        .copy_from_slice(&image.pixels[from..from + image.size[0]]);
                                }
                            }
                        }
                    }
                }
            }
        }

        fn paint(
            &self,
            ctx: &egui::Context,
            output: egui::FullOutput,
            size: [usize; 2],
            background: Color32,
        ) -> egui::ColorImage {
            let ppp = output.pixels_per_point;
            let mut image = egui::ColorImage::filled(size, background);
            for clipped in ctx.tessellate(output.shapes, ppp) {
                let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive else { continue };
                let Some(texture) = self.textures.get(&mesh.texture_id) else { continue };
                let clip = clipped.clip_rect * ppp;
                for triangle in mesh.indices.as_chunks::<3>().0 {
                    let v: Vec<_> = triangle.iter().map(|&i| mesh.vertices[i as usize]).collect();
                    let p = |n: usize| v[n].pos.to_vec2() * ppp;
                    let (a, b, c) = (p(0), p(1), p(2));
                    let area = (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
                    if area.abs() < 1e-6 {
                        continue;
                    }
                    let min = a.min(b).min(c).max(clip.min.to_vec2());
                    let max = a.max(b).max(c).min(clip.max.to_vec2());
                    for y in (min.y.floor() as i64).max(0)..(max.y.ceil() as i64).min(size[1] as i64) {
                        for x in (min.x.floor() as i64).max(0)..(max.x.ceil() as i64).min(size[0] as i64) {
                            let point = vec2(x as f32 + 0.5, y as f32 + 0.5);
                            let w0 = ((b.x - point.x) * (c.y - point.y) - (b.y - point.y) * (c.x - point.x)) / area;
                            let w1 = ((c.x - point.x) * (a.y - point.y) - (c.y - point.y) * (a.x - point.x)) / area;
                            let w2 = 1.0 - w0 - w1;
                            if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                                continue;
                            }
                            let uv = v[0].uv.to_vec2() * w0 + v[1].uv.to_vec2() * w1 + v[2].uv.to_vec2() * w2;
                            let tx = ((uv.x * texture.size[0] as f32) as usize).min(texture.size[0] - 1);
                            let ty = ((uv.y * texture.size[1] as f32) as usize).min(texture.size[1] - 1);
                            let tex = texture.pixels[ty * texture.size[0] + tx].to_array();
                            let col = |n: usize| v[n].color.to_array();
                            let mix = |k: usize| col(0)[k] as f32 * w0 + col(1)[k] as f32 * w1 + col(2)[k] as f32 * w2;
                            let src: Vec<f32> = (0..4).map(|k| mix(k) * tex[k] as f32 / 255.0).collect();
                            let dst = &mut image.pixels[y as usize * size[0] + x as usize];
                            let d = dst.to_array();
                            let keep = 1.0 - src[3] / 255.0;
                            *dst = Color32::from_rgba_premultiplied(
                                (src[0] + d[0] as f32 * keep) as u8,
                                (src[1] + d[1] as f32 * keep) as u8,
                                (src[2] + d[2] as f32 * keep) as u8,
                                255,
                            );
                        }
                    }
                }
            }
            image
        }
    }

    /// Saves the panel as it was last drawn when `ATOMICS_TICKETS_SCREENSHOTS` names a folder.
    fn screenshot(app: &mut Desktop<'_>, ctx: &egui::Context, name: &str) {
        let Some(dir) = std::env::var_os("ATOMICS_TICKETS_SCREENSHOTS").map(PathBuf::from) else { return };
        let (width, height) = (1020.0, 640.0);
        let mut last = None;
        for _ in 0..2 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, height))),
                    ..Default::default()
                },
                |ui| app.diff_panel(ui, Rect::from_min_max(pos2(0.0, 0.0), pos2(width, height))),
            );
            RASTER.with(|raster| raster.borrow_mut().feed(&output));
            output.textures_delta.clear();
            last = Some(output);
        }
        let ppp = ctx.pixels_per_point();
        let image = RASTER.with(|raster| {
            raster.borrow().paint(
                ctx,
                last.unwrap(),
                [(width * ppp) as usize, (height * ppp) as usize],
                app.runtime.palette.background,
            )
        });
        std::fs::create_dir_all(&dir).unwrap();
        save_png(&dir.join(format!("{name}.png")), &image).unwrap();
    }

    fn settle(app: &mut Desktop<'_>) {
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        loop {
            app.runtime.ticket_results();
            if app.runtime.tickets.pending == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "ticket jobs did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn sign_in_required_shows_the_banner_not_an_error() {
        with_app("app::tickets_panel::tests::sign_in_required_shows_the_banner_not_an_error", |app, ctx| {
            open_panel(app, ctx, TicketsView::Find);
            let seq = app.runtime.tickets.panel.seq + 1;
            app.runtime.tickets.panel.seq = seq;
            app.runtime.tickets.panel.find.seq = seq;
            app.runtime.panel_result(
                Job::Find,
                seq,
                Err(format!("{}: server rejected", crate::mcp_client::SIGN_IN_REQUIRED)),
            );
            assert!(app.runtime.tickets.panel.signin && app.runtime.tickets.panel.find.error.is_none());
            let text = texts(&frame(app, ctx, vec![])).join("\n");
            assert!(text.contains("demo is not signed in"), "{text}");
        });
    }

    #[test]
    fn plan_tabs_show_the_ticket_key_with_progress() {
        let mut plan = Plan { goal: "A long goal that the tab used to show".into(), ..Default::default() };
        assert_eq!(super::super::sidebar::plan_tab_name(&plan), plan.goal);
        plan.ticket = Some(TicketLink { id: "u".into(), key: Some("DEMO-7".into()), ..Default::default() });
        assert_eq!(super::super::sidebar::plan_tab_name(&plan), "DEMO-7 · planning…");
        plan.tasks = vec![Task { status: TaskStatus::Done, ..Default::default() }, Task::default()];
        assert_eq!(super::super::sidebar::plan_tab_name(&plan), "DEMO-7 · 1/2");
    }

    /// A chief whose messages queue in the hub, where the test reads what it was sent.
    fn quiet_chief(app: &mut Desktop<'_>) {
        let root = app.runtime.launch_dir.clone();
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &root);
        chief.chief = true;
        app.runtime.sessions = vec![chief];
    }

    fn sent_to_chief(app: &Desktop<'_>) -> Vec<String> {
        app.runtime.hub.lock().unwrap().outbox.get("chief").cloned().unwrap_or_default()
    }

    /// Searches and reads the fake server's epic, as a user would, until Read shows its children.
    fn read_epic(app: &mut Desktop<'_>, dir: &Path) -> TicketLink {
        fake_profile(app, dir);
        app.runtime.panel = PanelMode::Tickets;
        app.runtime.diff_open = true;
        app.runtime.tickets.panel.query = "personal-agent ticket workspace".into();
        app.runtime.tickets_search();
        settle(app);
        let epic =
            app.runtime.tickets.panel.find.results.iter().find(|c| c.link.label() == "DEMO-1").unwrap().link.clone();
        app.runtime.open_ticket(epic.clone(), Nav::Fresh);
        settle(app);
        assert_eq!(app.runtime.tickets.panel.read.as_ref().unwrap().children.items().len(), 4);
        epic
    }

    fn plan_chord() -> egui::Modifiers {
        crate::theme::shortcut("cmd+enter").unwrap().modifiers
    }

    fn press(key: Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
        vec![
            egui::Event::ModifiersChanged(modifiers),
            egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers },
        ]
    }

    fn dialog_frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1000.0, 800.0))),
                events,
                ..Default::default()
            },
            |ui| {
                app.shortcuts(ui.ctx());
                app.diff_panel(ui, Rect::from_min_max(pos2(0.0, 0.0), pos2(1000.0, 760.0)));
                app.plan_dialog(ui.ctx());
            },
        );
        output.textures_delta.clear();
        app.runtime.drain_actions();
        output
    }

    #[test]
    fn plan_with_chief_precreates_the_plan_and_sends_the_message_without_the_ticket_body() {
        with_app(
            "app::tickets_panel::tests::plan_with_chief_precreates_the_plan_and_sends_the_message_without_the_ticket_body",
            |app, ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                quiet_chief(app);
                let epic = read_epic(app, dir.path());
                let body = app.runtime.tickets.panel.read.as_ref().unwrap().card.body.clone().unwrap();
                assert!(body.contains("## Goal"));
                let before = app.runtime.plans.len();

                // Cmd/Ctrl+Enter in Read opens the dialog; Enter in it plans.
                ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
                dialog_frame(app, ctx, vec![]);
                dialog_frame(app, ctx, press(Key::Enter, plan_chord()));
                assert!(app.runtime.tickets.panel.dialog.is_some());
                let text = texts(&dialog_frame(app, ctx, vec![])).join("\n");
                assert!(text.contains("4 child tickets will become tasks"), "{text}");
                dialog_frame(app, ctx, press(Key::Enter, egui::Modifiers::NONE));
                assert!(app.runtime.tickets.panel.dialog.is_none());

                assert_eq!(app.runtime.plans.len(), before + 1);
                let plan = app.runtime.plans.last().unwrap().clone();
                assert!(plan.tasks.is_empty() && !plan.approved);
                assert_eq!(plan.ticket.as_ref().map(|t| t.label()), Some("DEMO-1"));
                assert_eq!(plan.ticket_mode, Some(TicketMode::Board));
                assert_eq!(super::super::sidebar::plan_tab_name(&plan), "DEMO-1 · planning…");
                assert!(
                    atomics_core::plan::Plan::load(
                        &paths::atomics_home().join("plans").join(format!("{}.json", plan.id))
                    )
                    .is_ok()
                );
                assert!(app.runtime.chief_open && app.runtime.panel == PanelMode::Tickets);

                let sent = sent_to_chief(app);
                assert_eq!(sent.len(), 1);
                let expected = atomics_core::tickets::plan_message(&atomics_core::tickets::PlanRequest {
                    system: &app.runtime.ticket_config().unwrap().system,
                    key: "DEMO-1",
                    title: epic.title.as_deref().unwrap(),
                    url: plan.ticket.as_ref().and_then(|t| t.url.as_deref()),
                    board: true,
                    plan_id: &plan.id,
                    extra: "",
                });
                assert_eq!(sent[0], expected);
                assert!(sent[0].contains(&format!("plan id {}.", plan.id)) && sent[0].contains("Mode: board"));
                for line in body.lines().filter(|l| l.len() > 12 && !epic.title.as_deref().unwrap().contains(*l)) {
                    assert!(!sent[0].contains(line), "the ticket's text rode in the message: {line}");
                }

                // A second press finds the open plan: no second plan, no second message.
                ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
                dialog_frame(app, ctx, press(Key::Enter, plan_chord()));
                dialog_frame(app, ctx, press(Key::Enter, egui::Modifiers::NONE));
                assert_eq!(app.runtime.plans.len(), before + 1);
                assert_eq!(sent_to_chief(app).len(), 1);
                let again = app.runtime.plan_with_chief(epic, TicketMode::Board, "").unwrap();
                assert_eq!(again, PlanStart::Existing(plan.id.clone()));
                assert_eq!(app.runtime.plans.len(), before + 1);
                assert_eq!(sent_to_chief(app).len(), 1);
            },
        );
    }

    #[test]
    fn plan_with_chief_carries_the_users_words_and_ticket_only_mode() {
        with_app(
            "app::tickets_panel::tests::plan_with_chief_carries_the_users_words_and_ticket_only_mode",
            |app, _| {
                let dir = crate::test_support::tempdir().unwrap();
                quiet_chief(app);
                let epic = read_epic(app, dir.path());
                let read = app.runtime.tickets.panel.read.as_ref().unwrap();
                let config = app.runtime.ticket_config().unwrap();
                assert_eq!(plan_mode(&config, &read.children, false), Some(TicketMode::Board));
                assert_eq!(plan_mode(&config, &read.children, true), Some(TicketMode::Ticket));
                assert_eq!(plan_mode(&config, &Part::Loading, false), None);
                assert_eq!(plan_mode(&config, &Part::Loaded(Vec::new()), false), Some(TicketMode::Ticket));
                let forced = TicketConfig { mode: TicketMode::Board, ..(*config).clone() };
                assert_eq!(plan_mode(&forced, &Part::Unmapped, false), Some(TicketMode::Board));
                let start = app.runtime.plan_with_chief(epic, TicketMode::Ticket, "Keep it\nsmall").unwrap();
                let PlanStart::Started(id) = start else { panic!("{start:?}") };
                let plan = app.runtime.plans.iter().find(|p| p.id == id).unwrap();
                assert_eq!(plan.ticket_mode, Some(TicketMode::Ticket));
                let sent = sent_to_chief(app);
                assert!(sent[0].contains("Mode: ticket (one plan for this ticket)\n"));
                assert!(sent[0].contains("Extra instructions from the user: Keep it small\n"));
            },
        );
    }

    #[test]
    fn plan_with_chief_signed_out_shows_the_sign_in_state_and_sends_nothing() {
        with_app(
            "app::tickets_panel::tests::plan_with_chief_signed_out_shows_the_sign_in_state_and_sends_nothing",
            |app, ctx| {
                quiet_chief(app);
                open_panel(app, ctx, TicketsView::Read);
                let link = TicketLink {
                    system: "demo".into(),
                    id: "1".into(),
                    key: Some("DEMO-1".into()),
                    ..Default::default()
                };
                app.runtime.tickets.panel.signin = true;
                let before = app.runtime.plans.len();
                assert_eq!(app.runtime.plan_with_chief(link, TicketMode::Ticket, "").unwrap(), PlanStart::SignIn);
                assert_eq!(app.runtime.plans.len(), before);
                assert!(sent_to_chief(app).is_empty());
                assert!(app.runtime.tickets.panel.signin && app.runtime.panel == PanelMode::Tickets);
                let text = texts(&frame(app, ctx, vec![])).join("\n");
                assert!(text.contains("demo is not signed in"), "{text}");
            },
        );
    }

    #[test]
    fn plan_with_chief_names_the_auto_decision_in_the_board_header() {
        let auto = TicketConfig { system: "demo".into(), mode: TicketMode::Auto, ..Default::default() };
        let mut plan = Plan { ticket_mode: Some(TicketMode::Board), ..Default::default() };
        assert_eq!(super::super::board::mode_label(&plan, &auto), "board (auto)");
        assert_eq!(super::super::board::tracked_label(&plan, &auto), None);
        plan.tasks = vec![
            Task { ticket: Some(TicketLink { id: "1".into(), ..Default::default() }), ..Default::default() },
            Task::default(),
        ];
        assert_eq!(super::super::board::tracked_label(&plan, &auto).as_deref(), Some("1 of 2 tasks tracked"));
        plan.ticket_mode = Some(TicketMode::Ticket);
        assert_eq!(super::super::board::mode_label(&plan, &auto), "ticket (auto)");
        assert_eq!(super::super::board::tracked_label(&plan, &auto), None);
        plan.ticket_mode = None;
        assert_eq!(super::super::board::mode_label(&plan, &auto), "auto (deciding…)");
        let forced = TicketConfig { mode: TicketMode::Board, ..auto };
        assert_eq!(super::super::board::mode_label(&Plan::default(), &forced), "board");
    }

    include!("ticket_demo.rs");
}
