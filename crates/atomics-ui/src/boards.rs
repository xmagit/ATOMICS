//! The top bar's Boards dropdown, one button for every plan's board. The button names the board on screen and marks
//! how many boards wait for the user. Its menu lists those first, then the other open boards with their progress and
//! state, then the finished ones not yet archived, then History with the archived ones, then the views that did not
//! fit in the bar. The sidebar's BOARDS list the same boards in the same order ([`listed`]). A row or button whose
//! command the backend cannot carry out is left out.

use atomics_core::plan::{PlanStatus, TaskStatus};
use egui::{Atom, AtomExt as _, Atoms, Color32, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::status::{Mark, PULSE_STEP, paint_mark, pulse};
use crate::theme::{Palette, lerp};
use crate::topbar::{measure, record_item};
use crate::touch::Touch;
use crate::{Backend, Client, Command, PanelMode, PlanRow, PlanState, controls};

/// The size of the text on the button and in the rows.
const FONT: f32 = 13.0;
/// A row's height, and the widest the menu gets.
const ROW: f32 = 28.0;
const MENU_WIDTH: f32 = 480.0;
/// The widest the button gets in the bar, however wide the window: a longer goal ends in an ellipsis, and the
/// button's hover shows it whole.
const BUTTON_WIDTH: f32 = 360.0;
/// The columns right of a board's name: its progress, then its state.
const PROGRESS: f32 = 44.0;
const STATE: f32 = 96.0;
/// The room right of a row's state.
const CLOSE: f32 = 10.0;

/// What showing a board asks for: an open plan's sessions and board, else the finished plan's board, read-only.
pub fn show_command(row: &PlanRow, index: usize) -> Command {
    if row.status == PlanStatus::Open { Command::ShowPlan(index) } else { Command::ViewPlanHistory(index) }
}

/// A board's name without the progress its own column shows.
fn name(row: &PlanRow) -> &str {
    let progress = format!(" · {}/{}", row.done, row.total);
    row.tab_name.strip_suffix(progress.as_str()).unwrap_or(&row.tab_name)
}

/// The boards in the order the dropdown and the sidebar both list them.
#[derive(Debug, Clone, Default)]
pub struct Listed {
    /// Open boards that wait for the user, in plan order.
    pub waiting: Vec<(usize, PlanRow)>,
    /// The other open boards, in plan order.
    pub active: Vec<(usize, PlanRow)>,
    /// Finished boards not archived yet, the latest first (in plan order where that ties): they read as done until
    /// the user archives them.
    pub done: Vec<(usize, PlanRow)>,
    /// Archived boards, in the same order.
    pub history: Vec<(usize, PlanRow)>,
}

impl Listed {
    /// The boards one steps through: all but History, in their order.
    pub fn steps(&self) -> impl Iterator<Item = &(usize, PlanRow)> {
        self.waiting.iter().chain(&self.active).chain(&self.done)
    }
}

/// Every board the backend can show, sorted as [`Listed`] says.
pub fn listed(backend: &dyn Backend) -> Listed {
    let mut listed = Listed::default();
    for (index, row) in backend.plan_list().into_iter().enumerate() {
        if row.id.is_empty() || !backend.available(&show_command(&row, index)) {
            continue;
        }
        let list = match row.status {
            PlanStatus::Open if row.needs_user() => &mut listed.waiting,
            PlanStatus::Open => &mut listed.active,
            PlanStatus::Completed => &mut listed.done,
            PlanStatus::Archived => &mut listed.history,
        };
        list.push((index, row));
    }
    // The latest first; those that tie keep the plans' order.
    for list in [&mut listed.done, &mut listed.history] {
        list.sort_by_key(|(_, row)| std::cmp::Reverse(row.last_active));
    }
    listed
}

/// What showing a board takes: the board, in the panel whichever view it was on.
pub fn show(row: &PlanRow, index: usize, backend: &dyn Backend, client: &mut Client) {
    client.send(show_command(row, index));
    for command in [Command::SetPanel(PanelMode::Plan), Command::OpenDiff] {
        if backend.available(&command) {
            client.send(command);
        }
    }
}

/// Shows the next board (`delta` 1) or the previous one (-1) in the listed order, from the board on screen; from no
/// board, the first or the last. History is left out. Returns whether there was one to show.
pub fn step(backend: &dyn Backend, client: &mut Client, delta: i32) -> bool {
    let listed = listed(backend);
    let rows: Vec<_> = listed.steps().collect();
    if rows.is_empty() {
        return false;
    }
    let hints = backend.view_hints();
    let at = hints.plan_view.then(|| rows.iter().position(|(index, _)| *index == hints.selected_plan)).flatten();
    let next = match at {
        Some(at) => (at as i32 + delta).rem_euclid(rows.len() as i32) as usize,
        None if delta > 0 => 0,
        None => rows.len() - 1,
    };
    let (index, row) = rows[next];
    show(row, *index, backend, client);
    true
}

/// The live agents of the plan `id`, which completing or archiving it closes: they stay under its board, closed,
/// a click away from resuming.
pub fn live_agents(backend: &dyn Backend, id: &str) -> Vec<crate::SessionRow> {
    use atomics_core::session::AgentStatus;
    backend
        .session_list()
        .into_iter()
        .filter(|s| !s.chief && s.status != AgentStatus::Exited && s.task.as_ref().is_some_and(|link| link.plan == id))
        .collect()
}

/// What completing or archiving the plan at `index` would leave behind, which a confirmation names: its tasks not
/// accepted, its agents at work, and what stays on disk: its task branches, and the worktrees with uncommitted
/// work, which cleanup keeps. Completing closes the agents of accepted tasks and leaves the others running;
/// archiving closes them all. An idle agent of an accepted task closing is what completing is for, not a stake.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stakes {
    pub unaccepted: Vec<String>,
    /// Agents at work, by title, and whether their task is accepted.
    pub working: Vec<(String, bool)>,
    pub branches: Vec<String>,
}

impl Stakes {
    pub fn of(backend: &dyn Backend, index: usize) -> Stakes {
        let Some(board) = backend.board(index) else { return Stakes::default() };
        let plan = &board.plan;
        let pending: Vec<_> = plan.pending_integrations().iter().map(|task| task.id.clone()).collect();
        let accepted = |id: &str| plan.tasks.iter().any(|t| t.id == id && t.status == TaskStatus::Done);
        let unaccepted = plan
            .tasks
            .iter()
            .filter(|task| task.status != TaskStatus::Done || pending.contains(&task.id))
            .map(|task| if task.title.is_empty() { task.id.clone() } else { task.title.clone() })
            .collect();
        let working = live_agents(backend, &plan.id)
            .into_iter()
            .filter(|s| s.status == atomics_core::session::AgentStatus::Working || s.needs_user())
            .map(|s| {
                let done = s.task.as_ref().is_some_and(|link| accepted(&link.task));
                (s.title, done)
            })
            .collect();
        let mut branches: Vec<String> = plan.tasks.iter().filter_map(|task| task.branch.clone()).collect();
        branches.dedup();
        Stakes { unaccepted, working, branches }
    }

    /// Nothing is lost by closing it: every task is accepted and no agent is at work.
    pub fn none(&self) -> bool {
        self.unaccepted.is_empty() && self.working.is_empty()
    }

    /// What becomes of the agents at work, in a sentence or two; `None` while none works.
    pub fn agents_line(&self, completing: bool) -> Option<String> {
        if self.working.is_empty() {
            return None;
        }
        let (close, stay): (Vec<_>, Vec<_>) = self.working.iter().partition(|(_, accepted)| !completing || *accepted);
        let names = |agents: Vec<&(String, bool)>| {
            listed_names(&agents.into_iter().map(|(n, _)| n.clone()).collect::<Vec<_>>())
        };
        let mut parts = Vec::new();
        if !close.is_empty() {
            parts.push(format!("Agents at work close: {}.", names(close)));
        }
        if !stay.is_empty() {
            parts.push(format!("Agents of tasks not accepted keep running: {}.", names(stay)));
        }
        Some(parts.join(" "))
    }
}

/// `names` in a line: the first three, and how many more.
fn listed_names(names: &[String]) -> String {
    let shown: Vec<_> = names.iter().take(3).map(String::as_str).collect();
    match names.len().saturating_sub(shown.len()) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// Completes the plan at `index`: at once, with an Undo, while every task is accepted and no agent runs; else it
/// asks first, naming what would be left behind.
pub fn complete(index: usize, backend: &dyn Backend, client: &mut Client) {
    let Some(row) = backend.plan_row(index) else { return };
    if !Stakes::of(backend, index).none() {
        client.confirm_plan_close = Some((row.id, PlanStatus::Completed));
        return;
    }
    client.send(Command::ClosePlan { plan: index, status: PlanStatus::Completed, force: false });
    offer_undo(backend, client, &row, Undo::Reopen, "Completed");
}

/// Archives the plan at `index` into History: at once, with an Undo, while nothing is at stake (a finished plan, or
/// an open one whose tasks are all accepted, with no agent running); else it asks first.
pub fn archive(index: usize, backend: &dyn Backend, client: &mut Client) {
    let Some(row) = backend.plan_row(index) else { return };
    let stakes = Stakes::of(backend, index);
    let finished = row.status != PlanStatus::Open;
    if !stakes.working.is_empty() || (!finished && !stakes.unaccepted.is_empty()) {
        client.confirm_plan_close = Some((row.id, PlanStatus::Archived));
        return;
    }
    client.send(Command::ArchivePlan(index));
    let undo = if finished { Undo::Complete } else { Undo::Reopen };
    offer_undo(backend, client, &row, undo, "Archived into History");
}

/// The short confirmation before completing or archiving a plan with something at stake, which every platform
/// draws: what is not accepted, which agents close, and what stays on disk; then the close anyway, or Cancel.
pub fn confirm_close(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    let Some((id, status)) = client.confirm_plan_close.clone() else { return };
    let rows = backend.plan_list();
    let Some(index) = rows.iter().position(|row| row.id == id && row.status != PlanStatus::Archived) else {
        client.confirm_plan_close = None;
        return;
    };
    let row = &rows[index];
    let stakes = Stakes::of(backend, index);
    let completing = status == PlanStatus::Completed && row.status == PlanStatus::Open;
    // A client whose host takes no forced complete asks for a plain one, which the host refuses with its reason
    // where the plan's workflow is not done.
    let forced =
        Command::ClosePlan { plan: index, status: PlanStatus::Completed, force: !stakes.unaccepted.is_empty() };
    let plain = Command::ClosePlan { plan: index, status: PlanStatus::Completed, force: false };
    let command = if !completing {
        Command::ArchivePlan(index)
    } else if backend.available(&forced) {
        forced
    } else {
        plain
    };
    if !backend.available(&command) {
        client.confirm_plan_close = None;
        return;
    }
    let p = client.palette.clone();
    let touch = client.touch;
    let mut answered = false;
    let mut open = true;
    let verb = if completing { "Complete" } else { "Archive" };
    let title = format!("{verb} “{}”?", crate::simple::goal(&row.goal).chars().take(48).collect::<String>());
    egui::Window::new(title)
        .id(egui::Id::NULL.with("confirm-plan-close"))
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_max_width((ctx.content_rect().width() - 48.0).clamp(200.0, 440.0));
            let line = |ui: &mut egui::Ui, text: String| {
                ui.add(egui::Label::new(RichText::new(text).color(p.foreground)).wrap());
            };
            if !stakes.unaccepted.is_empty() {
                line(ui, format!("Not accepted: {}.", listed_names(&stakes.unaccepted)));
            }
            if let Some(agents) = stakes.agents_line(completing) {
                line(ui, agents);
                if stakes.working.iter().any(|(_, accepted)| !completing || *accepted) {
                    let note = "A closed agent that can resume shows under the board.";
                    ui.label(RichText::new(note).color(p.muted).size(12.0));
                }
            }
            let kept = if stakes.branches.is_empty() {
                "Kept on disk: worktrees with uncommitted work.".to_string()
            } else {
                format!(
                    "Kept on disk: worktrees with uncommitted work, and the branches {}.",
                    listed_names(&stakes.branches)
                )
            };
            ui.label(RichText::new(kept).color(p.muted).size(12.0));
            if completing {
                ui.label(RichText::new("It stays in BOARDS as done until you archive it.").color(p.muted).size(12.0));
            } else {
                ui.label(RichText::new("History keeps it, and Reopen brings it back.").color(p.muted).size(12.0));
            }
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                let label = format!("{verb} anyway");
                let response = ui.add(crate::touch::sized(egui::Button::new(label), touch));
                crate::flow::record(ui, "close-anyway", response.rect);
                if response.clicked() {
                    client.send(command.clone());
                    if stakes.working.is_empty() {
                        let undo = if row.status == PlanStatus::Open { Undo::Reopen } else { Undo::Complete };
                        offer_undo(
                            backend,
                            client,
                            row,
                            undo,
                            if completing { "Completed" } else { "Archived into History" },
                        );
                    }
                    answered = true;
                }
                answered |= ui.add(crate::touch::sized(egui::Button::new("Cancel"), touch)).clicked();
            });
        });
    if answered || !open {
        client.confirm_plan_close = None;
    }
}

/// What undoing a close does: reopen the plan, and complete it again where it was done before it was archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undo {
    Reopen,
    Complete,
}

/// A close that can be undone for a few seconds: the plan, by id, what it says and how it is undone.
#[derive(Debug, Clone, PartialEq)]
pub struct Undoable {
    pub plan: String,
    pub text: String,
    pub undo: Undo,
    /// Until when it is offered, by this client's clock; set when it first shows.
    pub until: Option<f64>,
}

/// How long an Undo stays on offer.
const UNDO_SECONDS: f64 = 8.0;

fn offer_undo(backend: &dyn Backend, client: &mut Client, row: &PlanRow, undo: Undo, done: &str) {
    if !backend.available(&Command::ReopenPlan(0)) {
        client.send(Command::Toast(format!("{done}: {}", crate::simple::goal(&row.goal))));
        return;
    }
    let text = format!("{done}: {}", crate::simple::goal(&row.goal));
    client.undo = Some(Undoable { plan: row.id.clone(), text, undo, until: None });
}

/// The Undo bar at the foot of the window while a close can still be undone.
pub fn undo_bar(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    let Some(mut offer) = client.undo.take() else { return };
    let now = ctx.input(|i| i.time);
    let until = *offer.until.get_or_insert(now + UNDO_SECONDS);
    if now >= until {
        return;
    }
    ctx.request_repaint_after(std::time::Duration::from_secs_f64((until - now).max(0.05)));
    let p = client.palette.clone();
    let screen = ctx.content_rect();
    let mut undone = false;
    let mut dismissed = false;
    egui::Area::new(egui::Id::NULL.with("undo-bar"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_BOTTOM, vec2(0.0, -56.0))
        .constrain_to(screen)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.overlay.to_opaque())
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(10)
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let text: String = offer.text.chars().take(72).collect();
                        ui.label(RichText::new(text).font(crate::flow::font(13.0)).color(p.foreground));
                        ui.add_space(8.0);
                        let response = ui.add(egui::Button::new(
                            RichText::new("Undo").font(crate::flow::font(13.0)).color(p.accent).strong(),
                        ));
                        crate::flow::record(ui, "undo", response.rect);
                        undone = response.clicked();
                        dismissed =
                            ui.add(egui::Button::new(RichText::new("Dismiss").color(p.muted)).frame(false)).clicked();
                    });
                });
        });
    if undone {
        if let Some(index) = backend.plan_list().iter().position(|row| row.id == offer.plan) {
            client.send(Command::ReopenPlan(index));
            if offer.undo == Undo::Complete {
                client.send(Command::ClosePlan { plan: index, status: PlanStatus::Completed, force: true });
            }
        }
        return;
    }
    if !dismissed {
        client.undo = Some(offer);
    }
}

/// What a board's row says of its state.
fn state_word(row: &PlanRow) -> String {
    if row.awaits_approval() {
        return "needs approval".into();
    }
    match row.state {
        PlanState::Running(count) => format!("{count} running"),
        PlanState::NeedsYou => "needs you".into(),
        PlanState::Idle => "idle".into(),
        PlanState::Completed => "done".into(),
        PlanState::Archived => "archived".into(),
    }
}

/// The color of a board's name in a row: its state's, or what asks for the user.
fn row_color(row: &PlanRow, p: &Palette) -> Color32 {
    if row.needs_user() { p.warning } else { row.state.color(p) }
}

/// The button: the marker with its count, `label` and the chevron. Only the label gives way in a narrow slot.
fn button<'a>(label: &str, waiting: usize, selected: bool, p: &Palette) -> egui::Button<'a> {
    let (foreground, warning, muted) = (p.foreground, p.warning, p.muted);
    let font = p.ui(FONT);
    let mut atoms = Atoms::default();
    if waiting > 0 {
        atoms.push_right(Atom::paint(vec2(10.0, 10.0), move |ui: &egui::Ui, args: egui::AtomPaintArgs| {
            paint_mark(ui.painter(), args.rect.center(), Mark::Diamond, warning);
        }));
        atoms.push_right(RichText::new(waiting.to_string()).font(font.clone()).color(warning).strong());
    }
    atoms.push_right(RichText::new(label.to_string()).font(font).color(foreground).atom_shrink(true));
    atoms.push_right(controls::chevron_atom(muted));
    controls::flat(egui::Button::new(atoms).truncate(), selected, p)
}

/// One draw's view of the plans.
struct Boards {
    rows: Vec<PlanRow>,
    listed: Listed,
    /// The board on screen, if any: an open or a finished one.
    shown: Option<usize>,
}

impl Boards {
    /// `start`: the start screen shows, which is no board's place.
    fn read(backend: &dyn Backend, start: bool) -> Self {
        let rows = backend.plan_list();
        let hints = backend.view_hints();
        let on_board = hints.plan_view && rows.get(hints.selected_plan).is_some_and(|row| !row.id.is_empty());
        let shown = (on_board && !start).then_some(hints.selected_plan);
        Self { rows, listed: listed(backend), shown }
    }

    fn waiting(&self) -> usize {
        self.listed.waiting.len()
    }

    fn finished(&self) -> bool {
        !self.listed.history.is_empty()
    }

    /// What the button says: the board on screen, else the word.
    fn label(&self) -> String {
        self.shown.map_or_else(|| "Boards".to_string(), |index| name(&self.rows[index]).to_string())
    }
}

/// The dropdown's width in the bar: as it wants to be, up to [`BUTTON_WIDTH`], and with no more than the word for a
/// label.
pub fn widths(ui: &mut egui::Ui, backend: &dyn Backend, client: &Client) -> (f32, f32) {
    let palette = &client.palette;
    let boards = Boards::read(backend, client.home.shown);
    let waiting = boards.waiting();
    let selected = boards.shown.is_some();
    let natural = measure(ui, |ui| {
        ui.add(button(&boards.label(), waiting, selected, palette));
    });
    let word = measure(ui, |ui| {
        ui.add(button("Boards", waiting, selected, palette));
    });
    (natural.min(BUTTON_WIDTH.max(word)), natural.min(word))
}

/// The Boards dropdown, `width` wide, with what `folded` draws, if anything is folded, at the end of its menu.
pub fn dropdown(
    ui: &mut egui::Ui,
    width: f32,
    backend: &dyn Backend,
    client: &mut Client,
    folded: Option<impl FnOnce(&mut egui::Ui, &mut Client)>,
) {
    let palette = client.palette.clone();
    let boards = Boards::read(backend, client.home.shown);
    let waiting = boards.waiting();
    let button = button(&boards.label(), waiting, boards.shown.is_some(), &palette);
    let height = ui.spacing().interact_size.y;
    let response = ui
        .allocate_ui_with_layout(vec2(width, height), *ui.layout(), |ui| {
            ui.set_min_width(width);
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
            egui::containers::menu::MenuButton::from_button(button)
                .ui(ui, |ui| menu(ui, &boards, backend, client, folded))
                .0
        })
        .inner;
    record_item(ui, "Boards", &response);
    // Something changed on a board since the user last looked: the mark an update leaves on a menu.
    if boards.rows.iter().any(|row| row.changed) {
        ui.painter().circle_filled(response.rect.right_top() + vec2(-4.0, 4.0), 2.5, palette.accent);
    }
    response.on_hover_text(hover(&boards, waiting));
}

fn hover(boards: &Boards, waiting: usize) -> String {
    let shown = boards.shown.map(|index| boards.rows[index].goal.as_str()).filter(|goal| !goal.is_empty());
    let waits = match waiting {
        0 => None,
        1 => Some("1 board needs you".to_string()),
        n => Some(format!("{n} boards need you")),
    };
    match (shown, waits) {
        (Some(goal), Some(waits)) => format!("{goal}\n{waits}"),
        (Some(goal), None) => goal.to_string(),
        (None, Some(waits)) => waits,
        (None, None) => "The boards of your plans".into(),
    }
}

/// The menu's body: the boards that wait for the user, the other open ones, History, then what is folded in.
fn menu(
    ui: &mut egui::Ui,
    boards: &Boards,
    backend: &dyn Backend,
    client: &mut Client,
    folded: Option<impl FnOnce(&mut egui::Ui, &mut Client)>,
) {
    let p = client.palette.clone();
    let screen = ui.ctx().content_rect();
    let width = MENU_WIDTH.min(screen.width() - 32.0);
    ui.set_width(width);
    fn rows(list: &[(usize, PlanRow)]) -> Vec<(usize, &PlanRow)> {
        list.iter().map(|(index, row)| (*index, row)).collect()
    }
    let (waiting, active, done) =
        (rows(&boards.listed.waiting), rows(&boards.listed.active), rows(&boards.listed.done));
    let finished = boards.finished();
    egui::ScrollArea::vertical()
        .id_salt("boards")
        .max_height((screen.height() * 0.7).max(ROW))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            section(ui, "NEEDS YOU", p.warning, &waiting, boards.shown, backend, client);
            section(ui, "ACTIVE", p.muted, &active, boards.shown, backend, client);
            section(ui, "DONE", p.muted, &done, boards.shown, backend, client);
            if waiting.is_empty() && active.is_empty() && done.is_empty() {
                let hint = if finished {
                    "No open boards. Archived ones are under History."
                } else {
                    "No boards yet. Ask the chief to plan a goal: its board shows here."
                };
                ui.add_space(4.0);
                ui.label(RichText::new(hint).color(p.muted).size(12.0));
                ui.add_space(4.0);
            }
        });
    if finished {
        ui.separator();
        history_menu(ui, backend, client);
    }
    if let Some(folded) = folded {
        ui.separator();
        folded(ui, client);
    }
}

/// A heading and its boards; nothing without any.
fn section(
    ui: &mut egui::Ui,
    title: &str,
    color: Color32,
    rows: &[(usize, &PlanRow)],
    shown: Option<usize>,
    backend: &dyn Backend,
    client: &mut Client,
) {
    if rows.is_empty() {
        return;
    }
    let p = client.palette.clone();
    ui.add_space(4.0);
    let heading = RichText::new(format!("{title} · {}", rows.len())).size(10.5).color(color).strong();
    ui.add(egui::Label::new(heading).selectable(false));
    ui.add_space(3.0);
    for &(index, row) in rows {
        let actions = RowActions {
            complete: row.status == PlanStatus::Open
                && backend.available(&Command::ClosePlan { plan: index, status: PlanStatus::Completed, force: false }),
            archive: backend.available(&Command::ArchivePlan(index)),
        };
        match board_row(ui, row, shown == Some(index), actions, client.touch, &p) {
            Some(RowClick::Show) => {
                // A row is how a board is asked for: it shows, whichever view the panel was on.
                show(row, index, backend, client);
                ui.close();
            }
            Some(RowClick::Complete) => {
                complete(index, backend, client);
                ui.close();
            }
            Some(RowClick::Archive) => {
                archive(index, backend, client);
                ui.close();
            }
            None => {}
        }
    }
    ui.add_space(4.0);
}

/// Which of the closes a board's row offers.
#[derive(Debug, Clone, Copy)]
struct RowActions {
    complete: bool,
    archive: bool,
}

enum RowClick {
    Show,
    Complete,
    Archive,
}

/// A board's row: its mark and name, progress, state and close button, in columns that line up down the menu.
fn board_row(
    ui: &mut egui::Ui,
    row: &PlanRow,
    selected: bool,
    actions: RowActions,
    touch: Option<Touch>,
    p: &Palette,
) -> Option<RowClick> {
    let height = touch.map_or(ROW, |touch| touch.target);
    let close_width = touch.map_or(CLOSE, |touch| touch.target);
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let time = ui.input(|input| input.time);
    let painter = ui.painter_at(rect);
    let over = ui.rect_contains_pointer(rect);
    if selected {
        painter.rect_filled(rect, 4.0, lerp(p.overlay, p.accent, 0.14));
    }
    controls::feedback_where(ui, rect, &response, over, 4.0, p);
    let y = rect.center().y;
    let color = row_color(row, p);
    let mark = pos2(rect.left() + 12.0, y);
    match row.state {
        PlanState::Running(_) => {
            if crate::theme::still(ui.ctx()) {
                paint_mark(&painter, mark, Mark::Dot, color);
            } else {
                paint_mark(&painter, mark, Mark::Dot, pulse(p.overlay, color, time));
                ui.ctx().request_repaint_after(PULSE_STEP);
            }
        }
        _ if row.needs_user() => paint_mark(&painter, mark, Mark::Diamond, color),
        PlanState::Completed | PlanState::Archived => paint_mark(&painter, mark, Mark::Check, p.success),
        _ => paint_mark(&painter, mark, Mark::Ring, p.muted),
    }
    let close_left = rect.right() - close_width;
    let state_right = close_left - 4.0;
    let progress_right = state_right - STATE - 8.0;
    let name_left = rect.left() + 24.0;
    let name_right = progress_right - PROGRESS - 8.0;
    let galley = fitted(ui, name(row), p.ui(FONT), color, name_right - name_left);
    let name_width = galley.size().x;
    painter.galley(pos2(name_left, y - galley.size().y / 2.0), galley, color);
    if row.changed {
        painter.circle_filled(pos2(name_left + name_width + 7.0, y), 2.5, p.accent);
    }
    // On a row under the pointer, its closes in words where its progress and state were: Complete and Archive.
    let mut clicked = None;
    if over && (actions.complete || actions.archive) {
        let mut right = state_right;
        for (word, wanted, click, key) in [
            ("Archive", actions.archive, RowClick::Archive, "plan-close"),
            ("Complete", actions.complete, RowClick::Complete, "plan-complete"),
        ] {
            if !wanted {
                continue;
            }
            let hit = controls::row_action(ui, ui.scope_id().with((key, &row.id)), pos2(right, y), word, p);
            let area = hit.rect;
            record_rect(ui, (key, row.id.as_str(), ""), area);
            let tip = match click {
                RowClick::Complete => "Mark it done; asks first while tasks are not accepted or agents run",
                _ => "Move it to History, where it can be reopened; asks first while something is at stake",
            };
            if hit.on_hover_text(tip).clicked() {
                clicked = Some(click);
            }
            right = area.left() - 6.0;
        }
    } else {
        if row.total > 0 {
            let progress = format!("{}/{}", row.done, row.total);
            painter.text(pos2(progress_right, y), egui::Align2::RIGHT_CENTER, progress, p.ui(11.0), p.muted);
        }
        let word = if row.needs_user() { p.warning } else { p.muted };
        painter.text(pos2(state_right, y), egui::Align2::RIGHT_CENTER, state_word(row), p.ui(11.0), word);
    }
    let mut hover = row.goal.clone();
    if row.changed {
        hover.push_str("\nChanged since you last looked");
    }
    if response.on_hover_text(hover).clicked() && clicked.is_none() {
        clicked = Some(RowClick::Show);
    }
    clicked
}

/// `text` on one line of at most `width`, with an ellipsis where it was cut.
fn fitted(ui: &egui::Ui, text: &str, font: egui::FontId, color: Color32, width: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

/// The archived boards: each read-only behind View, and back to work with Reopen.
fn history_menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let mut view = None;
    let mut reopen = None;
    let can_view = backend.available(&Command::ViewPlanHistory(0));
    let can_reopen = backend.available(&Command::ReopenPlan(0));
    let screen = ui.ctx().content_rect();
    let response = ui
        .menu_button("History", |ui| {
            ui.set_max_width(MENU_WIDTH.min(screen.width() - 32.0));
            egui::ScrollArea::vertical().id_salt("boards-history").max_height((screen.height() * 0.7).max(ROW)).show(
                ui,
                |ui| {
                    for (index, plan) in listed(backend).history.iter().map(|(index, plan)| (*index, plan)) {
                        ui.push_id(&plan.id, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&plan.goal).color(plan.state.color(&p)));
                                let status = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
                                // A client has no date to give.
                                let when = match backend.plan_date(index) {
                                    date if date.is_empty() => status.to_string(),
                                    date => format!("{status} · {date}"),
                                };
                                ui.label(RichText::new(when).color(p.muted)).on_hover_text(
                                    "Date the plan was last saved, or its last task completion when no saved file exists",
                                );
                                if can_view {
                                    let response = ui.button("View");
                                    record_rect(ui, ("plan-history", plan.id.as_str(), "view"), response.rect);
                                    if response.clicked() {
                                        view = Some(index);
                                        ui.close();
                                    }
                                }
                                if can_reopen {
                                    let response = ui.button("Reopen");
                                    record_rect(ui, ("plan-history", plan.id.as_str(), "reopen"), response.rect);
                                    if response.clicked() {
                                        reopen = Some(index);
                                        ui.close();
                                    }
                                }
                            });
                        });
                    }
                },
            );
        })
        .response;
    record_item(ui, "History", &response);
    if let Some(index) = view {
        client.send(Command::ViewPlanHistory(index));
    }
    if let Some(index) = reopen {
        client.send(Command::ReopenPlan(index));
    }
}

/// Records where a control of the menu was drawn, for the desktop's tests (feature `test-support`).
fn record_rect(_ui: &egui::Ui, _key: (&str, &str, &str), _rect: Rect) {
    #[cfg(feature = "test-support")]
    if !_ui.is_sizing_pass() {
        _ui.ctx().data_mut(|data| data.insert_temp(egui::Id::NULL.with(_key), _rect));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(state: PlanState, approved: bool, total: usize) -> PlanRow {
        PlanRow {
            created_at: 0,
            first_message: None,
            id: "p".into(),
            goal: "Ship it".into(),
            tab_name: "Ship it".into(),
            status: PlanStatus::Open,
            state,
            approved,
            changed: false,
            done: 1,
            total,
            attention: 0,
            last_active: 0,
            sessions: Vec::new(),
        }
    }

    #[test]
    fn a_board_waits_for_the_user_when_it_needs_approval_or_needs_them() {
        let unapproved = row(PlanState::Idle, false, 2);
        assert!(unapproved.awaits_approval() && unapproved.needs_user());
        assert_eq!(state_word(&unapproved), "needs approval");

        let stuck = row(PlanState::NeedsYou, true, 2);
        assert!(!stuck.awaits_approval() && stuck.needs_user());
        assert_eq!(state_word(&stuck), "needs you");

        // Nothing to approve yet: its tasks are still being planned.
        let planning = row(PlanState::Idle, false, 0);
        assert!(!planning.awaits_approval() && !planning.needs_user());
        assert_eq!(state_word(&planning), "idle");

        assert_eq!(state_word(&row(PlanState::Running(2), true, 2)), "2 running");

        // A finished plan never waits, however it ended.
        let finished = PlanRow { status: PlanStatus::Completed, ..row(PlanState::Completed, false, 2) };
        assert!(!finished.awaits_approval() && !finished.needs_user());
    }

    #[test]
    fn a_boards_name_leaves_its_progress_to_the_column() {
        let tracked = PlanRow { tab_name: "DEMO-1 · 1/2".into(), ..row(PlanState::Idle, true, 2) };
        assert_eq!(name(&tracked), "DEMO-1");
        let planning = PlanRow { tab_name: "DEMO-1 · planning…".into(), ..row(PlanState::Idle, true, 0) };
        assert_eq!(name(&planning), "DEMO-1 · planning…");
        assert_eq!(name(&row(PlanState::Idle, true, 2)), "Ship it");
    }
}
