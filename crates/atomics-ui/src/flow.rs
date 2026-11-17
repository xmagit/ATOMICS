//! The flow of a plan's work: the chief as one node, fanning out in curves to a row per task. Each row is a card
//! with its agent's mark, its title and its track: where it started, the live agent's marker, a square per
//! acceptance check (filled as the agent's last report passes it, dotted between) and a larger square at the end,
//! which fills when the task is accepted. A row opens to show the agent's own threads (its subagents) and its
//! checks. What passes between the chief and the agents travels along the curves ([`crate::traffic`]).
//!
//! The widget reads only a [`Flow`], which [`rows`] makes from a plan's board; its geometry ([`geometry`],
//! [`track`], [`summary_parts`]) is plain maths, so it is tested without drawing. On a wide panel the chief's node
//! sits left of the rows; on a narrow one it sits on top of a spine down their left edge and each row's track goes
//! under its title.

use std::collections::HashSet;

use atomics_core::plan::{Report, TaskStatus};
use atomics_core::session::AgentStatus;
use egui::epaint::CubicBezierShape;
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::theme::{Palette, lerp};
use crate::traffic::{self, End as Way, Held, Kind, Traffic};
use crate::{BoardView, SessionRow};

/// What the agent on a task does now, as its marker shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// No session: not started, or finished and closed.
    None,
    Working,
    /// A permission request or a question waits for the user.
    Waiting,
    /// Between turns.
    Idle,
    /// The task is blocked.
    Blocked,
}

/// A check of a task's definition of done, as its last report stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Pending,
    Passed,
    Failed,
}

/// The end of a task's track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Open,
    /// The agent reported done; the task waits to be accepted.
    Review,
    Done,
    Integrated,
}

/// One of an agent's own threads: a subagent it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    /// What kind of agent it is, as the agent names it (`Explore`, `general-purpose`, `default`).
    pub kind: String,
    pub running: bool,
    /// Tool calls it finished.
    pub steps: u32,
    /// The last tool it used, by name.
    pub tool: Option<String>,
}

/// One task as its row shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: String,
    pub title: String,
    /// The agent and model, such as "Claude Opus 5.5".
    pub who: String,
    pub status: TaskStatus,
    /// Its session, by id, while it has one.
    pub session: Option<String>,
    /// The task has been dispatched.
    pub started: bool,
    pub live: Live,
    /// The checks of its definition of done, in the plan's order.
    pub checks: Vec<(String, Mark)>,
    pub end: End,
    /// Lines added and removed on its branch.
    pub changes: Option<(usize, usize)>,
    /// A short word on where it stands when the track does not say: "after t1", "needs you", "blocked".
    pub note: Option<String>,
    /// Its agent's subagents, oldest first.
    pub threads: Vec<Thread>,
    /// Tool calls its agent finished in its own thread.
    pub steps: u32,
    /// The first line of its last report: the agent's own words.
    pub report: Option<String>,
    /// The web servers its session offers, and whether this client can open them here.
    pub previews: Vec<RowPreview>,
    pub can_preview: bool,
    /// The box to message its agent is open.
    pub messaging: bool,
}

/// One of a session's previews, as its row offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPreview {
    pub title: String,
    pub port: u16,
    /// It is open now.
    pub active: bool,
}

/// A plan's work as the flow draws it.
#[derive(Debug, Clone, PartialEq)]
pub struct Flow {
    pub rows: Vec<Row>,
    /// What the chief does, for its node.
    pub chief: Live,
    /// The host's clock, in seconds: how long ago traffic passed.
    pub now: u64,
}

/// What one client holds of a flow: the rows open, picked and under the pointer, by task id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowState {
    pub expanded: HashSet<String>,
    pub selected: HashSet<String>,
    pub hovered: Option<String>,
    /// The row whose message box is open, by task id, and what it holds.
    pub messaging: Option<String>,
    pub message: String,
}

/// What a frame's pointer did to the flow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlowResponse {
    /// A row's card was clicked, by task id.
    pub clicked: Option<String>,
    /// A row's action was clicked: the task, and which.
    pub action: Option<(String, Action)>,
}

/// What a row offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Diff,
    Terminal,
    /// Open the box to message its agent, or close it.
    Message,
    /// Send what the box holds.
    Send,
    /// Open the preview at this position of the row's, or stop it while it is open.
    Preview(usize),
    /// End its agent's session, once the user confirms.
    Stop,
    /// Show the task in the full window.
    OpenInFull,
    /// Open the row in place, or close it: its chevron.
    Expand,
}

/// The sizes the flow is drawn at.
pub const ROW: f32 = 56.0;
pub const NARROW_ROW: f32 = 84.0;
pub const GAP: f32 = 10.0;
/// The room left of the rows for the chief's node and the curves on a wide panel.
pub const FAN: f32 = 108.0;
/// The room left of the rows for the spine on a narrow one.
pub const SPINE: f32 = 30.0;
pub const NODE: f32 = 17.0;
const THREAD: f32 = 30.0;
const CHECK_LINE: f32 = 20.0;
const ACTIONS: f32 = 40.0;
/// Below this width the flow folds: the node goes on top and the tracks under the titles.
pub const NARROW: f32 = 560.0;

/// UI text in the simple view: its own face where the platform has one (the web bundles Inter), else the
/// proportional one, whatever a theme's monospace UI.
pub fn font(size: f32) -> FontId {
    FontId::new(size, crate::theme::simple_font_family())
}

/// What the agent on a session does, for its row's marker.
pub fn live(status: TaskStatus, session: Option<&SessionRow>) -> Live {
    if status == TaskStatus::Blocked {
        return Live::Blocked;
    }
    match session {
        None => Live::None,
        Some(session) if session.needs_user() => Live::Waiting,
        Some(session) => match session.status {
            AgentStatus::Working => Live::Working,
            AgentStatus::Exited => Live::None,
            _ => Live::Idle,
        },
    }
}

/// How a report stands on each check: by position, an exit of 0 passes, another fails, and a check it did not
/// run, or a report not yet in, leaves the square empty.
pub fn marks(acceptance: &[String], report: Option<&Report>) -> Vec<(String, Mark)> {
    let checks = report.map_or(&[][..], |report| report.checks.as_slice());
    let mark = |k: usize| match checks.get(k).and_then(|check| check.exit) {
        Some(0) => Mark::Passed,
        Some(_) => Mark::Failed,
        None => Mark::Pending,
    };
    if acceptance.is_empty() {
        // A plan from an older host names no checks; the report's own are the track's squares.
        return checks.iter().enumerate().map(|(k, check)| (check.check.clone(), mark(k))).collect();
    }
    acceptance.iter().enumerate().map(|(k, text)| (text.clone(), mark(k))).collect()
}

/// The rows of a plan's board: each task with its session's state, its checks as its last report stands (the one
/// before while a new one travels to the chief) and its agent's threads.
pub fn rows(board: &BoardView, sessions: &[SessionRow], traffic: &Traffic, now: f64, reduce_motion: bool) -> Vec<Row> {
    let plan = &board.plan;
    plan.tasks
        .iter()
        .map(|task| {
            let session = task.session.as_deref().and_then(|id| sessions.iter().find(|s| s.id == id));
            let live = live(task.status, session);
            let report = traffic.shown_report(&task.id, task.report.as_ref(), now, reduce_motion);
            let end = match task.status {
                TaskStatus::Done if task.integrated.is_some() => End::Integrated,
                TaskStatus::Done => End::Done,
                TaskStatus::Review => End::Review,
                _ => End::Open,
            };
            let waiting_on = task.depends_on.iter().find_map(|id| {
                let dep = plan.tasks.iter().find(|t| &t.id == id)?;
                (dep.status != TaskStatus::Done).then(|| dep.title.clone())
            });
            let note = match (task.status, live) {
                (TaskStatus::Planned, _) => waiting_on.map(|title| format!("after {title}")),
                (TaskStatus::Blocked, _) => Some("blocked".into()),
                (TaskStatus::Review, _) => Some("ready for review".into()),
                (_, Live::Waiting) if session.is_some_and(|s| s.pending_approvals > 0) => Some("needs you".into()),
                (_, Live::Waiting) => Some("asks you".into()),
                _ => None,
            };
            let activity = session.and_then(|s| s.activity.clone()).unwrap_or_default();
            let who = board.models.get(&task.id).cloned().unwrap_or_else(|| {
                let mut chars = task.agent.chars();
                chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
            });
            Row {
                id: task.id.clone(),
                title: task.title.clone(),
                who,
                status: task.status,
                session: session.map(|s| s.id.clone()),
                started: task.status != TaskStatus::Planned || task.session.is_some(),
                live,
                checks: marks(&task.acceptance, report),
                end,
                changes: (task.additions + task.deletions > 0).then_some((task.additions, task.deletions)),
                note,
                threads: activity
                    .threads
                    .iter()
                    .map(|t| Thread { kind: t.kind.clone(), running: t.running, steps: t.steps, tool: t.tool.clone() })
                    .collect(),
                steps: activity.steps,
                report: report.and_then(|r| r.summary.lines().next()).map(str::to_string),
                previews: Vec::new(),
                can_preview: false,
                messaging: false,
            }
        })
        .collect()
}

/// Where the chief's node and every row go.
#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    pub node: Pos2,
    pub rows: Vec<Rect>,
    /// The whole flow's height, for a scroll area.
    pub height: f32,
    pub narrow: bool,
}

impl Geometry {
    /// Where a row's curve meets it: its left edge, level with its first line.
    pub fn anchor(&self, row: usize) -> Pos2 {
        let rect = self.rows[row];
        pos2(rect.left(), rect.top() + if self.narrow { 22.0 } else { ROW / 2.0 })
    }
}

/// How much an open row adds below its card's first line.
pub fn opened(row: &Row) -> f32 {
    let threads = row.threads.len() as f32 * THREAD;
    let checks = row.checks.len() as f32 * CHECK_LINE;
    let report = if row.report.is_some() { CHECK_LINE + 4.0 } else { 0.0 };
    let message = if row.messaging { 38.0 } else { 0.0 };
    8.0 + threads + if threads > 0.0 { 8.0 } else { 0.0 } + checks + report + ACTIONS + message
}

/// Lays the flow out from `origin`, `width` wide: one row under the other, `GAP` apart, open ones taller; the
/// chief's node left of them, level with their middle, or on top of the spine when `width` is under [`NARROW`].
pub fn geometry(origin: Pos2, width: f32, rows: &[Row], open: &dyn Fn(&str) -> bool) -> Geometry {
    let narrow = width < NARROW;
    let left = origin.x + if narrow { SPINE } else { FAN };
    let base = if narrow { NARROW_ROW } else { ROW };
    let mut y = origin.y + if narrow { NODE * 2.0 + 6.0 } else { 0.0 };
    let mut rects = Vec::with_capacity(rows.len());
    for row in rows {
        let height = base + if open(&row.id) { opened(row) } else { 0.0 };
        rects.push(Rect::from_min_size(pos2(left, y), vec2((origin.x + width - left).max(0.0), height)));
        y += height + GAP;
    }
    let bottom = if rects.is_empty() { origin.y + if narrow { NODE * 2.0 } else { base } } else { y - GAP };
    let node = if narrow {
        pos2(origin.x + NODE, origin.y + NODE)
    } else {
        // Level with the middle of the first lines of the first and last rows.
        let first = rects.first().map_or(origin.y + base / 2.0, |r| r.top() + base / 2.0);
        let last = rects.last().map_or(first, |r| r.top() + base / 2.0);
        pos2(origin.x + NODE + 10.0, (first + last) / 2.0)
    };
    Geometry { node, rows: rects, height: bottom - origin.y, narrow }
}

/// Where a track's parts go between `start` and `end`, the start point's and the end square's centres: a square
/// per check, evenly spaced between them.
pub fn track(start: f32, end: f32, checks: usize) -> Vec<f32> {
    let step = (end - start) / (checks as f32 + 1.0);
    (1..=checks).map(|k| start + step * k as f32).collect()
}

/// How far along the track the agent is: the checks passed in a row from the start, which the marker sits after.
/// `None` before the task starts and once it ends.
pub fn reached(row: &Row) -> Option<usize> {
    if !row.started || matches!(row.end, End::Done | End::Integrated) {
        return None;
    }
    Some(row.checks.iter().take_while(|(_, mark)| *mark == Mark::Passed).count())
}

/// The order the summary shows the statuses in.
pub const SUMMARY: [TaskStatus; 5] =
    [TaskStatus::Running, TaskStatus::Review, TaskStatus::Blocked, TaskStatus::Planned, TaskStatus::Done];

/// The counts of a plan's tasks by status, in the summary's order.
pub fn counts(rows: &[Row]) -> [(TaskStatus, usize); 5] {
    SUMMARY.map(|status| (status, rows.iter().filter(|row| row.status == status).count()))
}

/// The summary bar's parts, `width` wide from `left`: each status with tasks, sized by its share of them, in the
/// summary's order and touching end to end.
pub fn summary_parts(rows: &[Row], left: f32, width: f32) -> Vec<(TaskStatus, f32, f32)> {
    let total = rows.len().max(1) as f32;
    let mut x = left;
    counts(rows)
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(status, count)| {
            let w = width * count as f32 / total;
            let part = (status, x, x + w);
            x += w;
            part
        })
        .collect()
}

/// The colour a task status is drawn in, in the summary and on a row's mark.
pub fn status_color(status: TaskStatus, p: &Palette) -> Color32 {
    match status {
        TaskStatus::Planned => p.quiet,
        TaskStatus::Running => p.accent,
        TaskStatus::Review => p.info,
        TaskStatus::Blocked => p.error,
        TaskStatus::Done => p.success,
    }
}

pub fn status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Planned => "planned",
        TaskStatus::Running => "running",
        TaskStatus::Review => "review",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
    }
}

/// A faint grid of dots behind the flow.
pub fn dots(painter: &egui::Painter, rect: Rect, p: &Palette) {
    let color = lerp(p.background, p.quiet, 0.28);
    let step = 22.0;
    let mut y = rect.top() + step / 2.0;
    while y < rect.bottom() {
        let mut x = rect.left() + step / 2.0;
        while x < rect.right() {
            painter.circle_filled(pos2(x, y), 0.9, color);
            x += step;
        }
        y += step;
    }
}

/// The summary over the flow: a thin bar split by how many tasks are in each status, and the counts under it.
/// Hovering a part lists its tasks; a click picks them. Returns the status clicked.
pub fn summary(ui: &mut egui::Ui, rows: &[Row], p: &Palette) -> Option<TaskStatus> {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 34.0), Sense::hover());
    let bar = Rect::from_min_size(rect.min, vec2(width, 6.0));
    ui.painter().rect_filled(bar, 3.0, lerp(p.background, p.border, 0.7));
    let mut clicked = None;
    let mut label_x = rect.left();
    for (status, from, to) in summary_parts(rows, bar.left(), width) {
        let part = Rect::from_min_max(pos2(from, bar.top()), pos2(to, bar.bottom()));
        let color = status_color(status, p);
        let count = rows.iter().filter(|row| row.status == status).count();
        let text = format!("{count} {}", status_word(status));
        let galley = ui.painter().layout_no_wrap(text, font(12.0), p.muted);
        let label = Rect::from_min_size(pos2(label_x, bar.bottom() + 6.0), vec2(galley.size().x + 12.0, 20.0));
        let id = ui.scope_id().with(("summary", status_word(status)));
        let response = ui.interact(part.expand2(vec2(0.0, 6.0)).union(label), id, Sense::click());
        let response = response.on_hover_ui(|ui| {
            ui.label(egui::RichText::new(format!("{count} {}", status_word(status))).strong());
            for row in rows.iter().filter(|row| row.status == status) {
                ui.label(format!("{} · {}", row.id, row.title));
            }
        });
        record(ui, &format!("summary:{}", status_word(status)), response.rect);
        let shown = if response.hovered() { lerp(color, p.foreground, 0.25) } else { color };
        ui.painter().rect_filled(part.shrink2(vec2(1.0, 0.0)), 3.0, shown);
        if response.clicked() {
            clicked = Some(status);
        }
        ui.painter().circle_filled(pos2(label_x + 3.5, label.center().y), 3.5, color);
        let w = galley.size().x;
        ui.painter().galley(pos2(label_x + 10.0, label.center().y - galley.size().y / 2.0), galley, p.muted);
        label_x += w + 26.0;
    }
    clicked
}

/// Where a widget was drawn, by name, for the interaction tests.
pub fn record(ui: &egui::Ui, name: &str, rect: Rect) {
    #[cfg(any(test, feature = "test-support"))]
    ui.ctx().data_mut(|data| {
        let id = egui::Id::NULL.with("simple-shown");
        let mut shown: Vec<(String, Rect)> = data.get_temp(id).unwrap_or_default();
        shown.push((name.to_string(), rect));
        data.insert_temp(id, shown);
    });
    let _ = (ui, name, rect);
}

/// Draws the flow across the width the ui has, with its traffic at `now` (this client's clock), and returns
/// what the pointer did.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    flow: &Flow,
    state: &mut FlowState,
    traffic: &Traffic,
    now: f64,
    reduce_motion: bool,
    p: &Palette,
) -> FlowResponse {
    let origin = ui.cursor().min;
    let width = ui.available_width();
    let open = |id: &str| state.expanded.contains(id);
    let mut geometry = geometry(origin, width, &flow.rows, &open);
    ui.allocate_exact_size(vec2(width, geometry.height + 4.0), Sense::hover());
    // On a wide panel the node keeps level with the middle of the rows on screen.
    if !geometry.narrow
        && let (Some(first), Some(last)) = (geometry.rows.first(), geometry.rows.last())
    {
        let clip = ui.clip_rect();
        let top = first.top().max(clip.top()) + ROW / 2.0;
        let bottom = last.bottom().min(clip.bottom()) - ROW / 2.0;
        if bottom > top {
            geometry.node.y = (top + bottom) / 2.0;
        }
    }
    let time = ui.input(|i| i.time);
    let mut response = FlowResponse::default();
    let mut hovered = None;
    // The curves first, under the cards.
    for (k, row) in flow.rows.iter().enumerate() {
        let lit = state.selected.contains(&row.id) || state.hovered.as_deref() == Some(row.id.as_str());
        let color = if lit {
            p.accent
        } else if row.live == Live::Working {
            lerp(p.border, p.accent, 0.45)
        } else {
            lerp(p.background, p.muted, 0.45)
        };
        let points = curve(geometry.node, geometry.anchor(k), geometry.narrow);
        let stroke = Stroke::new(if lit { 1.6 } else { 1.2 }, color);
        ui.painter().add(CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke));
    }
    chief_node(ui.painter(), geometry.node, flow.chief, p, time);
    for (row, rect) in flow.rows.iter().zip(&geometry.rows) {
        let id = ui.scope_id().with(("flow-row", &row.id));
        let card = ui.interact(*rect, id, Sense::click());
        record(ui, &format!("row:{}", row.id), *rect);
        if card.hovered() {
            hovered = Some(row.id.clone());
        }
        if card.clicked() {
            response.clicked = Some(row.id.clone());
        }
        let selected = state.selected.contains(&row.id);
        let lit = selected || card.hovered();
        let open = state.expanded.contains(&row.id);
        let drawn = draw_row(ui, *rect, row, open, lit, selected, geometry.narrow, p, time, &mut state.message);
        if let Some(action) = drawn {
            response.action = Some((row.id.clone(), action));
            // A row's own controls are not a click on its card.
            response.clicked = response.clicked.filter(|id| id != &row.id);
        }
    }
    draw_traffic(ui, &geometry, flow, traffic, now, reduce_motion, p);
    state.hovered = hovered;
    if traffic.moving(now, reduce_motion) {
        ui.ctx().request_repaint();
    } else if traffic.active(now) {
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    }
    if !reduce_motion && (flow.rows.iter().any(|row| row.live == Live::Working) || flow.chief == Live::Working) {
        ui.ctx().request_repaint_after(crate::status::PULSE_STEP);
    }
    response
}

/// The control points of a curve from the chief's node to a row: across and into it on a wide panel, down the
/// spine and into it on a narrow one.
fn curve(from: Pos2, to: Pos2, narrow: bool) -> [Pos2; 4] {
    if narrow {
        let down = pos2(from.x, from.y + NODE);
        let bend = (to.y - down.y).min(26.0);
        [down, pos2(from.x, to.y - bend * 0.2), pos2(from.x + (to.x - from.x) * 0.1, to.y), to]
    } else {
        let start = pos2(from.x + NODE, from.y);
        let reach = (to.x - start.x) * 0.55;
        [start, pos2(start.x + reach, start.y), pos2(to.x - reach, to.y), to]
    }
}

/// The point at `t` of a cubic Bézier.
pub fn bezier(points: [Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let [a, b, c, d] = points.map(|p| p.to_vec2());
    (a * u * u * u + b * 3.0 * u * u * t + c * 3.0 * u * t * t + d * t * t * t).to_pos2()
}

/// The way a pulse travels between two ends of the flow: along a row's curve, or from row to row in an arc out
/// past the spine.
pub fn path(geometry: &Geometry, rows: &[Row], from: &Way, to: &Way) -> Option<[Pos2; 4]> {
    let row = |id: &str| rows.iter().position(|row| row.id == id).filter(|&k| k < geometry.rows.len());
    match (from, to) {
        (Way::Chief, Way::Task(id)) => Some(curve(geometry.node, geometry.anchor(row(id)?), geometry.narrow)),
        (Way::Task(id), Way::Chief) => {
            let [a, b, c, d] = curve(geometry.node, geometry.anchor(row(id)?), geometry.narrow);
            Some([d, c, b, a])
        }
        (Way::Task(a), Way::Task(b)) if a != b => {
            let (a, b) = (geometry.anchor(row(a)?), geometry.anchor(row(b)?));
            let reach = if geometry.narrow { SPINE * 1.4 } else { FAN * 0.75 };
            Some([a, pos2(a.x - reach, a.y), pos2(b.x - reach, b.y), b])
        }
        _ => None,
    }
}

/// The colour of what passed.
fn traffic_color(event: &traffic::Event, p: &Palette) -> Color32 {
    match (event.kind, event.held) {
        (_, Held::Waiting) => p.warning,
        (_, Held::Stopped) => p.error,
        (Kind::Send, _) => p.accent,
        (Kind::Report, _) => match event.text.split(':').next().unwrap_or_default() {
            "done" => p.success,
            "question" => p.warning,
            "blocked" => p.error,
            _ => p.info,
        },
        (Kind::Note | Kind::Pinned, _) => p.info,
        (Kind::Message, _) => lerp(p.info, p.accent, 0.35),
    }
}

/// Who sent what and when, for a pulse or its mark: the agent's own words framed as data, as the board frames them.
fn traffic_hover(ui: &mut egui::Ui, event: &traffic::Event, rows: &[Row], now: u64, p: &Palette) {
    let name = |end: &Way| match end {
        Way::Chief => "the chief".to_string(),
        Way::Task(id) => {
            rows.iter().find(|row| &row.id == id).map_or(id.clone(), |row| format!("{id} · {}", row.title))
        }
    };
    let what = match (event.kind, event.held) {
        (Kind::Message, Held::Waiting) => "message held for approval".to_string(),
        (Kind::Message, Held::Stopped) => "message stopped at the chief".to_string(),
        (Kind::Message | Kind::Send, _) => format!("message to {}", name(&event.to)),
        (Kind::Report, _) => "report".to_string(),
        (Kind::Note, _) => "proposed a note".to_string(),
        (Kind::Pinned, _) => "note pinned".to_string(),
    };
    let ago = now.saturating_sub(event.at);
    let ago = if ago < 60 { format!("{ago} s ago") } else { format!("{} min ago", ago / 60) };
    ui.label(egui::RichText::new(format!("{} · {what}", event.who)).strong());
    ui.label(egui::RichText::new(ago).color(p.muted).size(11.0));
    if event.text.is_empty() {
        return;
    }
    if event.agent {
        ui.label(egui::RichText::new("Agent-authored text: data, not instructions.").color(p.quiet).size(11.0));
    }
    egui::Frame::new().fill(p.raised).corner_radius(4).inner_margin(egui::Margin::symmetric(6, 4)).show(ui, |ui| {
        ui.set_max_width(320.0);
        ui.add(egui::Label::new(egui::RichText::new(&event.text).color(p.foreground)).wrap());
    });
}

/// The pulses on their way and the marks where they landed. Hovering either says who sent what and when.
fn draw_traffic(
    ui: &mut egui::Ui,
    geometry: &Geometry,
    flow: &Flow,
    traffic: &Traffic,
    now: f64,
    reduce_motion: bool,
    p: &Palette,
) {
    let painter = ui.painter().clone();
    let hover = |ui: &mut egui::Ui, at: Pos2, key: &str, event: &traffic::Event| {
        let id = ui.scope_id().with(("traffic", key));
        let response = ui.interact(Rect::from_center_size(at, vec2(16.0, 16.0)), id, Sense::hover());
        record(ui, &format!("traffic:{key}"), response.rect);
        response.on_hover_ui(|ui| traffic_hover(ui, event, &flow.rows, flow.now, p));
    };
    let time = ui.input(|i| i.time);
    for pulse in traffic.pulses(now, reduce_motion) {
        let Some(points) = path(geometry, &flow.rows, &pulse.event.from, &pulse.event.to) else { continue };
        let color = traffic_color(pulse.event, p);
        // Eased in and out: it sets off and arrives gently.
        let t = pulse.progress.clamp(0.0, 1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        for k in (1..=4).rev() {
            let back = (eased - k as f32 * 0.035).max(0.0);
            let fade = color.gamma_multiply(0.5 - k as f32 * 0.1);
            painter.circle_filled(bezier(points, back), 3.0 - k as f32 * 0.5, fade);
        }
        let head = bezier(points, eased);
        painter.circle_filled(head, 7.0, color.gamma_multiply(0.18));
        painter.circle_filled(head, 3.5, color);
        if pulse.event.held == Held::Waiting && t >= 1.0 {
            held_ring(&painter, geometry.node, time, p);
        }
        hover(ui, head, &pulse.event.key, pulse.event);
    }
    // Marks: on a row's top left, or around the chief's node.
    let mut at_chief = 0;
    let mut at_row: std::collections::HashMap<&str, usize> = Default::default();
    for (event, strength) in traffic.marks(now, reduce_motion) {
        let color = traffic_color(event, p).gamma_multiply(strength);
        let at = match &event.to {
            Way::Chief => {
                let angle = (-70.0 + at_chief as f32 * 28.0).to_radians();
                at_chief += 1;
                geometry.node + vec2(angle.cos(), angle.sin()) * (NODE + 6.0)
            }
            Way::Task(id) => {
                let Some(k) = flow.rows.iter().position(|row| &row.id == id) else { continue };
                let n = at_row.entry(id.as_str()).or_default();
                let at = pos2(geometry.rows[k].left() + 12.0 + *n as f32 * 9.0, geometry.rows[k].top() + 8.0);
                *n += 1;
                at
            }
        };
        match event.kind {
            Kind::Note | Kind::Pinned => {
                let r = Rect::from_center_size(at, vec2(7.0, 8.0));
                painter.rect(r, 1.5, p.raised, Stroke::new(1.2, color), StrokeKind::Middle);
                let line = [pos2(r.left() + 2.0, at.y), pos2(r.right() - 2.0, at.y)];
                painter.line_segment(line, Stroke::new(1.0, color));
            }
            _ if event.held == Held::Stopped => cross(&painter, at, 5.0, Stroke::new(1.4, color)),
            _ if event.held == Held::Waiting => held_ring(&painter, geometry.node, 0.0, p),
            _ => {
                painter.circle_filled(at, 3.0, color);
            }
        }
        hover(ui, at, &format!("mark:{}", event.key), event);
    }
}

/// The ring a message held at the chief's node draws around it.
fn held_ring(painter: &egui::Painter, node: Pos2, time: f64, p: &Palette) {
    let t = (time * 1.2).sin() as f32 * 0.5 + 0.5;
    painter.circle_stroke(node, NODE + 4.0 + t, Stroke::new(1.3, p.warning.gamma_multiply(0.5 + 0.3 * t)));
}

/// The chief's node: its orbit in a ring, glowing while it works.
fn chief_node(painter: &egui::Painter, center: Pos2, live: Live, p: &Palette, time: f64) {
    if live == Live::Working {
        let t = (time * 1.6).sin() as f32 * 0.5 + 0.5;
        painter.circle_filled(center, NODE + 3.0 + 3.0 * t, p.accent.gamma_multiply(0.08 + 0.06 * t));
    }
    painter.circle(center, NODE, lerp(p.background, p.accent, 0.12), Stroke::new(1.5, p.accent));
    let icon = Rect::from_center_size(center, vec2(NODE * 1.25, NODE * 1.25));
    crate::chief::orbit_icon(painter, icon, p.accent, p.foreground);
}

/// The letter that tells a row's model apart: its family's ("Claude Opus 5.5" → O), else its first.
pub fn monogram(who: &str) -> String {
    let mut words = who.split_whitespace();
    let first = words.next().unwrap_or_default();
    let word = if first.eq_ignore_ascii_case("claude") { words.next().unwrap_or(first) } else { first };
    word.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

/// Text on one line, cut with an ellipsis to `width`.
pub fn truncated(
    painter: &egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let job = egui::text::LayoutJob {
        wrap: egui::text::TextWrapping::truncate_at_width(width),
        ..egui::text::LayoutJob::simple_singleline(text.to_string(), font, color)
    };
    painter.fonts_mut(|f| f.layout_job(job))
}

/// A check mark, drawn: the theme's fonts need not have one.
pub fn tick(painter: &egui::Painter, c: Pos2, size: f32, stroke: Stroke) {
    let s = size / 2.0;
    let points = vec![pos2(c.x - s, c.y), pos2(c.x - s * 0.3, c.y + s * 0.7), pos2(c.x + s, c.y - s * 0.75)];
    painter.add(egui::Shape::line(points, stroke));
}

/// A cross, drawn.
pub fn cross(painter: &egui::Painter, c: Pos2, size: f32, stroke: Stroke) {
    let s = size / 2.0;
    painter.line_segment([pos2(c.x - s, c.y - s), pos2(c.x + s, c.y + s)], stroke);
    painter.line_segment([pos2(c.x - s, c.y + s), pos2(c.x + s, c.y - s)], stroke);
}

/// The agent's mark: its model family's letter in a ring of its task's status colour.
fn agent_mark(painter: &egui::Painter, center: Pos2, row: &Row, p: &Palette) {
    let color = status_color(row.status, p);
    painter.circle(center, 11.0, lerp(p.raised, color, 0.12), Stroke::new(1.2, lerp(p.border, color, 0.7)));
    painter.text(center, Align2::CENTER_CENTER, monogram(&row.who), font(11.5), lerp(p.muted, color, 0.6));
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut egui::Ui,
    rect: Rect,
    row: &Row,
    open: bool,
    lit: bool,
    selected: bool,
    narrow: bool,
    p: &Palette,
    time: f64,
    message: &mut String,
) -> Option<Action> {
    let painter = ui.painter().clone();
    let fill = if selected { lerp(p.raised, p.accent, 0.07) } else { p.raised };
    let stroke = if selected {
        Stroke::new(1.4, p.accent)
    } else if lit {
        Stroke::new(1.0, lerp(p.border, p.accent, 0.6))
    } else {
        Stroke::new(1.0, p.border)
    };
    painter.rect(rect, 12.0, fill, stroke, StrokeKind::Inside);
    let base = if narrow { NARROW_ROW } else { ROW };
    let first = Rect::from_min_size(rect.min, vec2(rect.width(), base));
    let title_y = if narrow { first.top() + 22.0 } else { first.center().y };
    agent_mark(&painter, pos2(first.left() + 24.0, title_y), row, p);
    let text_left = first.left() + 44.0;
    let chevron_x = first.right() - 18.0;
    let title_room = if narrow { chevron_x - text_left - 96.0 } else { (first.width() * 0.36).clamp(150.0, 320.0) };
    let title_color = if row.status == TaskStatus::Planned { p.muted } else { p.foreground };
    let title = truncated(&painter, &row.title, font(14.5), title_color, title_room);
    let who = match row.changes {
        Some((add, del)) => format!("{} · +{add} −{del}", row.who),
        None => row.who.clone(),
    };
    let h = title.size().y;
    if narrow {
        painter.galley(pos2(text_left, title_y - h / 2.0), title, title_color);
        painter.text(pos2(text_left, title_y + h / 2.0 + 2.0), Align2::LEFT_TOP, who, font(11.5), p.quiet);
    } else {
        painter.galley(pos2(text_left, title_y - h + 1.0), title, title_color);
        let who = truncated(&painter, &who, font(11.5), p.quiet, title_room);
        painter.galley(pos2(text_left, title_y + 3.0), who, p.quiet);
    }
    // The chevron that opens the row.
    let c = pos2(chevron_x, title_y);
    let points = if open {
        vec![pos2(c.x - 4.0, c.y - 2.0), pos2(c.x, c.y + 2.5), pos2(c.x + 4.0, c.y - 2.0)]
    } else {
        vec![pos2(c.x - 2.0, c.y - 4.0), pos2(c.x + 2.5, c.y), pos2(c.x - 2.0, c.y + 4.0)]
    };
    painter.add(egui::Shape::line(points, Stroke::new(1.4, if lit { p.foreground } else { p.quiet })));
    let chevron = Rect::from_center_size(c, vec2(28.0, 28.0));
    let chevron = ui.interact(chevron, ui.scope_id().with(("chevron", &row.id)), Sense::click());
    record(ui, &format!("chevron:{}", row.id), chevron.rect);
    let expand = chevron.on_hover_text(if open { "Close" } else { "Its subagents, checks and actions" }).clicked();
    // The track: right of the title on a wide panel, under it on a narrow one.
    let (track_left, track_right, track_y) = if narrow {
        (text_left, first.right() - 20.0, first.bottom() - 18.0)
    } else {
        (text_left + title_room + 28.0, chevron_x - 30.0, title_y)
    };
    // A short word on where it stands: before the chevron on a narrow panel, over the track's start on a wide one.
    if let Some(note) = &row.note {
        let color = match row.live {
            Live::Waiting => p.warning,
            Live::Blocked => p.error,
            _ => p.quiet,
        };
        let galley = painter.layout_no_wrap(note.clone(), font(11.5), color);
        let pos = if narrow {
            pos2(chevron_x - 14.0 - galley.size().x, title_y - galley.size().y / 2.0)
        } else {
            pos2(track_left, first.top() + 5.0)
        };
        painter.galley(pos, galley, color);
    }
    draw_track(&painter, track_left, track_right, track_y, row, p, time);
    let mut action = preview_chip(ui, first, row, narrow, chevron_x, title_y, p);
    if expand {
        action = Some(Action::Expand);
    }
    if open {
        action = action.or(draw_open(ui, rect, first, row, narrow, p, message));
    }
    action
}

/// A row's previews, on its first line: one to open or stop, or how many there are; on a client that cannot open
/// them, where they can be.
fn preview_chip(
    ui: &mut egui::Ui,
    first: Rect,
    row: &Row,
    narrow: bool,
    chevron_x: f32,
    title_y: f32,
    p: &Palette,
) -> Option<Action> {
    if row.previews.is_empty() {
        return None;
    }
    let active = row.previews.iter().any(|preview| preview.active);
    let text = match (row.can_preview, row.previews.len(), active) {
        (false, _, _) => "Preview on the Mac".to_string(),
        (true, 1, true) => "■ Stop preview".to_string(),
        (true, 1, false) => "▶ Preview".to_string(),
        (true, n, _) => format!("▶ {n} previews"),
    };
    let color = if !row.can_preview {
        p.quiet
    } else if active {
        p.success
    } else {
        p.accent
    };
    let galley = ui.painter().layout_no_wrap(text, font(11.5), color);
    let size = galley.size() + vec2(14.0, 6.0);
    // Narrow, under the chevron's line at the right; wide, just left of the chevron.
    let at = if narrow {
        pos2(chevron_x - 6.0 - size.x, first.top() + 34.0)
    } else {
        pos2(chevron_x - 16.0 - size.x, title_y - size.y / 2.0 - 16.0)
    };
    let chip = Rect::from_min_size(at, size);
    // One preview opens or stops from here; several are in the open row, which a click on the card opens.
    let single = row.can_preview && row.previews.len() == 1;
    let sense = if single { Sense::click() } else { Sense::hover() };
    let response = ui.interact(chip, ui.scope_id().with(("preview-chip", &row.id)), sense);
    record(ui, &format!("preview:{}", row.id), chip);
    let fill = if response.hovered() && single { lerp(p.raised, color, 0.15) } else { p.raised };
    let stroke = Stroke::new(1.0, lerp(p.border, color, 0.6));
    ui.painter().rect(chip, chip.height() / 2.0, fill, stroke, StrokeKind::Inside);
    ui.painter().galley(chip.min + vec2(7.0, 3.0), galley, color);
    let titles: Vec<&str> = row.previews.iter().map(|preview| preview.title.as_str()).collect();
    let response = if row.can_preview {
        response.on_hover_text(titles.join("\n"))
    } else {
        response.on_hover_text(format!(
            "{}\nThis window cannot open previews: open them from ATOMICS on the Mac.",
            titles.join("\n")
        ))
    };
    (single && response.clicked()).then_some(Action::Preview(0))
}

/// The track: the start point, the line done so far, the marker, the checks and the end square.
fn draw_track(painter: &egui::Painter, left: f32, right: f32, y: f32, row: &Row, p: &Palette, time: f64) {
    let start = pos2(left + 4.0, y);
    let end = pos2(right - 9.0, y);
    let xs = track(start.x, end.x, row.checks.len());
    let quiet = lerp(p.background, p.quiet, 0.8);
    let done = matches!(row.end, End::Done | End::Integrated);
    // Where the solid line ends: the marker, or the end square once done.
    let marker = reached(row).map(|n| {
        let from = if n == 0 { start.x } else { xs[n - 1] };
        let to = xs.get(n).copied().unwrap_or(end.x);
        let k = if row.end == End::Review { 1.0 } else { 0.5 };
        pos2(from + (to - from) * k, y)
    });
    let solid_to = if done { end.x } else { marker.map_or(start.x, |m| m.x) };
    let line_color = match row.live {
        Live::Waiting => p.warning,
        Live::Blocked => p.error,
        _ if done => p.success,
        _ => p.accent,
    };
    if solid_to < end.x - 10.0 {
        painter.extend(egui::Shape::dotted_line(&[pos2(solid_to + 8.0, y), pos2(end.x - 10.0, y)], quiet, 5.0, 0.8));
    }
    if row.started && solid_to > start.x {
        // A gentle wave, as if drawn by hand.
        let mid = (start.x + solid_to) / 2.0;
        let points = [start, pos2(mid, y + 5.0), pos2(mid, y - 5.0), pos2(solid_to, y)];
        let stroke = Stroke::new(1.6, if done { p.success.gamma_multiply(0.7) } else { line_color });
        painter.add(CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, stroke));
    }
    let start_color = if row.started { lerp(p.muted, line_color, 0.4) } else { quiet };
    let start_square = Rect::from_center_size(start, vec2(7.0, 7.0));
    painter.rect(start_square, 2.0, p.raised, Stroke::new(1.2, start_color), StrokeKind::Middle);
    for ((_, mark), x) in row.checks.iter().zip(&xs) {
        let square = Rect::from_center_size(pos2(*x, y), vec2(12.0, 12.0));
        let (fill, stroke) = match mark {
            Mark::Passed => (p.success.gamma_multiply(0.22), p.success),
            Mark::Failed => (p.error.gamma_multiply(0.18), p.error),
            Mark::Pending => (p.raised, quiet),
        };
        painter.rect(square, 3.0, fill, Stroke::new(1.3, stroke), StrokeKind::Middle);
    }
    let square = Rect::from_center_size(end, vec2(18.0, 18.0));
    let (fill, stroke) = match row.end {
        End::Open => (p.raised, quiet),
        End::Review => (p.info.gamma_multiply(0.14), p.info),
        End::Done => (p.success.gamma_multiply(0.22), p.success),
        End::Integrated => (p.success.gamma_multiply(0.85), p.success),
    };
    painter.rect(square, 4.0, fill, Stroke::new(1.5, stroke), StrokeKind::Middle);
    if row.end == End::Integrated {
        tick(painter, square.center(), 8.0, Stroke::new(1.8, p.raised));
    }
    if let Some(at) = marker
        && row.end != End::Review
    {
        let color = match row.live {
            Live::Working => p.accent,
            Live::Waiting => p.warning,
            Live::Blocked => p.error,
            Live::Idle | Live::None => lerp(p.muted, p.accent, 0.4),
        };
        if row.live == Live::Working {
            let t = ((time * 2.2).sin() as f32) * 0.5 + 0.5;
            painter.circle_filled(at, 7.0 + 3.0 * t, color.gamma_multiply(0.10 + 0.10 * t));
        }
        spark(painter, at, 6.5, color);
    }
}

/// The live agent's marker: a small four-pointed spark.
fn spark(painter: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let i = r * 0.28;
    let points = vec![
        pos2(c.x, c.y - r),
        pos2(c.x + i, c.y - i),
        pos2(c.x + r, c.y),
        pos2(c.x + i, c.y + i),
        pos2(c.x, c.y + r),
        pos2(c.x - i, c.y + i),
        pos2(c.x - r, c.y),
        pos2(c.x - i, c.y - i),
    ];
    painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
    painter.circle_filled(c, r * 0.22, lerp(color, Color32::WHITE, 0.5));
}

/// An open row: its agent's threads, each on its own small track, the checks of its definition of done with how
/// its last report stands on each, the first line of that report, and the row's actions.
fn draw_open(
    ui: &mut egui::Ui,
    rect: Rect,
    first: Rect,
    row: &Row,
    narrow: bool,
    p: &Palette,
    message: &mut String,
) -> Option<Action> {
    let painter = ui.painter().clone();
    let left = first.left() + 44.0;
    let mut y = first.bottom() + 4.0;
    let rule = Stroke::new(1.0, lerp(p.raised, p.border, 0.8));
    painter.hline(left..=rect.right() - 16.0, first.bottom() - 2.0, rule);
    let anchor = pos2(first.left() + 24.0, if narrow { first.top() + 33.0 } else { first.center().y + 11.0 });
    for thread in &row.threads {
        let mid = y + THREAD / 2.0;
        let to = pos2(left + 4.0, mid);
        let color = if thread.running { lerp(p.border, p.accent, 0.6) } else { lerp(p.background, p.muted, 0.5) };
        let points = [anchor, pos2(anchor.x, mid - 6.0), pos2(anchor.x + 4.0, mid), to];
        painter.add(CubicBezierShape::from_points_stroke(points, false, Color32::TRANSPARENT, Stroke::new(1.0, color)));
        painter.circle(to, 3.0, p.raised, Stroke::new(1.2, color));
        let kind = if thread.kind.is_empty() { "subagent" } else { thread.kind.as_str() };
        let kind = truncated(&painter, kind, font(12.5), p.foreground, 140.0);
        let kind_w = kind.size().x;
        painter.galley(pos2(left + 14.0, mid - kind.size().y / 2.0), kind, p.foreground);
        let state = match (&thread.tool, thread.running) {
            (Some(tool), true) => format!("{tool}…"),
            (_, true) => "working…".to_string(),
            (_, false) => "done".to_string(),
        };
        let x = left + 14.0 + kind_w.max(110.0) + 12.0;
        // A dot per step, up to what fits; the last one live while the thread runs.
        let room = (rect.right() - 130.0 - x).max(0.0);
        let fit = ((room / 9.0) as u32).min(thread.steps);
        for k in 0..fit {
            let live = thread.running && k + 1 == fit;
            let color = if live {
                p.accent
            } else if thread.running {
                lerp(p.muted, p.accent, 0.3)
            } else {
                p.success.gamma_multiply(0.8)
            };
            painter.circle_filled(pos2(x + 4.0 + k as f32 * 9.0, mid), if live { 3.0 } else { 2.2 }, color);
        }
        let text = format!("{} steps · {state}", thread.steps);
        painter.text(pos2(rect.right() - 16.0, mid), Align2::RIGHT_CENTER, text, font(11.5), p.quiet);
        y += THREAD;
    }
    if !row.threads.is_empty() {
        y += 8.0;
    }
    for (check, mark) in &row.checks {
        let mid = y + CHECK_LINE / 2.0;
        let at = pos2(left + 4.0, mid);
        match mark {
            Mark::Passed => tick(&painter, at, 8.0, Stroke::new(1.6, p.success)),
            Mark::Failed => cross(&painter, at, 7.0, Stroke::new(1.6, p.error)),
            Mark::Pending => {
                painter.circle_stroke(at, 3.5, Stroke::new(1.2, p.quiet));
            }
        }
        let color = if *mark == Mark::Pending { p.muted } else { p.foreground };
        let galley = truncated(&painter, check, font(12.0), color, rect.right() - 24.0 - (left + 16.0));
        painter.galley(pos2(left + 16.0, mid - galley.size().y / 2.0), galley, color);
        y += CHECK_LINE;
    }
    if let Some(report) = &row.report {
        y += 4.0;
        // The agent's own words, set apart as quoted.
        let galley = truncated(&painter, &format!("“{report}”"), font(12.0), p.muted, rect.right() - 24.0 - left);
        painter.galley(pos2(left, y + 2.0), galley, p.muted);
        y += CHECK_LINE;
    }
    let mut action = None;
    let bar = Rect::from_min_max(pos2(left, y + 6.0), pos2(rect.right() - 16.0, y + 6.0 + 28.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(("flow-actions", &row.id))
            .max_rect(bar)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let button = |ui: &mut egui::Ui, text: &str, color: Color32, name: String| {
        let response =
            ui.add(egui::Button::new(egui::RichText::new(text).font(font(12.5)).color(color)).corner_radius(7));
        record(ui, &name, response.rect);
        response
    };
    if button(&mut child, "Open diff", p.foreground, format!("diff:{}", row.id)).clicked() {
        action = Some(Action::Diff);
    }
    if row.session.is_some() {
        if button(&mut child, "Terminal", p.foreground, format!("terminal:{}", row.id)).clicked() {
            action = Some(Action::Terminal);
        }
        if button(&mut child, "Message", p.foreground, format!("message:{}", row.id)).clicked() {
            action = Some(Action::Message);
        }
    }
    for (k, preview) in row.previews.iter().enumerate() {
        if !row.can_preview {
            let text = egui::RichText::new("Preview on the Mac").font(font(12.0)).color(p.quiet);
            child
                .add(egui::Label::new(text))
                .on_hover_text("This window cannot open previews: open them from ATOMICS on the Mac.");
            break;
        }
        let text =
            if preview.active { format!("Stop {}", preview.title) } else { format!("Preview {}", preview.title) };
        let color = if preview.active { p.success } else { p.accent };
        if button(&mut child, &text, color, format!("preview:{}:{k}", row.id)).clicked() {
            action = Some(Action::Preview(k));
        }
    }
    child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if row.session.is_some()
            && button(ui, "Stop", p.error, format!("stop:{}", row.id))
                .on_hover_text("End this agent's session; you confirm first")
                .clicked()
        {
            action = Some(Action::Stop);
        }
        if button(ui, "Open in Full", p.muted, format!("full:{}", row.id)).clicked() {
            action = Some(Action::OpenInFull);
        }
    });
    if row.messaging {
        let field = Rect::from_min_max(pos2(left, bar.bottom() + 6.0), pos2(rect.right() - 16.0, bar.bottom() + 34.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("flow-message", &row.id))
                .max_rect(field)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let id = child.scope_id().with(("message-field", &row.id));
        let focused = child.memory(|m| m.has_focus(id));
        let enter = focused && child.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let edit = egui::TextEdit::singleline(message)
            .id(id)
            .desired_width((field.width() - 70.0).max(80.0))
            .font(font(13.0))
            .hint_text(egui::RichText::new("Message this agent…").font(font(13.0)).color(p.quiet));
        let response = child.add(edit);
        record(&child, &format!("message-field:{}", row.id), response.rect);
        crate::touch::opens_keyboard(&child, &response);
        if !response.has_focus() && message.is_empty() && !enter {
            response.request_focus();
        }
        let send = button(&mut child, "Send", p.accent, format!("send:{}", row.id)).clicked();
        if (send || enter) && !message.trim().is_empty() {
            action = Some(Action::Send);
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: TaskStatus) -> Row {
        Row {
            id: id.into(),
            title: format!("Task {id}"),
            who: "Claude Opus 5.5".into(),
            status,
            session: None,
            started: status != TaskStatus::Planned,
            live: Live::None,
            checks: Vec::new(),
            end: End::Open,
            changes: None,
            note: None,
            threads: Vec::new(),
            steps: 0,
            report: None,
            previews: Vec::new(),
            can_preview: false,
            messaging: false,
        }
    }

    #[test]
    fn rows_stack_without_overlap_and_open_ones_grow() {
        let mut rows: Vec<Row> = (0..5).map(|k| row(&format!("t{k}"), TaskStatus::Running)).collect();
        rows[1].checks = vec![("a".into(), Mark::Passed), ("b".into(), Mark::Pending)];
        let open = |id: &str| id == "t1";
        for width in [1200.0, 390.0] {
            let g = geometry(pos2(10.0, 20.0), width, &rows, &open);
            assert_eq!(g.rows.len(), 5);
            for pair in g.rows.windows(2) {
                assert!(pair[0].bottom() + GAP <= pair[1].top() + 0.01, "no overlap at {width}: {pair:?}");
            }
            assert!(g.rows[1].height() > g.rows[0].height(), "the open row is taller");
            assert!((g.rows[1].height() - g.rows[0].height() - opened(&rows[1])).abs() < 0.01);
            assert!(g.rows.iter().all(|r| r.left() >= 10.0 && r.right() <= 10.0 + width + 0.01));
            assert!((g.height - (g.rows[4].bottom() - 20.0)).abs() < 0.01);
            // The node never sits on a card.
            assert!(g.rows.iter().all(|r| !r.expand(1.0).contains(g.node)));
            assert_eq!(g.narrow, width < NARROW);
        }
        // Wide: the node is level with the middle of the rows' first lines.
        let g = geometry(pos2(0.0, 0.0), 1200.0, &rows, &|_| false);
        let middle = (g.rows[0].top() + g.rows[4].top() + ROW) / 2.0;
        assert!((g.node.y - middle).abs() < 0.01);
    }

    #[test]
    fn an_empty_plan_and_a_one_task_plan_lay_out() {
        let g = geometry(pos2(0.0, 0.0), 900.0, &[], &|_| false);
        assert!(g.rows.is_empty() && g.height > 0.0);
        let one = [row("t1", TaskStatus::Planned)];
        let g = geometry(pos2(0.0, 0.0), 900.0, &one, &|_| false);
        assert_eq!(g.rows.len(), 1);
        assert!((g.node.y - g.anchor(0).y).abs() < 0.01, "one row: the curve runs straight across");
        assert_eq!(summary_parts(&one, 0.0, 300.0), [(TaskStatus::Planned, 0.0, 300.0)]);
        assert!(summary_parts(&[], 0.0, 300.0).is_empty());
        assert_eq!(track(0.0, 100.0, 0), Vec::<f32>::new());
        assert_eq!(reached(&one[0]), None, "not started: no marker");
    }

    #[test]
    fn the_summary_is_proportional_to_task_counts_and_tracks_space_checks_evenly() {
        let rows = [
            row("a", TaskStatus::Running),
            row("b", TaskStatus::Running),
            row("c", TaskStatus::Running),
            row("d", TaskStatus::Done),
        ];
        let parts = summary_parts(&rows, 10.0, 400.0);
        assert_eq!(parts, [(TaskStatus::Running, 10.0, 310.0), (TaskStatus::Done, 310.0, 410.0)]);
        let xs = track(0.0, 100.0, 3);
        assert_eq!(xs, [25.0, 50.0, 75.0]);
        let mut r = row("x", TaskStatus::Running);
        r.checks = vec![("1".into(), Mark::Passed), ("2".into(), Mark::Failed), ("3".into(), Mark::Passed)];
        assert_eq!(reached(&r), Some(1), "the marker stops at the first check not passed");
        r.end = End::Done;
        assert_eq!(reached(&r), None);
    }

    #[test]
    fn a_reports_checks_fill_squares_by_position_and_none_leaves_them_empty() {
        use atomics_core::plan::{Check, Report};
        let acceptance = vec!["builds".to_string(), "tests".to_string(), "clippy".to_string()];
        assert!(marks(&acceptance, None).iter().all(|(_, m)| *m == Mark::Pending));
        let check = |exit| Check { check: "c".into(), command: "x".into(), exit, output_tail: String::new() };
        let report = Report {
            status: "progress".into(),
            summary: String::new(),
            at: 1,
            checks: vec![check(Some(0)), check(Some(2))],
        };
        let marked = marks(&acceptance, Some(&report));
        assert_eq!(marked.iter().map(|(_, m)| *m).collect::<Vec<_>>(), [Mark::Passed, Mark::Failed, Mark::Pending]);
        assert_eq!(marked[0].0, "builds", "the plan's words name the squares");
        // An older host's plan names no checks: the report's own are the squares.
        let fallback = marks(&[], Some(&report));
        assert_eq!(fallback.len(), 2);
        let unknown = Report { checks: vec![check(None)], ..report };
        assert_eq!(marks(&acceptance, Some(&unknown))[0].1, Mark::Pending, "no exit code is no pass");
    }

    #[test]
    fn pulses_follow_the_curves_and_messages_arc_past_the_spine() {
        let rows = [row("t1", TaskStatus::Running), row("t2", TaskStatus::Running)];
        let g = geometry(pos2(0.0, 0.0), 1000.0, &rows, &|_| false);
        let out = path(&g, &rows, &Way::Chief, &Way::Task("t2".into())).unwrap();
        assert_eq!(bezier(out, 0.0), pos2(g.node.x + NODE, g.node.y));
        assert_eq!(bezier(out, 1.0), g.anchor(1));
        let back = path(&g, &rows, &Way::Task("t2".into()), &Way::Chief).unwrap();
        assert_eq!(bezier(back, 0.0), g.anchor(1));
        let across = path(&g, &rows, &Way::Task("t1".into()), &Way::Task("t2".into())).unwrap();
        assert_eq!((bezier(across, 0.0), bezier(across, 1.0)), (g.anchor(0), g.anchor(1)));
        assert!(bezier(across, 0.5).x < g.anchor(0).x - FAN * 0.4, "out past the spine");
        assert!(path(&g, &rows, &Way::Task("gone".into()), &Way::Chief).is_none());
        assert!(path(&g, &rows, &Way::Task("t1".into()), &Way::Task("t1".into())).is_none());
    }

    #[test]
    fn monograms_tell_models_apart() {
        assert_eq!(monogram("Claude Opus 5.5"), "O");
        assert_eq!(monogram("Claude Sonnet 5.5"), "S");
        assert_eq!(monogram("GPT-6.1 Sol (high)"), "G");
        assert_eq!(monogram("Codex"), "C");
        assert_eq!(monogram(""), "");
    }
}
