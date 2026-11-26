//! The full window's start screen, where work begins: it shows while no board is on screen and nothing runs, and
//! after **New work** until the window goes somewhere else. A box to tell the chief what to do (simple mode's
//! composer), with a ticket to plan from where a ticket system is configured; the recent boards to continue; and,
//! quietly, an agent to start by hand. While it shows and the chief has nothing to do, the chief is held behind it
//! rather than docked beside it ([`Home::held`]): the composer is the way in, and the chief docks again once it
//! works, a board opens, or the user asks for it.

use atomics_core::session::AgentStatus;
use atomics_core::ticket_ui::TicketAction;
use atomics_core::tickets::{TicketCard, TicketLink};
use egui::{Align2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::flow::font;
use crate::simple::look::{self, Weight};
use crate::theme::lerp;
use crate::{Backend, Client, Command, PlanRow, ViewHints, boards, simple};

/// The widest the start screen's column gets.
const COLUMN: f32 = 720.0;
/// How many recent boards it offers to continue.
const RECENT: usize = 4;
const CARD: f32 = 66.0;

/// What this window holds of its start screen.
#[derive(Debug, Default)]
pub struct Home {
    /// New work was asked for: the start screen shows from the next frame.
    asked: bool,
    /// Where New work left the window; the start screen shows until it goes somewhere else.
    at: Option<ViewHints>,
    /// The start screen was on screen in the last frame.
    pub shown: bool,
    /// The user asked for the chief while the start screen showed: it docks as usual from then on.
    pub summoned: bool,
    /// The next thing to do starts in a session of its own.
    pub fresh: bool,
    /// The board whose session was last brought in front.
    pub board: Option<String>,
    /// The chief is open, but held behind the start screen in this frame.
    pub held: bool,
    ticket: TicketPick,
}

/// The ticket picked for the next message, and the search that finds one.
#[derive(Debug, Default)]
struct TicketPick {
    query: String,
    /// The query last searched, and when it last changed, for a search a moment after typing stops.
    searched: Option<String>,
    edited: f64,
    search: Option<String>,
    results: Vec<TicketCard>,
    picked: Option<TicketLink>,
    /// The plan asked for with the picked ticket, by request id.
    planning: Option<String>,
    error: Option<String>,
    focus: bool,
}

impl Home {
    /// Whether the user asked for the start screen and has not gone anywhere since.
    pub fn asked(&self) -> bool {
        self.asked || self.at.is_some()
    }
}

/// Goes to the start screen: no board, no diff, the composer ready, and the chief back behind it unless it works.
pub fn new_work(backend: &dyn Backend, client: &mut Client) {
    for command in [Command::SetView(None), Command::CloseDiff] {
        if backend.available(&command) {
            client.send(command);
        }
    }
    client.home.asked = true;
    client.home.summoned = false;
    client.simple.focus_composer = true;
}

/// Follows where the window is: the start screen asked for stays until the place changes.
pub fn track(backend: &dyn Backend, client: &mut Client) {
    let hints = backend.view_hints();
    let home = &mut client.home;
    if std::mem::take(&mut home.asked) {
        home.at = Some(hints);
    } else if home.at.as_ref().is_some_and(|at| *at != hints) {
        home.at = None;
    }
}

/// Brings the session of the board on screen in front as the window goes to it. From there the user may choose another.
pub fn follow_board(backend: &dyn Backend, client: &mut Client) {
    let hints = backend.view_hints();
    let plans = backend.plans();
    let board = plans.get(hints.selected_plan).filter(|row| hints.plan_view && !row.id.is_empty());
    let shown = board.map(|row| row.id.clone());
    if client.home.board == shown {
        return;
    }
    client.home.board = shown;
    if let Some(row) = board {
        simple::select_provider_session(backend, client, &row.thread);
    }
}

/// The chief works, or waits for the user: then it shows wherever it is, start screen or not.
pub fn chief_busy(backend: &dyn Backend) -> bool {
    simple::chief_session(backend).is_some_and(|(_, chief)| chief.status == AgentStatus::Working || chief.needs_user())
}

/// What a command does to the start screen and the chief held behind it; true where that is all it does. The chief
/// toggle shows the held chief rather than hiding it, and from then on the chief is the user's, as it is after
/// docking or floating it; hiding it leaves nothing held. Going to a session or a board leaves the start screen, the
/// one already active included. Each platform asks before it carries the command out.
pub fn chief_command(client: &mut Client, command: &Command) -> bool {
    match command {
        Command::Activate { .. }
        | Command::OpenTaskSession(_)
        | Command::Resume(_)
        | Command::ShowPlan(_)
        | Command::ViewPlanHistory(_) => {
            client.home.asked = false;
            client.home.at = None;
            false
        }
        Command::ToggleChief => {
            if client.home.shown {
                client.home.summoned = true;
            }
            std::mem::take(&mut client.home.held)
        }
        Command::SetChiefDock(_) => {
            client.home.summoned = true;
            client.home.held = false;
            false
        }
        Command::HideChief => {
            client.home.held = false;
            false
        }
        _ => false,
    }
}

/// The start screen in `rect`.
pub fn draw(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.background);
    let ink = look::Ink::of(&p);
    simple::flush(backend, client);
    client.simple.outbox.retain(|out| !out.sent);
    sync_tickets(backend, client);
    let column_width = (rect.width() - 48.0).clamp(1.0, COLUMN);
    let chief = simple::chief_session(backend).map(|(_, session)| session);
    let composer_h = simple::composer_height(ui, column_width, rect.height(), &client.simple.composer);
    let mut recent = recent_boards(&boards::in_frame(ui.ctx(), backend).1);
    let columns = if column_width >= 520.0 { 2 } else { 1 };
    // What fits under the composer: fewer cards in a short window, none in a very short one.
    let room = rect.height() - composer_h - 246.0;
    let rows_fit = ((room - 40.0) / (CARD + 10.0)).floor().max(0.0) as usize;
    recent.truncate((rows_fit * columns).min(RECENT));
    let rows = recent.len().div_ceil(columns);
    let cards_h = if rows == 0 { 0.0 } else { 40.0 + rows as f32 * (CARD + 10.0) };
    let content_h = 44.0 + composer_h + 92.0 + cards_h;
    let top = rect.top() + ((rect.height() - content_h) * 0.38).max(24.0);
    let left = rect.center().x - column_width / 2.0;
    look::paint(
        ui.painter(),
        pos2(rect.center().x, top + 30.0),
        Align2::CENTER_BOTTOM,
        "Start work with the chief",
        look::DISPLAY,
        Weight::Semibold,
        ink.primary,
    );
    let composer = Rect::from_min_size(pos2(left, top + 44.0), vec2(column_width, composer_h));
    let hint = match &client.home.ticket.picked {
        Some(link) => format!("What should the chief do with {}?", link.label()),
        None => "Tell the chief what to do…".to_string(),
    };
    if let Some(plain) = simple::composer_input(ui, composer, chief.as_ref(), Some(&hint), backend, client) {
        send(plain, chief.as_ref(), ui.input(|i| i.time), backend, client);
    }
    flow_record(ui, "home-composer", composer);
    // Under the composer: the ticket on the left, an agent to start by hand on the right, both quiet.
    let controls = Rect::from_min_size(pos2(left, composer.bottom() + 10.0), vec2(column_width, 28.0));
    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("home-controls")
            .max_rect(controls)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    ticket_picker(&mut row, backend, client);
    row.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| start_agent(ui, backend, client));
    // A picked ticket's plan is made by the session in front.
    let mut under = controls.bottom() + 6.0;
    if backend.chief_threads() && client.home.ticket.picked.is_none() {
        let area = Rect::from_min_size(pos2(left, under), vec2(column_width, 44.0));
        let mut row = ui.new_child(egui::UiBuilder::new().id_salt("home-context").max_rect(area));
        simple::fresh_context(&mut row, &mut client.home.fresh, chief.as_ref(), &p, None::<fn(&mut egui::Ui, bool)>);
        under = row.min_rect().bottom() + 6.0;
    }
    if let Some(error) = client.home.ticket.error.clone() {
        let at = pos2(left, under);
        ui.painter().text(at, Align2::LEFT_TOP, error, font(look::SMALL), p.warning);
    }
    if recent.is_empty() {
        return;
    }
    let heading = under + 28.0;
    look::paint(
        ui.painter(),
        pos2(left, heading),
        Align2::LEFT_TOP,
        "Continue recent boards…",
        look::SMALL,
        Weight::Medium,
        ink.secondary,
    );
    let gap = 10.0;
    let width = (column_width - gap * (columns - 1) as f32) / columns as f32;
    for (n, (index, row)) in recent.iter().enumerate() {
        let at =
            pos2(left + (n % columns) as f32 * (width + gap), heading + 26.0 + (n / columns) as f32 * (CARD + gap));
        let card = Rect::from_min_size(at, vec2(width, CARD));
        if board_card(ui, card, *index, row, backend, client).clicked() {
            boards::show(row, *index, backend, client);
        }
    }
}

/// Records where a part of the start screen was drawn, for the tests.
fn flow_record(ui: &egui::Ui, name: &str, rect: Rect) {
    crate::flow::record(ui, name, rect);
}

/// The boards to continue: the open and the done ones, the latest first.
fn recent_boards(listed: &boards::Listed) -> Vec<(usize, PlanRow)> {
    let mut boards: Vec<_> = listed.waiting.iter().chain(&listed.active).chain(&listed.done).collect();
    boards.sort_by_key(|(index, row)| std::cmp::Reverse((row.last_active, *index)));
    boards.into_iter().take(RECENT).cloned().collect()
}

/// A board as a quiet card: its state's mark, its goal, how it stands with when it last moved, and its progress.
fn board_card(
    ui: &mut egui::Ui,
    rect: Rect,
    index: usize,
    row: &PlanRow,
    backend: &dyn Backend,
    client: &Client,
) -> egui::Response {
    let p = &client.palette;
    let ink = look::Ink::of(p);
    let response = ui.interact(rect, ui.scope_id().with(("home-board", &row.id)), Sense::click());
    crate::flow::record(ui, &format!("home-board:{index}"), rect);
    let hover = crate::controls::hover_amount(ui, response.id, response.hovered());
    let edge = lerp(p.border, p.control_hover_stroke(), hover);
    ui.painter().rect(rect, 10.0, lerp(p.background, p.raised, 0.45), Stroke::new(1.0, edge), StrokeKind::Inside);
    crate::controls::feedback(ui, rect, &response, 10.0, p);
    let painter = ui.painter().with_clip_rect(rect.shrink(1.0));
    simple::plan_mark(&painter, pos2(rect.left() + 18.0, rect.top() + 22.0), row, p);
    let width = rect.width() - 48.0;
    let title = crate::flow::truncated(&painter, &simple::goal(&row.goal), font(look::LABEL), ink.primary, width);
    painter.galley(pos2(rect.left() + 34.0, rect.top() + 22.0 - title.size().y / 2.0), title, ink.primary);
    let (mut detail, color) = simple::plan_detail(backend, index, row, false, p);
    if let Some(ago) = last_active(row, backend) {
        detail.push_str(&format!(" · {ago}"));
    }
    let detail = crate::flow::truncated(&painter, &detail, font(look::SMALL), color, width);
    painter.galley(pos2(rect.left() + 34.0, rect.top() + 42.0 - detail.size().y / 2.0), detail, color);
    if row.total > 0 {
        let track = Rect::from_min_max(
            pos2(rect.left() + 34.0, rect.bottom() - 10.0),
            pos2(rect.right() - 14.0, rect.bottom() - 8.0),
        );
        painter.rect_filled(track, 1.0, p.border);
        let done = row.done as f32 / row.total as f32;
        painter.rect_filled(track.with_max_x(track.left() + track.width() * done), 1.0, lerp(p.success, p.muted, 0.35));
    }
    response.on_hover_text(boards::hover_goal(&row.goal))
}

/// How long ago the board last moved, in a few words; nothing where that is not known.
pub fn last_active(row: &PlanRow, backend: &dyn Backend) -> Option<String> {
    let now = backend.trace_now_ms();
    (row.last_active > 0).then(|| simple::ago(row.last_active * 1000, now)).flatten().map(|ago| match ago.as_str() {
        "just now" => ago,
        _ => format!("{ago} ago"),
    })
}

/// Sends what was typed: with a picked ticket, a plan for that ticket (as the Tickets panel's Plan with chief does);
/// otherwise to the chief, starting it where it does not run. Either way the chief shows from now on.
fn send(plain: String, chief: Option<&crate::SessionRow>, now: f64, backend: &dyn Backend, client: &mut Client) {
    client.home.summoned = true;
    let picked = client.home.ticket.picked.clone();
    match (picked, backend.tickets()) {
        (Some(ticket), Some(info)) => match atomics_core::ticket_ui::request_id() {
            Ok(request) => {
                client.home.ticket.planning = Some(request.clone());
                client.home.ticket.error = None;
                let action = TicketAction::Plan { ticket, mode: info.mode, extra: plain };
                client.send(Command::Ticket { request, action });
            }
            Err(_) => client.home.ticket.error = Some("Could not ask for the plan. Try again.".into()),
        },
        _ if std::mem::take(&mut client.home.fresh) => simple::send_fresh(&plain, now, backend, client),
        _ => simple::send_composed(&plain, chief, false, now, backend, client),
    }
}

/// Takes the replies to this screen's ticket requests: search results, and the plan a ticket started.
fn sync_tickets(backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.tickets() else { return };
    let pick = &mut client.home.ticket;
    for reply in &info.replies {
        if pick.search.as_ref() == Some(&reply.request) {
            pick.search = None;
            pick.results = reply.cards.clone();
            pick.error = reply.error.clone();
        } else if pick.planning.as_ref() == Some(&reply.request) {
            pick.planning = None;
            if let Some(error) = &reply.error {
                pick.error = Some(error.clone());
                continue;
            }
            pick.picked = None;
            if let Some(plan) = &reply.plan
                && let Some((index, row)) = backend.plan_list().into_iter().enumerate().find(|(_, row)| &row.id == plan)
            {
                boards::show(&row, index, backend, client);
                return;
            }
        }
    }
}

/// Whether this backend can search the ticket system.
fn can_search(backend: &dyn Backend) -> bool {
    backend.tickets().is_some_and(|info| info.search || info.semantic_search)
        && backend.available(&Command::Ticket {
            request: String::new(),
            action: TicketAction::Search {
                query: String::new(),
                keyword: false,
                view: 0,
                filters: Vec::new(),
                cursor: None,
            },
        })
}

/// The ticket for the next message: a quiet button that opens a search of the ticket system, or the ticket picked,
/// which the next message plans from. Only where a ticket system is configured.
fn ticket_picker(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if !can_search(backend) {
        return;
    }
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let now = ui.input(|i| i.time);
    let now_ms = backend.trace_now_ms();
    let label = match &client.home.ticket.picked {
        Some(link) => format!("{} · {}", link.label(), link.title.as_deref().unwrap_or_default()),
        None => "Plan from a ticket".to_string(),
    };
    let label: String = label.chars().take(48).collect();
    let color = if client.home.ticket.picked.is_some() { ink.primary } else { ink.secondary };
    let button = quiet_menu(look::text(label, look::LABEL, Weight::Regular, color), ink.tertiary)
        .frame_when_inactive(client.home.ticket.picked.is_some());
    let response = ui.add(button);
    crate::flow::record(ui, "home-ticket", response.rect);
    if response.clicked() {
        client.home.ticket.focus = true;
        if client.home.ticket.searched.is_none() {
            search(backend, client, now);
        }
    }
    if client.home.ticket.picked.is_some() {
        let (rect, clear) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
        crate::controls::feedback(ui, rect, &clear, 6.0, &p);
        let hover = crate::controls::hover_amount(ui, clear.id, clear.hovered());
        let cross = lerp(ink.tertiary, ink.primary, hover);
        crate::status::paint_mark(ui.painter(), rect.center(), crate::status::Mark::Cross, cross);
        if clear.on_hover_text("Leave the ticket out").clicked() {
            client.home.ticket.picked = None;
        }
    }
    egui::Popup::menu(&response).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
        ui.set_width(420.0);
        let pick = &mut client.home.ticket;
        let field = ui.add(
            crate::controls::single_line(&mut pick.query).hint_text("Search tickets…").desired_width(f32::INFINITY),
        );
        crate::touch::opens_keyboard(ui, &field);
        if std::mem::take(&mut pick.focus) {
            field.request_focus();
        }
        if field.changed() {
            pick.edited = now;
        }
        let waiting = pick.searched.as_deref() != Some(pick.query.trim());
        if waiting && now - pick.edited > 0.35 {
            search(backend, client, now);
        } else if waiting {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
        }
        let pick = &mut client.home.ticket;
        ui.add_space(6.0);
        if pick.search.is_some() {
            ui.label(look::text("Searching…", look::SMALL, Weight::Regular, ink.tertiary));
        } else if pick.results.is_empty() {
            ui.label(look::text("No tickets found", look::SMALL, Weight::Regular, ink.tertiary));
        }
        let mut chosen = None;
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            for card in &pick.results {
                let selected = pick.picked.as_ref().is_some_and(|link| link.same_ticket(&card.link));
                let updated = crate::tickets::updated(card, now_ms);
                if crate::tickets::simple_result(ui, card, selected, updated.as_deref(), &p).clicked() {
                    chosen = Some(card.link.clone());
                }
            }
        });
        if let Some(link) = chosen {
            pick.picked = Some(link);
            client.simple.focus_composer = true;
            ui.close();
        }
    });
}

/// A quiet button that opens a menu: its words and a painted chevron, framed only while the pointer is on it.
fn quiet_menu(text: egui::RichText, chevron: egui::Color32) -> egui::Button<'static> {
    let mut atoms = egui::Atoms::default();
    atoms.push_right(text);
    atoms.push_right(crate::controls::chevron_atom(chevron));
    egui::Button::new(atoms).frame_when_inactive(false).corner_radius(8)
}

/// Searches the ticket system for what the picker's field says; an empty field lists the latest.
fn search(backend: &dyn Backend, client: &mut Client, now: f64) {
    let Some(info) = backend.tickets() else { return };
    let Ok(request) = atomics_core::ticket_ui::request_id() else { return };
    let pick = &mut client.home.ticket;
    let query = pick.query.trim().to_string();
    pick.searched = Some(query.clone());
    pick.edited = now;
    pick.search = Some(request.clone());
    let action = TicketAction::Search {
        query,
        keyword: !info.semantic_search,
        view: info.default_view,
        filters: Vec::new(),
        cursor: None,
    };
    client.send(Command::Ticket { request, action });
}

/// Every configured agent, to start one by hand in the folder the host started in (or in a scratch folder on the
/// Ungrouped tab), behind one quiet menu.
fn start_agent(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let folder = if backend.view_hints().ungrouped { None } else { backend.launch_dir() };
    let start = |agent: String| Command::Start { agent, folder: folder.clone() };
    let agents = backend.ui_config().agents;
    if agents.is_empty() || !backend.available(&start(String::new())) {
        return;
    }
    let ink = look::Ink::of(&client.palette);
    let place = match &folder {
        Some(folder) => format!("In {}", backend.display(folder)),
        None => "Each in a scratch folder of its own, outside any project".into(),
    };
    let button = quiet_menu(look::text("Start an agent", look::LABEL, Weight::Regular, ink.secondary), ink.tertiary);
    let response = egui::containers::menu::MenuButton::from_button(button)
        .ui(ui, |ui| {
            ui.set_min_width(200.0);
            ui.label(look::text(&place, look::SMALL, Weight::Regular, ink.tertiary));
            ui.separator();
            for agent in agents {
                if ui.button(look::text(&agent, look::LABEL, Weight::Regular, ink.primary)).clicked() {
                    client.send(start(agent));
                    ui.close();
                }
            }
        })
        .0;
    crate::flow::record(ui, "home-start-agent", response.rect);
    response.on_hover_text("Start an agent yourself, without the chief");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Folds;
    use atomics_core::config::ChiefDock;

    #[test]
    fn the_chief_toggle_shows_a_held_chief_and_after_that_the_chief_is_the_users() {
        let mut client = Client::default();
        client.home.shown = true;
        client.home.held = true;
        assert!(chief_command(&mut client, &Command::ToggleChief), "it shows rather than hides");
        assert!(client.home.summoned && !client.home.held);
        assert!(!chief_command(&mut client, &Command::ToggleChief), "then the toggle hides it as before");

        let mut client = Client::default();
        client.home.shown = true;
        assert!(!chief_command(&mut client, &Command::ToggleChief), "a closed chief opens");
        assert!(client.home.summoned, "and is not held behind the start screen");

        let mut client = Client::default();
        client.home.held = true;
        assert!(!chief_command(&mut client, &Command::HideChief));
        assert!(!client.home.held, "nothing is held once it hides");
        assert!(!chief_command(&mut client, &Command::SetChiefDock(ChiefDock::Floating)));
        assert!(client.home.summoned, "docking or floating it asks for it");
        assert!(!chief_command(&mut client, &Command::CloseDiff));
    }

    #[test]
    fn going_to_a_session_or_a_board_leaves_the_start_screen_even_one_already_active() {
        for command in [
            Command::Activate { session: "s".into(), open_diff: true },
            Command::OpenTaskSession("s".into()),
            Command::ShowPlan(0),
            Command::ViewPlanHistory(0),
        ] {
            let mut client = Client::default();
            client.home.asked = true;
            assert!(!chief_command(&mut client, &command), "{command:?} is carried out too");
            assert!(!client.home.asked(), "{command:?}");
        }
        let mut client = Client::default();
        client.home.asked = true;
        assert!(!chief_command(&mut client, &Command::SetView(None)));
        assert!(client.home.asked(), "New work's own commands keep it");
    }

    #[test]
    fn a_window_keeps_its_open_sections_by_name() {
        for folds in [Folds::default(), Folds { agents: true, explorer: false }, Folds { agents: true, explorer: true }]
        {
            assert_eq!(Folds::from_names(&folds.names()), folds);
        }
        assert_eq!(Folds::from_names(&["explorer".into(), "later".into()]), Folds { agents: false, explorer: true });
    }
}
