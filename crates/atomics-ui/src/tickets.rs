//! The shared ticket reader. Requests run on the host; drafts, navigation and selection belong to
//! this client, so a second window never moves the ticket someone is reading here.
use std::collections::BTreeSet;

use crate::theme::{Palette, lerp};
use atomics_core::ticket_ui::{self, TicketAction, TicketsInfo};
use atomics_core::tickets::{TicketCard, TicketLink, TicketMode};
use egui::{Color32, Key, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::{Backend, Client, Command, PanelMode, markdown, panel_nav, signin, touch};

/// The height of one result.
const ROW_H: f32 = 58.0;

#[derive(Default)]
pub struct Tickets {
    /// The last legacy desktop navigation request imported by that client.
    pub external_revision: u64,
    pub query: String,
    pub keyword: bool,
    pub selected: Option<usize>,
    pub results: Vec<TicketCard>,
    pub read: Option<TicketCard>,
    pub crumbs: Vec<TicketLink>,
    pub recent: Vec<TicketLink>,
    pub dialog: bool,
    pub extra: String,
    pub ticket_only: bool,
    pub from_board: bool,
    view: Option<usize>,
    filters: BTreeSet<usize>,
    next: Option<String>,
    fallback: Option<String>,
    chief_query: Option<String>,
    error: Option<String>,
    pending: Vec<(String, Part, f64)>,
    context: Option<(String, Option<String>)>,
    focus_query: bool,
    focus_panel: bool,
    focus_extra: bool,
    /// Bring the selected result into view on the next frame.
    reveal: bool,
    searched: Option<(String, bool, usize, Vec<usize>)>,
    children: Vec<TicketCard>,
    related: Vec<TicketCard>,
    section: Option<usize>,
    jump_section: bool,
    expanded_comments: BTreeSet<usize>,
    read_offset: f32,
    read_height: f32,
    read_max_offset: f32,
    reset_scroll: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Search,
    More,
    Read,
    Children,
    Related,
    Plan,
    Link,
}

impl Tickets {
    pub fn show_find(&mut self, focus_query: bool) {
        self.read = None;
        self.dialog = false;
        self.focus_query = focus_query;
        self.pending.retain(|(_, part, _)| !matches!(part, Part::Read | Part::Children | Part::Related));
    }

    pub fn found(&mut self, query: &str, results: Vec<TicketCard>, from_chief: bool) {
        self.read = None;
        if !from_chief {
            self.query = query.into();
        }
        self.selected = (!results.is_empty()).then_some(0);
        self.results = results;
        self.chief_query = from_chief.then(|| query.into());
        self.pending.clear();
        self.searched =
            Some((query.into(), self.keyword, self.view.unwrap_or(0), self.filters.iter().copied().collect()));
    }

    pub fn active(&self) -> bool {
        self.context.is_some()
    }

    /// Gives the panel the keyboard where it next draws: the search field on Find, the panel on a ticket.
    pub fn take_keyboard(&mut self) {
        if self.read.is_some() {
            self.focus_panel = true;
        } else {
            self.focus_query = true;
        }
    }

    /// Asks to plan the ticket being read: a new draft, with the keyboard in its field.
    pub fn open_plan_dialog(&mut self) {
        if self.read.is_some() && !self.dialog {
            self.dialog = true;
            self.extra.clear();
            self.ticket_only = false;
            self.focus_extra = true;
        }
    }

    pub fn step(&mut self, delta: i32, now: f64, commands: &mut Vec<Command>) {
        if self.read.is_none() {
            self.selected = panel_nav::step_index(self.selected, self.results.len(), delta);
            self.reveal = true;
        } else if !self.crumbs.is_empty() {
            if delta < 0 {
                self.back(now, commands);
            }
        } else if let Some(index) =
            self.read.as_ref().and_then(|read| self.results.iter().position(|card| card.link.same_ticket(&read.link)))
            && let Some(next) = panel_nav::step_index(Some(index), self.results.len(), delta)
        {
            self.selected = Some(next);
            self.open(self.results[next].link.clone(), false, now, commands);
        }
    }

    pub fn back(&mut self, now: f64, commands: &mut Vec<Command>) {
        if let Some(previous) = self.crumbs.pop() {
            let crumbs = self.crumbs.clone();
            let from_board = self.from_board;
            self.open(previous, false, now, commands);
            self.crumbs = crumbs;
            self.from_board = from_board;
        } else if self.from_board {
            self.from_board = false;
            commands.push(Command::SetPanel(PanelMode::Plan));
        } else {
            self.show_find(false);
        }
    }

    /// Escape in a ticket: to the results with it selected and in view, or to the board it was opened from.
    fn back_to_list(&mut self, now: f64, commands: &mut Vec<Command>) {
        if self.from_board && self.crumbs.is_empty() {
            return self.back(now, commands);
        }
        if let Some(read) = &self.read
            && let Some(at) = self.results.iter().position(|card| card.link.same_ticket(&read.link))
        {
            self.selected = Some(at);
            self.reveal = true;
        }
        self.show_find(false);
    }

    fn busy(&self, part: Part) -> bool {
        self.pending.iter().any(|(_, waiting, _)| *waiting == part)
    }

    fn request(&mut self, action: TicketAction, part: Part, now: f64, commands: &mut Vec<Command>) {
        let request = match ticket_ui::request_id() {
            Ok(request) => request,
            Err(_) => {
                self.error = Some("Could not create a ticket request. Try again.".into());
                return;
            }
        };
        self.pending.retain(|(_, waiting, _)| *waiting != part);
        self.pending.push((request.clone(), part, now));
        self.error = None;
        commands.push(Command::Ticket { request, action });
    }

    pub fn open(&mut self, ticket: TicketLink, push: bool, now: f64, commands: &mut Vec<Command>) {
        if push {
            if let Some(card) = &self.read {
                self.crumbs.push(card.link.clone());
            }
        } else {
            self.crumbs.clear();
            self.from_board = false;
        }
        self.recent.retain(|link| !link.same_ticket(&ticket));
        self.recent.insert(0, ticket.clone());
        self.recent.truncate(5);
        self.read = Some(
            self.results
                .iter()
                .find(|card| card.link.same_ticket(&ticket))
                .cloned()
                .unwrap_or_else(|| TicketCard { link: ticket.clone(), ..TicketCard::default() }),
        );
        self.children.clear();
        self.related.clear();
        self.dialog = false;
        self.focus_panel = true;
        self.section = None;
        self.expanded_comments.clear();
        self.read_offset = 0.0;
        self.read_height = 0.0;
        self.read_max_offset = 0.0;
        self.reset_scroll = true;
        self.request(TicketAction::Read { ticket: ticket.clone() }, Part::Read, now, commands);
        self.request(TicketAction::Children { ticket: ticket.clone() }, Part::Children, now, commands);
        self.request(TicketAction::Related { ticket }, Part::Related, now, commands);
        commands.push(Command::SetPanel(PanelMode::Tickets));
        commands.push(Command::OpenDiff);
    }

    fn search(&mut self, info: &TicketsInfo, more: bool, now: f64, commands: &mut Vec<Command>) {
        let current = (
            self.query.trim().to_string(),
            self.keyword,
            self.view.unwrap_or(info.default_view),
            self.filters.iter().copied().collect::<Vec<_>>(),
        );
        let (query, keyword, view, filters) = if more {
            let Some(searched) = self.searched.clone() else { return };
            searched
        } else {
            self.searched = Some(current.clone());
            self.results.clear();
            self.selected = None;
            self.next = None;
            self.fallback = None;
            self.chief_query = None;
            self.pending.retain(|(_, part, _)| *part != Part::More);
            current
        };
        let cursor = if more { self.next.clone() } else { None };
        let action = TicketAction::Search { query, keyword, view, filters, cursor };
        self.request(action, if more { Part::More } else { Part::Search }, now, commands);
    }

    fn sync(&mut self, backend: &dyn Backend, info: &TicketsInfo, now: f64, commands: &mut Vec<Command>) {
        let context = (info.system.clone(), backend.project_menu().current);
        if self.context.as_ref().is_some_and(|old| *old != context) {
            let external_revision = self.external_revision;
            *self = Self::default();
            self.external_revision = external_revision;
        }
        self.context = Some(context);
        for reply in &info.replies {
            let Some(index) = self.pending.iter().position(|(id, _, _)| id == &reply.request) else { continue };
            let (_, part, _) = self.pending.remove(index);
            if let Some(error) = &reply.error {
                self.error = Some(error.clone());
                continue;
            }
            match part {
                Part::Search | Part::More => {
                    if part == Part::Search {
                        self.results.clear();
                    }
                    for card in &reply.cards {
                        if !self.results.iter().any(|old| old.link.same_ticket(&card.link)) {
                            self.results.push(card.clone());
                        }
                    }
                    self.selected = self.selected.or((!self.results.is_empty()).then_some(0));
                    self.next = reply.next.clone();
                    self.fallback = reply.fallback.clone();
                }
                Part::Read => {
                    if let Some(card) = reply.cards.first() {
                        let mut card = card.clone();
                        if card.link.url.is_none() {
                            card.link.url = self.read.as_ref().and_then(|old| old.link.url.clone());
                        }
                        self.read = Some(card);
                    }
                }
                Part::Children => self.children = reply.cards.clone(),
                Part::Related => self.related = reply.cards.clone(),
                Part::Plan => {
                    self.dialog = false;
                    self.focus_panel = true;
                    if let Some(plan) = &reply.plan {
                        if let Some(index) = backend.plan_list().iter().position(|row| &row.id == plan) {
                            commands.push(Command::ShowPlan(index));
                            commands.push(Command::SetPanel(PanelMode::Tickets));
                            commands.push(Command::OpenDiff);
                        }
                        if let Some(chief) = backend.session_list().iter().find(|session| session.chief) {
                            commands.push(Command::Activate { session: chief.id.clone(), open_diff: false });
                        }
                    }
                }
                Part::Link => commands.push(Command::Toast("Ticket linked to the plan".into())),
            }
        }
        if self.pending.iter().any(|(_, _, sent)| now - sent > 120.0) {
            self.pending.retain(|(_, _, sent)| now - sent <= 120.0);
            self.error = Some("The ticket request timed out. Try again when connected.".into());
        }
    }
}

fn can(backend: &dyn Backend) -> bool {
    backend.available(&Command::Ticket {
        request: String::new(),
        action: TicketAction::Read { ticket: TicketLink::default() },
    })
}

/// TICKETS: the selected plan's tree, or Find and this client's recent reads.
pub fn sidebar(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.tickets().is_none() {
        return;
    }
    let p = client.palette.clone();
    crate::sidebar::section(ui, &p, "TICKETS");
    if touch::button(ui, client.touch, "Find tickets…").clicked() {
        client.tickets.show_find(true);
        client.send(Command::SetPanel(PanelMode::Tickets));
        client.send(Command::OpenDiff);
    }
    let hints = backend.view_hints();
    let tree = hints.plan_view.then(|| backend.ticket_tree(hints.selected_plan)).flatten();
    let links: Vec<_> = match tree {
        Some(tree) => tree
            .ticket
            .iter()
            .map(|link| (0, link.clone()))
            .chain(tree.tasks.iter().chain(&tree.unassigned).map(|link| (1, link.clone())))
            .collect(),
        None => client.tickets.recent.iter().map(|link| (0, link.clone())).collect(),
    };
    for (depth, link) in links {
        let text = format!("{} {}", link.label(), link.title.as_deref().unwrap_or(""));
        let clicked = ui
            .horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                ui.add(touch::sized(egui::Button::selectable(false, text).truncate(), client.touch)).clicked()
            })
            .inner;
        if clicked {
            client.tickets.open(link, false, ui.input(|i| i.time), &mut client.commands);
        }
    }
    ui.add_space(10.0);
}

/// Search, read and the Plan with chief dialog, drawn by every platform.
pub fn panel(ui: &mut egui::Ui, rect: egui::Rect, backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.tickets() else { return };
    panel_nav::focusable(ui, rect);
    let now = ui.input(|i| i.time);
    let waiting = client.commands.len();
    client.tickets.sync(backend, &info, now, &mut client.commands);
    // A reply can come on a frame no input drew; what it asks for shows on the next one.
    if client.commands.len() > waiting {
        ui.ctx().request_repaint();
    }
    let p = client.palette.clone();
    let active = can(backend);
    let mut panel = std::mem::take(&mut client.tickets);
    // Asking again while it has the keys would reset the focus lock that keeps Escape for the panel.
    if std::mem::take(&mut panel.focus_panel) && !ui.memory(|m| m.has_focus(panel_nav::focus_id())) {
        ui.memory_mut(|m| m.request_focus(panel_nav::focus_id()));
    }
    // The open dialog holds the keyboard: what moved it to the panel behind must not start reading its keys.
    if panel.dialog && client.touch.is_none() && ui.memory(|m| m.has_focus(panel_nav::focus_id())) {
        ui.memory_mut(|m| m.request_focus(plan_extra_id()));
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink2(vec2(10.0, 6.0))).id_salt("tickets-panel"), |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Tickets");
            ui.label(RichText::new(info.identity.as_deref().unwrap_or(&info.system)).small().color(p.muted));
            if touch::button(ui, client.touch, "Find").clicked() {
                panel.show_find(true);
            }
            if touch::button(ui, client.touch, "×").on_hover_text("Close the panel").clicked() {
                client.send(Command::CloseDiff);
            }
        });
        ui.separator();
        if signin::banner(ui, backend, client) {
            panel.dialog = false;
            return;
        }
        if ui.memory(|m| m.has_focus(panel_nav::focus_id())) {
            for (key, delta) in [(Key::ArrowDown, 1), (Key::ArrowUp, -1)] {
                if ui.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, key)) {
                    panel.step(delta, now, &mut client.commands);
                }
            }
        }
        if let Some(error) = &panel.error {
            ui.label(RichText::new(error).color(p.error));
        }
        if panel.read.is_some() {
            read(ui, backend, client, &info, &mut panel, active, now);
        } else {
            find(ui, client, &info, &mut panel, active, now);
        }
    });
    if panel.dialog {
        plan_dialog(ui.ctx(), client, &info, &mut panel, active, now);
    }
    if !panel.pending.is_empty() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    client.tickets = panel;
}

fn find(ui: &mut egui::Ui, client: &mut Client, info: &TicketsInfo, panel: &mut Tickets, active: bool, now: f64) {
    let mut search = false;
    let query_id = egui::Id::NULL.with("tickets-query");
    if ui.memory(|m| m.has_focus(query_id)) {
        if info.search && info.semantic_search && panel_nav::consume_bare(ui, Key::Tab) {
            panel.keyword = !panel.keyword;
            search = true;
        }
        if panel_nav::consume_bare(ui, Key::ArrowDown) || panel_nav::consume_bare(ui, Key::Escape) {
            ui.memory_mut(|m| m.request_focus(panel_nav::focus_id()));
        }
    }
    ui.horizontal(|ui| {
        let response = ui.add(
            crate::controls::single_line(&mut panel.query)
                .id(query_id)
                .hint_text("Search tickets…")
                .desired_width((ui.available_width() - 84.0).max(40.0)),
        );
        if std::mem::take(&mut panel.focus_query) {
            response.request_focus();
        }
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    query_id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        search |= response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        search |= ui.add_enabled(active, touch::sized(egui::Button::new("Search"), client.touch)).clicked();
    });
    ui.horizontal_wrapped(|ui| {
        if info.search && info.semantic_search {
            for (keyword, label) in [(false, "meaning"), (true, "keyword")] {
                if ui.selectable_label(panel.keyword == keyword, label).clicked() {
                    panel.keyword = keyword;
                    search |= !panel.query.trim().is_empty();
                }
            }
        }
        for (index, label) in info.views.iter().enumerate() {
            let selected = panel.query.trim().is_empty() && panel.view.unwrap_or(info.default_view) == index;
            if ui.add(touch::sized(egui::Button::selectable(selected, label.as_str()), client.touch)).clicked() {
                panel.view = Some(index);
                panel.query.clear();
                search = true;
            }
        }
    });
    if ui.memory(|m| m.has_focus(query_id) || m.has_focus(panel_nav::focus_id())) {
        for (index, key) in
            [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9]
                .into_iter()
                .take(info.filters.len())
                .enumerate()
        {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, key)) {
                ui.input_mut(|i| i.events.retain(|event| !matches!(event, egui::Event::Text(_))));
                if !panel.filters.remove(&index) {
                    panel.filters.insert(index);
                }
                search = true;
            }
        }
    }
    for key in panel_nav::take_keys(ui, &[Key::ArrowLeft, Key::ArrowRight]) {
        let view = panel.view.unwrap_or(info.default_view);
        if let Some(next) =
            panel_nav::step_index(Some(view), info.views.len(), if key == Key::ArrowRight { 1 } else { -1 })
        {
            panel.view = Some(next);
            panel.query.clear();
            search = true;
        }
    }
    ui.horizontal_wrapped(|ui| {
        for (index, label) in info.filters.iter().enumerate() {
            if ui
                .add(touch::sized(egui::Button::selectable(panel.filters.contains(&index), label), client.touch))
                .clicked()
            {
                if !panel.filters.remove(&index) {
                    panel.filters.insert(index);
                }
                search = true;
            }
        }
    });
    if panel.searched.is_none() && !info.views.is_empty() && active {
        search = true;
    }
    if search && active {
        panel.search(info, false, now, &mut client.commands);
    }
    let busy = panel.busy(Part::Search) || panel.busy(Part::More);
    let count = panel.results.len();
    let suffix = if count == 1 { "" } else { "s" };
    let how = panel.searched.as_ref().map_or(String::new(), |(query, keyword, view, _)| {
        if query.is_empty() {
            info.views.get(*view).map_or(String::new(), |label| format!(" · {label}"))
        } else if panel.fallback.is_none() && info.semantic_search && (!keyword || !info.search) {
            " by meaning".into()
        } else {
            " by keyword".into()
        }
    });
    let status = if busy {
        "Loading tickets…".to_string()
    } else if panel.searched.is_some() {
        format!("{count} ticket{suffix}{how}")
    } else if active {
        "Type a query and press Enter.".to_string()
    } else {
        String::new()
    };
    ui.label(RichText::new(status).size(11.0).color(client.palette.muted));
    if let Some(query) = &panel.chief_query {
        ui.label(format!("from the chief: “{query}”"));
    }
    if let Some(fallback) = &panel.fallback {
        ui.label(format!("meaning search: {fallback}"));
    }
    let keys = panel_nav::take_keys(ui, &[Key::ArrowDown, Key::ArrowUp, Key::Home, Key::End, Key::Enter, Key::Escape]);
    let mut open = None;
    for key in keys {
        match key {
            Key::ArrowDown | Key::ArrowUp => {
                if key == Key::ArrowDown
                    && panel.selected == panel.results.len().checked_sub(1)
                    && panel.next.is_some()
                    && !busy
                    && active
                {
                    panel.search(info, true, now, &mut client.commands);
                } else {
                    panel.selected = panel_nav::step_index(
                        panel.selected,
                        panel.results.len(),
                        if key == Key::ArrowDown { 1 } else { -1 },
                    );
                    panel.reveal = true;
                }
            }
            Key::Home => {
                panel.selected = (!panel.results.is_empty()).then_some(0);
                panel.reveal = true;
            }
            Key::End => {
                panel.selected = panel.results.len().checked_sub(1);
                panel.reveal = true;
            }
            Key::Enter => {
                open = panel.selected.and_then(|index| panel.results.get(index)).map(|card| card.link.clone())
            }
            Key::Escape => client.send(Command::CloseDiff),
            _ => {}
        }
    }
    let scroll_id = ui.make_persistent_id(egui::IdSalt::new("tickets-find"));
    let mut jump = None;
    if std::mem::take(&mut panel.reveal)
        && let Some(selected) = panel.selected
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
    let jump =
        panel_nav::keyboard_scroll(ui, scroll_id, ROW_H, 0..panel.results.len(), &[Key::PageUp, Key::PageDown], jump);
    let mut area = egui::ScrollArea::vertical().id_salt("tickets-find").auto_shrink([false, false]);
    if let Some(offset) = jump {
        area = area.vertical_scroll_offset(offset);
    }
    area.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let range = score_range(&panel.results);
        for (index, card) in panel.results.iter().enumerate() {
            let response = find_row(ui, card, panel.selected == Some(index), range, &info.states, &client.palette);
            if response.clicked() {
                panel.selected = Some(index);
                open = Some(card.link.clone());
                ui.memory_mut(|m| m.request_focus(panel_nav::focus_id()));
            }
        }
        if panel.next.is_some() {
            ui.add_space(4.0);
            if ui.add_enabled(active && !busy, touch::sized(egui::Button::new("More"), client.touch)).clicked() {
                panel.search(info, true, now, &mut client.commands);
            }
        }
    });
    if let Some(ticket) = open {
        panel.open(ticket, false, now, &mut client.commands);
    }
}

/// What a ticket is at a glance: its state in that state's color, then its kind, assignee and last update.
fn facts(ui: &mut egui::Ui, card: &TicketCard, info: &TicketsInfo, p: &Palette) {
    if let Some(state) = &card.link.state {
        ui.label(RichText::new(format!("[{state}]")).size(12.0).color(state_color(&info.states, state, p)));
    }
    let facts = [
        card.kind.clone(),
        card.assignee.as_ref().map(|a| a.clone().unwrap_or_else(|| "unassigned".into())),
        card.link.updated_at.as_ref().map(|at| format!("updated {at}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    ui.label(RichText::new(facts).size(11.0).color(p.muted));
}

/// A section title in the reader, under the ticket's own text.
fn section_title(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).strong().size(12.0).color(p.muted));
    ui.separator();
}

fn read(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    info: &TicketsInfo,
    panel: &mut Tickets,
    active: bool,
    now: f64,
) {
    let card = panel.read.clone().expect("checked");
    let mut open = None;
    let mut back = false;
    ui.horizontal_wrapped(|ui| {
        back |= touch::button(ui, client.touch, "← Back").clicked();
        if let Some(url) = &card.link.url {
            web_link(ui, "Open in browser", url);
        }
        if ui
            .add_enabled(
                active && !panel.busy(Part::Read),
                touch::sized(egui::Button::new("Plan with chief"), client.touch),
            )
            .clicked()
        {
            panel.open_plan_dialog();
        }
        ui.menu_button("Link to plan", |ui| {
            for plan in
                backend.plan_list().into_iter().filter(|plan| plan.status == atomics_core::plan::PlanStatus::Open)
            {
                if ui.add_enabled(active, egui::Button::new(&plan.goal).wrap()).clicked() {
                    panel.request(
                        TicketAction::Link { plan: plan.id, task: None, ticket: card.link.clone() },
                        Part::Link,
                        now,
                        &mut client.commands,
                    );
                    ui.close();
                }
            }
        });
        if touch::button(ui, client.touch, "Refresh").clicked() && active {
            open = Some((card.link.clone(), false));
        }
    });
    if panel_nav::take_keys(ui, &[Key::Escape]).contains(&Key::Escape) {
        panel.back_to_list(now, &mut client.commands);
        ui.ctx().request_repaint();
        return;
    }
    // Alt+← is the panel's while it has the keyboard; in a terminal it is the terminal's.
    if ui.memory(|m| m.has_focus(panel_nav::focus_id()))
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::ALT, Key::ArrowLeft))
    {
        back = true;
    }
    if back {
        panel.back(now, &mut client.commands);
        ui.ctx().request_repaint();
        return;
    }
    let p = client.palette.clone();
    let parts = card.body.as_deref().map(markdown::sections).unwrap_or_default();
    let comments_start = parts.len();
    let children_start = comments_start + card.comments.len();
    let related_start = children_start + panel.children.len();
    let stops = related_start + panel.related.len();
    let trail = panel.crumbs.iter().map(|link| link.label()).chain([card.link.label()]).collect::<Vec<_>>().join(" › ");
    let place = match stops {
        0 => trail,
        stops => format!("{trail} · section {}/{stops}", panel.section.map_or("–".into(), |at| (at + 1).to_string())),
    };
    ui.label(RichText::new(place).size(11.0).color(p.muted));
    let title = card.link.title.as_deref().unwrap_or(card.link.label());
    ui.add(egui::Label::new(RichText::new(title).size(16.0).strong().color(p.foreground)).wrap());
    ui.horizontal_wrapped(|ui| {
        facts(ui, &card, info, &p);
        if !panel.children.is_empty() {
            ui.label(RichText::new(format!("· {} children", panel.children.len())).size(11.0).color(p.muted));
        }
    });
    ui.horizontal_wrapped(|ui| {
        for link in &card.links {
            let text = [link.label.as_deref(), link.status.as_deref(), link.ci.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            let url = link.url.as_deref().filter(|url| markdown::web_url(url).is_some());
            let button = ui.add_enabled_ui(url.is_some(), |ui| touch::button(ui, client.touch, text)).inner;
            if let Some(url) = url
                && button.on_hover_text(markdown::web_url_hover(url)).clicked()
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    });
    if panel.busy(Part::Read) {
        ui.label(RichText::new("loading…").size(11.0).color(p.muted));
    }
    ui.separator();
    for key in panel_nav::take_keys(ui, &[Key::PageUp, Key::PageDown, Key::Enter]) {
        match key {
            Key::PageUp | Key::PageDown => {
                panel.section = panel_nav::step_index(panel.section, stops, if key == Key::PageDown { 1 } else { -1 });
                panel.jump_section = true;
            }
            Key::Enter => {
                if let Some(section) = panel.section {
                    if section >= related_start {
                        open = panel.related.get(section - related_start).map(|card| (card.link.clone(), true));
                    } else if section >= children_start {
                        open = panel.children.get(section - children_start).map(|card| (card.link.clone(), true));
                    } else if section >= comments_start {
                        let index = section - comments_start;
                        if !panel.expanded_comments.remove(&index) {
                            panel.expanded_comments.insert(index);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let scroll_id = ui.make_persistent_id(egui::IdSalt::new("tickets-read"));
    let scroll_keys = [Key::ArrowUp, Key::ArrowDown, Key::Home, Key::End];
    let mut reset = std::mem::take(&mut panel.reset_scroll).then_some(0.0);
    if panel.crumbs.is_empty() && panel.results.iter().any(|result| result.link.same_ticket(&card.link)) {
        for (key, repeat) in panel_nav::take_key_presses_for(ui, panel_nav::focus_id(), &[Key::ArrowUp, Key::ArrowDown])
        {
            let down = key == Key::ArrowDown;
            let offset = reset.unwrap_or(panel.read_offset);
            if (down && offset < panel.read_max_offset) || (!down && offset > 0.0) {
                reset = Some((offset + if down { 18.0 } else { -18.0 }).clamp(0.0, panel.read_max_offset));
            } else if !repeat && !panel.busy(Part::Read) {
                panel.step(if down { 1 } else { -1 }, now, &mut client.commands);
                return;
            }
        }
    }
    let jump = panel_nav::keyboard_scroll(
        ui,
        scroll_id,
        18.0,
        0..(panel.read_height / 18.0).ceil() as usize,
        &scroll_keys,
        reset,
    );
    let mut area = egui::ScrollArea::vertical().id_salt("tickets-read").auto_shrink([false, false]);
    if let Some(offset) = jump {
        area = area.vertical_scroll_offset(offset.min(panel.read_max_offset));
    }
    let selected = panel.section;
    let jump_section = std::mem::take(&mut panel.jump_section);
    let section = |ui: &mut egui::Ui, index, body: &mut dyn FnMut(&mut egui::Ui)| {
        let frame = egui::Frame::new().inner_margin(egui::Margin { left: 8, right: 0, top: 2, bottom: 2 });
        let response = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
        if selected == Some(index) {
            let rect = response.response.rect;
            ui.painter().line_segment([rect.left_top(), rect.left_bottom()], Stroke::new(3.0, client.palette.accent));
            if jump_section {
                ui.scroll_to_rect(rect, Some(egui::Align::TOP));
            }
        }
    };
    let output = area.show(ui, |ui| {
        for (index, (_, body)) in parts.iter().enumerate() {
            section(ui, index, &mut |ui| markdown_text(ui, body, &p));
        }
        if card.body.is_none() && !panel.busy(Part::Read) {
            ui.label(RichText::new("No description.").color(p.muted));
        }
        if card.body.is_none() && !card.fields.is_empty() {
            egui::Grid::new("ticket-fields").num_columns(2).show(ui, |ui| {
                for (name, value) in &card.fields {
                    ui.label(RichText::new(name).color(p.muted).size(11.0));
                    ui.add(egui::Label::new(value).wrap());
                    ui.end_row();
                }
            });
        }
        if !card.comments.is_empty() {
            section_title(ui, &format!("Comments ({})", card.comments.len()), &p);
        }
        for (index, comment) in card.comments.iter().enumerate() {
            section(ui, comments_start + index, &mut |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(comment.author.as_deref().unwrap_or("someone")).strong().size(12.0));
                    if let Some(kind) = comment.author_kind.as_deref().or(comment.kind.as_deref()) {
                        ui.label(RichText::new(kind).size(10.5).color(p.info));
                    }
                    if let Some(at) = &comment.at {
                        ui.label(RichText::new(at).size(11.0).color(p.muted));
                    }
                });
                let lines = comment.text.lines().count();
                if lines <= 12 || panel.expanded_comments.contains(&index) {
                    markdown_text(ui, &comment.text, &p);
                } else {
                    markdown_text(ui, &comment.text.lines().take(12).collect::<Vec<_>>().join("\n"), &p);
                    ui.label(
                        RichText::new(format!("… {} more lines (Enter expands)", lines - 12)).size(11.0).color(p.muted),
                    );
                }
            });
        }
        for (title, part, start, cards) in [
            ("Children", Part::Children, children_start, &panel.children),
            ("Related", Part::Related, related_start, &panel.related),
        ] {
            if panel.busy(part) {
                section_title(ui, &format!("{title}…"), &p);
            } else if !cards.is_empty() {
                section_title(ui, &format!("{title} ({})", cards.len()), &p);
            }
            for (index, related) in cards.iter().enumerate() {
                section(ui, start + index, &mut |ui| {
                    let label = format!("{} {}", related.link.label(), related.link.title.as_deref().unwrap_or(""));
                    if ui.add(touch::sized(egui::Button::new(label).wrap(), client.touch)).clicked() {
                        open = Some((related.link.clone(), true));
                    }
                    ui.horizontal_wrapped(|ui| facts(ui, related, info, &p));
                });
            }
        }
        ui.add_space(24.0);
    });
    panel.read_offset = output.state.clamped_offset().y;
    panel.read_height = output.content_size.y;
    panel.read_max_offset = (output.content_size.y - output.inner_rect.height()).max(0.0);
    if let Some((ticket, push)) = open {
        panel.open(ticket, push, now, &mut client.commands);
    }
}

/// The keyboard's place in the Plan with chief dialog.
fn plan_extra_id() -> egui::Id {
    egui::Id::NULL.with("tickets-plan-extra")
}

/// Takes a bare Return out of the input, whether held or not; whether it was a new press. A Return still held
/// from the chord that opened the dialog must not start the plan.
fn take_return(ui: &egui::Ui) -> bool {
    let mut pressed = false;
    ui.input_mut(|i| {
        i.events.retain(|event| match event {
            egui::Event::Key { key: Key::Enter, pressed: true, modifiers, repeat, .. } if modifiers.is_none() => {
                pressed |= !repeat;
                false
            }
            _ => true,
        })
    });
    pressed
}

fn plan_dialog(
    ctx: &egui::Context,
    client: &mut Client,
    info: &TicketsInfo,
    panel: &mut Tickets,
    active: bool,
    now: f64,
) {
    let Some(card) = panel.read.clone() else {
        panel.dialog = false;
        return;
    };
    let mut go = false;
    let mut cancel = false;
    let mode = if panel.ticket_only {
        TicketMode::Ticket
    } else if info.mode != TicketMode::Auto {
        info.mode
    } else if panel.children.is_empty() {
        TicketMode::Ticket
    } else {
        TicketMode::Board
    };
    egui::Window::new(format!("Plan {} with the chief?", card.link.label()))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .default_width(320.0)
        .show(ctx, |ui| {
            ui.set_max_width((ctx.content_rect().width() - 32.0).min(440.0));
            let ready = active && !panel.busy(Part::Children) && !panel.busy(Part::Plan);
            // Return and Escape are the dialog's, taken before its field can see them, unless a click gave the
            // keyboard to something else: a terminal behind it, or one of its own buttons.
            if ui.memory(|m| m.focused().is_none() || m.has_focus(plan_extra_id())) {
                go |= take_return(ui) && ready;
                cancel |= panel_nav::consume_bare(ui, Key::Escape);
            }
            ui.label(card.link.title.as_deref().unwrap_or(card.link.label()));
            ui.label(if mode == TicketMode::Board {
                format!("{} child tickets will become tasks (board).", panel.children.len())
            } else {
                "Ticket mode: one plan; progress is reported on this ticket.".into()
            });
            if info.mode != TicketMode::Ticket && !panel.children.is_empty() {
                ui.checkbox(&mut panel.ticket_only, "one plan for this ticket only (ticket mode)");
            }
            ui.label("Extra instructions for the chief (optional)");
            let extra = ui.add(
                crate::controls::single_line(&mut panel.extra)
                    .id(plan_extra_id())
                    .char_limit(atomics_core::tickets::PLAN_EXTRA_MAX)
                    .desired_width(f32::INFINITY),
            );
            // After the field is added: a new window sizes itself first, with its widgets disabled, and a disabled
            // widget lets go of the focus. A phone's keyboard stays down until its field is tapped.
            if std::mem::take(&mut panel.focus_extra) && client.touch.is_none() {
                extra.request_focus();
            }
            ui.label("The chief reads the ticket, proposes a plan and waits for your approval.");
            if let Some(error) = &panel.error {
                ui.label(error);
            }
            ui.horizontal_wrapped(|ui| {
                go |=
                    ui.add_enabled(ready, touch::sized(egui::Button::new("Plan with chief ⏎"), client.touch)).clicked();
                cancel |= touch::button(ui, client.touch, "Cancel").clicked();
            });
        });
    if cancel {
        panel.dialog = false;
        ctx.memory_mut(|m| m.request_focus(panel_nav::focus_id()));
        ctx.request_repaint();
    } else if go {
        panel.request(
            TicketAction::Plan { ticket: card.link, mode, extra: panel.extra.clone() },
            Part::Plan,
            now,
            &mut client.commands,
        );
    }
}

fn web_link(ui: &mut egui::Ui, label: &str, url: &str) {
    if markdown::web_url(url).is_some() && ui.link(label).on_hover_text(markdown::web_url_hover(url)).clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// The size of a markdown heading, by its level.
const HEADING_SIZES: [f32; 6] = [20.0, 17.0, 15.0, 14.0, 13.0, 13.0];
/// The size of markdown's running text.
const TEXT_SIZE: f32 = 13.0;

/// Markdown as egui text. A link opens only as a web address; any other keeps its text.
fn markdown_text(ui: &mut egui::Ui, source: &str, p: &Palette) {
    let inline = |ui: &mut egui::Ui, parts: &[markdown::Inline], size: f32, strong: bool| {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for part in parts {
                match part {
                    markdown::Inline::Text(text) => {
                        let text = RichText::new(text).size(size).color(p.foreground);
                        ui.label(if strong { text.strong() } else { text });
                    }
                    markdown::Inline::Bold(text) => {
                        ui.label(RichText::new(text).size(size).strong().color(p.foreground));
                    }
                    markdown::Inline::Code(text) => {
                        ui.label(RichText::new(text).monospace().size(size - 0.5).background_color(p.raised));
                    }
                    markdown::Inline::Link { text, url } => {
                        let link = ui.link(RichText::new(text).size(size));
                        if markdown::web_url(url).is_some()
                            && link.on_hover_text(markdown::web_url_hover(url)).clicked()
                        {
                            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                        }
                    }
                }
            }
        });
    };
    for block in markdown::parse(source) {
        match block {
            markdown::Block::Heading(level, parts) => {
                ui.add_space(6.0);
                inline(ui, &parts, HEADING_SIZES[level as usize - 1], true);
            }
            markdown::Block::Paragraph(parts) => inline(ui, &parts, TEXT_SIZE, false),
            markdown::Block::Quote(parts) => {
                ui.horizontal(|ui| {
                    ui.add_space(6.0);
                    ui.vertical(|ui| inline(ui, &parts, TEXT_SIZE, false));
                });
            }
            markdown::Block::Item(depth, marker, parts) => {
                ui.horizontal_top(|ui| {
                    ui.add_space(8.0 + depth as f32 * 14.0);
                    ui.label(RichText::new(marker).color(p.muted));
                    ui.vertical(|ui| inline(ui, &parts, TEXT_SIZE, false));
                });
            }
            markdown::Block::Code(code) => {
                egui::Frame::new().fill(p.raised).corner_radius(4).inner_margin(6).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(code).monospace().size(12.0)).wrap());
                });
            }
            markdown::Block::Rule => {
                ui.separator();
            }
        }
    }
}

pub use crate::trace_lane::{chip, fit, relative_score, score_range};

pub fn state_color(states: &atomics_core::tickets::StateMap, state: &str, p: &Palette) -> Color32 {
    states.status_of(state).map_or(p.muted, |status| crate::status::task_color(status, p))
}

/// One result: key and title, then score bar, state, assignee and kind, then the snippet.
pub fn find_row(
    ui: &mut egui::Ui,
    card: &TicketCard,
    selected: bool,
    range: Option<(f64, f64)>,
    states: &atomics_core::tickets::StateMap,
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
    let key = fit(ui, card.link.label(), p.mono(12.0), p.accent, right - left);
    let key_w = key.size().x;
    painter.galley(pos2(left, rect.top() + 5.0), key, p.accent);
    let title = card.link.title.as_deref().unwrap_or("");
    let galley = fit(ui, title, p.ui(12.0), p.foreground, right - left - key_w - 8.0);
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
        x = chip(ui, &painter, pos2(x, y), state, state_color(states, state, p), right, p);
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
        let galley = fit(ui, text, p.ui(11.0), p.muted, right - x);
        let width = galley.size().x;
        painter.galley(pos2(x, y + 1.0), galley, p.muted);
        x += width + 10.0;
    }
    if let Some(snippet) = card.snippet.as_deref().filter(|s| !s.is_empty()) {
        let galley = fit(ui, &snippet.replace("**", ""), p.ui(11.0), p.muted, right - left);
        painter.galley(pos2(left, rect.top() + 40.0), galley, p.muted);
    }
    response
}
