//! Simple mode (`[ui] mode = "simple"`): a chat-first window for people new to ATOMICS. The chief's conversation
//! is the centre; the plans are a list on the left, like conversations, each with its state; the right shows the
//! selected plan's work: what needs the user, as actions, and the [`flow`](crate::flow) of its tasks with what
//! passes between them ([`traffic`](crate::traffic)). Every full feature stays one click away: a task's diff in
//! the existing diff view, its agent's terminal, the full board, and the full window itself.
//!
//! The host's `[ui] mode` says what its windows open in; each client may switch for itself ([`Client::mode`]),
//! which the platform keeps on its own side and never writes to the host's config. Nothing here decides a
//! permission or an approval on its own: the cards draw the same banner and send the same commands as the full
//! window ([`approvals::banner`], [`Command::ApproveAndStartPlan`], the notebook relay's decisions).
//!
//! [`approvals::banner`]: crate::approvals::banner

use atomics_core::config::UiMode;
use atomics_core::plan::{PlanStatus, TaskStatus};
use atomics_core::session::AgentStatus;
use atomics_core::trace::{TraceOp, TurnRole};
use egui::{Align2, Color32, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::flow::{self, Flow, FlowState, Live, font};
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::theme::{Palette, lerp};
use crate::traffic::{Observed, Traffic};
use crate::working;
use crate::{Backend, Client, Command, PlanState, SessionRow, approvals, touch};
use look::Weight;

/// The plans rail's width, the top bar's height, and below which widths the window drops the rail and then shows
/// one of the chief and the work at a time.
pub const RAIL: f32 = 248.0;
pub const TOP: f32 = 44.0;
pub const RAIL_MIN: f32 = 1000.0;
pub const SPLIT_MIN: f32 = 700.0;

/// What the work panel shows: the flow, or one click away from it a task's diff, an agent's terminal or the full
/// board.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Work {
    #[default]
    Flow,
    /// The diff the window's diff target names: the task's whose diff was opened.
    Diff,
    /// An agent's terminal, by session id.
    Terminal(String),
    /// The plan's tasks as calm cards by status, with the board's own actions.
    Board,
    /// The ticket system: signing in, finding and reading tickets, and planning from them, as the Tickets panel does.
    Tickets,
}

impl Work {
    /// One of the work's tabs, rather than something opened from one.
    pub fn tab(&self) -> bool {
        matches!(self, Work::Flow | Work::Board | Work::Tickets)
    }
}

/// One pane at a time: on a phone, and in a window too narrow for both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Chief,
    Work,
    Plans,
}

/// What one client holds of the simple view.
#[derive(Debug, Clone, Default)]
pub struct State {
    pub flow: FlowState,
    pub traffic: Traffic,
    pub work: Work,
    /// The chief's terminal shows in place of its conversation.
    pub chief_terminal: bool,
    pub chat_focus: bool,
    /// The chief's share of the space beside Work in this window.
    chat_fraction: Option<f32>,
    switch_chief: Option<(String, atomics_core::projection::ChiefSelection)>,
    pub linear_team: String,
    /// What is typed to the chief.
    pub composer: String,
    /// The box to write to the chief takes the keyboard when it is next drawn.
    pub focus_composer: bool,
    /// The pane on screen where only one fits.
    pub pane: Pane,
    /// The rows last drawn, by task id with their status: what the user saw.
    pub shown: Vec<(String, TaskStatus)>,
    /// Answers being typed to agents' questions, by session id.
    pub answers: std::collections::HashMap<String, String>,
    /// The task whose diff the work shows, by id.
    pub diff_task: Option<String>,
    /// The tab a diff or a terminal was opened from, which its "‹" goes back to.
    pub back: Work,
    /// What the user sent the chief that its conversation does not show yet, oldest first.
    pub outbox: Vec<Outgoing>,
    /// When this client first saw the chief starting, by its clock.
    pub chief_starting: Option<f64>,
    /// The plan whose Needs you inbox is expanded.
    pub inbox: Option<String>,
    pub plan_query: String,
    /// A ticket action may be acknowledged before its board is published.
    pub(crate) ticket_plan: Option<String>,
    /// The last board viewed, so its final frame stays on screen when it closes.
    last_plan: Option<(String, PlanStatus)>,
    /// A board explicitly selected from the plan browser.
    chat_plan: Option<String>,
    past_chat: Option<history::Chat>,
    chat_id: Option<String>,
    transcript: TranscriptRows,
    plan_dialog: Option<new_plan::Dialog>,
    approving: Option<(String, f64)>,
    drafts: std::collections::HashMap<String, String>,
    /// Reveal the inbox after a click on the top bar, even if the flow has scrolled.
    pub reveal_inbox: bool,
    /// The tabs head the work, where the top bar does not have them (a narrow window, a phone).
    pub tabs_in_header: bool,
    /// The task read in full, by id, over whatever shows.
    pub task_open: Option<String>,
    /// The task whose long brief shows whole, by id.
    pub brief_open: Option<String>,
    /// The project the next message is about, picked in the composer; `None` for the open plan's, else the first.
    pub project: Option<std::path::PathBuf>,
    /// Each working agent's status line, as this client counts on from its last reading.
    pub working: crate::working::Working,
    /// The setup page is open.
    pub setup_open: bool,
    pub systems_open: bool,
}

/// What a working session's line shows now, counted on from its last reading; `None` unless it works.
fn working_now(session: &SessionRow, backend: &dyn Backend, client: &mut Client, now: f64) -> Option<working::Shown> {
    if session.status != AgentStatus::Working {
        return None;
    }
    let reduce = reduce_motion(backend, client);
    let activity = session.activity.as_ref();
    let live = activity.and_then(|a| a.live.as_ref());
    let since = activity.and_then(|a| a.since);
    let host_now = backend.trace_now_ms() / 1000;
    client.simple.working.shown(&session.id, live, since, host_now, now, reduce)
}

/// Redraws on the working line's beat while something works: the spinner's step and the timer; under reduced motion
/// once a second, for the timer alone; while a count climbs or a verb fades, every frame.
fn working_beat(ui: &egui::Ui, client: &Client, reduce: bool, now: f64) {
    if !reduce && client.simple.working.climbing(now) {
        ui.ctx().request_repaint();
    } else if reduce {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    } else {
        ui.ctx().request_repaint_after(working::TICK);
    }
}

/// The project a message is about: the one picked, while it is open, else the open plan's, else the first.
pub fn message_project(backend: &dyn Backend, client: &Client) -> Option<std::path::PathBuf> {
    let repos = backend.repos();
    let picked = client.simple.project.clone().filter(|p| repos.contains(p));
    let planned = selected_plan(backend)
        .and_then(|i| backend.board(i).and_then(|board| board.plan.project.clone()))
        .filter(|p| repos.contains(p));
    picked.or(planned).or_else(|| repos.first().cloned())
}

/// A message as the chief gets it: with the project it is about first, where several are open.
pub fn about_project(text: &str, project: Option<&std::path::Path>, open: usize) -> String {
    match project.and_then(|p| p.file_name()) {
        Some(name) if open > 1 => format!("In {}: {text}", name.to_string_lossy()),
        _ => text.to_string(),
    }
}

/// The repository the next message is about, inside the workspace menu.
fn message_projects(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let projects = backend.repos();
    if projects.len() < 2 {
        return;
    }
    let current = message_project(backend, client);
    ui.label(
        RichText::new("Send messages to").font(font(12.0)).color(client.palette.simple_text(client.palette.muted, 7.2)),
    );
    for project in projects {
        let name =
            project.file_name().map_or_else(|| backend.display(&project), |name| name.to_string_lossy().into_owned());
        if ui
            .selectable_label(Some(&project) == current.as_ref(), name)
            .on_hover_text(backend.display(&project))
            .clicked()
        {
            client.simple.project = Some(project);
            ui.close();
        }
    }
    ui.separator();
}

/// What this client shows: its own choice, else what the host's windows open in.
pub fn mode(backend: &dyn Backend, client: &Client) -> UiMode {
    client.mode.unwrap_or(backend.ui_config().mode)
}

/// Whether this client draws the simple view.
pub fn on(backend: &dyn Backend, client: &Client) -> bool {
    mode(backend, client) == UiMode::Simple
}

/// Switches this client alone: its choice is the host's own mode again when it matches, so a later change of
/// the host's `[ui] mode` reaches it. It keeps the place: the full window opens on the plan's board with the task
/// open, or on its diff or terminal; simple mode comes back to that plan, task and diff.
pub fn switch(backend: &dyn Backend, client: &mut Client, mode: UiMode) {
    if mode == self::mode(backend, client) {
        return;
    }
    match mode {
        UiMode::Full => to_full(backend, client),
        UiMode::Simple => to_simple(backend, client),
    }
    client.mode = (mode != backend.ui_config().mode).then_some(mode);
}

/// The other mode, for the shortcut (Command+Shift+L on a Mac, Ctrl+Shift+L elsewhere).
pub fn toggle(backend: &dyn Backend, client: &mut Client) {
    let other = if on(backend, client) { UiMode::Full } else { UiMode::Simple };
    switch(backend, client, other);
}

/// The shortcut that switches modes, on every client.
pub const SHORTCUT: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT), egui::Key::L);

fn to_full(backend: &dyn Backend, client: &mut Client) {
    let Some(index) = selected_plan(backend) else { return };
    let Some(row) = backend.plan_row(index) else { return };
    client.send(if row.status == PlanStatus::Open {
        Command::ShowPlan(index)
    } else {
        Command::ViewPlanHistory(index)
    });
    for task in client.simple.flow.selected.clone() {
        client.expanded_tasks.insert((row.id.clone(), task));
    }
    match client.simple.work.clone() {
        Work::Diff => {
            client.send(Command::SetPanel(crate::PanelMode::Diff));
            client.send(Command::OpenDiff);
        }
        Work::Terminal(session) if !session.is_empty() => {
            client.send(Command::Activate { session, open_diff: false });
        }
        _ => {
            client.send(Command::SetPanel(crate::PanelMode::Plan));
            client.send(Command::OpenDiff);
        }
    }
}

fn to_simple(backend: &dyn Backend, client: &mut Client) {
    let hints = backend.view_hints();
    let sessions = backend.session_list();
    let task = hints.active.as_ref().and_then(|id| sessions.iter().find(|s| &s.id == id)).and_then(|s| s.task.clone());
    client.simple.work = Work::Flow;
    if let Some(link) = &task {
        client.simple.flow.selected = std::iter::once(link.task.clone()).collect();
    }
    if hints.diff_open && hints.panel == crate::PanelMode::Diff && hints.diff_target.is_some() {
        client.simple.work = Work::Diff;
        client.simple.back = Work::Flow;
        client.simple.diff_task = task.map(|link| link.task);
    }
    client.simple.pane = Pane::Work;
}

/// How a client keeps its own choice: `simple`, `full`, or empty for the host's.
pub fn mode_name(mode: Option<UiMode>) -> &'static str {
    match mode {
        Some(UiMode::Simple) => "simple",
        Some(UiMode::Full) => "full",
        None => "",
    }
}

/// A choice a client kept ([`mode_name`]); anything else is the host's.
pub fn named_mode(name: &str) -> Option<UiMode> {
    match name.trim() {
        "simple" => Some(UiMode::Simple),
        "full" => Some(UiMode::Full),
        _ => None,
    }
}

/// Whether what would move stays still: the host's `[ui] reduce_motion`, or the platform's (a browser's own).
pub fn reduce_motion(backend: &dyn Backend, client: &Client) -> bool {
    client.reduce_motion || backend.ui_config().reduce_motion
}

/// Whether a plan is worth showing: one with a goal or a task. A host keeps an empty plan for the chief to fill,
/// which is nothing yet.
pub fn shown(plan: &crate::PlanRow) -> bool {
    !plan.goal.trim().is_empty() || plan.total > 0
}

/// The plan the view shows: the one the window has selected, else the first open one, else the newest; never an
/// empty one.
pub fn selected_plan(backend: &dyn Backend) -> Option<usize> {
    let plans = backend.plan_list();
    let hints = backend.view_hints();
    if hints.plan_view && plans.get(hints.selected_plan).is_some_and(shown) {
        return Some(hints.selected_plan);
    }
    let open = plans.iter().position(|plan| plan.status == PlanStatus::Open && shown(plan));
    open.or_else(|| plans.iter().rposition(shown))
}

fn sync_board(backend: &dyn Backend, client: &mut Client) {
    let plans = backend.plan_list();
    if let Some(dialog) = &client.simple.plan_dialog {
        if dialog.submitted
            && let Some(i) = plans.iter().position(|p| p.id == format!("plan-{}", dialog.request))
        {
            client.simple.plan_dialog = None;
            open_plan(backend, client, i);
            client.simple.pane = Pane::Chief;
        }
        return;
    }
    if client.simple.past_chat.is_some() {
        return;
    }
    if let Some(chat) = &client.simple.chat_id {
        let trace = backend.trace();
        let promoted =
            backend.chief_history().iter().find(|s| &s.conversation == chat).and_then(|s| s.plan.as_deref()).or_else(
                || {
                    trace
                        .entries
                        .iter()
                        .rev()
                        .filter(|e| e.chat_id.as_ref() == Some(chat) && e.tool == "atomics_plan_set")
                        .find_map(|e| e.plan.as_ref()?.id.as_deref())
                },
            );
        if let Some(i) = promoted.and_then(|id| plans.iter().position(|p| p.id == id)) {
            open_plan(backend, client, i);
            client.simple.pane = Pane::Chief;
        }
        return;
    }
    if let Some(id) = &client.simple.ticket_plan {
        if let Some(i) = plans.iter().position(|plan| &plan.id == id) {
            open_plan(backend, client, i);
            client.simple.pane = Pane::Chief;
            client.simple.ticket_plan = None;
        }
        return;
    }
    let selected = selected_plan(backend).and_then(|i| plans.get(i).map(|p| (i, p)));
    let closed = match &client.simple.last_plan {
        Some((id, PlanStatus::Open)) if client.simple.chat_plan.as_ref().is_none_or(|chat| chat == id) => {
            plans.iter().enumerate().find(|(_, row)| &row.id == id && row.status != PlanStatus::Open)
        }
        None => selected.filter(|(_, row)| row.status != PlanStatus::Open),
        _ => None,
    };
    if let Some((index, closed)) = closed.filter(|_| client.simple.chat_id.is_none()) {
        client.simple.last_plan = Some((closed.id.clone(), closed.status));
        client.simple.chat_plan = Some(closed.id.clone());
        client.send(Command::ViewPlanHistory(index));
        return;
    }
    client.simple.last_plan = selected.map(|(_, p)| (p.id.clone(), p.status));
    if client.simple.chat_plan.as_ref().is_some_and(|id| !plans.iter().any(|p| &p.id == id)) {
        client.simple.chat_plan = None;
    }
}

fn chief_session(backend: &dyn Backend) -> Option<(usize, SessionRow)> {
    let index = backend.chief().session?;
    Some((index, backend.session_row(index)?))
}

fn chief_live(backend: &dyn Backend) -> Live {
    chief_session(backend).map_or(Live::None, |(_, session)| flow::live(TaskStatus::Running, Some(&session)))
}

/// A pill-shaped button the simple view draws its actions with: filled for the main one.
fn pill(ui: &mut egui::Ui, text: &str, primary: bool, client: &Client) -> egui::Response {
    let p = &client.palette;
    let color = if primary { p.on_accent } else { p.foreground };
    let weight = if primary { Weight::Medium } else { Weight::Regular };
    let mut button = egui::Button::new(look::text(text, look::LABEL, weight, color)).corner_radius(8);
    button = if primary {
        button.fill(p.accent).stroke(Stroke::NONE)
    } else {
        button.fill(p.raised).stroke(Stroke::new(1.0, p.border))
    };
    let height = 32.0;
    let response = ui
        .scope(|ui| {
            ui.spacing_mut().button_padding.x = if primary { 14.0 } else { 12.0 };
            ui.add(touch::sized(button.min_size(vec2(0.0, height)), client.touch))
        })
        .inner;
    flow::record(ui, &format!("button:{text}"), response.rect);
    response
}

/// The top bar: the name, the plan on screen, what needs the user, the switch to the full window and settings.
/// Narrow, it also picks the plan and the pane.
/// A small chevron, drawn: down for a menu or something open, right for something to open. The faces the simple view
/// has everywhere it runs have no ▾ or ▸ of their own.
fn chevron(color: Color32, down: bool) -> egui::Atom<'static> {
    egui::Atom::paint(vec2(10.0, 10.0), move |ui: &egui::Ui, args: egui::AtomPaintArgs| {
        let c = args.rect.center();
        let points = if down {
            vec![c + vec2(-3.5, -1.5), c + vec2(0.0, 2.0), c + vec2(3.5, -1.5)]
        } else {
            vec![c + vec2(-1.5, -3.5), c + vec2(2.0, 0.0), c + vec2(-1.5, 3.5)]
        };
        ui.painter().add(egui::Shape::line(points, Stroke::new(1.3, color)));
    })
}

/// Text and a chevron after it, for a button.
fn with_chevron(text: RichText, color: Color32, down: bool) -> egui::Atoms<'static> {
    let mut atoms = egui::Atoms::default();
    atoms.push_right(text);
    atoms.push_right(chevron(color, down));
    atoms
}

/// Settings, behind a drawn cogwheel: the simple view's faces have no ⚙ everywhere it runs.
fn settings_gear(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    if backend.management().is_none() {
        return;
    }
    let p = client.palette.clone();
    let size = client.touch.map_or(28.0, |touch| touch.target.max(28.0));
    let hit = Rect::from_center_size(rect.center(), vec2(size, size).min(rect.size().max(vec2(24.0, 24.0))));
    let response = ui.interact(hit, ui.scope_id().with("simple-settings-gear"), Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "Settings"));
    crate::topbar::record_item(ui, "Settings", &response);
    let painter = ui.painter();
    if response.hovered() {
        painter.rect_filled(hit, 8.0, lerp(p.topbar, p.foreground, 0.08));
    }
    let color = if response.hovered() || client.management.settings { p.foreground } else { p.muted };
    let c = hit.center();
    for k in 0..8 {
        let angle = k as f32 / 8.0 * std::f32::consts::TAU;
        let dir = vec2(angle.cos(), angle.sin());
        painter.line_segment([c + dir * 5.0, c + dir * 7.5], Stroke::new(2.2, color));
    }
    painter.circle_stroke(c, 5.0, Stroke::new(1.6, color));
    painter.circle_stroke(c, 1.8, Stroke::new(1.3, color));
    if response.on_hover_text("Settings").clicked() {
        client.management.settings = !client.management.settings;
    }
}

fn top_bar(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client, narrow: bool, panes: bool) {
    let p = client.palette.clone();
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 0.0, p.topbar);
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.separator));
    let y = rect.center().y;
    let mut x = rect.left() + 16.0;
    let mark = Rect::from_center_size(pos2(x + 9.0, y), vec2(18.0, 18.0));
    crate::chief::orbit_icon(&painter, mark, p.accent, p.foreground);
    x += 26.0;
    // The workspace and its projects: a small menu, as the Project menu has them.
    let place = Rect::from_min_max(pos2(x, rect.top() + 6.0), pos2(x + 220.0, rect.bottom() - 6.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-workspace")
            .max_rect(place)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    x += workspace_switcher(&mut child, backend, client) + 10.0;
    let right = rect.right() - 12.0;
    let settings = backend.management().is_some();
    // The switch to the full window is a setting, under the cogwheel, and a shortcut.
    let switch = Rect::from_min_size(pos2(right - if settings { 40.0 } else { 0.0 }, y - 13.0), vec2(0.0, 26.0));
    if settings {
        let gear = Rect::from_min_max(pos2(switch.right() + 6.0, rect.top() + 6.0), pos2(right, rect.bottom() - 6.0));
        settings_gear(ui, gear, backend, client);
    }
    let mut end = switch.left() - 12.0;
    // Usage, the machine and where the agents run, right of the tabs, where the window has room for them.
    if !narrow {
        let strip = Rect::from_min_max(
            pos2(rect.center().x + tabs_width(shown_tabs(backend, client).len()) / 2.0 + 16.0, rect.top() + 6.0),
            pos2(end, rect.bottom() - 6.0),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("simple-status")
                .max_rect(strip)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        child.set_clip_rect(strip);
        status_strip(&mut child, backend, client);
    }
    if !panes {
        let tabs_rect =
            Rect::from_center_size(rect.center(), vec2(tabs_width(shown_tabs(backend, client).len()), 32.0));
        tabs(ui, tabs_rect, backend, client);
        end = end.min(tabs_rect.left() - 12.0);
    }
    let middle = Rect::from_min_max(pos2(x, rect.top() + 4.0), pos2(end, rect.bottom() - 4.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-top-middle")
            .max_rect(middle)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(middle);
    if panes {
        for (pane, label) in [(Pane::Chief, "Chief"), (Pane::Work, "Work")] {
            let on = client.simple.pane == pane;
            let text = RichText::new(label).font(font(13.0)).color(if on { p.accent } else { p.muted });
            let response = child.add(egui::Button::new(text).frame(false));
            flow::record(&child, &format!("pane:{label}"), response.rect);
            if response.clicked() {
                client.simple.pane = pane;
            }
        }
    }
    if narrow {
        plan_picker(&mut child, backend, client);
    } else if client.simple.chat_id.is_some() {
        child.label(look::text("Chat", look::LABEL, Weight::Medium, p.foreground));
    } else if let Some(index) = selected_plan(backend)
        && let Some(plan) = backend.plan_row(index)
    {
        child.label(look::text(
            format!("/  {}", goal(&plan.goal)),
            look::LABEL,
            Weight::Regular,
            look::Ink::of(&p).secondary,
        ));
    }
    // What needs the user: the same count as the Needs you list and the phone's badge.
    let waiting = waiting(backend);
    if waiting > 0 && client.simple.chat_id.is_none() {
        let text = RichText::new(format!("●  {waiting} need{} you", if waiting == 1 { "s" } else { "" }))
            .font(font(13.0))
            .color(p.warning);
        let response = child.add(egui::Button::new(text).frame(false));
        flow::record(&child, "need-you", response.rect);
        if response.clicked() {
            client.simple.inbox = selected_plan(backend).and_then(|i| backend.plan_row(i)).map(|plan| plan.id);
            client.simple.reveal_inbox = true;
            client.simple.work = Work::Flow;
            client.simple.pane = Pane::Work;
        }
    }
}

/// The workspace's mark (its initial) and name, which open its projects, "Open a project…" and the other
/// workspaces: the Project menu's own items. Returns the width it took.
pub fn workspace_switcher(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) -> f32 {
    let p = client.palette.clone();
    let menu = backend.project_menu();
    let workspace = menu.current.clone().unwrap_or_else(|| {
        backend
            .repos()
            .first()
            .and_then(|r| r.file_name())
            .map_or("Workspace".into(), |n| n.to_string_lossy().into_owned())
    });
    let project = message_project(backend, client);
    let name = if backend.repos().len() > 1 {
        project.as_deref().and_then(|p| p.file_name()).map_or(workspace.clone(), |n| n.to_string_lossy().into_owned())
    } else {
        workspace.clone()
    };
    let initial: String = workspace.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
    let label = with_chevron(look::text(&name, look::LABEL + 0.5, Weight::Medium, p.foreground), p.muted, true);
    let start = ui.cursor().min.x;
    let (badge, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
    ui.painter().rect_filled(badge, 6.0, lerp(p.raised, p.accent, 0.35));
    ui.painter().text(badge.center(), Align2::CENTER_CENTER, initial, font(12.5), p.foreground);
    let can = backend.repos().len() > 1 || backend.management().is_some() || backend.available(&Command::PickProject);
    let button = egui::Button::new(label).frame(false);
    if can {
        let response = egui::containers::menu::MenuButton::from_button(button)
            .ui(ui, |ui| {
                ui.set_min_width(240.0);
                message_projects(ui, backend, client);
                ui.label(RichText::new("Projects").font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
                crate::topbar::project_menu(ui, backend, client);
                ui.separator();
                if ui.button(RichText::new("Connected systems…").font(font(13.0))).clicked() {
                    setup::open_systems(backend, client);
                    ui.close();
                }
                if ui.button(RichText::new("Setup…").font(font(13.0))).clicked() {
                    setup::open(client);
                    ui.close();
                }
            })
            .0;
        flow::record(ui, "workspace", response.rect);
    } else {
        ui.add(egui::Label::new(label_text(&name, &p)));
    }
    ui.cursor().min.x - start
}

fn label_text(name: &str, p: &Palette) -> RichText {
    RichText::new(name).font(font(13.5)).color(p.foreground)
}

/// How a provider's quota stands: the least left of its session and week, unknown, or stale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Quota {
    /// No reading yet, or none for either window.
    Unknown,
    /// The last reading is old, or its windows have reset since: what it said, and that it may be out of date.
    Stale(Option<f64>),
    Left(f64),
}

/// How long a usage reading stays current, in seconds.
pub const USAGE_FRESH: i64 = 60 * 60;

/// How a provider's quota stands at `now` (seconds).
pub fn quota(usage: &atomics_core::usage::Usage, now: i64) -> Quota {
    if usage.observed_at <= 0 {
        return Quota::Unknown;
    }
    let left = [&usage.session, &usage.weekly]
        .into_iter()
        .flatten()
        .filter(|window| window.resets_at > now)
        .map(|window| window.left_percent)
        .fold(None, |least: Option<f64>, left| Some(least.map_or(left, |l| l.min(left))));
    let any = usage.session.is_some() || usage.weekly.is_some();
    match (left, any) {
        (_, false) => Quota::Unknown,
        (None, true) => Quota::Stale(None),
        (Some(left), _) if now - usage.observed_at > USAGE_FRESH => Quota::Stale(Some(left)),
        (Some(left), _) => Quota::Left(left),
    }
}

/// A provider runs low: less than a fifth of a window left.
pub fn low(quota: Quota) -> bool {
    matches!(quota, Quota::Left(left) | Quota::Stale(Some(left)) if left < 20.0)
}

/// Where the agents run, in a few words for the status strip and a sentence for its hover: the machine, the VM's
/// hardening, how agents are isolated, and this client's link to the host (`None` in a window on the host).
/// `fallback` is what a client says without the host's word (an older host).
pub fn place_words(
    agents: Option<&atomics_core::projection::AgentsInfo>,
    link: Option<bool>,
    fallback: Option<&str>,
) -> (String, String) {
    use atomics_core::projection::AgentMachine;
    let on_host = link.is_none();
    let (mut short, mut long) = match agents {
        Some(agents) => {
            let isolated = match agents.isolation.as_str() {
                "sandbox" => "each in a sandbox of its own unless its task says otherwise",
                "bare" if agents.machine == AgentMachine::Vm => "the agents share the guest as their sandbox boundary",
                "bare" => "bare, with your account's access, unless its task says otherwise",
                _ => "as the settings say",
            };
            match agents.machine {
                AgentMachine::Mac => (
                    if on_host { "This Mac" } else { "On the Mac" }.to_string(),
                    format!("The agents run on the Mac, {isolated}."),
                ),
                AgentMachine::Vm => {
                    let name = agents.name.as_deref().map(|n| format!(" ({n})")).unwrap_or_default();
                    let (word, why) = match agents.hardened {
                        Some(true) => (" · hardened", ": hardened, so its user cannot become root"),
                        Some(false) => ("", ": not hardened, so its user may become root"),
                        None => ("", ""),
                    };
                    (format!("In the VM{word}"), format!("The agents run in ATOMICS's VM{name}{why}; {isolated}."))
                }
                AgentMachine::Other => (
                    agents.name.as_deref().map_or("On the host".into(), |name| format!("On {name}")),
                    format!("The agents run on {}, {isolated}.", agents.name.as_deref().unwrap_or("the host")),
                ),
            }
        }
        None => (
            fallback.map_or_else(|| if on_host { "This Mac" } else { "On the host" }.to_string(), str::to_string),
            "Where the agents run.".to_string(),
        ),
    };
    match link {
        Some(false) => {
            short.push_str(" · reconnecting");
            long.push_str(" This window has lost the host and is reconnecting.");
        }
        Some(true) => long.push_str(" This window is connected to it."),
        None => {}
    }
    long.push_str(" Machine has the details.");
    (short, long)
}

/// The host and its current load, each opening Machine.
fn status_strip(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let agents = backend.agents();
    let (place, hover) = place_words(agents.as_ref(), client.link, client.machine_place.as_deref());
    let color = if client.link == Some(false) { p.warning } else { look::Ink::of(&p).secondary };
    let response = ui.add(egui::Button::new(RichText::new(place).font(font(look::SMALL)).color(color)).frame(false));
    flow::record(ui, "where", response.rect);
    if response.on_hover_text(hover).clicked() {
        client.machine_open = !client.machine_open;
    }
}

fn usage_footer(ui: &mut egui::Ui, backend: &dyn Backend, client: &Client) {
    let p = client.palette.clone();
    let Some(resources) = backend.resources() else { return };
    let now = resources.sampled_at;
    // A provider with no reading says nothing worth a row: an empty bar would only look broken.
    let known: Vec<_> = [("Claude", &resources.claude), ("Codex", &resources.codex)]
        .into_iter()
        .map(|(name, usage)| (name, quota(usage, now)))
        .filter(|(_, quota)| *quota != Quota::Unknown)
        .collect();
    if known.is_empty() {
        return;
    }
    ui.separator();
    let ink = look::Ink::of(&p);
    ui.label(look::text("Usage", look::SMALL, Weight::Medium, ink.secondary));
    for (name, quota) in known {
        let (value, color) = match quota {
            Quota::Unknown => ("Unknown".to_owned(), ink.tertiary),
            Quota::Stale(Some(left)) => (format!("{left:.0}% · stale"), ink.tertiary),
            Quota::Stale(None) => ("Stale".to_owned(), ink.tertiary),
            Quota::Left(left) if left < 10.0 => (format!("{left:.0}%"), p.error),
            Quota::Left(left) if left < 20.0 => (format!("{left:.0}%"), p.warning),
            Quota::Left(left) => (format!("{left:.0}%"), ink.secondary),
        };
        let (row, response) = ui.allocate_exact_size(vec2(ui.available_width(), 31.0), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{name} {value}")));
        let painter = ui.painter();
        painter.text(row.left_top(), Align2::LEFT_TOP, name, font(look::SMALL), ink.primary);
        painter.text(row.right_top(), Align2::RIGHT_TOP, value, font(look::SMALL), color);
        let track = Rect::from_min_max(pos2(row.left(), row.bottom() - 9.0), pos2(row.right(), row.bottom() - 5.0));
        painter.rect_filled(track, 2.0, p.border);
        if let Quota::Left(left) | Quota::Stale(Some(left)) = quota {
            let fill = track.with_max_x(track.left() + track.width() * (left as f32 / 100.0).clamp(0.0, 1.0));
            let bar = if low(quota) { color } else { lerp(p.success, p.muted, 0.35) };
            painter.rect_filled(fill, 2.0, bar);
        }
        egui::Popup::menu(&response).show(|ui| {
            ui.set_min_width(260.0);
            crate::resources::usage(ui, &resources, &p);
        });
        flow::record(ui, &format!("usage:{name}"), response.rect);
    }
}

/// The plans in a menu, where the window has no room for their rail.
fn plan_picker(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let plans = backend.plan_list();
    let selected = selected_plan(backend);
    let title = selected.and_then(|i| plans.get(i)).map_or("Plans".to_string(), |plan| goal(&plan.goal).to_string());
    let title = if let Some(chat) = &client.simple.past_chat {
        &chat.title
    } else if client.simple.chat_id.is_some() {
        "Chats"
    } else {
        &title
    };
    let title: String = title.chars().take(36).collect();
    let p = client.palette.clone();
    let button = egui::Button::new(with_chevron(RichText::new(title).font(font(13.0)), p.muted, true));
    let config =
        egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
    egui::containers::menu::MenuButton::from_button(button).config(config).ui(ui, |ui| {
        ui.set_min_width(240.0);
        ui.set_max_height((ui.ctx().content_rect().height() - 100.0).clamp(150.0, 420.0));
        plan_list(ui, backend, client, 0.0);
    });
}

fn open_chat(backend: &dyn Backend, client: &mut Client, id: String) {
    let previous = client
        .simple
        .chat_id
        .take()
        .or_else(|| client.simple.chat_plan.take())
        .or_else(|| selected_plan(backend).and_then(|i| backend.plan_row(i)).map(|p| p.id));
    if let Some(previous) = previous {
        client.simple.drafts.insert(previous, std::mem::take(&mut client.simple.composer));
    }
    client.simple.composer = client.simple.drafts.remove(&id).unwrap_or_default();
    client.simple.chat_id = Some(id);
    client.simple.chat_plan = None;
    client.simple.past_chat = None;
    client.simple.ticket_plan = None;
    client.simple.chief_terminal = false;
    client.simple.focus_composer = true;
    client.simple.pane = Pane::Chief;
    client.simple.work = Work::Flow;
}

fn open_plan(backend: &dyn Backend, client: &mut Client, index: usize) {
    let Some(plan) = backend.plan_row(index) else { return };
    client.simple.past_chat = None;
    let open = plan.status == PlanStatus::Open;
    let previous = if client.simple.chat_id.is_some() {
        client.simple.chat_id.clone()
    } else {
        client
            .simple
            .chat_plan
            .clone()
            .or_else(|| selected_plan(backend).and_then(|i| backend.plan_row(i)).map(|p| p.id))
    };
    if previous.as_ref() != Some(&plan.id) {
        if let Some(previous) = previous {
            client.simple.drafts.insert(previous, std::mem::take(&mut client.simple.composer));
        }
        client.simple.composer = client.simple.drafts.remove(&plan.id).unwrap_or_default();
    }
    client.simple.chat_id = None;
    client.simple.last_plan = Some((plan.id.clone(), plan.status));
    client.simple.chat_plan = Some(plan.id);
    client.send(if open { Command::ShowPlan(index) } else { Command::ViewPlanHistory(index) });
    client.simple.work = Work::Flow;
    client.simple.flow.selected.clear();
    client.simple.pane = Pane::Work;
}

/// The plans, like conversations: a new one at the top, the open ones with their state, then the earlier ones;
/// the agents' terminals and the full board at the bottom.
pub fn plans(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.sidebar);
    let inner = rect.shrink2(vec2(12.0, 14.0));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-plans").max_rect(inner));
    child.set_clip_rect(rect);
    // On phones the full account panel sits below this rail.
    let footer = rect.width() <= RAIL && backend.resources().is_some();
    let reserve = if footer { 104.0 } else { 0.0 };
    plan_list(&mut child, backend, client, reserve);
    if footer {
        let bottom = inner.with_min_y(inner.bottom() - reserve);
        let mut footer = ui.new_child(egui::UiBuilder::new().id_salt("simple-rail-usage").max_rect(bottom));
        footer.set_clip_rect(bottom);
        usage_footer(&mut footer, backend, client);
    }
}

fn plan_list(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, reserved: f32) {
    let p = client.palette.clone();
    let primary = egui::Button::new(RichText::new("New chat").font(font(14.0)).color(p.on_accent))
        .fill(p.accent)
        .corner_radius(9)
        .min_size(vec2(ui.available_width(), 36.0));
    if ui.add(primary).clicked()
        && let Ok(token) = atomics_core::ticket_ui::request_id()
    {
        open_chat(backend, client, format!("chat-{token}"));
        if egui::containers::menu::is_in_menu(ui) {
            egui::Popup::close_all(ui.ctx());
            ui.close();
        }
    }
    ui.add_space(4.0);
    let plan = egui::Button::new(RichText::new("New plan").font(font(13.5)).color(p.foreground))
        .fill(p.raised)
        .corner_radius(9)
        .min_size(vec2(ui.available_width(), 32.0));
    let response = ui.add(plan);
    flow::record(ui, "new-plan", response.rect);
    if response.clicked() {
        new_plan::open(backend, client);
        if egui::containers::menu::is_in_menu(ui) {
            egui::Popup::close_all(ui.ctx());
            ui.close();
        }
    }
    ui.add_space(8.0);
    let response = ui.add(
        egui::TextEdit::singleline(&mut client.simple.plan_query)
            .hint_text(look::text("Find a chat or plan…", look::LABEL, Weight::Regular, look::Ink::of(&p).tertiary))
            .font(font(look::LABEL))
            .desired_width(f32::INFINITY)
            .margin(egui::Margin::symmetric(10, 6)),
    );
    touch::opens_keyboard(ui, &response);
    let query = client.simple.plan_query.trim().to_lowercase();
    ui.add_space(12.0);
    let plans = backend.plan_list();
    let selected = selected_plan(backend);
    let bottom = reserved + 8.0;
    egui::ScrollArea::vertical()
        .id_salt("simple-plan-list")
        .max_height((ui.available_height() - bottom).max(60.0))
        .show(ui, |ui| {
            let mut matches = history::list(ui, backend, client, &query);
            if matches > 0 {
                ui.add_space(10.0);
            }
            if query.is_empty() || plans.iter().any(shown) {
                ui.label(look::text("Plans", look::SMALL, Weight::Medium, look::Ink::of(&p).secondary));
                ui.add_space(4.0);
            }
            for (label, status) in
                [("Plans", PlanStatus::Open), ("Completed", PlanStatus::Completed), ("Archived", PlanStatus::Archived)]
            {
                let mut indices: Vec<_> = plans
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, plan)| {
                        plan.status == status
                            && shown(plan)
                            && (query.is_empty()
                                || [
                                    plan.goal.as_str(),
                                    &plan.id,
                                    &plan.tab_name,
                                    plan.first_message.as_deref().unwrap_or(""),
                                ]
                                .iter()
                                .any(|text| text.to_lowercase().contains(&query)))
                    })
                    .map(|(i, _)| i)
                    .collect();
                indices.sort_by_key(|&i| std::cmp::Reverse(plans[i].created_at));
                if indices.is_empty() {
                    continue;
                }
                matches += indices.len();
                let mut rows = |ui: &mut egui::Ui| {
                    for &i in &indices {
                        if rail_row_visible(ui, PLAN_ROW) {
                            ui.push_id(i, |ui| {
                                plan_item(
                                    ui,
                                    backend,
                                    client,
                                    i,
                                    client.simple.chat_id.is_none()
                                        && client.simple.past_chat.is_none()
                                        && selected == Some(i),
                                );
                            });
                        }
                    }
                };
                if status == PlanStatus::Open {
                    rows(ui);
                } else {
                    egui::CollapsingHeader::new(look::text(
                        format!("{label} ({})", indices.len()),
                        look::SMALL,
                        Weight::Medium,
                        look::Ink::of(&p).secondary,
                    ))
                    .id_salt(("simple-plan-folder", label))
                    .open((!query.is_empty()).then_some(true))
                    .show_unindented(ui, rows);
                }
                ui.add_space(8.0);
            }
            if matches == 0 && !query.is_empty() {
                ui.label(look::text(
                    "No matching chats or plans",
                    look::LABEL,
                    Weight::Regular,
                    look::Ink::of(&p).secondary,
                ));
            }
            if !plans.iter().any(shown) && query.is_empty() {
                ui.label(look::text("No plans yet", look::LABEL, Weight::Regular, look::Ink::of(&p).tertiary));
            }
        });
}

/// A plan's goal on one line, or a word for a plan without one yet.
fn goal(text: &str) -> String {
    let title = crate::markdown::plain_title(text);
    if title.is_empty() { "Untitled plan".into() } else { title }
}

const PLAN_ROW: f32 = 50.0;

/// Preserve the scroll extent without constructing widgets for fixed-height rows outside the rail.
fn rail_row_visible(ui: &mut egui::Ui, height: f32) -> bool {
    let top = ui.next_widget_position().y;
    let visible = ui.clip_rect();
    if top + height >= visible.top() && top <= visible.bottom() {
        true
    } else {
        ui.add_space(height + ui.spacing().item_spacing.y);
        false
    }
}

/// One plan in the rail: its state's mark, its goal and how it stands.
fn plan_item(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, index: usize, selected: bool) {
    let Some(plan) = backend.plan_row(index) else { return };
    let p = client.palette.clone();
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), PLAN_ROW), Sense::click());
    flow::record(ui, &format!("plan:{index}"), rect);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 10.0, lerp(p.sidebar, p.foreground, 0.07));
        painter.rect_filled(Rect::from_min_size(pos2(rect.left(), rect.top() + 13.0), vec2(3.0, 24.0)), 1.5, p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect, 10.0, p.hover());
    }
    let dot = pos2(rect.left() + 16.0, rect.top() + 17.0);
    match plan.state {
        PlanState::Running(_) => {
            painter.circle_filled(dot, 7.0, p.accent.gamma_multiply(0.18));
            painter.circle_filled(dot, 3.5, p.accent);
        }
        PlanState::NeedsYou => {
            let d = |x: f32, y: f32| dot + vec2(x, y);
            let diamond = vec![d(0.0, -5.0), d(5.0, 0.0), d(0.0, 5.0), d(-5.0, 0.0)];
            painter.add(egui::Shape::convex_polygon(diamond, p.warning, Stroke::NONE));
        }
        PlanState::Completed | PlanState::Archived => {
            flow::tick(painter, dot, 9.0, Stroke::new(1.6, p.success.gamma_multiply(0.8)))
        }
        PlanState::Idle => {
            painter.circle_stroke(dot, 3.5, Stroke::new(1.2, p.quiet));
        }
    }
    let closed = plan.status != PlanStatus::Open;
    let goal = goal(plan.first_message.as_deref().unwrap_or(&plan.goal));
    let width = rect.width() - 40.0;
    let ink = look::Ink::of(&p);
    let goal =
        flow::truncated(painter, &goal, font(look::LABEL), if closed { ink.secondary } else { ink.primary }, width);
    painter.galley(pos2(rect.left() + 32.0, rect.top() + 17.0 - goal.size().y / 2.0), goal, p.foreground);
    let (detail, color) = if plan.awaits_approval() {
        ("waits for your approval".to_string(), p.warning)
    } else if plan.state == PlanState::NeedsYou {
        ("needs you".to_string(), p.warning)
    } else if closed {
        let word = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
        let date = backend.plan_date(index);
        (if date.is_empty() { word.to_string() } else { format!("{word} · {date}") }, ink.tertiary)
    } else {
        let running = match plan.state {
            PlanState::Running(n) => format!("{n} running · "),
            _ => String::new(),
        };
        (format!("{running}{} of {} done", plan.done, plan.total), ink.secondary)
    };
    let detail =
        flow::truncated(painter, &detail, font(look::SMALL), p.simple_text(color, look::TERTIARY_CONTRAST), width);
    painter.galley(pos2(rect.left() + 32.0, rect.top() + 35.0 - detail.size().y / 2.0), detail, color);
    if response.clicked() {
        open_plan(backend, client, index);
        if egui::containers::menu::is_in_menu(ui) {
            egui::Popup::close_all(ui.ctx());
            ui.close();
        }
    }
}

/// One part of the chief's conversation, as the transcript draws it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Said {
    /// What the user (or a steering human) asked.
    User(String),
    /// A note ATOMICS itself gave the chief, such as a wake.
    Atomics(String),
    /// The chief's answer.
    Chief(String),
    /// The chief planned tasks: the plan, by id, and how many.
    Planned(Option<String>, usize),
}

/// The chief's conversation from its trace: its prompts and answers, and the plans it made, oldest first.
pub fn conversation(trace: &atomics_core::trace_feed::TraceData) -> Vec<Said> {
    timed(trace).into_iter().map(|(_, said)| said).collect()
}

/// [`conversation`], each with when it happened (milliseconds since the epoch, by the host's clock).
pub fn timed(trace: &atomics_core::trace_feed::TraceData) -> Vec<(u64, Said)> {
    timed_for(trace, None)
}

fn timed_for(trace: &atomics_core::trace_feed::TraceData, plan: Option<&str>) -> Vec<(u64, Said)> {
    let conversation = trace.entries.last().map(|entry| entry.session_id.clone());
    let original_chat = plan.and_then(|id| {
        trace.entries.iter().find_map(|e| {
            (e.tool == "atomics_plan_set" && e.plan.as_ref()?.id.as_deref() == Some(id))
                .then_some(e.chat_id.as_deref())
                .flatten()
        })
    });
    trace
        .entries
        .iter()
        .filter(|entry| match plan {
            Some(id) => {
                entry.plan_id.as_deref() == Some(id)
                    || entry.plan.as_ref().and_then(|p| p.id.as_deref()) == Some(id)
                    || (original_chat.is_some() && original_chat == entry.chat_id.as_deref())
            }
            None => Some(&entry.session_id) == conversation.as_ref(),
        })
        .filter_map(|entry| match &entry.turn {
            Some(turn) => Some(match turn.role {
                TurnRole::User if turn.text.starts_with("ATOMICS ") => Said::Atomics(turn.text.clone()),
                TurnRole::User => Said::User(turn.text.clone()),
                TurnRole::Chief => Said::Chief(turn.text.clone()),
            })
            .map(|said| (entry.at, said)),
            None if entry.op == TraceOp::Plan
                && entry.tool == "atomics_plan_set"
                && entry.state == atomics_core::trace::TraceState::Ok =>
            {
                let plan = entry.plan.as_ref();
                Some((
                    entry.at,
                    Said::Planned(
                        plan.and_then(|p| p.id.clone()).or(entry.plan_id.clone()),
                        plan.map_or(0, |p| p.tasks.len() + entry.omitted),
                    ),
                ))
            }
            None => None,
        })
        .collect()
}

/// A task's state in a word, for the conversation's list of what the chief started.
fn state_word(status: TaskStatus, live: Live, p: &Palette) -> (&'static str, Color32) {
    match (status, live) {
        (_, Live::Waiting) => ("Needs you", p.warning),
        (TaskStatus::Running, Live::Working) => ("Working…", p.accent),
        (TaskStatus::Running, _) => ("Waiting", p.muted),
        (TaskStatus::Planned, _) => ("Planned", p.quiet),
        (TaskStatus::Review, _) => ("Ready for review", p.info),
        (TaskStatus::Blocked, _) => ("Blocked", p.error),
        (TaskStatus::Done, _) => ("Done", p.success),
    }
}

/// A message the user sent the chief, until the chief's conversation shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub text: String,
    plan: Option<String>,
    /// This client's clock when it was written.
    pub at: f64,
    /// It went to the chief; until then it waits for the chief to start or to be ready.
    pub sent: bool,
}

/// How long a starting chief may go without its hooks before the view shows its terminal: it sits at a question.
pub const QUICK_ANSWER_AFTER: f64 = 6.0;

/// Example first messages, for an empty conversation.
/// Each with the few words its chip shows and the whole first message a click fills in.
const EXAMPLES: [(&str, &str); 3] = [
    ("Explain this project", "Look around this project and tell me what it does"),
    ("Fix the failing tests", "Fix the failing tests and explain what was wrong"),
    ("Plan from my tickets", "Plan this week's work from my tickets"),
];
/// The third example where no ticket system is connected.
const WITHOUT_TICKETS: (&str, &str) =
    ("Suggest improvements", "Suggest three small improvements to this project, without changing anything yet");

/// How long ago, in a few words.
/// How long ago, in a few words; nothing for a time that cannot be right (before 2020, ahead of now, or more than a
/// year back), rather than a wrong one.
pub fn ago(then_ms: u64, now_ms: u64) -> Option<String> {
    const YEAR_MS: u64 = 366 * 24 * 3600 * 1000;
    const SINCE_MS: u64 = 1_577_836_800_000; // 2020-01-01
    if then_ms < SINCE_MS || now_ms < SINCE_MS || then_ms > now_ms + 60_000 || now_ms - then_ms.min(now_ms) > YEAR_MS {
        return None;
    }
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    Some(match secs {
        0..45 => "just now".into(),
        45..3600 => format!("{} min", secs.div_ceil(60)),
        3600..86400 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    })
}

/// The same reading width for a live conversation and its saved transcript.
fn conversation_column(rect: Rect) -> Rect {
    Rect::from_center_size(rect.center(), vec2(rect.width().min(808.0), rect.height()))
}

/// The chief's conversation as a chat: what the user sent at once, with where it stands; the chief's answers; the
/// plans it made, as cards; while it works, what it is doing; while it starts, how long, and when it sits at a
/// question of its own (a sign-in, a trust or theme prompt), its terminal right in the chat. One click shows its
/// whole terminal in place, and "← Chat" comes back. The box to write to it is there before the chief runs: the
/// first message starts it.
pub fn chat(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    if let Some(saved) = client.simple.past_chat.clone() {
        history::read(ui, rect, backend, client, &saved);
        return;
    }
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.background);
    let gutter = if touch::stacks(client.touch) { 16.0 } else { 24.0 };
    let inner = rect.shrink2(vec2(gutter, 0.0));
    let now = ui.input(|i| i.time);
    let chosen = client.simple.chat_plan.as_ref().and_then(|id| backend.plan_list().into_iter().find(|p| &p.id == id));
    let history = chosen.as_ref().is_some_and(|p| p.status != PlanStatus::Open);
    let chief = chief_session(backend);
    let starting = chief.as_ref().is_some_and(|(_, s)| s.status == AgentStatus::Starting);
    client.simple.chief_starting = match (starting, client.simple.chief_starting) {
        (true, None) => Some(now),
        (true, since) => since,
        (false, _) => None,
    };
    let stuck = client.simple.chief_starting.is_some_and(|since| now - since >= QUICK_ANSWER_AFTER);
    // The header: who the chief is and how it stands, and its terminal one click away.
    let header = Rect::from_min_size(pos2(inner.left(), rect.top() + 12.0), vec2(inner.width(), 34.0));
    let painter = ui.painter().clone();
    let c = pos2(header.left() + 14.0, header.center().y);
    painter.circle(c, 14.0, lerp(p.background, p.accent, 0.12), Stroke::new(1.2, p.accent));
    crate::chief::orbit_icon(&painter, Rect::from_center_size(c, vec2(18.0, 18.0)), p.accent, p.foreground);
    let ink = look::Ink::of(&p);
    let options = backend.available_chiefs().filter(|_| !history);
    // A word under the name only for what the thread does not show: its own activity is there, animated.
    let status = match &chief {
        _ if history => Some(("Plan history", ink.tertiary)),
        _ if options.as_ref().is_some_and(|o| o.switching) => Some(("Switching chief…", ink.tertiary)),
        Some(_) if stuck => Some(("Needs a quick answer", p.warning)),
        _ => None,
    };
    let name_y = if status.is_some() { c.y - 7.0 } else { c.y };
    look::paint(
        &painter,
        pos2(c.x + 24.0, name_y),
        Align2::LEFT_CENTER,
        "Chief",
        look::BODY,
        Weight::Medium,
        ink.primary,
    );
    if let Some((status, color)) = status {
        painter.circle_filled(pos2(c.x + 27.5, c.y + 9.0), 3.0, color);
        painter.text(pos2(c.x + 35.0, c.y + 9.0), Align2::LEFT_CENTER, status, font(look::SMALL), color);
    }
    let has_chat = backend.chat();
    let expand = Rect::from_min_size(pos2(header.right() - 34.0, header.top()), vec2(34.0, 34.0));
    let response = ui.interact(expand, ui.scope_id().with("chief-expand"), Sense::click());
    let name = if client.simple.chat_focus { "Restore layout" } else { "Expand chat" };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
    flow::record(ui, "expand-chat", expand);
    if response.hovered() {
        painter.rect_filled(expand, 8.0, p.hover());
    }
    for x in [-1.0, 1.0] {
        for y in [-1.0, 1.0] {
            let (distance, leg) = if client.simple.chat_focus { (3.0, 4.0) } else { (7.0, -4.0) };
            let corner = expand.center() + vec2(x * distance, y * distance);
            painter.line_segment([corner + vec2(leg * x, 0.0), corner], Stroke::new(1.5, p.muted));
            painter.line_segment([corner, corner + vec2(0.0, leg * y)], Stroke::new(1.5, p.muted));
        }
    }
    if response.on_hover_text(name).clicked() {
        client.simple.chat_focus = !client.simple.chat_focus;
    }
    // The chief's terminal: back from it with a pill, to it from a menu, where a newcomer need not see it.
    if chief.is_some() && has_chat && !history {
        if client.simple.chief_terminal {
            let toggle = Rect::from_min_size(pos2(expand.left() - 122.0, header.top() + 4.0), vec2(118.0, 26.0));
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("simple-chief-toggle")
                    .max_rect(toggle)
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            if pill(&mut child, "←  Chat", false, client).clicked() {
                client.simple.chief_terminal = false;
            }
        } else {
            let more = Rect::from_min_size(pos2(expand.left() - 36.0, header.top()), vec2(34.0, 34.0));
            chief_menu(ui, more, client);
        }
    }
    let line = header.bottom() + 10.0;
    if let Some(options) = options {
        let controls =
            if chief.is_some() && has_chat { if client.simple.chief_terminal { 126.0 } else { 40.0 } } else { 4.0 };
        let (top, bottom) =
            if status.is_some() { (header.top() - 1.0, header.center().y + 7.0) } else { (c.y - 13.0, c.y + 13.0) };
        let slot = Rect::from_min_max(pos2(header.left() + 83.0, top), pos2(expand.left() - controls, bottom));
        model::picker(ui, slot, &options, chief.as_ref().map(|(_, s)| s.id.as_str()), backend, client);
    }
    painter.hline(inner.x_range(), line, Stroke::new(1.0, p.separator));
    let body = Rect::from_min_max(pos2(rect.left(), line + 1.0), rect.max);
    if !history
        && let Some((index, session)) = &chief
        && (client.simple.chief_terminal || !has_chat || session.outside)
    {
        chief_terminal(ui, body.shrink2(vec2(6.0, 4.0)), *index, session, backend, client, terminals);
        return;
    }
    // The chief's own permission requests wait here too, over its conversation, as in its window.
    let body = match &chief {
        Some((index, _)) => approvals::banner(ui, body, *index, backend, client),
        None => body,
    };
    let composer_h = 58.0;
    let composer = Rect::from_min_max(
        pos2(inner.left(), body.bottom() - composer_h - 14.0),
        pos2(inner.right(), body.bottom() - 14.0),
    );
    let archived = chosen.as_ref().is_some_and(|plan| plan.status == PlanStatus::Archived);
    let transcript_bottom = if archived { body.bottom() } else { composer.top() - 8.0 };
    let transcript = Rect::from_min_max(pos2(inner.left(), body.top()), pos2(inner.right(), transcript_bottom));
    flush(backend, client);
    let mut trace = backend.trace();
    if let Some(plan) = chosen.as_ref().filter(|_| history)
        && let Some(past) = backend.past_turns(&plan.id)
    {
        for entry in past {
            if !trace.entries.iter().any(|e| (e.at, e.seq, &e.session_id) == (entry.at, entry.seq, &entry.session_id)) {
                trace.entries.push(entry);
            }
        }
        trace.entries.sort_by_key(|e| (e.at, e.seq));
    }
    let said = if let Some(chat) = &client.simple.chat_id {
        let mut scoped = trace.clone();
        scoped.entries.retain(|e| e.chat_id.as_ref() == Some(chat));
        let mut said = timed(&scoped);
        if said.is_empty()
            && let Some(saved) = backend.chief_history().iter().find(|c| &c.conversation == chat)
        {
            said.extend(saved.turns.iter().map(|t| {
                (0, if t.speaker == "user" { Said::User(t.text.clone()) } else { Said::Chief(t.text.clone()) })
            }));
        }
        said
    } else {
        timed_for(&trace, chosen.as_ref().map(|p| p.id.as_str()))
    };
    // ATOMICS's own notes to the chief (a wake, a restart) are not the user's conversation.
    let said: Vec<_> = said.into_iter().filter(|(_, said)| !matches!(said, Said::Atomics(_))).collect();
    // A message the chief's conversation shows is delivered: its bubble is the conversation's own now.
    let users: std::collections::HashSet<_> = trace
        .entries
        .iter()
        .filter_map(|entry| {
            let turn = entry.turn.as_ref()?;
            (turn.role == atomics_core::trace::TurnRole::User).then_some((entry.chat_id.as_deref(), turn.text.trim()))
        })
        .collect();
    client.simple.outbox.retain(|out| {
        !(out.sent
            && users.contains(&(
                atomics_core::trace::chat_scope(&out.text),
                atomics_core::trace::chat_body(&out.text).trim(),
            )))
            && now - out.at < 600.0
    });
    let outbox: Vec<_> = client
        .simple
        .outbox
        .iter()
        .filter(|out| out.plan.as_ref() == client.simple.chat_id.as_ref().or(client.simple.chat_plan.as_ref()))
        .cloned()
        .collect();
    let empty = chosen.is_none() && said.is_empty() && outbox.is_empty() && !stuck;
    if empty && body.height() > 290.0 {
        // On the optical centre, a little above the middle, as a page's title would be.
        let middle = body.top() + body.height() * 0.42;
        let composer = Rect::from_center_size(pos2(inner.center().x, middle), vec2(inner.width(), composer_h));
        let heading = if client.simple.chat_id.is_some() { "How can I help?" } else { "Tell the chief what to do" };
        look::paint(
            ui.painter(),
            pos2(composer.center().x, composer.top() - 24.0),
            Align2::CENTER_BOTTOM,
            heading,
            look::DISPLAY,
            Weight::Semibold,
            p.foreground,
        );
        composer_box(ui, composer, chief.as_ref().map(|(_, s)| s), backend, client);
        let suggestions = Rect::from_min_max(pos2(inner.left(), composer.bottom() + 14.0), inner.max);
        let mut chips = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("welcome-suggestions")
                .max_rect(suggestions)
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        examples(&mut chips, backend, client, true);
        return;
    }
    let now_ms = backend.trace_now_ms().max(said.last().map_or(0, |(at, _)| *at));
    // The scroll bar runs in the gutter beside the column, clear of the bubbles' and cards' edges.
    let scrolled = transcript.with_max_x(transcript.right() + (gutter - 6.0).clamp(0.0, 14.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-transcript")
            .max_rect(scrolled)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    child.set_clip_rect(scrolled);
    egui::ScrollArea::vertical()
        .id_salt(("simple-transcript-scroll", client.simple.chat_id.as_ref(), chosen.as_ref().map(|p| &p.id)))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(&mut child, |ui| {
            ui.set_max_width(transcript.width());
            if let Some(plan) = chosen.as_ref().filter(|_| history) {
                let ink = look::Ink::of(&p);
                ui.label(look::text(goal(&plan.goal), look::TITLE, Weight::Semibold, ink.primary));
                let state = if plan.status == PlanStatus::Completed { "Completed" } else { "Archived" };
                let summary = format!("{state} · {} of {} tasks accepted", plan.done, plan.total);
                ui.label(look::text(summary, look::LABEL, Weight::Regular, ink.secondary));
                if pill(ui, "View final board", false, client).clicked() {
                    client.simple.chat_focus = false;
                    client.simple.pane = Pane::Work;
                    client.simple.work = Work::Flow;
                }
                ui.add_space(16.0);
            }
            ui.add_space(14.0);
            if !history && said.is_empty() && outbox.is_empty() {
                if let Some(plan) = chosen.as_ref() {
                    ui.label(look::text(goal(&plan.goal), look::TITLE, Weight::Semibold, p.foreground));
                } else {
                    if client.simple.chat_id.is_some() {
                        ui.label(look::text("How can I help?", look::TITLE, Weight::Semibold, p.foreground));
                    } else {
                        first_use(ui, backend, client);
                    }
                }
            }
            transcript_messages(ui, &said, now_ms, backend, client);
            // Restored and ctl-created proposals can arrive before their trace card.
            if client.simple.chat_id.is_none()
                && let Some(index) = selected_plan(backend)
                && let Some(row) = backend.plan_row(index)
                && row.status == PlanStatus::Open
                && !said.iter().any(|(_, s)| matches!(s, Said::Planned(Some(id), _) if id == &row.id))
            {
                if row.awaits_approval() {
                    transcript_item(ui, &Said::Planned(Some(row.id), row.total), backend, client);
                } else {
                    approval_controls(ui, index, backend, client);
                }
            }
            for out in &outbox {
                let state = match (&chief, out.sent) {
                    (None, _) => "starting the chief…",
                    (Some(_), false) => "waiting for the chief to be ready…",
                    (Some(_), true) => "sent",
                };
                bubble(ui, &out.text, Some(state), client);
                ui.add_space(ACTIONS);
            }
            if (client.simple.chat_id.is_none() || !said.is_empty() || !outbox.is_empty())
                && (!history || !outbox.is_empty())
                && let Some((index, session)) = &chief
            {
                chief_now(ui, *index, session, stuck, now, backend, client, terminals);
            }
            ui.add_space(8.0);
        });
    if !archived {
        composer_box(ui, composer, chief.as_ref().map(|(_, s)| s), backend, client);
    }
}

/// What an empty conversation offers: what to do, and examples to start from; and what setup still misses.
/// Variable-height bubbles are measured once at the pane's current width. A scroll keeps their
/// real heights and draws only rows near the viewport, including their links and hover actions.
#[derive(Debug, Clone, Default)]
struct TranscriptRows {
    layout: Option<(egui::Id, u32, u32)>,
    rows: Vec<((u64, Said), Option<f32>)>,
}

impl TranscriptRows {
    fn sync(&mut self, layout: (egui::Id, u32, u32), said: &[(u64, Said)]) {
        if self.layout != Some(layout) {
            self.rows.clear();
            self.layout = Some(layout);
        }
        if self.rows.len() == said.len() && self.rows.iter().zip(said).all(|((row, _), item)| row == item) {
            return;
        }
        let mut heights: std::collections::HashMap<_, _> = std::mem::take(&mut self.rows).into_iter().collect();
        self.rows = said.iter().map(|item| (item.clone(), heights.remove(item).flatten())).collect();
    }
}

fn transcript_messages(ui: &mut egui::Ui, said: &[(u64, Said)], now: u64, backend: &dyn Backend, client: &mut Client) {
    let mut transcript = std::mem::take(&mut client.simple.transcript);
    transcript.sync((ui.scope_id(), ui.available_width().to_bits(), ui.ctx().pixels_per_point().to_bits()), said);
    let visible = ui.clip_rect().expand(80.0);
    let latest = said.iter().rposition(|(_, item)| matches!(item, Said::User(_)));
    for (index, ((at, item), height)) in transcript.rows.iter_mut().enumerate() {
        let top = ui.next_widget_position().y;
        if let Some(height) = *height
            && (top + height < visible.top() || top > visible.bottom())
        {
            ui.add_space(height);
            continue;
        }
        ui.push_id(index, |ui| {
            let message = ui.scope(|ui| transcript_item(ui, item, backend, client)).response;
            if matches!(item, Said::User(_) | Said::Chief(_)) {
                message_actions(ui, message.rect, item, ago(*at, now), latest == Some(index), backend, client);
            }
        });
        *height = Some(ui.next_widget_position().y - top);
    }
    client.simple.transcript = transcript;
}

fn first_use(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.add_space(24.0);
    let ink = look::Ink::of(&p);
    ui.label(look::text("Tell the chief what to do", look::TITLE, Weight::Semibold, ink.primary));
    ui.add_space(4.0);
    let line = "It plans the work with you, runs agents to do it, and keeps you posted here. Nothing starts until you approve its plan.";
    ui.add(egui::Label::new(look::text(line, look::BODY, Weight::Regular, ink.secondary)).wrap());
    ui.add_space(14.0);
    examples(ui, backend, client, false);
}

/// The example first messages as chips in a row, centred under the welcome's composer or from the left in the
/// conversation; a click puts the whole message in the composer.
fn examples(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, centred: bool) {
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let (gap, pad) = (8.0, 14.0);
    let mut examples = EXAMPLES;
    if backend.tickets().is_none() {
        examples[2] = WITHOUT_TICKETS;
    }
    let widths: Vec<f32> = examples
        .iter()
        .map(|(label, _)| ui.painter().layout_no_wrap(label.to_string(), font(look::LABEL), ink.secondary).size().x)
        .map(|text| text + 2.0 * pad)
        .collect();
    let row = widths.iter().sum::<f32>() + gap * (widths.len() - 1) as f32;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(gap, gap);
        ui.spacing_mut().button_padding = vec2(pad, 6.0);
        if centred && row < ui.available_width() {
            ui.add_space((ui.available_width() - row) / 2.0);
        }
        for (label, prompt) in examples {
            let button = egui::Button::new(look::text(label, look::LABEL, Weight::Regular, ink.secondary))
                .fill(lerp(p.background, p.raised, 0.6))
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(16)
                .min_size(vec2(0.0, 32.0));
            let response = ui.add(touch::sized(button, client.touch)).on_hover_text(prompt);
            flow::record(ui, &format!("example:{label}"), response.rect);
            if response.clicked() {
                client.simple.composer = prompt.to_string();
                client.simple.focus_composer = true;
            }
        }
    });
    if let Some(missing) = setup_missing(backend) {
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(look::text(missing, look::LABEL, Weight::Regular, p.warning));
            if pill(ui, "Open setup", false, client).clicked() {
                setup::open(client);
            }
        });
    }
}

/// What setup still misses, in a line, from the host's own setup steps; `None` when nothing does or it does not say.
fn setup_missing(backend: &dyn Backend) -> Option<String> {
    let setup = backend.management()?.setup;
    let missing: Vec<String> = setup
        .programs
        .iter()
        .filter(|program| program.agent.is_some() && program.found.is_none())
        .map(|program| program.name.clone())
        .take(3)
        .collect();
    (!missing.is_empty()).then(|| format!("Not found on the host yet: {}.", missing.join(", ")))
}

/// What the chief does now, under its conversation: starting and for how long; at a question of its own, with its
/// terminal right here; or thinking, with what it is doing.
#[allow(clippy::too_many_arguments)]
fn chief_now(
    ui: &mut egui::Ui,
    index: usize,
    session: &SessionRow,
    stuck: bool,
    now: f64,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    let since = client.simple.chief_starting;
    if stuck {
        ui.add_space(6.0);
        ui.label(look::text("The chief needs a quick answer", look::BODY, Weight::Medium, p.warning));
        let note = "It asks something before it can start, such as a sign-in or a question about this folder. Answer it here; the chat comes back when it is done.";
        ui.add(egui::Label::new(look::text(note, look::LABEL, Weight::Regular, look::Ink::of(&p).secondary)).wrap());
        ui.add_space(6.0);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 300.0), Sense::hover());
        terminal(ui, rect, index, session, backend, client, terminals, "simple-chief-inline");
        flow::record(ui, "quick-answer", rect);
        return;
    }
    if let Some(since) = since {
        let text = format!("Starting the chief… {} s", (now - since).max(0.0) as u64);
        ui.label(look::text(text, look::LABEL, Weight::Regular, look::Ink::of(&p).secondary));
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
        return;
    }
    let Some(shown) = working_now(session, backend, client, now) else { return };
    let reduce = reduce_motion(backend, client);
    working_beat(ui, client, reduce, now);
    working::line(ui, &shown, font(look::LABEL + 1.0), p.accent, look::Ink::of(&p).secondary, now, reduce);
    use atomics_core::projection::ActivityKind;
    let step = session.activity.as_ref().and_then(|activity| activity.recent_tools.first());
    let label = step.and_then(|step| match step.kind {
        ActivityKind::Delegate => Some("Starting an agent"),
        ActivityKind::Plan => Some("Updating the plan"),
        ActivityKind::Communicate => Some("Sending an update"),
        ActivityKind::Wait => Some("Waiting for agents"),
        _ => None,
    });
    if let Some(label) = label {
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.label(look::text(label, look::SMALL, Weight::Regular, look::Ink::of(&p).secondary));
        });
    }
}

/// Sends what waits in the outbox once the chief can take it: started and ready.
fn flush(backend: &dyn Backend, client: &mut Client) {
    let Some((_, chief)) = chief_session(backend) else { return };
    if chief.status == AgentStatus::Starting || !chief.can_receive {
        return;
    }
    let waiting: Vec<String> = client
        .simple
        .outbox
        .iter_mut()
        .filter(|out| !out.sent)
        .map(|out| {
            out.sent = true;
            out.text.clone()
        })
        .collect();
    for text in waiting {
        client.send(Command::Steer { sessions: vec![chief.id.clone()], text, now: false });
    }
}

/// A message of the user's: on the right, its text left-aligned within, and where it stands while it is on its way.
fn bubble(ui: &mut egui::Ui, text: &str, state: Option<&str>, client: &Client) {
    let p = &client.palette;
    let width = ui.available_width();
    let max = (width * 0.8).min(560.0);
    let text = said_by_you(text, client);
    let mut job = egui::text::LayoutJob::simple(text.into(), font(look::BODY), p.foreground, max - 32.0);
    job.sections[0].format.line_height = Some(look::leading(look::BODY));
    let galley = ui.painter().layout_job(job);
    let size = galley.size() + vec2(32.0, 20.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, size.y), Sense::hover());
    let bubble = Rect::from_min_size(pos2(rect.right() - size.x, rect.top()), size);
    let fill = lerp(p.background, p.foreground, if state.is_some() { 0.05 } else { 0.075 });
    ui.painter().rect_filled(bubble, 18.0, fill);
    ui.painter().galley(bubble.min + vec2(16.0, 10.0), galley, p.foreground);
    if let Some(state) = state {
        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
            ui.label(look::text(state, look::SMALL, Weight::Regular, look::Ink::of(p).tertiary));
        });
    }
}

/// A message of the user's as they wrote it, without the line that scopes it to a chat or a plan.
fn said_by_you<'a>(text: &'a str, client: &Client) -> &'a str {
    let text =
        atomics_core::trace::chat_scope(text).and_then(|_| text.split_once('\n').map(|(_, body)| body)).unwrap_or(text);
    client.simple.chat_plan.as_ref().and_then(|id| text.strip_prefix(&format!("[Plan {id}]\n"))).unwrap_or(text)
}

/// The height of the row under a message where its time and actions show on hover.
const ACTIONS: f32 = 26.0;

/// The row under a message of the user's or the chief's: empty until the pointer is on the message, then when it was
/// said, Copy, and on the user's latest message Retry, which sends it to the chief again. Its words go right under
/// the user's bubble and left under the chief's reply, where each message is.
#[allow(clippy::too_many_arguments)]
fn message_actions(
    ui: &mut egui::Ui,
    message: Rect,
    item: &Said,
    when: Option<String>,
    latest: bool,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let (text, mine) = match item {
        Said::User(text) => (said_by_you(text, client).to_owned(), true),
        Said::Chief(text) => (text.clone(), false),
        _ => return,
    };
    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), ACTIONS), Sense::hover());
    if !ui.rect_contains_pointer(message.union(row)) {
        return;
    }
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let id = ui.scope_id().with(("message-copied", &text));
    let now = ui.input(|i| i.time);
    let copied = ui.data(|d| d.get_temp::<f64>(id)).is_some_and(|at| now - at < 1.6);
    let steer = Command::Steer { sessions: Vec::new(), text: String::new(), now: false };
    let retry = mine && latest && chief_session(backend).is_some() && backend.available(&steer);
    let layout = if mine {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    let mut child =
        ui.new_child(egui::UiBuilder::new().id_salt(id).max_rect(row.expand2(vec2(7.0, -2.0))).layout(layout));
    child.spacing_mut().item_spacing.x = 2.0;
    child.spacing_mut().button_padding = vec2(7.0, 2.0);
    let action = |ui: &mut egui::Ui, label: &str| {
        let words = look::text(label, look::SMALL, Weight::Medium, ink.secondary);
        ui.add(egui::Button::new(words).frame_when_inactive(false).corner_radius(6).min_size(vec2(0.0, 22.0)))
    };
    // The row reaches out by the buttons' padding, so their words line up with the message's edge. Right to left
    // for the user's: what reads last is added first.
    let mut parts: Vec<&str> = Vec::new();
    if let Some(when) = &when {
        parts.push(when);
    }
    parts.push(if copied { "Copied" } else { "Copy" });
    if retry {
        parts.push("Retry");
    }
    if mine {
        parts.reverse();
    }
    for part in parts {
        match part {
            "Copy" | "Copied" => {
                if action(&mut child, part).clicked() {
                    child.ctx().copy_text(text.clone());
                    child.data_mut(|d| d.insert_temp(id, now));
                }
            }
            "Retry" => {
                if action(&mut child, part).on_hover_text("Send this to the chief again").clicked() {
                    let raw = match item {
                        Said::User(raw) => raw.clone(),
                        _ => continue,
                    };
                    resend(raw, backend, client, now);
                }
            }
            when => {
                child.add_space(5.0);
                child.label(look::text(when, look::SMALL, Weight::Regular, ink.tertiary));
                child.add_space(5.0);
            }
        }
    }
    if copied {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
    }
}

/// A message the user sent before, to the chief again as it was: at once where the chief is ready, else once it is.
fn resend(text: String, backend: &dyn Backend, client: &mut Client, now: f64) {
    let Some((_, chief)) = chief_session(backend) else { return };
    let ready = chief.status != AgentStatus::Starting && chief.can_receive;
    if ready {
        client.send(Command::Steer { sessions: vec![chief.id.clone()], text: text.clone(), now: false });
    }
    let plan = client.simple.chat_id.clone().or_else(|| client.simple.chat_plan.clone());
    client.simple.outbox.push(Outgoing { text, at: now, sent: ready, plan });
}

fn transcript_item(ui: &mut egui::Ui, item: &Said, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    match item {
        Said::User(text) => bubble(ui, text, None, client),
        Said::Atomics(text) => {
            let line = text.lines().next().unwrap_or_default();
            ui.add(
                egui::Label::new(look::text(line, look::SMALL, Weight::Regular, look::Ink::of(&p).tertiary)).truncate(),
            );
            ui.add_space(10.0);
        }
        Said::Chief(text) => prose::show(ui, text, &p),
        Said::Planned(plan, count) => {
            let index = plan.as_ref().and_then(|id| backend.plan_list().iter().position(|row| &row.id == id));
            match index {
                Some(index) => plan_card(ui, index, backend, client),
                // A plan this window no longer has: only the line that it was made.
                None => {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                        ui.painter().circle_stroke(rect.center(), 5.5, Stroke::new(1.2, p.success));
                        flow::tick(ui.painter(), rect.center(), 5.0, Stroke::new(1.3, p.success));
                        let words = format!("Planned {count} task{}", if *count == 1 { "" } else { "s" });
                        ui.label(look::text(words, look::LABEL, Weight::Medium, look::Ink::of(&p).secondary));
                    });
                }
            }
            ui.add_space(22.0);
        }
    }
}

/// A plan the chief made, as one card in the chat: its title and where it stands, its tasks with who would run them
/// or how they stand, the final review as a quiet line under them, and, while the plan waits, the decision itself:
/// the one place simple mode approves a plan.
fn plan_card(ui: &mut egui::Ui, index: usize, backend: &dyn Backend, client: &mut Client) {
    let Some(board) = backend.board(index) else { return };
    let Some(row) = backend.plan_row(index) else { return };
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let proposal = row.awaits_approval();
    let is_review = |task: &&atomics_core::plan::Task| task.id == atomics_core::plan::REVIEW_TASK;
    let work: Vec<_> = board.plan.tasks.iter().filter(|task| !is_review(task)).collect();
    let review = board.plan.tasks.iter().find(is_review);
    let edge = if proposal { lerp(p.border, p.warning, 0.2) } else { p.border };
    egui::Frame::new()
        .fill(lerp(p.background, p.foreground, 0.03))
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(14)
        .inner_margin(egui::Margin { left: 16, right: 16, top: 14, bottom: 14 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(
                egui::Label::new(look::text(goal(&board.plan.goal), look::BODY, Weight::Semibold, ink.primary))
                    .truncate(),
            );
            ui.add_space(4.0);
            let count = format!("{} task{}  ·  ", work.len(), if work.len() == 1 { "" } else { "s" });
            let done = work.iter().filter(|task| task.status == TaskStatus::Done).count();
            let (state, color) = match () {
                _ if proposal => ("Proposed".to_string(), p.warning),
                _ if !board.plan.is_open() => ("Finished".to_string(), ink.secondary),
                _ => (format!("{done} of {} done", work.len()), ink.secondary),
            };
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(look::text(count, look::SMALL, Weight::Regular, ink.secondary));
                ui.label(look::text(state, look::SMALL, Weight::Medium, color));
            });
            ui.add_space(10.0);
            task_rows(ui, index, &board, &work, backend, client, proposal);
            if let Some(review) = review {
                let words = match review.status {
                    TaskStatus::Planned => "+ An automatic review when they're done",
                    TaskStatus::Running | TaskStatus::Review => "Final review in progress",
                    TaskStatus::Done => "Final review passed",
                    TaskStatus::Blocked => "Final review is blocked",
                };
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.label(look::text(words, look::SMALL, Weight::Regular, ink.tertiary));
                    if proposal {
                        let model = board.models.get(&review.id).cloned().unwrap_or_else(|| review.agent.clone());
                        ui.label(look::text(format!("  ·  {model}"), look::SMALL, Weight::Regular, ink.tertiary));
                    }
                });
            }
            approval_controls(ui, index, backend, client);
        });
}

/// What a plan's approval asks or where it stands: while it waits, Approve and start, Change and what approving
/// means; once approved, the agents starting and any task that could not start, with a retry.
fn approval_controls(ui: &mut egui::Ui, index: usize, backend: &dyn Backend, client: &mut Client) {
    let Some(board) = backend.board(index) else { return };
    if !board.plan.is_open() {
        return;
    }
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let now = ui.input(|i| i.time);
    let pending = client.simple.approving.as_ref().is_some_and(|(id, at)| id == &board.plan.id && now - at < 5.0);
    let sessions = backend.session_list();
    let starting = board
        .plan
        .tasks
        .iter()
        .filter(|task| {
            task.status == TaskStatus::Running
                && task
                    .session
                    .as_ref()
                    .is_none_or(|id| sessions.iter().any(|s| &s.id == id && s.status == AgentStatus::Starting))
        })
        .count();
    let status = |ui: &mut egui::Ui, words: &str| {
        ui.add_space(12.0);
        ui.label(look::text(words, look::LABEL, Weight::Medium, p.accent));
    };
    if starting > 0 {
        status(ui, &format!("Starting {starting} agent{}…", if starting == 1 { "" } else { "s" }));
    } else if !board.plan.approved && !board.plan.tasks.is_empty() {
        if pending {
            status(ui, "Starting…");
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
            return;
        }
        ui.add_space(14.0);
        let (line, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().hline(line.x_range(), line.center().y, Stroke::new(1.0, lerp(p.border, p.background, 0.3)));
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            if backend.available(&Command::ApproveAndStartPlan(index))
                && pill(ui, "Approve and start", true, client).clicked()
            {
                client.simple.approving = Some((board.plan.id.clone(), now));
                client.send(Command::ApproveAndStartPlan(index));
            }
            if pill(ui, "Change", false, client).on_hover_text("Tell the chief what to change").clicked() {
                client.simple.composer = "About the plan: ".into();
                client.simple.focus_composer = true;
            }
            ui.add_space(6.0);
            ui.label(look::text("Nothing runs until you approve", look::SMALL, Weight::Regular, ink.tertiary));
        });
    }
    if board.plan.approved {
        for task in board.plan.tasks.iter().filter(|t| t.status == TaskStatus::Planned) {
            if let Some(reason) = task.notes.last().filter(|s| s.starts_with("Could not start:")) {
                ui.add_space(10.0);
                let words = format!("{} · {reason}", task.title);
                ui.add(egui::Label::new(look::text(words, look::LABEL, Weight::Regular, p.warning)).wrap());
                let retry = Command::StartTask { plan: index, task: task.id.clone() };
                if backend.available(&retry) && pill(ui, &format!("Retry {}", task.title), false, client).clicked() {
                    client.send(retry);
                }
            }
        }
    }
}

/// A plan's tasks in its card, each with its live state, or while proposed who would run it; a click opens it.
#[allow(clippy::too_many_arguments)]
fn task_rows(
    ui: &mut egui::Ui,
    index: usize,
    board: &crate::BoardView,
    tasks: &[&atomics_core::plan::Task],
    backend: &dyn Backend,
    client: &mut Client,
    proposal: bool,
) {
    let p = client.palette.clone();
    let ink = look::Ink::of(&p);
    let sessions = backend.session_list();
    let large = tasks.len() > 12;
    let busy = |t: &&&atomics_core::plan::Task| {
        matches!(t.status, TaskStatus::Running | TaskStatus::Review | TaskStatus::Blocked)
    };
    let ongoing = tasks.iter().any(|t| busy(&t));
    let shown: Vec<_> =
        tasks.iter().filter(|t| !large || !ongoing || busy(t)).take(if large { 3 } else { 12 }).collect();
    for (k, task) in shown.into_iter().enumerate() {
        let session = task
            .session
            .as_deref()
            .filter(|_| board.plan.is_open())
            .and_then(|id| sessions.iter().find(|s| s.id == id));
        let (word, color) = state_word(task.status, flow::live(task.status, session), &p);
        // Proposed, a task says who would run it: its agent and model.
        let model = board.models.get(&task.id).cloned().unwrap_or_else(|| task.agent.clone());
        let (word, color, dot) = if proposal {
            (model.as_str(), ink.tertiary, ink.tertiary)
        } else {
            (word, lerp(ink.secondary, color, 0.7), color)
        };
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
        let painter = ui.painter();
        if k > 0 {
            painter.hline(rect.x_range(), rect.top(), Stroke::new(1.0, lerp(p.background, p.border, 0.7)));
        }
        if response.hovered() {
            painter.rect_filled(rect.expand2(vec2(8.0, -2.0)), 6.0, p.hover());
        }
        painter.circle_filled(pos2(rect.left() + 3.0, rect.center().y), 3.0, dot);
        let words = painter.layout_no_wrap(word.to_string(), font(look::SMALL), color);
        let room = rect.width() - words.size().x - 36.0;
        let title = flow::truncated(painter, &task.title, font(look::LABEL + 1.0), ink.primary, room);
        painter.galley(pos2(rect.left() + 16.0, rect.center().y - title.size().y / 2.0), title, ink.primary);
        painter.galley(pos2(rect.right() - words.size().x, rect.center().y - words.size().y / 2.0), words, color);
        if response.clicked() {
            if Some(index) != selected_plan(backend) {
                open_plan(backend, client, index);
            }
            open_task(client, &task.id);
        }
    }
    if large {
        ui.add_space(6.0);
        if pill(ui, &format!("View all {} tasks", tasks.len()), false, client).clicked() {
            open_plan(backend, client, index);
            client.simple.chat_focus = false;
        }
    }
}

/// The box to write to the chief: Enter sends, Shift+Enter starts a new line. Before the chief runs, the first
/// message starts it (with that message, where the platform can, else it follows once the chief is ready). A file
/// dragged over the window shows where to drop it, and a drop goes to [`Backend::attach`].
fn composer_box(ui: &mut egui::Ui, rect: Rect, chief: Option<&SessionRow>, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let steer = Command::Steer { sessions: Vec::new(), text: String::new(), now: false };
    let can_send = match chief {
        Some(_) => backend.available(&steer),
        None => backend.available(&Command::VoiceToChief(String::new())) || backend.available(&Command::ToggleChief),
    };
    let dragging = ui.input(|i| !i.raw.hovered_files.is_empty());
    let dropped: Vec<std::path::PathBuf> = ui.input(|i| {
        i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect()
    });
    if !dropped.is_empty() && !backend.attach(dropped) {
        client.send(Command::Toast("Sending files to the chief is not available yet".into()));
    }
    let edge = if dragging { Stroke::new(1.5, p.accent) } else { Stroke::new(1.0, p.border) };
    ui.painter().rect(rect, 16.0, p.raised, edge, StrokeKind::Inside);
    let send = Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(32.0, 32.0));
    let field =
        Rect::from_min_max(pos2(rect.left() + 14.0, rect.top() + 8.0), pos2(send.left() - 10.0, rect.bottom() - 8.0));
    let id = ui.scope_id().with("simple-composer");
    let focused = ui.memory(|m| m.has_focus(id));
    let mut sent =
        focused && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-composer-field").max_rect(field));
    let hint = if dragging {
        "Drop to send it to the chief"
    } else if !can_send {
        "This window can only watch"
    } else if chief.is_none() {
        "Tell the chief what to do…"
    } else {
        "Message the chief…"
    };
    let edit = egui::TextEdit::multiline(&mut client.simple.composer)
        .id(id)
        .frame(egui::Frame::NONE)
        .desired_rows(1)
        .desired_width(field.width())
        .font(font(look::BODY))
        .hint_text(look::text(
            hint,
            look::BODY,
            Weight::Regular,
            if dragging { p.accent } else { look::Ink::of(&p).tertiary },
        ));
    let response = child.add_enabled(can_send, edit);
    flow::record(&child, "composer", response.rect);
    touch::opens_keyboard(&child, &response);
    if std::mem::take(&mut client.simple.focus_composer) {
        response.request_focus();
    }
    let ready = can_send && !client.simple.composer.trim().is_empty();
    let button = ui.interact(send, id.with("send"), if ready { Sense::click() } else { Sense::hover() });
    flow::record(ui, "send", send);
    let painter = ui.painter();
    painter.circle_filled(send.center(), 16.0, if ready { p.accent } else { lerp(p.raised, p.border, 0.8) });
    let c = send.center();
    let ink = if ready { p.on_accent } else { p.quiet };
    painter.line_segment([pos2(c.x, c.y + 6.0), pos2(c.x, c.y - 6.0)], Stroke::new(2.0, ink));
    let head = vec![pos2(c.x - 5.0, c.y - 1.0), pos2(c.x, c.y - 6.0), pos2(c.x + 5.0, c.y - 1.0)];
    painter.add(egui::Shape::line(head, Stroke::new(2.0, ink)));
    sent |= button.on_hover_text("Send to the chief (Enter)").clicked();
    if !(sent && ready) {
        return;
    }
    let plain = client.simple.composer.trim();
    let scope = client.simple.chat_id.clone().or_else(|| client.simple.chat_plan.clone());
    let text = if let Some(chat) = &client.simple.chat_id {
        format!("[Chat {chat}]\n{plain}")
    } else {
        let text = about_project(plain, message_project(backend, client).as_deref(), backend.repos().len());
        client.simple.chat_plan.as_ref().map_or_else(|| text.clone(), |id| format!("[Plan {id}]\n{text}"))
    };
    client.simple.composer.clear();
    let now = ui.input(|i| i.time);
    let ready_chief = chief.filter(|c| c.status != AgentStatus::Starting && c.can_receive);
    let sent = match (chief, ready_chief) {
        (_, Some(chief)) => {
            client.send(Command::Steer { sessions: vec![chief.id.clone()], text: text.clone(), now: false });
            true
        }
        // Starting: it goes once the chief is ready.
        (Some(_), None) => false,
        // Not running: the first message starts it, with the message where the platform can.
        (None, _) if backend.available(&Command::VoiceToChief(String::new())) => {
            client.send(Command::VoiceToChief(text.clone()));
            true
        }
        (None, _) => {
            client.send(Command::ToggleChief);
            false
        }
    };
    client.simple.outbox.push(Outgoing { text, at: now, sent, plan: scope });
}

/// The chief's "⋯" menu: what a newcomer seldom needs, such as the chief's own terminal.
fn chief_menu(ui: &mut egui::Ui, rect: Rect, client: &mut Client) {
    let ink = look::Ink::of(&client.palette);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-chief-more")
            .max_rect(rect)
            .layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight)),
    );
    let dots = look::text("•••", 10.0, Weight::Regular, ink.secondary).extra_letter_spacing(-0.5);
    let button = egui::Button::new(dots).frame_when_inactive(false).corner_radius(8).min_size(rect.size());
    let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(&mut child, |ui| {
        ui.set_min_width(180.0);
        if ui.button(look::text("Open terminal", look::LABEL, Weight::Regular, ink.primary)).clicked() {
            client.simple.chief_terminal = true;
            ui.close();
        }
    });
    flow::record(ui, "chief-more", response.rect);
    response.on_hover_text("More");
}

/// The chief's terminal where its conversation was, with its permission requests over it, as in its window.
fn chief_terminal(
    ui: &mut egui::Ui,
    rect: Rect,
    index: usize,
    session: &SessionRow,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    terminal(ui, rect, index, session, backend, client, terminals, "simple-chief-term");
}

#[allow(clippy::too_many_arguments)]
fn terminal(
    ui: &mut egui::Ui,
    rect: Rect,
    index: usize,
    session: &SessionRow,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
    salt: &str,
) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 8.0, p.terminal);
    let rect = approvals::banner(ui, rect, index, backend, client);
    if session.outside {
        return;
    }
    let metrics = Metrics::new(ui, &p, backend.ui_config().font.size);
    let id = egui::Id::NULL.with((salt, &session.id));
    if client.focus_terminal.as_deref() == Some(session.id.as_str()) {
        ui.memory_mut(|m| m.request_focus(id));
        client.focus_terminal = None;
    }
    terminals.show(ui, index, &session.id, rect.shrink(4.0), id, &metrics, &p);
}

/// One thing the user is asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Need {
    /// A permission request of the session at this position of the session list (the chief's too).
    Permission(usize),
    /// An agent waits for an answer: the session at this position, and its question where its report asks one.
    Question(usize, Option<String>),
    /// A task is ready for review, by id.
    Review(String),
    /// A message the relay holds, by its notebook entry.
    Held(Box<atomics_core::plan::NotebookEntry>),
}

/// What needs the user in the plan at `index` (and the chief): permission requests, questions, work
/// ready for review and messages the relay holds, in that order.
pub fn wanted(index: usize, backend: &dyn Backend) -> Vec<Need> {
    let Some(row) = backend.plan_row(index) else { return Vec::new() };
    if row.status != PlanStatus::Open {
        return Vec::new();
    }
    let board = backend.board(index);
    let sessions = backend.session_list();
    let mine = |session: &SessionRow| session.chief || session.task.as_ref().is_some_and(|link| link.plan == row.id);
    let mut wanted = Vec::new();
    for (i, _) in sessions.iter().enumerate().filter(|(_, s)| mine(s)) {
        if backend.session_approval(i).is_some() {
            wanted.push(Need::Permission(i));
        }
    }
    for (i, session) in sessions.iter().enumerate().filter(|(_, s)| !s.chief && mine(s)) {
        if session.status == AgentStatus::NeedsInput && session.pending_approvals == 0 {
            let question = session.task.as_ref().and_then(|link| {
                let task = board.as_ref()?.plan.tasks.iter().find(|t| t.id == link.task)?;
                task.report.as_ref().filter(|r| r.status == "question").map(|r| r.summary.clone())
            });
            wanted.push(Need::Question(i, question));
        }
    }
    if let Some(board) = &board {
        for task in board.plan.tasks.iter().filter(|t| t.status == TaskStatus::Review) {
            wanted.push(Need::Review(task.id.clone()));
        }
        let pending = |note: &&atomics_core::plan::NotebookEntry| {
            note.relay.as_ref().is_some_and(|r| r.state == atomics_core::notebook::RelayState::Pending)
        };
        for note in board.plan.notebook.iter().filter(pending) {
            wanted.push(Need::Held(Box::new(note.clone())));
        }
    }
    wanted
}

/// What needs the user, as cards with clear actions; returns how many. Each acts through the same commands as the
/// full window: the approval banner's decisions, the plan's approval, a typed message to the agent, the board's
/// Accept and the relay's decisions.
pub fn needs(ui: &mut egui::Ui, index: usize, backend: &dyn Backend, client: &mut Client) -> usize {
    let p = client.palette.clone();
    let wanted = wanted(index, backend);
    if wanted.is_empty() {
        return 0;
    }
    let Some(row) = backend.plan_row(index) else { return 0 };
    let expanded = client.simple.inbox.as_deref() == Some(&row.id);
    let label = format!("Needs you ({})", wanted.len());
    let response = ui.add(
        egui::Button::new(with_chevron(RichText::new(label).font(font(13.0)).color(p.warning), p.muted, expanded))
            .min_size(vec2(128.0, if touch::stacks(client.touch) { 40.0 } else { 32.0 }))
            .fill(lerp(p.raised, p.warning, 0.045))
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(8),
    );
    if response.clicked() {
        client.simple.inbox = (!expanded).then(|| row.id.clone());
    }
    if client.simple.inbox.as_deref() != Some(&row.id) {
        return wanted.len();
    }
    let board = backend.board(index);
    let sessions = backend.session_list();
    ui.add_space(4.0);
    let card = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
        egui::Frame::new()
            .fill(p.raised)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(12)
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui);
            });
        ui.add_space(8.0);
    };
    let title = |ui: &mut egui::Ui, text: &str| {
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(10.0, 16.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.5, p.warning);
            ui.add(egui::Label::new(RichText::new(text).font(font(14.0)).color(p.foreground)).truncate());
        });
    };
    let agent_text = |ui: &mut egui::Ui, text: &str| quoted(ui, text, &p);
    let task_title = |session: &SessionRow| {
        session
            .task
            .as_ref()
            .and_then(|link| board.as_ref()?.plan.tasks.iter().find(|t| t.id == link.task).map(|t| t.title.clone()))
            .unwrap_or_else(|| if session.chief { "The chief".into() } else { session.title.clone() })
    };
    for need in &wanted {
        match need {
            Need::Permission(i) => card(ui, &mut |ui| {
                let tool = backend.session_approval(*i).map(|a| a.request.tool).unwrap_or_default();
                title(ui, &format!("{} wants to run {tool}", task_title(&sessions[*i])));
                ui.add_space(4.0);
                // The banner's own buttons and decisions, as on the agent's terminal.
                approvals::card(ui, *i, backend, client, p.accent);
            }),
            Need::Question(i, question) => card(ui, &mut |ui| {
                let session = &sessions[*i];
                title(ui, &format!("{} asks you", task_title(session)));
                match question {
                    Some(question) => agent_text(ui, question),
                    None => {
                        let hint = session.status_hint.clone().unwrap_or_else(|| "It waits for your answer.".into());
                        ui.label(RichText::new(hint).font(font(13.0)).color(p.simple_text(p.muted, 7.2)));
                    }
                }
                ui.add_space(6.0);
                answer(ui, session, backend, client);
            }),
            Need::Review(id) => card(ui, &mut |ui| {
                let Some(board) = &board else { return };
                let Some(task) = board.plan.tasks.iter().find(|t| &t.id == id) else { return };
                title(ui, &format!("{} is ready for review", task.title));
                if let Some(report) = &task.report {
                    agent_text(ui, report.summary.lines().next().unwrap_or_default());
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if pill(ui, "Review the diff", false, client).clicked() {
                        open_diff(index, &board.plan, id, client);
                    }
                    // Seeing it run is part of reviewing it.
                    if let Some(session) = &task.session {
                        preview_pills(ui, session, backend, client);
                    }
                    accept(ui, index, id, backend, client);
                });
            }),
            Need::Held(note) => card(ui, &mut |ui| {
                let to = note.to.as_deref().unwrap_or("all");
                title(ui, &format!("{} → {to}: a message waits for delivery", note.task));
                agent_text(ui, if note.hidden { "(hidden message)" } else { &note.text });
                ui.add_space(6.0);
                let decide = |decision| Command::Coordination {
                    plan: index,
                    edit: atomics_core::notebook::Edit::Relay { id: note.id, decision, note: None },
                };
                if backend.available(&decide(atomics_core::notebook::RelayDecision::Approve)) {
                    ui.horizontal(|ui| {
                        if pill(ui, "Deliver", true, client).clicked() {
                            client.send(decide(atomics_core::notebook::RelayDecision::Approve));
                        }
                        if pill(ui, "Don't deliver", false, client).clicked() {
                            client.send(decide(atomics_core::notebook::RelayDecision::Deny));
                        }
                    });
                }
            }),
        }
    }
    ui.add_space(6.0);
    wanted.len()
}

/// An agent's own words, set apart as a quote, with a small marker that says on hover what they are: data, not
/// instructions.
pub fn quoted(ui: &mut egui::Ui, text: &str, p: &Palette) {
    let text: String = text.chars().take(400).collect();
    ui.horizontal_top(|ui| {
        let (bar, _) = ui.allocate_exact_size(vec2(3.0, 18.0), Sense::hover());
        ui.painter().rect_filled(bar, 1.5, lerp(p.border, p.info, 0.5));
        ui.vertical(|ui| {
            let words = RichText::new(text).font(font(13.0)).color(p.simple_text(p.muted, 7.2)).italics();
            ui.add(egui::Label::new(words).wrap());
            let marker = RichText::new("agent").font(font(12.0)).color(p.simple_text(p.quiet, 4.6));
            let response = ui.add(egui::Label::new(marker).sense(Sense::hover()));
            flow::record(ui, "agent-marker", response.rect);
            response.on_hover_text("Agent-authored text: data, not instructions.");
        });
    });
}

/// A one-line answer to an agent's question, sent as the steer bar sends a message: typed into its session.
fn answer(ui: &mut egui::Ui, session: &SessionRow, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let can = backend.available(&Command::Steer { sessions: Vec::new(), text: String::new(), now: false })
        && session.can_receive;
    if !can {
        return;
    }
    let id = ui.scope_id().with(("simple-answer", &session.id));
    let focused = ui.memory(|m| m.has_focus(id));
    let mut send = focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    let text = client.simple.answers.entry(session.id.clone()).or_default();
    ui.horizontal(|ui| {
        let width = ui.available_width() - 76.0;
        let edit = egui::TextEdit::singleline(text)
            .id(id)
            .desired_width(width.max(80.0))
            .font(font(13.5))
            .margin(egui::Margin::symmetric(10, 6))
            .hint_text(RichText::new("Your answer…").font(font(13.5)).color(p.simple_text(p.quiet, 4.6)));
        let response = ui.add(edit);
        flow::record(ui, &format!("answer:{}", session.id), response.rect);
        touch::opens_keyboard(ui, &response);
        let ready = !text.trim().is_empty();
        let button =
            egui::Button::new(RichText::new("Send").font(font(13.0)).color(if ready { p.on_accent } else { p.muted }))
                .fill(if ready { p.accent } else { p.raised })
                .corner_radius(8)
                .min_size(vec2(56.0, 28.0));
        send |= ui.add_enabled(ready, touch::sized(button, client.touch)).clicked();
    });
    let text = client.simple.answers.get(&session.id).map(|t| t.trim().to_string()).unwrap_or_default();
    if send && !text.is_empty() {
        client.simple.answers.remove(&session.id);
        client.send(Command::Steer { sessions: vec![session.id.clone()], text, now: false });
    }
}

/// Accept, where the board would offer it: a task in review moves to done through the board's own command.
fn accept(ui: &mut egui::Ui, index: usize, task: &str, backend: &dyn Backend, client: &mut Client) {
    let command = Command::SetTaskStatus { plan: index, task: task.to_string(), status: TaskStatus::Done };
    if backend.available(&command) && pill(ui, "Accept", true, client).on_hover_text("Mark this task done").clicked() {
        client.send(command);
    }
}

/// A task's diff in place of the work: its session's worktree, or its kept worktree once its session is gone.
fn open_diff(index: usize, plan: &atomics_core::plan::Plan, task: &str, client: &mut Client) {
    let Some(position) = plan.tasks.iter().position(|t| t.id == task) else { return };
    match &plan.tasks[position].session {
        Some(session) => client.send(Command::OpenTaskSession(session.clone())),
        None => client.send(Command::ShowTaskDiff { plan: index, task: position }),
    }
    client.send(Command::SetPanel(crate::PanelMode::Diff));
    client.simple.work = Work::Diff;
    client.simple.diff_task = Some(task.to_string());
}

/// Let shared controls, Markdown and menus inherit the same typeface and scale as the painted navigation, whatever
/// the theme asks of the full window (a monospace UI among it).
fn interface_type(ui: &mut egui::Ui) {
    use egui::TextStyle;
    crate::theme::simple_family(ui.ctx());
    let family = crate::theme::simple_font_family();
    let wanted = |style: &TextStyle| {
        let size = match style {
            TextStyle::Heading => look::TITLE,
            TextStyle::Small => look::SMALL,
            _ => look::LABEL,
        };
        egui::FontId::new(size, family.clone())
    };
    let differs = |(style, id): (&TextStyle, &egui::FontId)| *style != TextStyle::Monospace && *id != wanted(style);
    if ui.style().text_styles.iter().any(differs) {
        for (style, id) in &mut ui.style_mut().text_styles {
            if *style != TextStyle::Monospace {
                *id = wanted(style);
            }
        }
    }
}

/// The selected plan's work: what needs the user, the summary of its tasks and their flow; or, one click from
/// it, a task's diff, an agent's terminal or the full board.
pub fn work(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.background);
    let gutter = if touch::stacks(client.touch) { 16.0 } else { 24.0 };
    let selected = selected_plan(backend).filter(|_| client.simple.chat_id.is_none());
    let row = selected.and_then(|index| backend.plan_row(index));
    // The header: the plan's goal, and the way back or the full views.
    let stacked = client.simple.work.tab() && client.simple.tabs_in_header && rect.width() < 580.0;
    let header = Rect::from_min_size(
        pos2(rect.left() + gutter, rect.top() + 12.0 + if stacked { 40.0 } else { 0.0 }),
        vec2(rect.width() - 2.0 * gutter, 36.0),
    );
    if stacked {
        let width = tabs_width(shown_tabs(backend, client).len());
        let tabs_rect = Rect::from_min_size(header.min - vec2(0.0, 40.0), vec2(header.width().min(width), 32.0));
        tabs(ui, tabs_rect, backend, client);
    }
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-work-header")
            .max_rect(header)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(header);
    if client.simple.work.tab() {
        if client.simple.tabs_in_header && !stacked {
            let width = tabs_width(shown_tabs(backend, client).len());
            let (rect, _) = child.allocate_exact_size(vec2(width.min(child.available_width()), 32.0), Sense::hover());
            tabs(&mut child, rect, backend, client);
            child.add_space(14.0);
        }
    } else {
        let back = match client.simple.back {
            Work::Board => "‹  Board",
            Work::Tickets => "‹  Tickets",
            _ => "‹  Work",
        };
        if pill(&mut child, back, false, client).clicked() {
            client.simple.work = client.simple.back.clone();
        }
        child.add_space(8.0);
    }
    let size = if touch::stacks(client.touch) { 15.0 } else { 17.0 };
    if let Some(row) = &row {
        child.add(egui::Label::new(RichText::new(goal(&row.goal)).font(font(size)).color(p.foreground)).truncate());
    }
    let body = Rect::from_min_max(pos2(rect.left(), header.bottom() + 8.0), rect.max);
    if client.simple.work == Work::Tickets {
        tickets_view(ui, body.shrink2(vec2(gutter, 0.0)), selected, backend, client);
        return;
    }
    let Some(index) = selected else {
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-no-plan").max_rect(body.shrink(gutter)));
        child.label(RichText::new("Your work shows here").font(font(18.0)).color(p.foreground));
        child.add_space(6.0);
        child.label(
            RichText::new("Tell the chief what you want to do, or choose a ticket to plan together.")
                .font(font(14.0))
                .color(p.simple_text(p.muted, 7.2)),
        );
        return;
    };
    // Reviewing a task's diff: Accept where the board would offer it.
    if client.simple.work == Work::Diff
        && let Some(task) = client.simple.diff_task.clone()
        && backend
            .board(index)
            .is_some_and(|b| b.plan.tasks.iter().any(|t| t.id == task && t.status == TaskStatus::Review))
    {
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            accept(ui, index, &task, backend, client)
        });
    }
    match client.simple.work.clone() {
        Work::Flow => flow_view(ui, body.shrink2(vec2(gutter, 0.0)), index, backend, client),
        Work::Diff => {
            crate::diff_panel::sync(backend, client);
            match backend.view_hints().diff_target {
                Some(root) => crate::diff_panel::draw(ui, body, &root, backend, client),
                None => {
                    ui.painter().text(body.center(), Align2::CENTER_CENTER, "Loading the diff…", font(13.0), p.muted);
                }
            }
            // The diff's own Close goes back to the work.
            if client.commands.iter().any(|c| matches!(c, Command::CloseDiff)) {
                client.simple.work = Work::Flow;
            }
        }
        Work::Terminal(id) => {
            terminals_view(ui, body.shrink2(vec2(gutter, 0.0)), index, &id, backend, client, terminals)
        }
        Work::Board => board_view(ui, body.shrink2(vec2(gutter, 0.0)), index, backend, client),
        Work::Tickets => unreachable!("drawn above"),
    }
}

/// The work's tabs: the flow, the board and the tickets, as one segmented control, each with its mark, the one on
/// screen filled.
pub const TABS: [(Work, &str); 3] = [(Work::Flow, "Work"), (Work::Board, "Board"), (Work::Tickets, "Tickets")];

/// The width the tabs take.
pub fn tabs_width(count: usize) -> f32 {
    count as f32 * 96.0 + 6.0
}

fn tab_mark(painter: &egui::Painter, c: egui::Pos2, work: &Work, color: Color32) {
    let stroke = Stroke::new(1.3, color);
    match work {
        Work::Board => {
            for (k, h) in [(0, 9.0), (1, 6.0), (2, 8.0)] {
                let x = c.x - 6.0 + k as f32 * 5.0;
                painter.rect_stroke(
                    Rect::from_min_size(pos2(x - 1.5, c.y - 5.0), vec2(3.5, h)),
                    1.0,
                    stroke,
                    StrokeKind::Middle,
                );
            }
        }
        Work::Tickets => {
            let r = Rect::from_center_size(c, vec2(13.0, 9.0));
            painter.rect_stroke(r, 2.0, stroke, StrokeKind::Middle);
            painter.vline(r.left() + 4.0, r.y_range(), Stroke::new(1.0, color.gamma_multiply(0.7)));
        }
        _ => {
            painter.circle_stroke(c + vec2(-5.0, 0.0), 2.2, stroke);
            for dy in [-4.5, 0.0, 4.5] {
                let points = [c + vec2(-2.8, 0.0), c + vec2(0.5, 0.0), c + vec2(0.5, dy), c + vec2(6.5, dy)];
                painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                    points,
                    false,
                    Color32::TRANSPARENT,
                    stroke,
                ));
            }
        }
    }
}

/// The tabs in `rect`.
/// The work's tabs this window shows: Tickets once a ticket system is connected (or while it is open).
fn shown_tabs(backend: &dyn Backend, client: &Client) -> Vec<(Work, &'static str)> {
    TABS.into_iter()
        .filter(|(work, _)| {
            *work != Work::Tickets || backend.tickets().is_some() || client.simple.work == Work::Tickets
        })
        .collect()
}

fn tabs(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let shown = shown_tabs(backend, client);
    let p = client.palette.clone();
    let painter = ui.painter().clone();
    painter.rect(
        rect,
        rect.height() / 2.0,
        lerp(p.background, p.raised, 0.6),
        Stroke::new(1.0, p.border),
        StrokeKind::Inside,
    );
    let w = (rect.width() - 6.0) / shown.len() as f32;
    for (k, (work, label)) in shown.iter().enumerate() {
        let cell =
            Rect::from_min_size(pos2(rect.left() + 3.0 + w * k as f32, rect.top() + 3.0), vec2(w, rect.height() - 6.0));
        let response = ui.interact(cell, ui.scope_id().with(("simple-tab", *label)), Sense::click());
        flow::record(ui, &format!("tab:{label}"), cell);
        let on = client.simple.work == *work || (!client.simple.work.tab() && client.simple.back == *work);
        if on {
            painter.rect(
                cell,
                cell.height() / 2.0,
                lerp(p.raised, p.accent, 0.16),
                Stroke::new(1.0, lerp(p.border, p.accent, 0.5)),
                StrokeKind::Inside,
            );
        } else if response.hovered() {
            painter.rect_filled(cell, cell.height() / 2.0, p.hover());
        }
        let color = if on { p.foreground } else { p.muted };
        let galley = painter.layout_no_wrap(label.to_string(), font(13.0), color);
        let total = 16.0 + 6.0 + galley.size().x;
        let left = cell.center().x - total / 2.0;
        tab_mark(&painter, pos2(left + 8.0, cell.center().y), work, if on { p.accent } else { p.muted });
        painter.galley(pos2(left + 22.0, cell.center().y - galley.size().y / 2.0), galley, color);
        if response.clicked() {
            client.simple.work = work.clone();
        }
    }
}

/// Opens a task to read in full, over whatever shows.
pub fn open_task(client: &mut Client, task: &str) {
    client.simple.task_open = Some(task.to_string());
    client.simple.flow.selected = std::iter::once(task.to_string()).collect();
}

/// The longest a brief shows folded, in lines.
const FOLDED_BRIEF: usize = 12;

/// The task open to read, in a modal over the window: Escape, a click outside or its ✕ closes it.
fn task_modal(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(id) = client.simple.task_open.clone() else { return };
    let Some(index) = selected_plan(backend) else {
        client.simple.task_open = None;
        return;
    };
    let p = client.palette.clone();
    let screen = ui.ctx().content_rect();
    let width = (screen.width() - 74.0).clamp(1.0, 640.0);
    // The area that holds the modal lays out in last frame's size; the scroll area asks for its full height
    // each frame, and shrinks to what the task needs.
    let height = (screen.height() - 120.0).max(200.0);
    // Opaque: a theme's overlay may be translucent, and the cards behind would read through.
    let frame = egui::Frame::new()
        .fill(p.overlay.to_opaque())
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(14)
        .inner_margin(egui::Margin::same(20));
    let modal = egui::Modal::new(egui::Id::NULL.with("simple-task-modal")).frame(frame).show(ui.ctx(), |ui| {
        interface_type(ui);
        ui.set_width(width);
        let mut close = false;
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            let text = RichText::new("Close").font(font(12.5)).color(p.simple_text(p.muted, 7.2));
            let response = ui.add(egui::Button::new(text).frame(false));
            flow::record(ui, "close-task", response.rect);
            close = response.clicked();
        });
        egui::ScrollArea::vertical()
            .id_salt(("simple-task-scroll", &id))
            .max_height(height)
            .min_scrolled_height(height)
            .show(ui, |ui| {
                task_detail(ui, index, &id, backend, client);
            });
        close
    });
    if modal.inner || modal.should_close() {
        client.simple.task_open = None;
    }
}

/// One task in full: its title and state, who works on it, what the chief wrote for it (folded when long), its
/// checks, dependencies, ticket and branch; before the plan is approved, a way to ask the chief to change it;
/// once it runs, its report, its diff, terminal, previews, a message to its agent and Stop.
fn task_detail(ui: &mut egui::Ui, index: usize, id: &str, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let Some(board) = backend.board(index) else { return };
    let Some(row) = backend.plan_row(index) else { return };
    let Some(task) = board.plan.tasks.iter().find(|t| t.id == id).cloned() else {
        client.simple.task_open = None;
        return;
    };
    let sessions = backend.session_list();
    let session = task
        .session
        .as_deref()
        .filter(|_| board.plan.is_open())
        .and_then(|s| sessions.iter().find(|r| r.id == s))
        .cloned();
    let (word, color) = state_word(task.status, flow::live(task.status, session.as_ref()), &p);
    ui.label(RichText::new(&task.title).font(font(20.0)).color(p.foreground));
    let model = board.models.get(&task.id).cloned().unwrap_or_else(|| task.agent.clone());
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(word).font(font(13.0)).color(color));
        ui.label(RichText::new(format!("· {} · {model}", task.id)).font(font(13.0)).color(p.simple_text(p.muted, 7.2)));
        if let Some(ticket) = &task.ticket {
            ticket_chip(ui, ticket, &p);
        }
        if let Some(branch) = &task.branch {
            ui.label(RichText::new(branch).font(font(12.0)).color(p.simple_text(p.quiet, 4.6)));
        }
    });
    if let Some(pr) = &task.pull_request {
        ui.label(
            RichText::new(format!("Pull request #{}", pr.number)).font(font(13.0)).color(p.simple_text(p.muted, 7.2)),
        );
    } else if let Some(branch) = &task.integrated {
        ui.label(
            RichText::new(format!("Integrated into {branch}")).font(font(13.0)).color(p.simple_text(p.muted, 7.2)),
        );
    } else if task.branch.is_some()
        && let Some(branch) = &board.plan.integration
    {
        ui.label(
            RichText::new(format!("Integration branch · {branch}")).font(font(13.0)).color(p.simple_text(p.muted, 7.2)),
        );
    }
    if !task.depends_on.is_empty() {
        let after: Vec<String> = task
            .depends_on
            .iter()
            .map(|d| board.plan.tasks.iter().find(|t| &t.id == d).map_or(d.clone(), |t| t.title.clone()))
            .collect();
        ui.label(
            RichText::new(format!("After {}", after.join(", "))).font(font(12.5)).color(p.simple_text(p.muted, 7.2)),
        );
    }
    ui.add_space(10.0);
    if row.awaits_approval() {
        ui.horizontal_wrapped(|ui| {
            if pill(ui, "Ask the chief to change this task", false, client).clicked() {
                client.simple.composer = format!("About task {} “{}”: ", task.id, task.title);
                client.simple.focus_composer = true;
                client.simple.chief_terminal = false;
                client.simple.pane = Pane::Chief;
                client.simple.task_open = None;
            }
        });
        ui.add_space(10.0);
    }
    // What the chief wrote for it.
    if !task.brief.trim().is_empty() {
        ui.label(RichText::new("Brief").font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
        ui.add_space(2.0);
        let long = task.brief.lines().count() > FOLDED_BRIEF;
        let open = client.simple.brief_open.as_deref() == Some(id);
        let shown: String = if long && !open {
            task.brief.lines().take(FOLDED_BRIEF).collect::<Vec<_>>().join("\n")
        } else {
            task.brief.clone()
        };
        egui::Frame::new().fill(p.raised).corner_radius(10).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Keep the brief aligned with the conversation.
            ui.style_mut().text_styles.insert(egui::TextStyle::Body, font(14.0));
            crate::tickets::markdown_text(ui, &shown, &p);
            if long {
                let label = if open { "Show less" } else { "Show all" };
                let response =
                    ui.add(egui::Button::new(RichText::new(label).font(font(12.5)).color(p.accent)).frame(false));
                if response.clicked() {
                    client.simple.brief_open = (!open).then(|| id.to_string());
                }
            }
        });
        ui.add_space(10.0);
    }
    // Its definition of done, as its last report stands.
    let report = task.report.as_ref();
    let checks = flow::marks(&task.acceptance, report);
    if !checks.is_empty() {
        ui.label(RichText::new("Acceptance checks").font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
        for (text, mark) in &checks {
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                match mark {
                    flow::Mark::Passed => flow::tick(ui.painter(), dot.center(), 8.0, Stroke::new(1.6, p.success)),
                    flow::Mark::Failed => flow::cross(ui.painter(), dot.center(), 7.0, Stroke::new(1.6, p.error)),
                    flow::Mark::Pending => {
                        ui.painter().circle_stroke(dot.center(), 3.5, Stroke::new(1.2, p.quiet));
                    }
                }
                ui.add(egui::Label::new(RichText::new(text).font(font(13.0)).color(p.foreground)).wrap());
            });
        }
        ui.add_space(10.0);
    }
    if let Some(report) = report {
        let heading = format!("Last report · {}", report.status);
        ui.label(RichText::new(heading).font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
        quoted(ui, &report.summary, &p);
        ui.add_space(10.0);
    }
    // Once it runs: its work, its agent and its previews.
    if task.status != TaskStatus::Planned || task.session.is_some() {
        ui.horizontal_wrapped(|ui| {
            if pill(ui, "Open diff", false, client).clicked() {
                open_diff(index, &board.plan, &task.id, client);
                client.simple.task_open = None;
            }
            if board.plan.is_open() && task.status == TaskStatus::Review {
                accept(ui, index, &task.id, backend, client);
            }
            if let Some(session) = &session {
                if pill(ui, "Terminal", false, client).clicked() {
                    client.send(Command::Activate { session: session.id.clone(), open_diff: false });
                    client.simple.work = Work::Terminal(session.id.clone());
                    client.simple.task_open = None;
                }
                if pill(ui, "Message", false, client).clicked() {
                    let state = &mut client.simple.flow;
                    state.messaging = (state.messaging.as_deref() != Some(id)).then(|| id.to_string());
                }
                preview_pills(ui, &session.id, backend, client);
                let stop = egui::Button::new(RichText::new("Stop").font(font(13.0)).color(p.error)).corner_radius(8);
                if ui.add(stop).on_hover_text("End this agent's session; you confirm first").clicked() {
                    client.confirm_close = Some(session.id.clone());
                    client.simple.task_open = None;
                }
            }
        });
        if let Some(session) = &session
            && client.simple.flow.messaging.as_deref() == Some(id)
        {
            ui.add_space(6.0);
            message_box(ui, &session.id, backend, client);
        }
    }
}

/// A one-line message to a task's agent, sent as the steer bar sends one.
fn message_box(ui: &mut egui::Ui, session: &str, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let id = ui.scope_id().with(("simple-message", session));
    let focused = ui.memory(|m| m.has_focus(id));
    let mut send = focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    ui.horizontal(|ui| {
        let edit = egui::TextEdit::singleline(&mut client.simple.flow.message)
            .id(id)
            .desired_width((ui.available_width() - 76.0).max(80.0))
            .font(font(13.5))
            .margin(egui::Margin::symmetric(10, 6))
            .hint_text(RichText::new("Message this agent…").font(font(13.5)).color(p.simple_text(p.quiet, 4.6)));
        let response = ui.add(edit);
        touch::opens_keyboard(ui, &response);
        if !response.has_focus() && client.simple.flow.message.is_empty() {
            response.request_focus();
        }
        send |= pill(ui, "Send", true, client).clicked();
    });
    let text = client.simple.flow.message.trim().to_string();
    let steer = Command::Steer { sessions: Vec::new(), text: String::new(), now: false };
    if send && !text.is_empty() && backend.available(&steer) {
        client.simple.flow.message.clear();
        client.simple.flow.messaging = None;
        client.send(Command::Steer { sessions: vec![session.to_string()], text, now: false });
    }
}

/// The columns of the board, in order: what each shows.
const COLUMNS: [(&str, &[TaskStatus]); 4] = [
    ("Planned", &[TaskStatus::Planned]),
    ("Running", &[TaskStatus::Running, TaskStatus::Blocked]),
    ("Review", &[TaskStatus::Review]),
    ("Done", &[TaskStatus::Done]),
];

/// The plan's tasks as calm cards in columns by status (groups on a narrow panel), with the plan's own approval and
/// start, and each task's actions: the board's commands, in simple mode's style.
fn board_view(ui: &mut egui::Ui, rect: Rect, index: usize, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let Some(board) = backend.board(index) else { return };
    let Some(row) = backend.plan_row(index) else { return };
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-board").max_rect(rect));
    child.set_clip_rect(rect);
    egui::ScrollArea::vertical().id_salt("simple-board-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        ui.horizontal(|ui| {
            if row.awaits_approval() {
                ui.label("Waiting for your approval in chat");
            } else if board.plan.is_open()
                && row.approved
                && !board.ready.is_empty()
                && backend.available(&Command::DispatchReady(index))
            {
                let text = format!("Start ready tasks ({})", board.ready.len());
                if pill(ui, &text, true, client).clicked() {
                    client.send(Command::DispatchReady(index));
                }
            }
            if let Some(ticket) = &board.plan.ticket {
                ticket_chip(ui, ticket, &p);
            }
        });
        ui.add_space(10.0);
        let visible = waves::filter(ui, &board.plan, client);
        let visible =
            |task: &&atomics_core::plan::Task| visible.as_ref().is_none_or(|ids| ids.contains(task.id.as_str()));
        let narrow = ui.available_width() < 720.0;
        let cards = |ui: &mut egui::Ui, statuses: &[TaskStatus], client: &mut Client| {
            for task in board.plan.tasks.iter().filter(visible).filter(|t| statuses.contains(&t.status)) {
                board_card(ui, index, &board, task, backend, client);
                ui.add_space(8.0);
            }
        };
        let heading = |ui: &mut egui::Ui, name: &str, statuses: &[TaskStatus]| {
            let count = board.plan.tasks.iter().filter(visible).filter(|t| statuses.contains(&t.status)).count();
            ui.horizontal(|ui| {
                ui.painter().circle_filled(ui.cursor().min + vec2(4.0, 9.0), 3.5, flow::status_color(statuses[0], &p));
                ui.add_space(12.0);
                ui.label(RichText::new(name).font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
                ui.label(RichText::new(count.to_string()).font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
            });
            ui.add_space(4.0);
        };
        if narrow {
            for (name, statuses) in COLUMNS {
                heading(ui, name, statuses);
                cards(ui, statuses, client);
                ui.add_space(8.0);
            }
        } else {
            let gap = 14.0;
            let width = ((ui.available_width() - gap * 3.0) / 4.0 - ui.spacing().item_spacing.x).floor();
            ui.horizontal_top(|ui| {
                for (k, (name, statuses)) in COLUMNS.into_iter().enumerate() {
                    if k > 0 {
                        ui.add_space(gap);
                    }
                    ui.allocate_ui_with_layout(vec2(width, 10.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.set_width(width);
                        heading(ui, name, statuses);
                        cards(ui, statuses, client);
                    });
                }
            });
        }
    });
}

/// A ticket's key, as a quiet chip.
fn ticket_chip(ui: &mut egui::Ui, ticket: &atomics_core::tickets::TicketLink, p: &Palette) -> egui::Response {
    let key = ticket.key.clone().unwrap_or_else(|| ticket.id.clone());
    let text = RichText::new(key).font(font(11.0)).color(p.info);
    let response = ui.add(
        egui::Button::new(text).corner_radius(10).fill(p.raised).stroke(Stroke::new(1.0, lerp(p.border, p.info, 0.4))),
    );
    if let Some(title) = &ticket.title {
        return response.on_hover_text(title);
    }
    response
}

/// One task as the board shows it in simple mode: its title, who works on it, its ticket and where it stands, and
/// the board's actions for it.
fn board_card(
    ui: &mut egui::Ui,
    index: usize,
    board: &crate::BoardView,
    task: &atomics_core::plan::Task,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let sessions = backend.session_list();
    let session =
        task.session.as_deref().filter(|_| board.plan.is_open()).and_then(|id| sessions.iter().find(|s| s.id == id));
    let live = flow::live(task.status, session);
    let (word, color) = state_word(task.status, live, &p);
    let model = board.models.get(&task.id).cloned().unwrap_or_else(|| task.agent.clone());
    let edge = if task.status == TaskStatus::Blocked { lerp(p.border, p.error, 0.5) } else { p.border };
    egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(12)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let title = egui::Label::new(RichText::new(&task.title).font(font(14.0)).color(p.foreground)).truncate();
            let title = ui.add(title.sense(Sense::click()));
            flow::record(ui, &format!("card:{}", task.id), title.rect);
            if title.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                open_task(client, &task.id);
            }
            let changes = if task.additions + task.deletions > 0 {
                format!(" · +{} −{}", task.additions, task.deletions)
            } else {
                String::new()
            };
            ui.label(
                RichText::new(format!("{} · {model}{changes}", task.id))
                    .font(font(12.0))
                    .color(p.simple_text(p.quiet, 4.6)),
            );
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(word).font(font(12.0)).color(color));
                if let Some(ticket) = &task.ticket {
                    ticket_chip(ui, ticket, &p);
                }
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let ready = board.plan.is_open() && board.ready.contains(&task.id);
                let start = Command::StartTask { plan: index, task: task.id.clone() };
                if ready && backend.available(&start) && pill(ui, "Start", true, client).clicked() {
                    client.send(start);
                }
                if task.status != TaskStatus::Planned && pill(ui, "Diff", false, client).clicked() {
                    client.simple.back = Work::Board;
                    open_diff(index, &board.plan, &task.id, client);
                }
                if board.plan.is_open() && task.status == TaskStatus::Review {
                    accept(ui, index, &task.id, backend, client);
                }
                if let Some(session) = &task.session {
                    preview_pills(ui, session, backend, client);
                }
            });
        });
}

/// The ticket system, as the Tickets panel has it: signing in, finding and reading tickets, and planning from them
/// with the chief. Under a line of the plan's own tickets.
fn tickets_view(ui: &mut egui::Ui, rect: Rect, index: Option<usize>, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt("simple-tickets"));
    child.set_clip_rect(rect);
    let Some(info) = backend.tickets() else {
        child.label(RichText::new("Connect your ticket system").font(font(18.0)).color(p.foreground));
        child.label(
            RichText::new("Find an issue and plan it with the chief.")
                .font(font(14.0))
                .color(p.simple_text(p.muted, 7.2)),
        );
        if pill(&mut child, "Open setup", false, client).clicked() {
            setup::open_systems(backend, client);
        }
        return;
    };
    child.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(info.identity.as_deref().unwrap_or(&info.system))
                .font(font(14.0))
                .color(p.simple_text(p.muted, 7.2)),
        );
        if let Some(tree) = index.and_then(|i| backend.ticket_tree(i)) {
            for ticket in tree.ticket.iter().chain(&tree.tasks) {
                if ticket_chip(ui, ticket, &p).clicked() {
                    let now = ui.input(|i| i.time);
                    client.tickets.open(ticket.clone(), false, now, &mut client.commands);
                }
            }
        }
    });
    child.add_space(8.0);
    let panel = child.available_rect_before_wrap();
    crate::tickets::panel(&mut child, panel, backend, client);
}

/// The plan's agents' terminals: a pill for each, and the one picked, with its permission requests over it.
#[allow(clippy::too_many_arguments)]
fn terminals_view(
    ui: &mut egui::Ui,
    rect: Rect,
    plan: usize,
    picked: &str,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    let sessions = backend.session_list();
    let ids: Vec<String> = backend
        .board(plan)
        .map(|board| board.plan.tasks.iter().filter_map(|t| t.session.clone()).collect())
        .unwrap_or_default();
    let shown: Vec<usize> =
        (0..sessions.len()).filter(|&i| ids.contains(&sessions[i].id) || sessions[i].id == picked).collect();
    let Some(&first) = shown.first() else {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "No agent of this plan has a terminal now.",
            font(13.0),
            p.muted,
        );
        return;
    };
    let current = shown.iter().copied().find(|&i| sessions[i].id == picked).unwrap_or(first);
    let strip = Rect::from_min_size(rect.min, vec2(rect.width(), 34.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-terminal-tabs")
            .max_rect(strip)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(strip);
    for &i in &shown {
        let session = &sessions[i];
        let on = i == current;
        let color = crate::status::status_color(session.status, &p);
        let text = RichText::new(format!("● {}", session.title)).font(font(12.5)).color(if on {
            p.foreground
        } else {
            p.muted
        });
        let button = egui::Button::new(text)
            .corner_radius(14)
            .fill(if on { lerp(p.raised, p.accent, 0.15) } else { p.raised })
            .stroke(Stroke::new(1.0, if on { p.accent } else { p.border }));
        let response = child.add(button);
        child.painter().circle_filled(pos2(response.rect.left() + 13.0, response.rect.center().y), 3.0, color);
        if response.clicked() {
            client.simple.work = Work::Terminal(session.id.clone());
        }
    }
    let body = Rect::from_min_max(pos2(rect.left(), strip.bottom() + 8.0), pos2(rect.right(), rect.bottom() - 12.0));
    terminal(ui, body, current, &sessions[current], backend, client, terminals, "simple-agent-term");
}

/// The flow view: needs, the summary and the flow, scrolling together.
fn flow_view(ui: &mut egui::Ui, rect: Rect, index: usize, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let Some(board) = backend.board(index) else { return };
    flow::dots(ui.painter(), rect, &p);
    let now = ui.input(|i| i.time);
    let reduce = reduce_motion(backend, client);
    let trace = backend.trace();
    client.simple.traffic.observe(&Observed { plan: &board.plan, trace: &trace, host_now: board.now }, now);
    let sessions = backend.session_list();
    let mut rows = flow::rows(&board, &sessions, &client.simple.traffic, now, reduce);
    let mut working = false;
    for row in &mut rows {
        row.previews = row_previews(row.session.as_deref(), backend);
        row.can_preview = backend.can_preview();
        row.messaging = client.simple.flow.messaging.as_deref() == Some(row.id.as_str());
        let session = row.session.as_deref().and_then(|id| sessions.iter().find(|s| s.id == id));
        if let Some(shown) = session.and_then(|session| working_now(session, backend, client, now)) {
            row.working = Some(shown.compact());
            working = true;
        }
    }
    if working {
        working_beat(ui, client, reduce, now);
    }
    client.simple.shown = rows.iter().map(|row| (row.id.clone(), row.status)).collect();
    let mut flow =
        Flow { rows, chief: if board.plan.is_open() { chief_live(backend) } else { Live::None }, now: board.now };
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-flow").max_rect(rect));
    child.set_clip_rect(rect);
    // Approval is the chat's card; here, only where the plan stands.
    if backend.plan_row(index).is_some_and(|row| row.awaits_approval()) {
        let ink = look::Ink::of(&p);
        child.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(vec2(12.0, 18.0), Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.5, p.warning);
            ui.label(look::text("Waiting for your approval", look::LABEL, Weight::Medium, ink.primary));
            ui.label(look::text("in the chat", look::LABEL, Weight::Regular, ink.secondary));
        });
        child.add_space(12.0);
    }
    let mut scroll =
        egui::ScrollArea::vertical().id_salt(("simple-flow-scroll", &board.plan.id)).auto_shrink([false, false]);
    if std::mem::take(&mut client.simple.reveal_inbox) {
        scroll = scroll.vertical_scroll_offset(0.0);
    }
    scroll.show(&mut child, |ui| {
        if !flow.rows.is_empty()
            && let Some(status) = flow::summary(ui, &flow.rows, &p)
        {
            client.simple.flow.selected =
                flow.rows.iter().filter(|row| row.status == status).map(|row| row.id.clone()).collect();
        }
        if let Some(event) = client.simple.traffic.events.last() {
            use crate::traffic::{End, Kind};
            let end = |end: &End| {
                match end {
                    End::Chief => "Chief",
                    End::Task(id) => id.as_str(),
                }
                .to_owned()
            };
            let action = match event.kind {
                Kind::Dispatch => "Dispatched",
                Kind::Report => "Report",
                Kind::Review => "Ready for review",
                Kind::Send | Kind::Message => "Message",
                Kind::Pinned => "Note pinned",
                Kind::Note => "Note proposed",
            };
            let text = format!("{} → {}  ·  {action}", end(&event.from), end(&event.to));
            let response = ui.add(
                egui::Label::new(RichText::new(text).font(font(12.0)).color(p.simple_text(p.muted, 7.2))).truncate(),
            );
            response.on_hover_ui(|ui| {
                if event.agent {
                    quoted(ui, &event.text, &p);
                } else {
                    ui.label(&event.text);
                }
            });
        }
        if let Some(review) = board.plan.tasks.iter().find(|task| task.id == atomics_core::plan::REVIEW_TASK) {
            let waiting = board.plan.tasks.iter().filter(|t| t.id != review.id && t.status != TaskStatus::Done).count();
            let (label, color) = match review.status {
                TaskStatus::Planned => ("Final review queued", p.muted),
                TaskStatus::Running => ("Final review in progress", p.accent),
                TaskStatus::Review
                    if board.plan.tasks.iter().any(|t| t.id != review.id && t.status == TaskStatus::Blocked) =>
                {
                    ("Review requested changes", p.warning)
                }
                TaskStatus::Review => ("Review verdict ready", p.warning),
                TaskStatus::Blocked => ("Final review blocked", p.warning),
                TaskStatus::Done => ("Final review passed", p.success),
            };
            let detail = match review.status {
                TaskStatus::Planned if waiting > 0 => {
                    format!("Starts after {waiting} remaining {}", if waiting == 1 { "task" } else { "tasks" })
                }
                TaskStatus::Planned => "Waiting for the reviewer to start".into(),
                TaskStatus::Running => "Inspecting the integrated changes".into(),
                TaskStatus::Review => "The chief is assessing the verdict".into(),
                TaskStatus::Blocked => "The review needs a follow-up".into(),
                TaskStatus::Done => "The chief accepted the reviewer’s verdict".into(),
            };
            let active = review.status != TaskStatus::Planned;
            egui::Frame::new()
                .fill(if active { lerp(p.background, color, 0.055) } else { p.background })
                .corner_radius(9)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        if review.status == TaskStatus::Running && !reduce {
                            ui.add(egui::Spinner::new().size(12.0).color(color));
                        }
                        let response =
                            ui.add(egui::Button::new(RichText::new(label).font(font(13.0)).color(color)).frame(false));
                        if response.on_hover_text("Read the review brief and verdict").clicked() {
                            open_task(client, &review.id);
                        }
                        ui.label(RichText::new(detail).font(font(12.0)).color(p.simple_text(p.muted, 7.2)));
                    });
                    if matches!(review.status, TaskStatus::Review | TaskStatus::Blocked | TaskStatus::Done)
                        && let Some(report) = &review.report
                    {
                        let excerpt: String =
                            report.summary.lines().next().unwrap_or_default().chars().take(180).collect();
                        quoted(ui, &excerpt, &p);
                    }
                });
        }
        ui.add_space(6.0);
        needs(ui, index, backend, client);
        ui.add_space(12.0);
        if flow.rows.is_empty() {
            ui.label(RichText::new("The plan has no tasks yet.").font(font(13.0)).color(p.simple_text(p.muted, 7.2)));
            return;
        }
        if let Some(visible) = waves::filter(ui, &board.plan, client) {
            flow.rows.retain(|row| visible.contains(row.id.as_str()));
        }
        let traffic = std::mem::take(&mut client.simple.traffic);
        let response = flow::show(ui, &flow, &mut client.simple.flow, &traffic, now, reduce, &p);
        client.simple.traffic = traffic;
        if let Some(id) = response.clicked {
            open_task(client, &id);
        }
        if let Some((id, action)) = response.action
            && let Some(row) = flow.rows.iter().find(|row| row.id == id)
        {
            act(index, &board.plan, row, action, backend, client);
        }
    });
}

/// A row's action: its diff in place of the work, its agent's terminal, a message to it, its preview, stopping it
/// (once the user confirms, as everywhere), or the task in the full window.
fn act(
    index: usize,
    plan: &atomics_core::plan::Plan,
    row: &flow::Row,
    action: flow::Action,
    backend: &dyn Backend,
    client: &mut Client,
) {
    match action {
        flow::Action::Diff => open_diff(index, plan, &row.id, client),
        flow::Action::Terminal => {
            let Some(session) = row.session.clone() else { return };
            client.send(Command::Activate { session: session.clone(), open_diff: false });
            client.focus_terminal = Some(session.clone());
            client.simple.work = Work::Terminal(session);
        }
        flow::Action::Message => {
            let state = &mut client.simple.flow;
            state.messaging = (state.messaging.as_deref() != Some(row.id.as_str())).then(|| row.id.clone());
            state.message.clear();
        }
        flow::Action::Send => {
            let Some(session) = row.session.clone() else { return };
            let text = std::mem::take(&mut client.simple.flow.message).trim().to_string();
            client.simple.flow.messaging = None;
            if !text.is_empty() {
                client.send(Command::Steer { sessions: vec![session], text, now: false });
            }
        }
        flow::Action::Preview(k) => {
            let Some(session) = row.session.clone() else { return };
            if let Some(preview) = backend.previews(Some(&session)).get(k).cloned() {
                preview_toggle(&session, preview, backend, client);
            }
        }
        flow::Action::Stop => client.confirm_close = row.session.clone(),
        flow::Action::Expand => {
            let state = &mut client.simple.flow;
            if !state.expanded.remove(&row.id) {
                state.expanded.insert(row.id.clone());
            }
        }
        flow::Action::OpenInFull => {
            client.simple.flow.selected = std::iter::once(row.id.clone()).collect();
            switch(backend, client, UiMode::Full);
        }
    }
}

/// Opens a session's preview through the tunnel the full window's tiles use, or stops it while it is open.
fn preview_toggle(session: &str, preview: atomics_core::preview::Preview, backend: &dyn Backend, client: &mut Client) {
    if backend.preview_active(preview.port) {
        client.send(Command::StopPreview { port: preview.port });
    } else {
        client.send(Command::OpenPreview {
            source: atomics_core::preview::PreviewSource::Session(session.into()),
            preview,
        });
    }
}

/// A session's previews as pills: open or stop each, or where it can be opened when this window cannot.
fn preview_pills(ui: &mut egui::Ui, session: &str, backend: &dyn Backend, client: &mut Client) {
    let previews = backend.previews(Some(session));
    if previews.is_empty() {
        return;
    }
    if !backend.can_preview() {
        let text = RichText::new("Preview on the Mac").font(font(12.5)).color(client.palette.quiet);
        ui.label(text).on_hover_text("This window cannot open previews: open them from ATOMICS on the Mac.");
        return;
    }
    for preview in previews {
        let active = backend.preview_active(preview.port);
        let text = if active { format!("Stop {}", preview.title()) } else { format!("Preview {}", preview.title()) };
        if pill(ui, &text, false, client).clicked() {
            preview_toggle(session, preview, backend, client);
        }
    }
}

/// A session's previews as its row offers them.
fn row_previews(session: Option<&str>, backend: &dyn Backend) -> Vec<flow::RowPreview> {
    let Some(session) = session else { return Vec::new() };
    backend
        .previews(Some(session))
        .into_iter()
        .map(|preview| flow::RowPreview {
            title: preview.title(),
            port: preview.port,
            active: backend.preview_active(preview.port),
        })
        .collect()
}

/// The whole window in simple mode: the top bar; the plans, the chief and the work side by side where they fit,
/// fewer of them where they do not.
pub fn wide(
    ui: &mut egui::Ui,
    screen: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    interface_type(ui);
    ui.painter().rect_filled(screen, 0.0, p.background);
    if client.simple.chat_focus {
        focused_chat(ui, screen, backend, client, terminals);
        return;
    }
    sync_board(backend, client);
    task_modal(ui, backend, client);
    setup::modal(ui, backend, client);
    new_plan::draw(ui, backend, client);
    let rail = screen.width() >= RAIL_MIN;
    let split = screen.width() >= SPLIT_MIN;
    client.simple.tabs_in_header = !split;
    top_bar(ui, screen.with_max_y(screen.top() + TOP), backend, client, !rail, !split);
    let body = screen.with_min_y(screen.top() + TOP);
    let mut rest = body;
    if rail {
        let rect = body.with_max_x(body.left() + RAIL);
        plans(ui, rect, backend, client);
        ui.painter().vline(rect.right(), body.y_range(), Stroke::new(1.0, p.separator));
        rest.min.x = rect.right() + 1.0;
    }
    let plain_chat =
        client.simple.past_chat.is_some() || client.simple.chat_id.is_some() || selected_plan(backend).is_none();
    if plain_chat && client.simple.work != Work::Tickets {
        chat(ui, conversation_column(rest), backend, client, terminals);
        return;
    }
    if !split {
        match client.simple.pane {
            Pane::Work => work(ui, rest, backend, client, terminals),
            _ => chat(ui, rest, backend, client, terminals),
        }
        return;
    }
    let width = rest.width() - 1.0;
    let min_pane = 340.0;
    let chat_width = (width * client.simple.chat_fraction.unwrap_or(0.5)).clamp(min_pane, width - min_pane);
    let chat_rect = rest.with_max_x(rest.left() + chat_width);
    let work_rect = rest.with_min_x(chat_rect.right() + 1.0);
    chat(ui, chat_rect, backend, client, terminals);
    ui.painter().vline(chat_rect.right(), body.y_range(), Stroke::new(1.0, p.separator));
    work(ui, work_rect, backend, client, terminals);
    let handle = crate::shell::split_handle(ui, work_rect, "simple-chat-divider", client);
    if (handle.dragged() || handle.drag_stopped())
        && let Some(pointer) = handle.interact_pointer_pos()
    {
        client.simple.chat_fraction = Some((pointer.x - rest.left()).clamp(min_pane, width - min_pane) / width);
    }
    if handle.double_clicked() {
        client.simple.chat_fraction = None;
    }
}

/// The same conversation expanded into the space available to this window.
pub fn focused_chat(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    sync_board(backend, client);
    interface_type(ui);
    if client.simple.plan_dialog.is_none()
        && !client.simple.setup_open
        && client.simple.task_open.is_none()
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        client.simple.chat_focus = false;
    }
    ui.painter().rect_filled(rect, 0.0, client.palette.background);
    let bar = rect.with_max_y(rect.top() + TOP);
    let mut nav = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("focused-navigation")
            .max_rect(bar.shrink2(vec2(12.0, 3.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    workspace_switcher(&mut nav, backend, client);
    let sidebar = rect.width() >= RAIL_MIN;
    if !sidebar {
        nav.separator();
        plan_picker(&mut nav, backend, client);
    }
    ui.painter().hline(rect.x_range(), bar.bottom(), Stroke::new(1.0, client.palette.separator));
    let mut body = rect.with_min_y(bar.bottom() + 1.0);
    if sidebar {
        let rail = body.with_max_x(body.left() + RAIL);
        plans(ui, rail, backend, client);
        ui.painter().vline(rail.right(), rail.y_range(), Stroke::new(1.0, client.palette.separator));
        body.min.x = rail.right() + 1.0;
    }
    let column = conversation_column(body);
    chat(ui, column, backend, client, terminals);
    task_modal(ui, backend, client);
    setup::modal(ui, backend, client);
    new_plan::draw(ui, backend, client);
}

/// A phone's pane in simple mode, in `rect`; the platform draws the bars around it ([`pane_bar`]).
pub fn phone(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    sync_board(backend, client);
    interface_type(ui);
    client.simple.tabs_in_header = true;
    match client.simple.pane {
        Pane::Chief => chat(ui, rect, backend, client, terminals),
        Pane::Work => work(ui, rect, backend, client, terminals),
        Pane::Plans => {
            // The phone has no status strip: the account and machine are under the plans.
            let split = rect.bottom() - (rect.height() * 0.42).clamp(170.0, 280.0);
            plans(ui, Rect::from_min_max(rect.min, pos2(rect.right(), split)), backend, client);
            account(ui, Rect::from_min_max(pos2(rect.left(), split), rect.max), backend, client);
        }
    }
    task_modal(ui, backend, client);
    setup::modal(ui, backend, client);
    new_plan::draw(ui, backend, client);
}

/// A provider runs low, for the phone's badge on Plans, where its usage is.
pub fn usage_low(backend: &dyn Backend) -> bool {
    backend.resources().is_some_and(|r| [&r.claude, &r.codex].into_iter().any(|u| low(quota(u, r.sampled_at))))
}

/// The phone's account and machine, under its plans: each provider's quota in full, where the agents run and the
/// machine's load, which open Machine.
fn account(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.sidebar);
    ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.separator));
    let inner = rect.shrink2(vec2(14.0, 12.0));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-account").max_rect(inner));
    child.set_clip_rect(rect);
    let ui = &mut child;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Usage").font(font(11.0)).color(p.simple_text(p.quiet, 4.6)));
        if usage_low(backend) {
            ui.label(RichText::new("· running low").font(font(11.0)).color(p.warning));
        }
    });
    ui.add_space(4.0);
    let resources = backend.resources();
    egui::ScrollArea::vertical()
        .id_salt("simple-account-scroll")
        .max_height((ui.available_height() - 40.0).max(40.0))
        .show(ui, |ui| match &resources {
            Some(resources) => crate::resources::usage(ui, resources, &p),
            None => {
                ui.label(
                    RichText::new("No usage readings from the host yet.")
                        .font(font(12.5))
                        .color(p.simple_text(p.muted, 7.2)),
                );
            }
        });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let agents = backend.agents();
        let (place, hover) = place_words(agents.as_ref(), client.link, client.machine_place.as_deref());
        let color = if client.link == Some(false) { p.warning } else { p.muted };
        let response = ui.add(egui::Button::new(RichText::new(place).font(font(12.5)).color(color)).frame(false));
        flow::record(ui, "phone-where", response.rect);
        if response.on_hover_text(hover).clicked() {
            client.machine_open = !client.machine_open;
        }
        if crate::resources::load(ui, backend, &p, false) {
            client.machine_open = !client.machine_open;
        }
        let text = RichText::new("Setup").font(font(12.5)).color(p.accent);
        let response = ui.add(egui::Button::new(text).frame(false));
        flow::record(ui, "phone-setup", response.rect);
        if response.clicked() {
            setup::open(client);
        }
    });
}

/// How many things wait for the user in the selected plan, for a pane's badge: the cards of [`needs`].
pub fn waiting(backend: &dyn Backend) -> usize {
    selected_plan(backend).map_or(0, |index| wanted(index, backend).len())
}

/// A phone's bar of panes at the bottom: the chief, the work (with what waits for the user) and the plans, and
/// the switch to the full window.
pub fn pane_bar(ui: &mut egui::Ui, bar: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let painter = ui.painter().clone();
    painter.rect_filled(bar, 0.0, p.topbar);
    painter.hline(bar.x_range(), bar.top(), Stroke::new(1.0, p.separator));
    let tabs = [(Pane::Chief, "Chief", 0), (Pane::Work, "Work", waiting(backend)), (Pane::Plans, "Plans", 0)];
    // Plans holds the phone's usage: a provider running low marks it.
    let low = usage_low(backend);
    let w = bar.width() / tabs.len() as f32;
    for (k, (pane, label, badge)) in tabs.into_iter().enumerate() {
        let cell = Rect::from_min_size(pos2(bar.left() + w * k as f32, bar.top()), vec2(w, bar.height()));
        let response = ui.interact(cell, ui.scope_id().with(("simple-pane", label)), Sense::click());
        flow::record(ui, &format!("pane:{label}"), cell);
        let on = client.simple.pane == pane;
        let color = if on { p.accent } else { p.muted };
        let icon = pos2(cell.center().x, cell.top() + 18.0);
        match pane {
            Pane::Chief => crate::chief::orbit_icon(
                &painter,
                Rect::from_center_size(icon, vec2(20.0, 20.0)),
                color,
                if on { p.foreground } else { p.muted },
            ),
            Pane::Work => {
                painter.circle_stroke(icon + vec2(-6.0, 0.0), 2.5, Stroke::new(1.4, color));
                for dy in [-6.0, 0.0, 6.0] {
                    let points = [icon + vec2(-3.5, 0.0), icon, icon + vec2(0.0, dy), icon + vec2(7.0, dy)];
                    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                        points,
                        false,
                        Color32::TRANSPARENT,
                        Stroke::new(1.4, color),
                    ));
                }
            }
            Pane::Plans => {
                for dy in [-5.0, 0.0, 5.0] {
                    painter.line_segment([icon + vec2(-7.0, dy), icon + vec2(7.0, dy)], Stroke::new(1.5, color));
                }
            }
        }
        if badge > 0 {
            let c = icon + vec2(11.0, -8.0);
            painter.circle_filled(c, 7.0, p.warning);
            painter.text(c, Align2::CENTER_CENTER, badge.to_string(), font(10.0), p.on_accent);
        }
        if pane == Pane::Plans && low {
            let c = icon + vec2(11.0, -8.0);
            painter.circle_filled(c, 7.0, p.warning);
            painter.text(c, Align2::CENTER_CENTER, "!", font(10.0), p.on_accent);
            flow::record(ui, "usage-low", Rect::from_center_size(c, vec2(14.0, 14.0)));
        }
        painter.text(pos2(cell.center().x, cell.top() + 38.0), Align2::CENTER_CENTER, label, font(12.0), color);
        if response.clicked() {
            client.simple.pane = pane;
            // Again on the pane on screen: back from what it opened to the pane itself.
            if on && pane == Pane::Work {
                client.simple.work = Work::Flow;
            }
            if on && pane == Pane::Chief {
                client.simple.chief_terminal = false;
            }
        }
    }
}

/// A phone's top bar in simple mode: the name and the switch to the full window.
pub fn phone_top(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    interface_type(ui);
    let p = client.palette.clone();
    let painter = ui.painter().clone();
    let y = rect.center().y;
    let mark = Rect::from_center_size(pos2(rect.left() + 9.0, y), vec2(18.0, 18.0));
    crate::chief::orbit_icon(&painter, mark, p.accent, p.foreground);
    let area = Rect::from_min_max(pos2(rect.left() + 24.0, rect.top()), pos2(rect.right() - 48.0, rect.bottom()));
    let mut menu = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("phone-workspace")
            .max_rect(area)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    workspace_switcher(&mut menu, backend, client);
    if backend.management().is_some() {
        let gear = Rect::from_min_size(pos2(rect.right() - 44.0, y - 20.0), vec2(44.0, 40.0));
        settings_gear(ui, gear, backend, client);
    }
}

mod history;
pub mod look;
mod model;
mod new_plan;
mod prose;
mod setup;
mod waves;
pub use setup::open as open_setup;

#[cfg(test)]
mod tests;
