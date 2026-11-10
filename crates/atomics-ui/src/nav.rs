//! Where the user is and how they came there. A [`Place`] is what the window shows: the tab (a plan's, the
//! Ungrouped one or a project's), the active session or the worktree picked without one, and the panel's
//! view. Each frame [`track`] compares it with the last frame's; a change is a step the user took, which
//! [`back`] and [`forward`] walk, restoring a place through the view commands any platform carries out. The
//! location line over the agents ([`location`]) says where the user is, plan › task › session, with the
//! buttons for both directions; its parts are links to the plan's board and to the session.

use std::path::PathBuf;

use atomics_core::plan::PlanStatus;
use egui::{Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::keys::shortcut_label;
use crate::status::status_color;
use crate::theme::{Palette, lerp};
use crate::topbar::record_item;
use crate::{Backend, Client, Command, PanelMode, switcher};

/// The height of the location line.
pub const HEIGHT: f32 = 24.0;

/// The location line's height for a finger, whose arrows it holds.
pub fn height(touch: Option<crate::touch::Touch>) -> f32 {
    touch.map_or(HEIGHT, |touch| touch.target)
}

/// How many places each direction keeps.
const KEEP: usize = 50;

/// The tab of a [`Place`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    /// A plan's, by id; a finished plan's board shows read-only.
    Plan {
        id: String,
        open: bool,
    },
    Ungrouped,
    /// Every project's sessions, or one project's.
    Project(Option<PathBuf>),
}

/// What the window shows, as far as going back and forth goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub tab: Tab,
    /// The active session, by id.
    pub active: Option<String>,
    /// The worktree the diff shows while no session is active.
    pub root: Option<PathBuf>,
    /// The panel's view and whether it shows.
    pub panel: PanelMode,
    pub open: bool,
}

impl Place {
    /// Where `backend` says the window is.
    pub fn of(backend: &dyn Backend) -> Place {
        let hints = backend.view_hints();
        let plan = backend.plan_row(hints.selected_plan).filter(|row| !row.id.is_empty());
        let tab = match plan {
            Some(row) if hints.plan_view => Tab::Plan { id: row.id, open: row.status == PlanStatus::Open },
            _ if hints.ungrouped => Tab::Ungrouped,
            _ => Tab::Project(hints.project.clone()),
        };
        let root = if hints.active.is_none() { hints.diff_target.clone() } else { None };
        Place { tab, active: hints.active, root, panel: switcher::view_of(hints.panel), open: hints.diff_open }
    }

    /// Whether going from `self` to `next` is a step to remember: another tab, session, worktree or view.
    /// Hiding or showing the panel alone is not.
    fn differs(&self, next: &Place) -> bool {
        self.tab != next.tab || self.active != next.active || self.root != next.root || self.panel != next.panel
    }
}

/// The places behind and ahead of the one on screen, for this window alone.
#[derive(Debug, Clone, Default)]
pub struct History {
    back: Vec<Place>,
    forward: Vec<Place>,
    current: Option<Place>,
    /// [`back`] or [`forward`] restores a place: for this many frames what shows is where it went, no new step.
    /// The platform carries the restoring commands out after the frame's first piece, and a browser settles
    /// the tab's session on the frame after.
    going: u8,
    /// When the place on screen first showed, by the UI's clock.
    since: f64,
}

impl History {
    pub fn can_go(&self, delta: i32) -> bool {
        if delta < 0 { !self.back.is_empty() } else { !self.forward.is_empty() }
    }
}

/// A place that showed for less than this many seconds was on the way to the next: a browser picks a new
/// tab's session on the frame after the tab.
const PASSING: f64 = 0.3;

/// Notes where the window is at `time` (the UI's clock, in seconds); a step from the last place is
/// remembered, unless [`back`] or [`forward`] went there or the last place only passed by.
pub fn track(backend: &dyn Backend, client: &mut Client, time: f64) {
    let now = Place::of(backend);
    let history = &mut client.history;
    let going = history.going > 0;
    history.going = history.going.saturating_sub(1);
    match history.current.take() {
        Some(last) if last.differs(&now) => {
            if !going {
                if time - history.since >= PASSING {
                    history.back.push(last);
                    if history.back.len() > KEEP {
                        history.back.remove(0);
                    }
                }
                history.forward.clear();
            }
            history.since = time;
        }
        Some(_) => {}
        None => history.since = time,
    }
    history.current = Some(now);
}

/// Goes back (`delta` < 0) or forward to the place there, skipping places whose tab or session is gone.
pub fn go(backend: &dyn Backend, client: &mut Client, delta: i32) {
    loop {
        let history = &mut client.history;
        let Some(place) = (if delta < 0 { history.back.pop() } else { history.forward.pop() }) else { return };
        let Some(commands) = restore(backend, &place) else { continue };
        if let Some(current) = history.current.clone() {
            if delta < 0 { history.forward.push(current) } else { history.back.push(current) }
        }
        history.going = 2;
        for command in commands {
            client.send(command);
        }
        return;
    }
}

/// [`go`] back.
pub fn back(backend: &dyn Backend, client: &mut Client) {
    go(backend, client, -1);
}

/// [`go`] forward.
pub fn forward(backend: &dyn Backend, client: &mut Client) {
    go(backend, client, 1);
}

/// The view commands that show `place`, in order: its tab, its session or worktree, its view. `None` when
/// its tab is gone, or it named a session that is gone and nothing else.
fn restore(backend: &dyn Backend, place: &Place) -> Option<Vec<Command>> {
    let mut commands = Vec::new();
    let mut push = |command: Command| {
        if backend.available(&command) {
            commands.push(command);
        }
    };
    match &place.tab {
        Tab::Plan { id, .. } => {
            let plans = backend.plan_list();
            let index = plans.iter().position(|row| &row.id == id)?;
            push(if plans[index].status == PlanStatus::Open {
                Command::ShowPlan(index)
            } else {
                Command::ViewPlanHistory(index)
            });
        }
        Tab::Ungrouped => push(Command::ShowUngrouped),
        Tab::Project(project) => push(Command::SetView(project.clone())),
    }
    let sessions = backend.session_list();
    match (&place.active, &place.root) {
        (Some(id), _) if sessions.iter().any(|s| &s.id == id) => {
            push(Command::Activate { session: id.clone(), open_diff: false })
        }
        (Some(_), _) => return None,
        (None, Some(root)) => push(Command::ShowRoot(root.clone())),
        (None, None) => {}
    }
    push(Command::SetPanel(place.panel));
    push(if place.open { Command::OpenDiff } else { Command::CloseDiff });
    Some(commands)
}

/// A part of the location: its text and color, and what a click on it shows.
struct Crumb {
    text: String,
    color: egui::Color32,
    hover: String,
    command: Option<Command>,
}

/// The location's parts: the tab (a plan's goal, Ungrouped, a project or every project), the task the active
/// session works on, and the session with its status; or the worktree whose diff shows.
fn crumbs(backend: &dyn Backend, p: &Palette) -> Vec<Crumb> {
    let hints = backend.view_hints();
    let plans = backend.plan_list();
    let mut crumbs = Vec::new();
    let plan = plans.get(hints.selected_plan).filter(|row| hints.plan_view && !row.id.is_empty());
    let board = |index: usize, row: &crate::PlanRow| {
        if row.status == PlanStatus::Open { Command::ShowPlan(index) } else { Command::ViewPlanHistory(index) }
    };
    match plan {
        Some(row) => crumbs.push(Crumb {
            text: row.goal.clone(),
            color: row.state.color(p),
            hover: format!("The plan: {}", row.goal),
            command: Some(board(hints.selected_plan, row)),
        }),
        None if hints.ungrouped => crumbs.push(Crumb {
            text: "Ungrouped".into(),
            color: p.foreground,
            hover: "Sessions outside any plan".into(),
            command: None,
        }),
        None => crumbs.push(Crumb {
            text: hints.project.as_ref().map_or("All projects".into(), |root| backend.display(root)),
            color: p.foreground,
            hover: "Every project's sessions".into(),
            command: None,
        }),
    }
    let sessions = backend.session_list();
    let active = hints.active.as_deref().and_then(|id| sessions.iter().find(|s| s.id == id));
    if let Some(session) = active {
        // The task it works on, in whichever plan: the plan comes first when it is not the tab's.
        if let Some(link) = &session.task
            && let Some(index) = plans.iter().position(|row| row.id == link.plan)
        {
            if plan.is_none_or(|row| row.id != link.plan) {
                let row = &plans[index];
                crumbs.push(Crumb {
                    text: row.goal.clone(),
                    color: row.state.color(p),
                    hover: format!("The plan: {}", row.goal),
                    command: Some(board(index, row)),
                });
            }
            let title = backend
                .board(index)
                .and_then(|board| board.plan.tasks.iter().find(|t| t.id == link.task).map(|t| t.title.clone()))
                .unwrap_or_else(|| link.task.clone());
            crumbs.push(Crumb {
                text: title,
                color: p.foreground,
                hover: format!("Task {}: its card is on the plan's board", link.task),
                command: Some(board(index, &plans[index])),
            });
        }
        crumbs.push(Crumb {
            text: format!("{} · {}", session.title, session.status_label()),
            color: status_color(session.status, p),
            hover: format!("{}\n{} in {}", session.hover(), session.agent, backend.display(&session.root)),
            command: Some(Command::Activate { session: session.id.clone(), open_diff: true }),
        });
    } else if let Some(root) = &hints.diff_target {
        crumbs.push(Crumb {
            text: backend.display(root),
            color: p.muted,
            hover: "A worktree no session is active in".into(),
            command: Some(Command::ShowRoot(root.clone())),
        });
    }
    crumbs
}

/// A back or forward button: an arrow painted in `rect`, dim when there is nowhere to go.
fn arrow(ui: &mut egui::Ui, rect: Rect, delta: i32, enabled: bool, tip: String, p: &Palette) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let response = ui.interact(rect, ui.scope_id().with(("nav-arrow", delta)), sense);
    let painter = ui.painter();
    if enabled && response.hovered() {
        painter.rect_filled(rect, 3.0, lerp(p.raised, p.foreground, 0.08));
    }
    if response.has_focus() {
        painter.rect_stroke(rect, 3.0, Stroke::new(2.0, ui.visuals().selection.stroke.color), StrokeKind::Inside);
    }
    let color = if enabled { p.foreground } else { p.quiet };
    let c = rect.center();
    let d = if delta < 0 { -1.0 } else { 1.0 };
    let stroke = Stroke::new(1.6, color);
    // A chevron pointing the way it goes: its tip on the side of `d`.
    painter.line_segment([c + vec2(-2.5 * d, -4.0), c + vec2(2.5 * d, 0.0)], stroke);
    painter.line_segment([c + vec2(2.5 * d, 0.0), c + vec2(-2.5 * d, 4.0)], stroke);
    response.on_hover_text(tip)
}

/// The location line in `rect`: back and forward, then plan › task › session, each part a link.
pub fn location(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.painter().rect_filled(rect, 0.0, p.panel);
    ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.separator));
    let keys = backend.has_shortcuts().then(|| backend.ui_config().keys);
    let tip = |verb: &str, chord: Option<&String>| match chord {
        Some(chord) => format!("{verb} ({})", shortcut_label(ui.ctx(), chord)),
        None => verb.to_string(),
    };
    let size = client.touch.map_or(vec2(22.0, rect.height() - 4.0), |touch| vec2(touch.target, rect.height() - 4.0));
    let left = Rect::from_min_size(pos2(rect.left() + 4.0, rect.top() + 2.0), size);
    let right = left.translate(vec2(size.x + 2.0, 0.0));
    let (can_back, can_forward) = (client.history.can_go(-1), client.history.can_go(1));
    let back_tip = tip("Back to where you were", keys.as_ref().map(|k| &k.back));
    let forward_tip = tip("Forward", keys.as_ref().map(|k| &k.forward));
    if arrow(ui, left, -1, can_back, back_tip, &p).clicked() {
        self::back(backend, client);
    }
    if arrow(ui, right, 1, can_forward, forward_tip, &p).clicked() {
        self::forward(backend, client);
    }
    let crumbs = crumbs(backend, &p);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("nav-location")
            .max_rect(rect.with_min_x(right.right() + 8.0).shrink2(vec2(0.0, 2.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(ui.clip_rect().intersect(rect));
    child.spacing_mut().item_spacing.x = 4.0;
    let count = crumbs.len();
    for (index, crumb) in crumbs.into_iter().enumerate() {
        if index > 0 {
            child.label(RichText::new("›").color(p.quiet).size(12.0));
        }
        // The last part is where the user is; the others shrink first when the line is short.
        let last = index + 1 == count;
        let max = if last { child.available_width() } else { (child.available_width() * 0.4).max(60.0) };
        let text = RichText::new(&crumb.text).color(crumb.color).font(p.ui(12.0));
        let text = if last { text.strong() } else { text };
        let label = egui::Label::new(text).truncate().sense(if crumb.command.is_some() {
            Sense::click()
        } else {
            Sense::hover()
        });
        let response = child.scope(|ui| {
            ui.set_max_width(max);
            ui.add(label)
        });
        let response = response.inner.on_hover_text(&crumb.hover);
        record_item(&child, &format!("location:{index}"), &response);
        if let Some(command) = crumb.command {
            let response = crate::touch::reach(&mut child, response).on_hover_cursor(egui::CursorIcon::PointingHand);
            if response.has_focus() {
                child.painter().rect_stroke(
                    response.rect.expand(2.0),
                    3.0,
                    Stroke::new(2.0, child.visuals().selection.stroke.color),
                    StrokeKind::Outside,
                );
            }
            if response.clicked() && backend.available(&command) {
                client.send(command.clone());
                if matches!(command, Command::ShowPlan(_) | Command::ViewPlanHistory(_)) {
                    for command in [Command::SetPanel(PanelMode::Plan), Command::OpenDiff] {
                        if backend.available(&command) {
                            client.send(command);
                        }
                    }
                }
            }
        }
    }
}
