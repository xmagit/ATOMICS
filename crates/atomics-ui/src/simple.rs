//! Simple mode (`[ui] mode = "simple"`): a chat-first window for people new to ATOMICS. The chief's conversation
//! is the centre; the plans are a list on the left, like conversations, each with its state; the right shows the
//! selected plan's work: what needs the user, as actions, and the [`flow`](crate::flow) of its tasks with what
//! passes between them ([`traffic`](crate::traffic)). Every full feature stays one click away: a task's diff in
//! the existing diff view, its agent's terminal, the full board, and the full window itself.
//!
//! The host's `[ui] mode` says what its windows open in; each client may switch for itself ([`Client::mode`]),
//! which the platform keeps on its own side and never writes to the host's config. Nothing here decides a
//! permission or an approval on its own: the cards draw the same banner and send the same commands as the full
//! window ([`approvals::banner`], [`Command::ApprovePlan`], the notebook relay's decisions).
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
use crate::{Backend, Client, Command, PlanState, SessionRow, approvals, touch};

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
    /// The plan's full board.
    Board,
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
/// the host's `[ui] mode` reaches it.
pub fn switch(backend: &dyn Backend, client: &mut Client, mode: UiMode) {
    client.mode = (mode != backend.ui_config().mode).then_some(mode);
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

/// The plan the view shows: the one the window has selected, else the first open one, else the newest.
pub fn selected_plan(backend: &dyn Backend) -> Option<usize> {
    let plans = backend.plan_list();
    let hints = backend.view_hints();
    if hints.plan_view && hints.selected_plan < plans.len() {
        return Some(hints.selected_plan);
    }
    plans.iter().position(|plan| plan.status == PlanStatus::Open).or_else(|| plans.len().checked_sub(1))
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
    let mut button = egui::Button::new(RichText::new(text).font(font(13.0)).color(color)).corner_radius(8);
    button = if primary {
        button.fill(p.accent).stroke(Stroke::NONE)
    } else {
        button.fill(p.raised).stroke(Stroke::new(1.0, p.border))
    };
    let response = ui.add(touch::sized(button.min_size(vec2(0.0, 28.0)), client.touch));
    flow::record(ui, &format!("button:{text}"), response.rect);
    response
}

/// The switch between this view and the full window, for this client alone.
fn switcher(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let painter = ui.painter();
    painter.rect(rect, rect.height() / 2.0, p.background, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let half = Rect::from_min_size(rect.min, vec2(rect.width() / 2.0, rect.height())).shrink(2.0);
    painter.rect_filled(half, half.height() / 2.0, lerp(p.raised, p.accent, 0.18));
    painter.text(half.center(), Align2::CENTER_CENTER, "Simple", font(12.0), p.foreground);
    let full = Rect::from_min_max(pos2(rect.center().x, rect.top()), rect.max);
    painter.text(full.center(), Align2::CENTER_CENTER, "Full", font(12.0), p.muted);
    let response = ui.interact(full, ui.scope_id().with("simple-full"), Sense::click());
    flow::record(ui, "switch:full", full);
    let response = response.on_hover_text(
        "Every panel at once: the agents' terminals, the board and the diffs. Only this window switches.",
    );
    if response.clicked() {
        switch(backend, client, UiMode::Full);
    }
}

/// The top bar: the name, the plan on screen, what needs the user, the switch to the full window and settings.
/// Narrow, it also picks the plan and the pane.
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
    let brand = painter.layout_no_wrap("ATOMICS".into(), egui::FontId::monospace(13.0), p.accent);
    let w = brand.size().x;
    painter.galley(pos2(x, y - brand.size().y / 2.0), brand, p.accent);
    x += w + 14.0;
    let right = rect.right() - 12.0;
    let settings = backend.management().is_some();
    let switch =
        Rect::from_min_size(pos2(right - 112.0 - if settings { 40.0 } else { 0.0 }, y - 13.0), vec2(112.0, 26.0));
    switcher(ui, switch, backend, client);
    if settings {
        let gear = Rect::from_min_max(pos2(switch.right() + 6.0, rect.top() + 6.0), pos2(right, rect.bottom() - 6.0));
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-settings").max_rect(gear));
        crate::management::settings_button(&mut child, backend, client, true);
    }
    let middle = Rect::from_min_max(pos2(x, rect.top() + 4.0), pos2(switch.left() - 12.0, rect.bottom() - 4.0));
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
    } else if let Some(index) = selected_plan(backend)
        && let Some(plan) = backend.plan_row(index)
    {
        child.label(RichText::new(format!("/  {}", goal(&plan.goal))).font(font(13.5)).color(p.muted));
    }
    crate::topbar::notice(&mut child, backend, client);
}

/// The plans in a menu, where the window has no room for their rail.
fn plan_picker(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let plans = backend.plan_list();
    let selected = selected_plan(backend);
    let title = selected.and_then(|i| plans.get(i)).map_or("Plans".to_string(), |plan| goal(&plan.goal).to_string());
    let title: String = title.chars().take(36).collect();
    ui.menu_button(RichText::new(format!("{title} ▾")).font(font(13.0)), |ui| {
        for (index, plan) in plans.iter().enumerate() {
            if ui.selectable_label(Some(index) == selected, plan.state.label(&plan.goal)).clicked() {
                open_plan(backend, client, index);
                ui.close();
            }
        }
    });
}

fn open_plan(backend: &dyn Backend, client: &mut Client, index: usize) {
    let open = backend.plan_row(index).is_some_and(|plan| plan.status == PlanStatus::Open);
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
    let ui = &mut child;
    let new = egui::Button::new(RichText::new("+   New plan").font(font(13.5)).color(p.foreground))
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(10)
        .min_size(vec2(inner.width(), 34.0));
    let response = ui.add(new).on_hover_text("Tell the chief what you want done; it plans the work with you.");
    flow::record(ui, "new-plan", response.rect);
    if response.clicked() {
        client.simple.focus_composer = true;
        client.simple.chief_terminal = false;
        client.simple.pane = Pane::Chief;
    }
    ui.add_space(16.0);
    let plans = backend.plan_list();
    let selected = selected_plan(backend);
    let bottom = 84.0;
    egui::ScrollArea::vertical()
        .id_salt("simple-plan-list")
        .max_height((ui.available_height() - bottom).max(60.0))
        .show(ui, |ui| {
            let open: Vec<usize> = (0..plans.len()).filter(|&i| plans[i].status == PlanStatus::Open).rev().collect();
            let earlier: Vec<usize> = (0..plans.len()).filter(|&i| plans[i].status != PlanStatus::Open).rev().collect();
            for (heading, list) in [("PLANS", &open), ("EARLIER", &earlier)] {
                if list.is_empty() {
                    continue;
                }
                ui.label(RichText::new(heading).font(font(11.0)).color(p.quiet));
                ui.add_space(4.0);
                for &index in list.iter().take(40) {
                    plan_item(ui, backend, client, index, Some(index) == selected);
                }
                ui.add_space(10.0);
            }
            if plans.is_empty() {
                ui.label(
                    RichText::new("No plans yet. Tell the chief what you want done.").font(font(12.5)).color(p.muted),
                );
            }
        });
    let links = Rect::from_min_max(pos2(inner.left(), inner.bottom() - 70.0), inner.max);
    ui.painter().hline(links.x_range(), links.top() - 4.0, Stroke::new(1.0, p.separator));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-links").max_rect(links));
    let terminals = selected
        .and_then(|i| backend.board(i))
        .map_or(0, |board| board.plan.tasks.iter().filter(|t| t.session.is_some()).count());
    for (label, count, work) in
        [("Agent terminals", terminals, Work::Terminal(String::new())), ("Full board", 0, Work::Board)]
    {
        let text = if count > 0 { format!("{label}  ·  {count}") } else { label.to_string() };
        let button =
            egui::Button::new(RichText::new(format!("{text}   ›")).font(font(12.5)).color(p.muted)).frame(false);
        let response = child.add(touch::sized(button, client.touch));
        flow::record(&child, &format!("link:{label}"), response.rect);
        if response.clicked() {
            client.simple.work = work;
            client.simple.pane = Pane::Work;
        }
    }
}

/// A plan's goal on one line, or a word for a plan without one yet.
fn goal(text: &str) -> &str {
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("Untitled plan")
}

/// One plan in the rail: its state's mark, its goal and how it stands.
fn plan_item(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, index: usize, selected: bool) {
    let Some(plan) = backend.plan_row(index) else { return };
    let p = client.palette.clone();
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::click());
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
    let goal = goal(&plan.goal);
    let width = rect.width() - 40.0;
    let goal = flow::truncated(painter, goal, font(13.5), if closed { p.muted } else { p.foreground }, width);
    painter.galley(pos2(rect.left() + 32.0, rect.top() + 17.0 - goal.size().y / 2.0), goal, p.foreground);
    let (detail, color) = if plan.awaits_approval() {
        ("waits for your approval".to_string(), p.warning)
    } else if plan.state == PlanState::NeedsYou {
        ("needs you".to_string(), p.warning)
    } else if closed {
        let word = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
        let date = backend.plan_date(index);
        (if date.is_empty() { word.to_string() } else { format!("{word} · {date}") }, p.quiet)
    } else {
        let running = match plan.state {
            PlanState::Running(n) => format!("{n} running · "),
            _ => String::new(),
        };
        (format!("{running}{} of {} done", plan.done, plan.total), p.quiet)
    };
    let detail = flow::truncated(painter, &detail, font(11.5), color, width);
    painter.galley(pos2(rect.left() + 32.0, rect.top() + 35.0 - detail.size().y / 2.0), detail, color);
    if response.clicked() {
        open_plan(backend, client, index);
    }
}

/// One part of the chief's conversation, as the transcript draws it.
#[derive(Debug, Clone, PartialEq)]
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
    let conversation = trace.entries.last().map(|entry| entry.session_id.clone());
    trace
        .entries
        .iter()
        .filter(|entry| Some(&entry.session_id) == conversation.as_ref())
        .filter_map(|entry| match &entry.turn {
            Some(turn) => Some(match turn.role {
                TurnRole::User if turn.text.starts_with("ATOMICS ") => Said::Atomics(turn.text.clone()),
                TurnRole::User => Said::User(turn.text.clone()),
                TurnRole::Chief => Said::Chief(turn.text.clone()),
            }),
            None if entry.op == TraceOp::Plan
                && entry.tool == "atomics_plan_set"
                && entry.state == atomics_core::trace::TraceState::Ok =>
            {
                let plan = entry.plan.as_ref();
                Some(Said::Planned(
                    plan.and_then(|p| p.id.clone()).or(entry.plan_id.clone()),
                    plan.map_or(0, |p| p.tasks.len() + entry.omitted),
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

/// The chief's conversation and the box to write to it; one click shows its terminal instead. A host that keeps
/// no conversation (an older one) shows the terminal.
pub fn chat(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.background);
    let gutter = if touch::stacks(client.touch) { 16.0 } else { 24.0 };
    let inner = rect.shrink2(vec2(gutter, 0.0));
    let chief = chief_session(backend);
    // The header: who the chief is and how it stands, and its terminal one click away.
    let header = Rect::from_min_size(pos2(inner.left(), rect.top() + 12.0), vec2(inner.width(), 34.0));
    let painter = ui.painter().clone();
    let c = pos2(header.left() + 14.0, header.center().y);
    painter.circle(c, 14.0, lerp(p.background, p.accent, 0.12), Stroke::new(1.2, p.accent));
    crate::chief::orbit_icon(&painter, Rect::from_center_size(c, vec2(18.0, 18.0)), p.accent, p.foreground);
    painter.text(pos2(c.x + 24.0, c.y - 7.0), Align2::LEFT_CENTER, "Chief", font(15.0), p.foreground);
    let status = chief
        .as_ref()
        .map_or("not started".to_string(), |(_, s)| s.status_label().split(':').next().unwrap_or_default().to_string());
    let live = chief_live(backend);
    let dot = match live {
        Live::Working => p.accent,
        Live::Waiting => p.warning,
        _ => p.quiet,
    };
    painter.circle_filled(pos2(c.x + 27.5, c.y + 9.0), 3.0, dot);
    painter.text(pos2(c.x + 35.0, c.y + 9.0), Align2::LEFT_CENTER, status, font(11.5), p.quiet);
    let has_chat = backend.chat();
    if chief.is_some() && has_chat {
        let label = if client.simple.chief_terminal { "‹  Conversation" } else { "Terminal  ›" };
        let toggle = Rect::from_min_size(pos2(header.right() - 118.0, header.top() + 4.0), vec2(118.0, 26.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("simple-chief-toggle")
                .max_rect(toggle)
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
        );
        if pill(&mut child, label, false, client).clicked() {
            client.simple.chief_terminal = !client.simple.chief_terminal;
        }
    }
    let line = header.bottom() + 10.0;
    painter.hline(inner.x_range(), line, Stroke::new(1.0, p.separator));
    let body = Rect::from_min_max(pos2(rect.left(), line + 1.0), rect.max);
    let Some((index, session)) = chief else {
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-no-chief").max_rect(body.shrink(gutter)));
        child.add_space(40.0);
        child.label(
            RichText::new("The chief plans the work with you and runs the agents that do it.")
                .font(font(15.0))
                .color(p.foreground),
        );
        child.add_space(8.0);
        child.label(RichText::new("Start it, then tell it what you want done.").font(font(13.0)).color(p.muted));
        child.add_space(16.0);
        if backend.available(&Command::ToggleChief) && pill(&mut child, "Start the chief", true, client).clicked() {
            client.send(Command::ToggleChief);
        }
        return;
    };
    if client.simple.chief_terminal || !has_chat || session.outside {
        chief_terminal(ui, body.shrink2(vec2(6.0, 4.0)), index, &session, backend, client, terminals);
        return;
    }
    // The chief's own permission requests wait here too, over its conversation, as in its window.
    let body = approvals::banner(ui, body, index, backend, client);
    let composer_h = 58.0;
    let composer = Rect::from_min_max(
        pos2(inner.left(), body.bottom() - composer_h - 14.0),
        pos2(inner.right(), body.bottom() - 14.0),
    );
    let transcript = Rect::from_min_max(pos2(inner.left(), body.top()), pos2(inner.right(), composer.top() - 8.0));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-transcript").max_rect(transcript));
    child.set_clip_rect(transcript);
    let said = conversation(&backend.trace());
    egui::ScrollArea::vertical().id_salt("simple-transcript-scroll").stick_to_bottom(true).auto_shrink([false, false]).show(&mut child, |ui| {
        ui.add_space(14.0);
        if said.is_empty() {
            ui.label(RichText::new("Tell the chief what you want done. It plans the work with you, starts agents for it, and keeps you posted here.").font(font(14.0)).color(p.muted));
        }
        for item in &said {
            transcript_item(ui, item, backend, client);
        }
        ui.add_space(8.0);
    });
    composer_box(ui, composer, &session, backend, client);
}

fn transcript_item(ui: &mut egui::Ui, item: &Said, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let width = ui.available_width();
    match item {
        Said::User(text) => {
            // On the right, its text left-aligned within.
            let max = (width * 0.82).min(520.0);
            let galley = ui.painter().layout(text.clone(), font(14.0), p.foreground, max - 28.0);
            let size = galley.size() + vec2(28.0, 20.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, size.y), Sense::hover());
            let bubble = Rect::from_min_size(pos2(rect.right() - size.x, rect.top()), size);
            ui.painter().rect_filled(bubble, 16.0, lerp(p.background, p.foreground, 0.075));
            ui.painter().galley(bubble.min + vec2(14.0, 10.0), galley, p.foreground);
            ui.add_space(14.0);
        }
        Said::Atomics(text) => {
            let line = text.lines().next().unwrap_or_default();
            ui.add(egui::Label::new(RichText::new(line).font(font(11.5)).color(p.quiet)).truncate());
            ui.add_space(10.0);
        }
        Said::Chief(text) => {
            ui.add(egui::Label::new(RichText::new(text).font(font(14.0)).color(p.foreground)).wrap());
            ui.add_space(14.0);
        }
        Said::Planned(plan, count) => {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                ui.painter().circle_stroke(rect.center(), 5.5, Stroke::new(1.2, p.success));
                flow::tick(ui.painter(), rect.center(), 5.0, Stroke::new(1.3, p.success));
                ui.label(
                    RichText::new(format!("Planned {count} task{}", if *count == 1 { "" } else { "s" }))
                        .font(font(12.5))
                        .color(p.muted),
                );
            });
            ui.add_space(6.0);
            let index = plan.as_ref().and_then(|id| backend.plan_list().iter().position(|row| &row.id == id));
            if let Some(index) = index {
                // A plan waiting for approval is a decision: its tasks with who would run them, and Approve or Change.
                let proposal = backend.plan_row(index).is_some_and(|row| row.awaits_approval());
                task_list(ui, index, backend, client, proposal);
                if proposal {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if backend.available(&Command::ApprovePlan(index))
                            && pill(ui, "Approve plan", true, client).clicked()
                        {
                            client.send(Command::ApprovePlan(index));
                        }
                        if pill(ui, "Change", false, client)
                            .on_hover_text("Tell the chief what to change; nothing starts until you approve")
                            .clicked()
                        {
                            client.simple.composer = "About the plan: ".into();
                            client.simple.focus_composer = true;
                        }
                    });
                }
            }
            ui.add_space(14.0);
        }
    }
}

/// The tasks of a plan the chief made, each with its live state, as one quiet card: a click shows it in the flow.
fn task_list(ui: &mut egui::Ui, index: usize, backend: &dyn Backend, client: &mut Client, proposal: bool) {
    let Some(board) = backend.board(index) else { return };
    let p = client.palette.clone();
    let sessions = backend.session_list();
    egui::Frame::new().fill(p.panel).corner_radius(12).inner_margin(egui::Margin::symmetric(14, 4)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (k, task) in board.plan.tasks.iter().enumerate().take(12) {
            let session = task.session.as_deref().and_then(|id| sessions.iter().find(|s| s.id == id));
            let (word, color) = state_word(task.status, flow::live(task.status, session), &p);
            // Proposed, a task says who would run it: its agent and model.
            let model = board.models.get(&task.id).cloned().unwrap_or_else(|| task.agent.clone());
            let (word, color) = if proposal { (model.as_str(), p.muted) } else { (word, color) };
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::click());
            let painter = ui.painter();
            if k > 0 {
                painter.hline(rect.x_range(), rect.top(), Stroke::new(1.0, lerp(p.panel, p.border, 0.6)));
            }
            painter.circle_filled(pos2(rect.left() + 3.0, rect.center().y), 3.0, color);
            let title = flow::truncated(painter, &task.title, font(13.0), p.foreground, rect.width() - 150.0);
            painter.galley(pos2(rect.left() + 16.0, rect.center().y - title.size().y / 2.0), title, p.foreground);
            painter.text(
                pos2(rect.right(), rect.center().y),
                Align2::RIGHT_CENTER,
                word,
                font(12.0),
                lerp(p.muted, color, 0.55),
            );
            if response.clicked() {
                if Some(index) != selected_plan(backend) {
                    open_plan(backend, client, index);
                }
                client.simple.flow.selected = std::iter::once(task.id.clone()).collect();
                client.simple.work = Work::Flow;
                client.simple.pane = Pane::Work;
            }
        }
    });
}

/// The box to write to the chief: Enter sends, Shift+Enter starts a new line.
fn composer_box(ui: &mut egui::Ui, rect: Rect, chief: &SessionRow, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.painter().rect(rect, 16.0, p.raised, Stroke::new(1.0, p.border), StrokeKind::Inside);
    let send = Rect::from_center_size(pos2(rect.right() - 26.0, rect.center().y), vec2(32.0, 32.0));
    let field =
        Rect::from_min_max(pos2(rect.left() + 14.0, rect.top() + 8.0), pos2(send.left() - 10.0, rect.bottom() - 8.0));
    let can_send = backend.available(&Command::Steer { sessions: Vec::new(), text: String::new(), now: false })
        && chief.can_receive;
    let id = ui.scope_id().with("simple-composer");
    let focused = ui.memory(|m| m.has_focus(id));
    let mut sent =
        focused && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-composer-field").max_rect(field));
    let edit = egui::TextEdit::multiline(&mut client.simple.composer)
        .id(id)
        .frame(egui::Frame::NONE)
        .desired_rows(1)
        .desired_width(field.width())
        .font(font(14.0))
        .hint_text(
            RichText::new(if can_send { "Message the chief…" } else { "This window can only watch" })
                .font(font(14.0))
                .color(p.quiet),
        );
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
    painter.add(egui::Shape::line(
        vec![pos2(c.x - 5.0, c.y - 1.0), pos2(c.x, c.y - 6.0), pos2(c.x + 5.0, c.y - 1.0)],
        Stroke::new(2.0, ink),
    ));
    sent |= button.on_hover_text("Send to the chief (Enter)").clicked();
    if sent && ready {
        let text = client.simple.composer.trim().to_string();
        client.simple.composer.clear();
        client.send(Command::Steer { sessions: vec![chief.id.clone()], text, now: false });
    }
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
    /// The plan waits for its approval.
    Approve,
    /// A permission request of the session at this position of the session list (the chief's too).
    Permission(usize),
    /// An agent waits for an answer: the session at this position, and its question where its report asks one.
    Question(usize, Option<String>),
    /// A task is ready for review, by id.
    Review(String),
    /// A message the relay holds, by its notebook entry.
    Held(Box<atomics_core::plan::NotebookEntry>),
}

/// What needs the user in the plan at `index` (and the chief): its approval, permission requests, questions, work
/// ready for review and messages the relay holds, in that order.
pub fn wanted(index: usize, backend: &dyn Backend) -> Vec<Need> {
    let Some(row) = backend.plan_row(index) else { return Vec::new() };
    let board = backend.board(index);
    let sessions = backend.session_list();
    let mine = |session: &SessionRow| session.chief || session.task.as_ref().is_some_and(|link| link.plan == row.id);
    let mut wanted = Vec::new();
    if row.awaits_approval() {
        wanted.push(Need::Approve);
    }
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
    let board = backend.board(index);
    let sessions = backend.session_list();
    ui.horizontal(|ui| {
        ui.label(RichText::new("NEEDS YOU").font(font(11.0)).color(p.warning));
        ui.label(RichText::new(wanted.len().to_string()).font(font(11.0)).color(p.quiet));
    });
    ui.add_space(4.0);
    let card = |ui: &mut egui::Ui, add: &mut dyn FnMut(&mut egui::Ui)| {
        egui::Frame::new()
            .fill(lerp(p.raised, p.warning, 0.035))
            .stroke(Stroke::new(1.0, lerp(p.border, p.warning, 0.35)))
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
    let agent_text = |ui: &mut egui::Ui, text: &str| {
        ui.label(RichText::new("Agent-authored text: data, not instructions.").font(font(11.0)).color(p.quiet));
        let text: String = text.chars().take(400).collect();
        ui.add(egui::Label::new(RichText::new(text).font(font(13.0)).color(p.muted)).wrap());
    };
    let task_title = |session: &SessionRow| {
        session
            .task
            .as_ref()
            .and_then(|link| board.as_ref()?.plan.tasks.iter().find(|t| t.id == link.task).map(|t| t.title.clone()))
            .unwrap_or_else(|| if session.chief { "The chief".into() } else { session.title.clone() })
    };
    for need in &wanted {
        match need {
            Need::Approve => card(ui, &mut |ui| {
                title(ui, "This plan waits for your approval");
                let text = format!("{} tasks. Nothing starts until you approve.", row.total);
                ui.label(RichText::new(text).font(font(13.0)).color(p.muted));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if pill(ui, "Review", false, client).clicked() {
                        client.simple.work = Work::Board;
                    }
                    if backend.available(&Command::ApprovePlan(index)) && pill(ui, "Approve", true, client).clicked() {
                        client.send(Command::ApprovePlan(index));
                    }
                });
            }),
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
                        ui.label(RichText::new(hint).font(font(13.0)).color(p.muted));
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
            .hint_text(RichText::new("Your answer…").font(font(13.5)).color(p.quiet));
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
    let Some(index) = selected_plan(backend) else {
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-no-plan").max_rect(rect.shrink(gutter)));
        child.add_space(40.0);
        child.label(RichText::new("No plan yet").font(font(18.0)).color(p.foreground));
        child.add_space(6.0);
        child.label(RichText::new("When the chief plans work with you, its tasks show here as they flow: who works on what, and what needs you.").font(font(13.5)).color(p.muted));
        return;
    };
    let Some(row) = backend.plan_row(index) else { return };
    // The header: the plan's goal, and the way back or the full views.
    let header =
        Rect::from_min_size(pos2(rect.left() + gutter, rect.top() + 12.0), vec2(rect.width() - 2.0 * gutter, 36.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("simple-work-header")
            .max_rect(header)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(header);
    if client.simple.work != Work::Flow {
        if pill(&mut child, "‹  Work", false, client).clicked() {
            client.simple.work = Work::Flow;
        }
        child.add_space(8.0);
    }
    let size = if touch::stacks(client.touch) { 17.0 } else { 20.0 };
    child.add(egui::Label::new(RichText::new(goal(&row.goal)).font(font(size)).color(p.foreground)).truncate());
    if client.simple.work == Work::Flow {
        child.add_space(12.0);
        plan_previews(&mut child, index, backend, client);
    }
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
    let body = Rect::from_min_max(pos2(rect.left(), header.bottom() + 8.0), rect.max);
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
        Work::Board => crate::board::draw(ui, body, index, backend, client),
    }
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
    for row in &mut rows {
        row.previews = row_previews(row.session.as_deref(), backend);
        row.can_preview = backend.can_preview();
        row.messaging = client.simple.flow.messaging.as_deref() == Some(row.id.as_str());
    }
    client.simple.shown = rows.iter().map(|row| (row.id.clone(), row.status)).collect();
    let flow = Flow { rows, chief: chief_live(backend), now: board.now };
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt("simple-flow").max_rect(rect));
    child.set_clip_rect(rect);
    egui::ScrollArea::vertical().id_salt("simple-flow-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        if !flow.rows.is_empty()
            && let Some(status) = flow::summary(ui, &flow.rows, &p)
        {
            client.simple.flow.selected =
                flow.rows.iter().filter(|row| row.status == status).map(|row| row.id.clone()).collect();
        }
        ui.add_space(6.0);
        needs(ui, index, backend, client);
        ui.label(RichText::new("THE WORK").font(font(11.0)).color(p.quiet));
        ui.add_space(8.0);
        if flow.rows.is_empty() {
            ui.label(RichText::new("The plan has no tasks yet.").font(font(13.0)).color(p.muted));
            return;
        }
        let traffic = std::mem::take(&mut client.simple.traffic);
        let response = flow::show(ui, &flow, &mut client.simple.flow, &traffic, now, reduce, &p);
        client.simple.traffic = traffic;
        if let Some(id) = response.clicked {
            let state = &mut client.simple.flow;
            if !state.expanded.remove(&id) {
                state.expanded.insert(id.clone());
            }
            state.selected = std::iter::once(id).collect();
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
        client.send(Command::OpenPreview { session: Some(session.to_string()), preview });
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

/// The plan's previews in one place, for the work's header: every session of its tasks with one.
fn plan_previews(ui: &mut egui::Ui, index: usize, backend: &dyn Backend, client: &mut Client) {
    let Some(board) = backend.board(index) else { return };
    for task in &board.plan.tasks {
        let Some(session) = &task.session else { continue };
        for preview in backend.previews(Some(session)) {
            let p = client.palette.clone();
            let active = backend.preview_active(preview.port);
            let color = if !backend.can_preview() {
                p.quiet
            } else if active {
                p.success
            } else {
                p.accent
            };
            let mark = if active { "■" } else { "▶" };
            let text = RichText::new(format!("{mark} {} · {}", task.id, preview.title())).font(font(12.0)).color(color);
            let button = egui::Button::new(text)
                .corner_radius(12)
                .fill(p.raised)
                .stroke(Stroke::new(1.0, lerp(p.border, color, 0.5)));
            let response = ui.add_enabled(backend.can_preview(), button);
            flow::record(ui, &format!("plan-preview:{}:{}", task.id, preview.port), response.rect);
            let response =
                response.on_disabled_hover_text("This window cannot open previews: open them from ATOMICS on the Mac.");
            if response.clicked() {
                preview_toggle(session, preview, backend, client);
            }
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
    crate::theme::simple_family(ui.ctx());
    ui.painter().rect_filled(screen, 0.0, p.background);
    let rail = screen.width() >= RAIL_MIN;
    let split = screen.width() >= SPLIT_MIN;
    top_bar(ui, screen.with_max_y(screen.top() + TOP), backend, client, !rail, !split);
    let body = screen.with_min_y(screen.top() + TOP);
    let mut rest = body;
    if rail {
        let rect = body.with_max_x(body.left() + RAIL);
        plans(ui, rect, backend, client);
        ui.painter().vline(rect.right(), body.y_range(), Stroke::new(1.0, p.separator));
        rest.min.x = rect.right() + 1.0;
    }
    if !split {
        match client.simple.pane {
            Pane::Work => work(ui, rest, backend, client, terminals),
            _ => chat(ui, rest, backend, client, terminals),
        }
        return;
    }
    let chat_width = (rest.width() * 0.42).clamp(340.0, 620.0);
    let chat_rect = rest.with_max_x(rest.left() + chat_width);
    chat(ui, chat_rect, backend, client, terminals);
    ui.painter().vline(chat_rect.right(), body.y_range(), Stroke::new(1.0, p.separator));
    work(ui, rest.with_min_x(chat_rect.right() + 1.0), backend, client, terminals);
}

/// A phone's pane in simple mode, in `rect`; the platform draws the bars around it ([`pane_bar`]).
pub fn phone(
    ui: &mut egui::Ui,
    rect: Rect,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
) {
    crate::theme::simple_family(ui.ctx());
    match client.simple.pane {
        Pane::Chief => chat(ui, rect, backend, client, terminals),
        Pane::Work => work(ui, rect, backend, client, terminals),
        Pane::Plans => plans(ui, rect, backend, client),
    }
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
        painter.text(pos2(cell.center().x, cell.top() + 38.0), Align2::CENTER_CENTER, label, font(11.5), color);
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
    let p = client.palette.clone();
    let painter = ui.painter().clone();
    let y = rect.center().y;
    let mark = Rect::from_center_size(pos2(rect.left() + 9.0, y), vec2(18.0, 18.0));
    crate::chief::orbit_icon(&painter, mark, p.accent, p.foreground);
    painter.text(pos2(rect.left() + 26.0, y), Align2::LEFT_CENTER, "ATOMICS", egui::FontId::monospace(14.0), p.accent);
    let switch = Rect::from_min_size(pos2(rect.right() - 104.0, y - 15.0), vec2(104.0, 30.0));
    switcher(ui, switch, backend, client);
}

#[cfg(test)]
mod tests;
