//! The frame's screens, drawn by atomics-ui over the remote backend in one of two layouts ([`crate::layout`]):
//!
//! - **Wide**: the desktop's own window, `atomics_ui::shell::frame`, through [`Web`], this page's
//!   `Platform`: the top bar, the sidebar, the tiled terminals, the board or a diff in the right panel and the
//!   chief, docked or floating, in the host's theme. The host's own items (settings, sandboxes, credentials,
//!   ticket sign-in, GitHub) are hooks this page leaves empty; its connection's state and the layout menu
//!   take the top bar's last slots, connection trouble a line under the frame, and what the page has to say,
//!   why a command failed above all, a note under the top bar that a click dismisses.
//! - **Phone**: one pane at a time, switched from a bottom bar, each a shared widget: the AGENTS list, the
//!   current plan's board under a picker of the others, the approval banners, a worktree's diff panel, an
//!   agent's fleet tile at full size and the chief's window. It draws inside the part of the viewport the user
//!   sees, in the phone's look ([`Looks`]): the theme's colours with proportional UI text, zoomed, spaced for a
//!   finger, the shared views fitted to a phone ([`atomics_ui::touch`]).
//!
//! Terminals are drawn by [`TermPane`]s through [`Terminals`]. After each piece of the frame, what the user
//! asked for in it is carried out: a view command here, on the page's [`Desk`], any other through the
//! backend.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::layout::{DiffPlacement, Layout as Grid};
use atomics_core::noise;
use atomics_core::ops::Grant;
use atomics_core::plan::PlanStatus;
use atomics_core::theme::ThemeTokens;
use atomics_core::wire::Role;
use atomics_ui::shell::{self, Platform};
use atomics_ui::terminal::Metrics;
use atomics_ui::terminal_surface::TerminalSurface;
use atomics_ui::theme::{FontHook, Fonts, Palette, UI_SIZE, definitions};
use atomics_ui::touch::{self, Touch};
use atomics_ui::{Backend, Client, Command, PanelMode, PlanRow, approvals, board, chief, diff_panel, fleet, sidebar};
use egui::{Align, Color32, Layout, Rect, RichText, Sense, Stroke, TextStyle, pos2, vec2};

use crate::boot::Boot;
use crate::desk::{self, Desk};
use crate::dialogs::{self, Picker};
use crate::driver::{Driver, Status};
use crate::layout::{self, LayoutMode, Viewport};
use crate::remote::RemoteBackend;
use crate::term_pane::{TermPane, Terminals};

/// The phone layout's panes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    /// The AGENTS list.
    Agents,
    #[default]
    Board,
    /// Every permission request waiting, from the top bar's badge.
    Approvals,
    Diffs,
    /// The active agent's tile.
    Terminal,
    /// The chief's window.
    Chief,
}

impl Tab {
    /// The pane a boot's `view` names; anything else opens on the board, plans too.
    pub fn named(view: &str) -> Tab {
        match view {
            "agents" => Tab::Agents,
            "approvals" => Tab::Approvals,
            "diffs" => Tab::Diffs,
            "terminal" => Tab::Terminal,
            "chief" => Tab::Chief,
            _ => Tab::Board,
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Tab::Terminal | Tab::Chief)
    }
}

/// The phone layout's pane switcher, in order.
pub const BAR: [(Tab, &str); 5] = [
    (Tab::Agents, "Agents"),
    (Tab::Board, "Board"),
    (Tab::Diffs, "Diffs"),
    (Tab::Terminal, "Terminal"),
    (Tab::Chief, "Chief"),
];

/// How long the wide layout keeps a note of this page's.
const NOTICE_MS: f64 = 12_000.0;
/// How far under the window's top the wide layout's note hangs: clear of the top bar.
const NOTE_TOP: f32 = 40.0;

/// What this frame shows.
#[derive(Default)]
pub struct View {
    /// The phone's pane.
    pub tab: Tab,
    /// What this page shows of its host in either layout: the active session, the plan, the tab, the right
    /// panel and the chief's window.
    pub desk: Desk,
    /// The worktree whose diff the phone's Diffs pane shows; the list of worktrees while none.
    pub diff: Option<PathBuf>,
    pub layout: LayoutMode,
    pub viewport: Viewport,
    /// The terminal the page's input goes to, with the overlay.
    pub term: TermPane,
    /// Every other terminal on screen, by session id.
    pub tiles: HashMap<String, TermPane>,
    /// The new-session picker, while it is open.
    pub picker: Option<Picker>,
    /// A ticket key from the boot, opened once the board is in.
    pub ticket_key: Option<String>,
    /// The clock, in milliseconds.
    pub now_ms: f64,
    /// The last frame drew the phone layout.
    pub compact: bool,
    pub looks: Looks,
    /// The terminal the page's input goes to, by session id, as the pane on screen says.
    target: Option<String>,
    /// The note on the notice line and when it first showed there.
    noted: Option<(String, f64)>,
}

impl View {
    /// Open on what a boot's `view` names: the phone's pane, and in the wide layout the right panel or the
    /// chief.
    pub fn open(&mut self, tab: Tab) {
        self.tab = tab;
        match tab {
            Tab::Diffs => self.desk.panel = PanelMode::Diff,
            Tab::Chief => self.desk.chief.open = true,
            _ => {}
        }
    }
}

/// egui's bundled fonts: a browser has no font lookup, and a theme's named fonts are the desktop's.
pub struct BundledFonts;

impl FontHook for BundledFonts {
    fn apply(&self, ctx: &egui::Context, _fonts: &Fonts) {
        ctx.set_fonts(definitions(None, None, |_| None));
    }
}

/// The theme in each layout's look: as the desktop draws it (monospace UI text where the theme says so), and
/// on a phone with proportional UI text (monospace stays for code, terminals and IDs), zoomed by
/// [`layout::ZOOM`] through the theme's UI size, with controls a finger's size and room between them.
pub struct Looks {
    pub desktop: Arc<Palette>,
    pub phone: Arc<Palette>,
    /// The look egui has: the phone's (`true`) or the desktop's; `None` until a frame gives it one.
    applied: Option<bool>,
}

impl Looks {
    pub fn new(tokens: &ThemeTokens) -> Looks {
        let mut phone = tokens.clone();
        phone.mono_ui = Some(false);
        let zoomed = Fonts { ui_size: Some(UI_SIZE * layout::ZOOM), ..Fonts::default() };
        Looks {
            desktop: Arc::new(Palette::new(tokens, Fonts::default())),
            phone: Arc::new(Palette::new(&phone, zoomed)),
            applied: None,
        }
    }

    /// Give egui and the client the look of the layout this frame draws. A change discards the frame, and
    /// egui draws it again at the look's zoom.
    fn apply(&mut self, ctx: &egui::Context, client: &mut Client, compact: bool) {
        client.touch = compact.then_some(Touch { target: layout::TOUCH, gutter: layout::GUTTER });
        if self.applied == Some(compact) {
            return;
        }
        let palette = if compact { &self.phone } else { &self.desktop };
        palette.apply(ctx, &BundledFonts);
        if compact {
            ctx.all_styles_mut(phone_style);
        }
        client.palette = palette.clone();
        self.applied = Some(compact);
        ctx.request_discard("the layout changed");
    }
}

impl Default for Looks {
    fn default() -> Self {
        let tokens = atomics_core::theme::builtin(atomics_core::theme::DEFAULT).expect("the default theme is built in");
        Looks::new(&tokens)
    }
}

/// The phone's spacing and text sizes, in its zoomed points.
fn phone_style(style: &mut egui::Style) {
    let spacing = &mut style.spacing;
    spacing.interact_size = vec2(layout::TOUCH, layout::TOUCH);
    spacing.button_padding = vec2(10.0, 4.0);
    spacing.item_spacing = vec2(8.0, 6.0);
    spacing.menu_margin = egui::Margin::same(6);
    spacing.icon_width = 16.0;
    spacing.icon_width_inner = 9.0;
    spacing.icon_spacing = 6.0;
    for (text, size) in [(TextStyle::Small, 10.5), (TextStyle::Monospace, 11.0), (TextStyle::Heading, 16.0)] {
        if let Some(font) = style.text_styles.get_mut(&text) {
            font.size = size;
        }
    }
}

/// Look as the hosting page does: its theme, or a built-in one of its scheme, from the next frame on.
pub fn apply_theme(view: &mut View, boot: &Boot) {
    view.looks = Looks::new(&boot.theme_tokens());
}

/// One frame, and then what the user asked for in it.
pub fn draw(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    open_ticket(driver, view);
    view.term.begin_frame();
    let screen = ui.max_rect();
    // The layout follows the page's width in CSS pixels, which the phone's zoom leaves as they are.
    view.compact = layout::compact(screen.width() * ui.ctx().zoom_factor(), view.viewport.coarse, view.layout);
    view.looks.apply(ui.ctx(), client, view.compact);
    view.term.touch = client.touch;
    sync(ui.ctx(), driver, client, view);
    if view.compact {
        phone(ui, screen, driver, client, view);
    } else {
        wide(ui, driver, client, view);
    }
    dialogs::draw(ui.ctx(), &driver.remote, client, &mut view.picker);
    view.term.end_frame(ui.ctx(), &driver.remote);
    driver.remote.end_frame();
    carry_out(driver, client, view);
    want(&driver.remote, view);
    driver.flush();
}

/// The boot's ticket key, once the board is in: its plan on the board, or the board the page opens on when no
/// plan names it.
fn open_ticket(driver: &Driver, view: &mut View) {
    let remote = &driver.remote;
    if view.ticket_key.is_none() || !remote.live() || remote.with_mirror(|m| m.epoch.is_none()) {
        return;
    }
    let key = view.ticket_key.take().expect("checked");
    view.tab = Tab::Board;
    if let Some(index) = remote.plan_for_ticket(&key) {
        let open = remote.plan_row(index).is_some_and(|row| row.status == PlanStatus::Open);
        view.desk.apply(if open { Command::ShowPlan(index) } else { Command::ViewPlanHistory(index) }, remote);
    }
    view.desk.panel = PanelMode::Plan;
    view.desk.diff_open = true;
}

/// Bring the client's own state and the page's view in line with the backend before drawing.
fn sync(ctx: &egui::Context, driver: &mut Driver, client: &mut Client, view: &mut View) {
    let remote = &driver.remote;
    for refused in remote.take_refused() {
        client.approval_decisions.remove(&refused);
    }
    let pending: HashSet<(String, String)> =
        remote.with_mirror(|m| m.approvals.values().map(|a| (a.session.clone(), a.id.clone())).collect());
    client.approval_decisions.retain(|decision| pending.contains(decision));
    // A steer that did not go through is the user's still, unless they started another.
    if let Some(text) = remote.take_unsent().filter(|_| client.steer_text.is_empty()) {
        client.steer_text = text;
    }
    // So is a comment, in its box, or in the note where there is no box for it. It takes the keyboard from nobody.
    for (root, comment) in remote.take_unsent_comments() {
        let free = !view.term.typing() && ctx.memory(|m| m.focused().is_none());
        if !client.diff_views.entry(root).or_default().reopen_comment(&comment, free) {
            let said = remote.notice().map_or(String::new(), |notice| format!("{notice} "));
            remote.say(format!("{said}Your comment was: {}", comment.body));
        }
    }
    // A new session is outside any plan, and shows on the Ungrouped tab.
    if remote.take_started() {
        view.desk.apply(Command::ShowUngrouped, remote);
    }
    view.desk.settle(remote, |hints| {
        remote.set_view(hints.clone());
        remote.visible_sessions()
    });
    hint(remote, view);
    let sessions = remote.session_list();
    view.tiles.retain(|id, _| sessions.iter().any(|s| &s.id == id));
    client.selected.retain(|id| sessions.iter().any(|s| &s.id == id));
    view.target = if view.compact { None } else { view.desk.typing_target() };
    // The wide layout's note has no close button: it goes after a while.
    match (remote.notice(), &view.noted) {
        (None, _) => view.noted = None,
        (Some(note), Some((seen, since))) if note == *seen => {
            if !view.compact && view.now_ms - since > NOTICE_MS {
                remote.dismiss_notice();
                view.noted = None;
            }
        }
        (Some(note), _) => view.noted = Some((note, view.now_ms)),
    }
}

/// Tell the backend what the page shows: the phone's Agents pane lists every agent, the wide layout the tab's.
fn hint(remote: &RemoteBackend, view: &View) {
    let mut hints = view.desk.hints(remote);
    if view.compact {
        (hints.plan_view, hints.ungrouped) = (false, false);
    }
    remote.set_view(hints);
    remote.set_chief_window(view.desk.chief, !view.compact);
}

/// Fetch the hunks of the diff on screen, and of no other.
fn want(remote: &RemoteBackend, view: &View) {
    let shown = if view.compact {
        view.diff.clone().filter(|_| view.tab == Tab::Diffs)
    } else {
        let hints = view.desk.hints(remote);
        hints.diff_target.filter(|_| hints.diff_open && hints.panel == PanelMode::Diff)
    };
    remote.set_wanted(shown.as_deref());
}

/// The wide layout's platform for the shared frame: it lends each piece the remote backend, this page's
/// client and its terminals, and carries out what the piece recorded before the next draws.
struct Web<'a> {
    driver: &'a mut Driver,
    client: &'a mut Client,
    view: &'a mut View,
}

impl Platform for Web<'_> {
    fn draw<R>(&mut self, draw: impl FnOnce(&dyn Backend, &mut Client, &mut dyn TerminalSurface) -> R) -> R {
        let result = lend(self.driver, self.client, self.view, draw);
        carry_out(self.driver, self.client, self.view);
        result
    }

    fn laid_out(&mut self, layout: &Grid) {
        self.view.desk.dock_fits = layout.dock_width > 0.0;
    }

    /// Where the desktop has its Tickets, GitHub and Remote items: how this page's connection is.
    fn top_bar_items(&mut self, ui: &mut egui::Ui, _in_menu: bool) {
        let (text, good) = status_line(self.driver);
        let p = &self.client.palette;
        ui.label(RichText::new(text).color(status_color(good, p)).size(11.5));
    }

    /// The chief's window, with the permission request it waits on over its terminal.
    fn chief_window(&mut self, ui: &mut egui::Ui) {
        self.draw(|backend, client, terminals| chief_window(ui, backend, client, terminals));
    }

    /// Where the desktop has its Settings: the layout this page draws.
    fn top_bar_settings(&mut self, ui: &mut egui::Ui) {
        layout_menu(ui, self.view);
    }
}

/// Lends `draw` the backend, the client and the page's terminals; a terminal a click picks takes the
/// page's input next.
fn lend<R>(
    driver: &Driver,
    client: &mut Client,
    view: &mut View,
    draw: impl FnOnce(&dyn Backend, &mut Client, &mut dyn TerminalSurface) -> R,
) -> R {
    if !view.compact {
        view.target = view.desk.typing_target();
    }
    let mut terminals = Terminals {
        pane: &mut view.term,
        others: &mut view.tiles,
        remote: &driver.remote,
        target: view.target.clone(),
        now_ms: view.now_ms,
        picked: None,
    };
    let result = draw(&driver.remote, client, &mut terminals);
    if let Some(session) = terminals.picked.take() {
        // A click into a terminal types there, as on the desktop: the chief's, or an agent's, which the tile
        // makes the active one.
        view.desk.typing_in = (view.desk.active.as_deref() != Some(session.as_str())).then_some(session);
        view.term.focus_next = true;
    }
    result
}

/// Carry out what the draw recorded, in order: what asks first by its dialog, a view command on the page, any
/// other through the backend.
fn carry_out(driver: &mut Driver, client: &mut Client, view: &mut View) {
    let remote = &driver.remote;
    for command in std::mem::take(&mut client.commands) {
        match command {
            Command::CloseSession(id) if dialogs::may_be_scratch(remote, &id) => client.confirm_close = Some(id),
            Command::ClosePlanTab(plan) => client.confirm_plan_close = dialogs::plan_to_close(remote, plan),
            Command::NewSession => view.picker = Some(Picker::default()),
            // The changed files' column names files the page has the hunks of: the diff scrolls to them.
            Command::OpenFile { root, path } => {
                view.desk.diff_open = true;
                client.diff_views.entry(root).or_default().open_resolved(&path, false);
            }
            command if command.is_view() || desk::window_command(&command) => view_command(remote, view, command),
            command => remote.send(command),
        }
    }
    hint(remote, view);
}

/// A view command: the page's desk as the desktop's model, and the phone's pane follows what was asked for.
fn view_command(remote: &RemoteBackend, view: &mut View, command: Command) {
    let root = |id: &str| remote.session_list().into_iter().find(|s| s.id == id).map(|s| s.root);
    if view.compact {
        match &command {
            Command::Activate { .. } if view.tab == Tab::Agents => view.tab = Tab::Terminal,
            Command::Activate { session, open_diff: true } | Command::OpenTaskSession(session) => {
                if let Some(worktree) = root(session) {
                    view.diff = Some(worktree);
                    view.tab = Tab::Diffs;
                }
            }
            Command::ShowTaskDiff { plan, task } => {
                let session = remote.board(*plan).and_then(|b| b.plan.tasks.get(*task).and_then(|t| t.session.clone()));
                if let Some(worktree) = session.as_deref().and_then(root) {
                    view.diff = Some(worktree);
                    view.tab = Tab::Diffs;
                }
            }
            Command::ShowRoot(worktree) => {
                view.diff = Some(worktree.clone());
                view.tab = Tab::Diffs;
            }
            Command::OpenDiff | Command::ToggleDiff | Command::SetPanel(PanelMode::Diff) => view.tab = Tab::Diffs,
            Command::CloseDiff => view.diff = None,
            Command::SetPanel(PanelMode::Plan) | Command::TogglePlan | Command::ShowPlan(_) => view.tab = Tab::Board,
            Command::ViewPlanHistory(_) => view.tab = Tab::Board,
            _ => {}
        }
    }
    if let Command::Toast(text) = &command {
        remote.say(text.clone());
    }
    // Picking an agent types there, as on the desktop. A finger taps the terminal itself, which opens the keyboard.
    view.term.focus_next |= !view.compact && matches!(command, Command::Activate { .. } | Command::OpenTaskSession(_));
    view.desk.apply(command, remote);
}

/// The wide layout: the desktop's window, or before there is a board the connection's state.
fn wide(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    if waiting_room(driver) {
        let p = client.palette.clone();
        egui::Panel::top("atomics-top")
            .exact_size(34.0)
            .frame(egui::Frame::new().fill(p.topbar).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(RichText::new("ATOMICS").strong().size(14.0).color(p.accent).monospace());
                    let (text, good) = status_line(driver);
                    ui.label(RichText::new(text).color(status_color(good, &p)).size(11.5));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| layout_menu(ui, view));
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.background).inner_margin(egui::Margin::same(8)))
            .show(ui, |ui| {
                status_screen(ui, driver, client);
            });
        return;
    }
    // Under the frame, where its coming and going moves nothing the user may be about to click.
    if let Some((text, color)) = connection_note(driver, &client.palette) {
        let fill = client.palette.panel;
        egui::Panel::bottom("atomics-connection")
            .resizable(false)
            .frame(egui::Frame::new().fill(fill).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| {
                ui.label(RichText::new(text).color(color));
            });
    }
    shell::frame(ui, &mut Web { driver, client, view });
    note(ui.ctx(), driver, client);
}

/// What the page has to say, why a command failed above all, whole: the top bar's slot would cut it to a few
/// words. It lies over the frame and moves nothing. Like any window it takes the pointer, and its part of a
/// terminal's input, so a click on it reaches nothing under it: it dismisses the note.
fn note(ctx: &egui::Context, driver: &Driver, client: &Client) {
    let Some(text) = driver.remote.notice() else { return };
    let p = &client.palette;
    let shown = egui::Area::new(egui::Id::NULL.with("atomics-note"))
        .anchor(egui::Align2::CENTER_TOP, vec2(0.0, NOTE_TOP))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.raised)
                .stroke(Stroke::new(1.0, p.warning))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.set_max_width(ctx.content_rect().width() * 0.6);
                    ui.add(egui::Label::new(RichText::new(text).color(p.foreground)).selectable(false));
                });
        });
    // Over a floating window a click may have raised.
    ctx.move_to_top(shown.response.layer_id);
    if shown.response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        driver.remote.dismiss_notice();
    }
}

/// The board is not in: pairing, confirming a new pairing, connected before the host's first snapshot, or no
/// board yet while connecting or stopped.
fn waiting_room(driver: &Driver) -> bool {
    let (has_board, snapshot) =
        driver.remote.with_mirror(|m| (!m.plans.is_empty() || !m.sessions.is_empty(), m.epoch.is_some()));
    match driver.status() {
        Status::Pairing { .. } | Status::ConfirmRepair => true,
        Status::Waiting | Status::Connecting | Status::Reconnecting { .. } | Status::Stopped(_) => !has_board,
        Status::Live => !snapshot,
    }
}

/// A child drawing into `rect` alone.
fn region(ui: &mut egui::Ui, rect: Rect, salt: &str, add: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(egui::UiBuilder::new().id_salt(salt).max_rect(rect));
    child.set_clip_rect(rect);
    add(&mut child);
}

/// The phone layout: the top bar, one pane, and the pane switcher or the key bar's space.
fn phone(ui: &mut egui::Ui, screen: Rect, driver: &mut Driver, client: &mut Client, view: &mut View) {
    let p = client.palette.clone();
    ui.painter().rect_filled(screen, 0.0, p.background);
    // The key bar's space replaces the switcher while it shows (a touch device typing); with a mouse the
    // switcher stays.
    let typing = view.tab.terminal() && view.term.typing() && view.viewport.keybar > 0.0;
    let regions = layout::regions(screen, &view.viewport.points(ui.ctx().zoom_factor()), typing);
    // The bars' colour reaches the screen's edges, under the notch and the home indicator.
    let top = Rect::from_x_y_ranges(screen.x_range(), screen.top()..=regions.top.bottom());
    ui.painter().rect_filled(top, 0.0, p.topbar);
    region(ui, regions.top.shrink2(vec2(layout::GUTTER, 0.0)), "phone-top", |ui| phone_top(ui, driver, view, &p));
    region(ui, regions.body, "phone-body", |ui| phone_body(ui, driver, client, view));
    ui.painter().hline(screen.x_range(), top.bottom() - 0.5, Stroke::new(1.0, p.separator));
    if let Some(bar) = regions.bar {
        pane_bar(ui, bar, screen, driver, view, &p);
    }
    if let Some(keys) = regions.keybar {
        ui.painter().rect_filled(keys, 0.0, p.topbar);
    }
}

fn phone_top(ui: &mut egui::Ui, driver: &Driver, view: &mut View, p: &Palette) {
    let approvals = driver.remote.with_mirror(|m| m.approvals.len());
    ui.horizontal_centered(|ui| {
        ui.label(RichText::new("ATOMICS").strong().size(14.0).color(p.accent).monospace());
        let (text, good) = status_line(driver);
        ui.label(RichText::new(text).color(status_color(good, p)).size(11.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            layout_menu(ui, view);
            if approvals > 0 {
                // Selected while the approvals show, as a pane is in the bar.
                let badge = RichText::new(format!("◆ {approvals}")).color(p.warning).strong();
                let open = view.tab == Tab::Approvals;
                let button = egui::Button::new(badge).selected(open).min_size(vec2(layout::TOUCH, layout::TOUCH));
                if ui.add(button).on_hover_text("Approvals").clicked() {
                    view.tab = Tab::Approvals;
                }
            }
        });
    });
}

/// "Layout": automatic (from the screen), phone or desktop.
fn layout_menu(ui: &mut egui::Ui, view: &mut View) {
    let button = egui::Button::new("⋯");
    let button = if view.compact { button.min_size(vec2(layout::TOUCH, layout::TOUCH)) } else { button };
    egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        ui.label(RichText::new("Layout").small());
        for (mode, label) in
            [(LayoutMode::Auto, "Automatic"), (LayoutMode::Phone, "Phone"), (LayoutMode::Desktop, "Desktop")]
        {
            if ui.radio(view.layout == mode, label).clicked() {
                view.layout = mode;
                ui.close();
            }
        }
    });
}

/// The phone layout's pane switcher: its colour across the screen, its panes inside the safe area.
fn pane_bar(ui: &mut egui::Ui, bar: Rect, screen: Rect, driver: &Driver, view: &mut View, p: &Palette) {
    ui.painter().rect_filled(Rect::from_x_y_ranges(screen.x_range(), bar.y_range()), 0.0, p.topbar);
    ui.painter().hline(screen.x_range(), bar.top(), Stroke::new(1.0, p.separator));
    let waits = |chief: bool| {
        driver
            .remote
            .with_mirror(|m| m.approvals.values().any(|a| m.sessions.get(&a.session).is_some_and(|s| s.chief == chief)))
    };
    let (agent_waits, chief_waits) = (waits(false), waits(true));
    for ((tab, label), item) in BAR.iter().zip(layout::bar_items(bar, BAR.len())) {
        let response = ui.interact(item, ui.scope_id().with(("pane", label)), Sense::click());
        let active = view.tab == *tab;
        let color = if active {
            p.accent
        } else if response.hovered() {
            p.foreground
        } else {
            p.muted
        };
        if active {
            let line =
                Rect::from_min_size(item.left_top() + vec2(item.width() * 0.25, 0.0), vec2(item.width() * 0.5, 2.0));
            ui.painter().rect_filled(line, 1.0, p.accent);
        }
        let galley = ui.painter().layout_no_wrap(label.to_string(), p.ui(12.0), color);
        let at = item.center() - galley.size() / 2.0;
        let end = at.x + galley.size().x;
        ui.painter().galley(at, galley, color);
        // A permission request waits: a dot by the name of the pane that answers it.
        if (*tab == Tab::Terminal && agent_waits) || (*tab == Tab::Chief && chief_waits) {
            ui.painter().circle_filled(pos2(end + 5.0, at.y + 3.0), 3.0, p.warning);
        }
        if response.clicked() {
            view.tab = *tab;
        }
    }
}

fn status_line(driver: &Driver) -> (String, Option<bool>) {
    match driver.status() {
        Status::Waiting => ("Waiting for the page".into(), None),
        Status::Connecting => ("Connecting".into(), None),
        Status::Pairing { .. } | Status::ConfirmRepair => ("Pairing".into(), None),
        Status::Live => match driver.remote.welcome() {
            Some(welcome) if welcome.role == Role::Viewer => ("Connected · view only".into(), Some(true)),
            Some(_) => ("Connected".into(), Some(true)),
            None => ("Connecting".into(), None),
        },
        Status::Reconnecting { .. } => ("Reconnecting".into(), Some(false)),
        Status::Stopped(_) => ("Stopped".into(), Some(false)),
    }
}

fn status_color(good: Option<bool>, p: &Palette) -> Color32 {
    match good {
        Some(true) => p.success,
        Some(false) => p.warning,
        None => p.muted,
    }
}

/// What the connection's state says of the board on screen, and in what colour: why this page stopped, or that
/// the board may be out of date.
fn connection_note(driver: &Driver, p: &Palette) -> Option<(&'static str, Color32)> {
    match driver.status() {
        Status::Stopped(stop) => Some((stop.message(), p.error)),
        Status::Reconnecting { .. } => Some((
            "Reconnecting. The board may be out of date, and nothing can be changed until the computer is back.",
            p.warning,
        )),
        _ => None,
    }
}

fn notice(ui: &mut egui::Ui, driver: &Driver, touch: Option<Touch>) {
    let remote = &driver.remote;
    if let Some(notice) = remote.notice() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(notice).small());
            if touch::small_button(ui, touch, "×").on_hover_text("Dismiss").clicked() {
                remote.dismiss_notice();
            }
        });
    }
}

/// The space a phone keeps between the screen's sides and text; none on a desktop.
fn gutter(client: &Client) -> f32 {
    client.touch.map_or(0.0, |touch| touch.gutter)
}

/// `add`, `gutter` away from both sides, below what the ui holds already. What draws nothing takes no room.
fn padded<R>(ui: &mut egui::Ui, gutter: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if gutter <= 0.0 {
        return add(ui);
    }
    let rect = ui.available_rect_before_wrap().shrink2(vec2(gutter, 0.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    let inner = add(&mut child);
    let drawn = child.min_rect();
    if drawn.width() > 0.0 || drawn.height() > 0.0 {
        ui.advance_cursor_after_rect(drawn);
    }
    inner
}

/// A phone's list: rows a finger can tap, scrolling under the top bar.
fn list(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(8.0);
        add(ui);
    });
}

/// One row of a phone's list: a card as wide as the list and at least a finger tall, tapped as a whole.
fn list_row(
    ui: &mut egui::Ui,
    p: &Palette,
    id: impl std::hash::Hash + std::fmt::Debug,
    add: impl FnOnce(&mut egui::Ui),
) -> bool {
    let card = egui::Frame::new()
        .fill(p.raised)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(10, 8));
    let shown = card.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(layout::TOUCH - 16.0);
        ui.style_mut().interaction.selectable_labels = false;
        add(ui);
    });
    let response = ui.interact(shown.response.rect, ui.scope_id().with(("row", id)), Sense::click());
    if response.is_pointer_button_down_on() {
        ui.painter().rect_filled(shown.response.rect, 4.0, p.hover());
    }
    ui.add_space(8.0);
    response.clicked()
}

fn centred(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add_space((ui.available_height() / 4.0).min(160.0));
        add(ui);
    });
}

/// The connection's state while there is no board to show, filling the body; false when there is a board.
fn status_screen(ui: &mut egui::Ui, driver: &mut Driver, client: &Client) -> bool {
    let p = client.palette.clone();
    let gutter = gutter(client);
    if !waiting_room(driver) {
        return false;
    }
    match driver.status().clone() {
        Status::Pairing { code } => padded(ui, gutter, |ui| {
            centred(ui, |ui| {
                ui.label(RichText::new("Pairing this browser with your computer").size(18.0));
                ui.add_space(16.0);
                match code {
                    Some(code) => {
                        ui.label(RichText::new(noise::show_code(&code)).monospace().size(40.0).color(p.accent));
                        ui.add_space(16.0);
                        ui.label("Check that ATOMICS on the computer shows the same code, then accept it there.");
                    }
                    None if driver.pairing_busy() => {
                        let busy = "Another device is pairing with the computer right now. Trying again…";
                        ui.label(RichText::new(busy).color(p.muted));
                    }
                    None => {
                        ui.label(RichText::new("Connecting…").color(p.muted));
                    }
                }
            })
        }),
        Status::ConfirmRepair => padded(ui, gutter, |ui| {
            centred(ui, |ui| {
                ui.label(RichText::new("Pair this browser again?").size(18.0));
                ui.add_space(16.0);
                ui.label(
                    "This browser is already paired with this computer, under another key. Pairing again \
                     replaces it. Go on only if you started this pairing yourself, just now, from Remote devices \
                     in ATOMICS on the computer.",
                );
                ui.add_space(16.0);
                // The two buttons wrap on a narrow screen rather than run off it.
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Pair again").clicked() {
                        driver.repair(true);
                    }
                    if ui.button("Keep the current pairing").clicked() {
                        driver.repair(false);
                    }
                });
            })
        }),
        Status::Stopped(stop) => padded(ui, gutter, |ui| {
            centred(ui, |ui| {
                ui.label(stop.message());
            })
        }),
        _ => padded(ui, gutter, |ui| {
            centred(ui, |ui| {
                ui.label(RichText::new("Connecting to your computer…").color(p.muted));
            })
        }),
    }
    true
}

/// The phone's body: the connection's state while there is no board, else the pane, under what the
/// connection and the host have to say.
fn phone_body(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    if status_screen(ui, driver, client) {
        return;
    }
    let p = client.palette.clone();
    // A phone keeps text off the screen's sides; the panes that reach them keep to the gutter themselves.
    let gutter = gutter(client);
    if let Some((text, color)) = connection_note(driver, &p) {
        padded(ui, gutter, |ui| ui.label(RichText::new(text).color(color)));
    }
    padded(ui, gutter, |ui| notice(ui, driver, client.touch));
    match view.tab {
        Tab::Agents => agents_tab(ui, driver, client),
        Tab::Board => board_tab(ui, &driver.remote, client, view),
        Tab::Approvals => approvals_tab(ui, driver, client),
        Tab::Diffs => diffs_tab(ui, driver, client, view),
        Tab::Terminal => terminal_tab(ui, driver, client, view),
        Tab::Chief => chief_tab(ui, driver, client, view),
    }
}

/// The AGENTS list, as the desktop's sidebar has it: a tap opens the agent's terminal.
fn agents_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client) {
    let backend: &dyn Backend = &driver.remote;
    if backend.visible_sessions().is_empty() {
        return empty(ui, client, "No agents are running on this computer.");
    }
    padded(ui, gutter(client), |ui| list(ui, |ui| sidebar::sessions(ui, backend, client)));
}

/// The active agent's tile at the pane's size (the first agent's while none is active): its header, the
/// approval it waits on and its terminal.
fn terminal_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let sessions = driver.remote.session_list();
    let agents: Vec<usize> = (0..sessions.len()).filter(|&i| !sessions[i].chief).collect();
    let Some(&first) = agents.first() else {
        return empty(ui, client, "No agents are running on this computer.");
    };
    let active = view.desk.active.as_deref();
    let index = agents.iter().copied().find(|&i| Some(sessions[i].id.as_str()) == active).unwrap_or(first);
    view.target = Some(sessions[index].id.clone());
    let grid = Grid {
        sidebar: false,
        dock_width: 0.0,
        diff: DiffPlacement::Hidden,
        columns: 1,
        rows: 1,
        tiled: 1,
        tabs: false,
        completed: 0,
    };
    fill(ui, |ui, rect| {
        lend(driver, client, view, |backend, client, terminals| {
            fleet::tiles(ui, rect, &grid, &[index], backend, client, terminals)
        })
    });
}

/// The chief's window at the pane's size: its header, the permission request it waits on and its terminal.
fn chief_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let Some(chief) = driver.remote.session_list().into_iter().find(|s| s.chief) else {
        return empty(ui, client, "No chief is running on this computer.");
    };
    view.target = Some(chief.id);
    fill(ui, |ui, rect| {
        let mut child = ui.new_child(egui::UiBuilder::new().id_salt("chief").max_rect(rect));
        lend(driver, client, view, |backend, client, terminals| {
            chief_window(&mut child, backend, client, terminals);
        });
    });
}

/// The terminal a view asks for, kept to draw once what goes over it is drawn.
#[derive(Default)]
struct Kept(Option<(usize, String, Rect, egui::Id, Metrics)>);

impl TerminalSurface for Kept {
    fn show(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        session: &str,
        rect: Rect,
        id: egui::Id,
        metrics: &Metrics,
        _palette: &Palette,
    ) -> egui::Response {
        let metrics = Metrics { font: metrics.font.clone(), cell: metrics.cell };
        self.0 = Some((index, session.to_string(), rect, id, metrics));
        ui.interact(Rect::from_min_size(rect.min, vec2(0.0, 0.0)), id.with("kept"), Sense::hover())
    }
}

/// The chief's window, with the permission request the chief waits on over its terminal, as an agent's tile has
/// it: the browser's one place to answer it where it asks.
fn chief_window(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, terminals: &mut dyn TerminalSurface) {
    let mut kept = Kept::default();
    chief::content(ui, backend, client, &mut kept, |_| 0.0);
    let Some((index, session, rect, id, metrics)) = kept.0 else { return };
    let p = client.palette.clone();
    let rest = approvals::banner(ui, rect, index, backend, client);
    terminals.show(ui, index, &session, rest, id, &metrics, &p);
}

fn empty(ui: &mut egui::Ui, client: &Client, text: &str) {
    let muted = client.palette.muted;
    padded(ui, gutter(client), |ui| {
        centred(ui, |ui| {
            ui.label(RichText::new(text).color(muted));
        })
    });
}

/// The rest of the panel, given to a draw function that paints where it is told.
fn fill(ui: &mut egui::Ui, draw: impl FnOnce(&mut egui::Ui, Rect)) {
    let rect = ui.available_rect_before_wrap();
    draw(ui, rect);
    ui.allocate_rect(rect, Sense::hover());
}

/// The selected plan's board, under a picker when there are other plans to pick.
fn board_tab(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, view: &View) {
    let plans = backend.plan_list();
    if plans.is_empty() {
        return empty(ui, client, "No plans on this computer yet.");
    }
    let selected = view.desk.hints(backend).selected_plan;
    padded(ui, gutter(client), |ui| plan_picker(ui, &plans, selected, client));
    if selected < plans.len() {
        fill(ui, |ui, rect| board::draw(ui, rect, selected, backend, client));
    } else {
        empty(ui, client, "No plan is open. Finished plans are under History.");
    }
}

/// A plan's name on a phone: the state and goal the desktop's tab shows, as a command picks it.
fn plan_pick(ui: &mut egui::Ui, row: &PlanRow, index: usize, selected: usize, client: &mut Client) -> egui::Response {
    let p = client.palette.clone();
    let label = RichText::new(row.state.label(&row.tab_name)).color(row.state.color(&p));
    let name = ui.selectable_label(index == selected, label);
    if name.clicked() {
        client.send(if row.status == PlanStatus::Open {
            Command::ShowPlan(index)
        } else {
            Command::ViewPlanHistory(index)
        });
    }
    name
}

/// The plans a phone picks from, one row high so the board stays on screen: the open ones, and a finished one
/// while it shows, in a strip that scrolls sideways, and the other finished ones in a scrolling History menu.
/// No plan but the one showing draws nothing.
fn plan_picker(ui: &mut egui::Ui, plans: &[PlanRow], selected: usize, client: &mut Client) {
    let named = || plans.iter().enumerate().filter(|(_, row)| !row.id.is_empty());
    if named().all(|(index, _)| index == selected) {
        return;
    }
    let mut strip: Vec<(usize, &PlanRow)> =
        named().filter(|(index, row)| row.status == PlanStatus::Open || *index == selected).collect();
    strip.sort_by_key(|(_, row)| row.status != PlanStatus::Open);
    let history: Vec<(usize, &PlanRow)> = named().filter(|(_, row)| row.status != PlanStatus::Open).collect();
    ui.add_space(4.0);
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, layout::TOUCH), Layout::right_to_left(Align::Center), |ui| {
        if !history.is_empty() {
            let button = egui::Button::new("History").min_size(vec2(layout::TOUCH, layout::TOUCH));
            let height = (ui.ctx().content_rect().height() * 0.6).max(layout::TOUCH);
            egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
                ui.set_max_width(width - ui.spacing().menu_margin.sum().x);
                egui::ScrollArea::vertical().id_salt("plan-history").max_height(height).show(ui, |ui| {
                    for (index, row) in &history {
                        if plan_pick(ui, row, *index, selected, client).clicked() {
                            ui.close();
                        }
                    }
                });
            });
        }
        // The plan just picked, from History too, scrolls into sight, with a tap's width of the one before it.
        let shown = egui::Id::NULL.with("plan-strip-shows");
        let moved = ui.data(|d| d.get_temp::<usize>(shown)) != Some(selected);
        ui.data_mut(|d| d.insert_temp(shown, selected));
        let rest = ui.available_rect_before_wrap();
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(rest).layout(Layout::left_to_right(Align::Center)));
        egui::ScrollArea::horizontal()
            .id_salt("plan-strip")
            .auto_shrink([false, true])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(&mut left, |ui| {
                ui.horizontal(|ui| {
                    for (index, row) in &strip {
                        let name = plan_pick(ui, row, *index, selected, client);
                        if moved && *index == selected {
                            ui.scroll_to_rect(name.rect.with_min_x(name.rect.left() - layout::TOUCH), None);
                        }
                    }
                });
            });
    });
}

fn approvals_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client) {
    let backend: &dyn Backend = &driver.remote;
    let sessions = backend.session_list();
    let waiting: Vec<usize> = (0..sessions.len()).filter(|i| backend.session_approval(*i).is_some()).collect();
    if waiting.is_empty() {
        return empty(ui, client, "Nothing is waiting for you.");
    }
    let p = client.palette.clone();
    // On a phone the banners span the screen, as over a terminal, and the rest keeps to the gutter.
    let gutter = gutter(client);
    if driver.remote.welcome().is_some() && !driver.remote.may(Grant::Approve) {
        padded(ui, gutter, |ui| ui.label(RichText::new("This browser may only view this computer.").color(p.muted)));
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if gutter > 0.0 {
            ui.add_space(8.0);
        }
        for index in waiting {
            let session = &sessions[index];
            padded(ui, gutter, |ui| {
                ui.label(RichText::new(&session.title).strong());
                ui.label(RichText::new(backend.display(&session.root)).color(p.muted).small());
            });
            let top = ui.cursor().min;
            let width = ui.available_width();
            // The banner takes what it needs of this; the rest stays free.
            let body = Rect::from_min_size(top, vec2(width, 380.0));
            let rest = approvals::banner(ui, body, index, backend, client);
            ui.allocate_rect(Rect::from_min_max(top, pos2(top.x + width, rest.min.y)), Sense::hover());
            ui.add_space(12.0);
        }
    });
}

/// The worktrees with changes; one picked shows in the diff panel.
fn diffs_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &View) {
    let backend: &dyn Backend = &driver.remote;
    let roots = backend.diff_roots();
    if let Some(root) = view.diff.clone().filter(|root| roots.contains(root)) {
        diff_panel::sync(backend, client);
        return fill(ui, |ui, rect| diff_panel::draw(ui, rect, &root, backend, client));
    }
    if roots.is_empty() {
        return empty(ui, client, "No changes to review.");
    }
    let p = client.palette.clone();
    let rows: Vec<(String, usize, usize, usize)> = driver.remote.with_mirror(|m| {
        m.diffs.values().map(|d| (d.worktree.clone(), d.files.len(), d.additions(), d.deletions())).collect()
    });
    let name =
        |worktree: &str| Path::new(worktree).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    padded(ui, gutter(client), |ui| {
        list(ui, |ui| {
            for (worktree, files, added, removed) in rows {
                let tapped = list_row(ui, &p, ("diff", &worktree), |ui| {
                    ui.label(RichText::new(name(&worktree)).color(p.foreground).strong().size(13.5));
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("+{added}")).color(p.success));
                        ui.label(RichText::new(format!("−{removed}")).color(p.error));
                        ui.label(RichText::new(format!("{files} files")).color(p.muted));
                    });
                    ui.add(egui::Label::new(RichText::new(&worktree).color(p.muted).monospace()).truncate());
                });
                if tapped {
                    client.send(Command::ShowRoot(PathBuf::from(&worktree)));
                }
            }
        })
    });
}

#[cfg(test)]
mod tests;
