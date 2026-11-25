//! The top bar's Boards dropdown, one button for every plan's board. The button names the board on screen and marks
//! how many boards wait for the user. Its menu lists those first, then the other open boards with their progress and
//! state, then the finished ones not yet archived, then History with the archived ones, then the views that did not
//! fit in the bar. The sidebar's BOARDS list the same boards in the same order ([`listed`]). A row or button whose
//! command the backend cannot carry out is left out.

use atomics_core::plan::PlanStatus;
use egui::{Atom, AtomExt as _, Atoms, Color32, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};

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
/// The room for a row's close button.
const CLOSE: f32 = 26.0;

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
    fn read(backend: &dyn Backend) -> Self {
        let rows = backend.plan_list();
        let hints = backend.view_hints();
        let shown = (hints.plan_view && rows.get(hints.selected_plan).is_some_and(|row| !row.id.is_empty()))
            .then_some(hints.selected_plan);
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
pub fn widths(ui: &mut egui::Ui, backend: &dyn Backend, palette: &Palette) -> (f32, f32) {
    let boards = Boards::read(backend);
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
    let boards = Boards::read(backend);
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
        let can_close = backend.available(&Command::ClosePlanTab(index));
        match board_row(ui, row, shown == Some(index), can_close, client.touch, &p) {
            Some(RowClick::Show) => {
                // A row is how a board is asked for: it shows, whichever view the panel was on.
                show(row, index, backend, client);
                ui.close();
            }
            Some(RowClick::Close) => {
                client.send(Command::ClosePlanTab(index));
                ui.close();
            }
            None => {}
        }
    }
    ui.add_space(4.0);
}

enum RowClick {
    Show,
    Close,
}

/// A board's row: its mark and name, progress, state and close button, in columns that line up down the menu.
fn board_row(
    ui: &mut egui::Ui,
    row: &PlanRow,
    selected: bool,
    can_close: bool,
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
        painter.rect_filled(rect, 2.0, lerp(p.overlay, p.accent, 0.14));
    } else if over {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    if response.has_focus() {
        let ring = Stroke::new(2.0, ui.visuals().selection.stroke.color);
        painter.rect_stroke(rect.shrink(1.0), 2.0, ring, StrokeKind::Inside);
    }
    let y = rect.center().y;
    let color = row_color(row, p);
    let mark = pos2(rect.left() + 12.0, y);
    match row.state {
        PlanState::Running(_) => {
            paint_mark(&painter, mark, Mark::Dot, pulse(p.overlay, color, time));
            ui.ctx().request_repaint_after(PULSE_STEP);
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
    if row.total > 0 {
        let progress = format!("{}/{}", row.done, row.total);
        painter.text(pos2(progress_right, y), egui::Align2::RIGHT_CENTER, progress, p.ui(11.0), p.muted);
    }
    let word = if row.needs_user() { p.warning } else { p.muted };
    painter.text(pos2(state_right, y), egui::Align2::RIGHT_CENTER, state_word(row), p.ui(11.0), word);
    let mut hover = row.goal.clone();
    if row.changed {
        hover.push_str("\nChanged since you last looked");
    }
    let mut clicked = response.on_hover_text(hover).clicked().then_some(RowClick::Show);
    if can_close {
        let close_size = touch.map_or(18.0, |touch| touch.target);
        let close = Rect::from_center_size(pos2(close_left + close_width / 2.0, y), vec2(close_size, close_size));
        let hit = ui.interact(close, ui.scope_id().with(("board-close", &row.id)), Sense::click());
        if hit.hovered() {
            painter.rect_filled(close, 2.0, p.border);
        }
        let color = if hit.hovered() { p.foreground } else { p.quiet };
        paint_mark(&painter, close.center(), Mark::Cross, color);
        record_rect(ui, ("plan-close", row.id.as_str(), ""), close);
        let hover = if row.status == PlanStatus::Open {
            "Close this plan: asks first"
        } else {
            "Archive this board: it moves to History"
        };
        if hit.on_hover_text(hover).clicked() {
            clicked = Some(RowClick::Close);
        }
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
