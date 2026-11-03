//! The frame's screens: Board, Plans, Approvals, Diffs, an agent's Terminal and the Chief, drawn by
//! atomics-ui's draw functions over the remote backend, and terminals by [`TermPane`]. Two layouts
//! ([`crate::layout`]): on a desktop a top bar with the connection's state and a tab per view; on a phone
//! one pane at a time, switched from a bottom bar, inside the part of the viewport the user sees, in the
//! phone's look ([`Looks`]): the theme's colours, zoomed, spaced for a finger, with the shared views fitted
//! to a phone ([`atomics_ui::touch`]). After each draw the commands the user gave are carried out: a view
//! command here, any other through the backend.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::noise;
use atomics_core::ops::Grant;
use atomics_core::theme::ThemeTokens;
use atomics_core::wire::Role;
use atomics_ui::theme::{FontHook, Fonts, Palette, UI_SIZE, definitions};
use atomics_ui::touch::{self, Touch};
use atomics_ui::{Backend, Client, Command, PanelMode, PlanRow, ViewHints, approvals, board, diff_panel};
use egui::{Align, Layout, Rect, RichText, Sense, Stroke, TextStyle, pos2, vec2};

use crate::boot::Boot;
use crate::driver::{Driver, Status};
use crate::layout::{self, LayoutMode, Viewport};
use crate::term_pane::TermPane;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Board,
    Plans,
    Approvals,
    Diffs,
    /// An agent's terminal.
    Terminal,
    /// The chief's terminal.
    Chief,
}

impl Tab {
    /// The tab a boot's `view` names; anything else opens on the board.
    pub fn named(view: &str) -> Tab {
        match view {
            "plans" => Tab::Plans,
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
    (Tab::Board, "Board"),
    (Tab::Plans, "Plans"),
    (Tab::Diffs, "Diffs"),
    (Tab::Terminal, "Terminal"),
    (Tab::Chief, "Chief"),
];

/// What this frame shows.
#[derive(Default)]
pub struct View {
    pub tab: Tab,
    /// The board's plan, by position in the plan list.
    pub plan: usize,
    /// The worktree whose diff is open.
    pub diff: Option<PathBuf>,
    /// The agent the Terminal tab shows; the first agent while none was picked.
    pub session: Option<String>,
    pub layout: LayoutMode,
    pub viewport: Viewport,
    pub term: TermPane,
    /// A ticket key from the boot, opened once the board is in.
    pub ticket_key: Option<String>,
    /// The clock, in milliseconds.
    pub now_ms: f64,
    /// The last frame drew the phone layout.
    pub compact: bool,
    pub looks: Looks,
}

/// egui's bundled fonts: a browser has no font lookup, and a theme's named fonts are the desktop's.
pub struct BundledFonts;

impl FontHook for BundledFonts {
    fn apply(&self, ctx: &egui::Context, _fonts: &Fonts) {
        ctx.set_fonts(definitions(None, None, |_| None));
    }
}

/// The theme in each layout's look: as the desktop draws it, and on a phone with proportional UI text
/// (monospace stays for code, terminals and IDs), zoomed by [`layout::ZOOM`] through the theme's UI size,
/// with controls a finger's size and room between them.
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
    sync(driver, client, view);
    open_ticket(driver, view);
    view.term.begin_frame();
    let screen = ui.max_rect();
    // The layout follows the page's width in CSS pixels, which the phone's zoom leaves as they are.
    view.compact = layout::compact(screen.width() * ui.ctx().zoom_factor(), view.viewport.coarse, view.layout);
    view.looks.apply(ui.ctx(), client, view.compact);
    view.term.touch = client.touch;
    let p = client.palette.clone();
    if view.compact {
        phone(ui, screen, driver, client, view);
    } else {
        egui::Panel::top("atomics-top")
            .frame(egui::Frame::new().fill(p.topbar).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| top(ui, driver, view, &p));
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.background).inner_margin(egui::Margin::same(8)))
            .show(ui, |ui| body(ui, driver, client, view));
    }
    view.term.end_frame();
    driver.remote.end_frame();
    carry_out(driver, client, view);
}

/// The boot's ticket key, once the board is in: its plan on the board, or the board when none names it.
fn open_ticket(driver: &Driver, view: &mut View) {
    let remote = &driver.remote;
    if view.ticket_key.is_none() || !remote.live() || remote.with_mirror(|m| m.epoch.is_none()) {
        return;
    }
    let key = view.ticket_key.take().expect("checked");
    view.tab = Tab::Board;
    view.plan = remote.plan_for_ticket(&key).unwrap_or(0);
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
    region(ui, regions.body, "phone-body", |ui| body(ui, driver, client, view));
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
                let badge = RichText::new(format!("◆ {approvals}")).color(p.warning).strong();
                let button = egui::Button::new(badge).min_size(vec2(layout::TOUCH, layout::TOUCH));
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
    let waiting = driver
        .remote
        .with_mirror(|m| m.approvals.values().any(|a| m.sessions.get(&a.session).is_some_and(|s| !s.chief)));
    for ((tab, label), item) in BAR.iter().zip(layout::bar_items(bar, BAR.len())) {
        let response = ui.interact(item, ui.scope_id().with(("pane", label)), Sense::click());
        let active = view.tab == *tab || (*tab == Tab::Board && view.tab == Tab::Approvals);
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
        // An agent waits on an approval: a dot by the Terminal pane's name.
        if *tab == Tab::Terminal && waiting {
            ui.painter().circle_filled(pos2(end + 5.0, at.y + 3.0), 3.0, p.warning);
        }
        if response.clicked() {
            view.tab = *tab;
        }
    }
}

/// Bring the client's own state in line with the backend before drawing.
fn sync(driver: &mut Driver, client: &mut Client, view: &View) {
    let remote = &driver.remote;
    for refused in remote.take_refused() {
        client.approval_decisions.remove(&refused);
    }
    let pending: HashSet<(String, String)> =
        remote.with_mirror(|m| m.approvals.values().map(|a| (a.session.clone(), a.id.clone())).collect());
    client.approval_decisions.retain(|decision| pending.contains(decision));
    if let Some(root) = &view.diff
        && let Some(data) = remote.diff(root)
    {
        let state = client.diff_views.entry(root.clone()).or_default();
        state.prepared = data.prepared;
        state.error = data.error;
        state.loading = data.loading;
        state.comments = data.comments;
    }
    remote.set_view(ViewHints {
        active: None,
        diff_target: view.diff.clone(),
        plan_view: view.tab == Tab::Board,
        ungrouped: false,
        project: None,
        selected_plan: view.plan,
        diff_open: view.diff.is_some(),
        panel: if view.tab == Tab::Diffs { PanelMode::Diff } else { PanelMode::Plan },
    });
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

fn status_color(good: Option<bool>, p: &Palette) -> egui::Color32 {
    match good {
        Some(true) => p.success,
        Some(false) => p.warning,
        None => p.muted,
    }
}

fn top(ui: &mut egui::Ui, driver: &mut Driver, view: &mut View, p: &Palette) {
    let remote = &driver.remote;
    let (plans, approvals, diffs) = remote.with_mirror(|m| (m.plans.len(), m.approvals.len(), m.diffs.len()));
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("ATOMICS").color(p.accent).strong());
        let (text, good) = status_line(driver);
        ui.label(RichText::new(text).color(status_color(good, p)).small());
        ui.separator();
        let mut tab = |ui: &mut egui::Ui, which: Tab, label: String| {
            if ui.selectable_label(view.tab == which, label).clicked() {
                view.tab = which;
            }
        };
        tab(ui, Tab::Board, "Board".into());
        tab(ui, Tab::Plans, format!("Plans {plans}"));
        tab(ui, Tab::Approvals, if approvals > 0 { format!("Approvals ◆ {approvals}") } else { "Approvals".into() });
        tab(ui, Tab::Diffs, format!("Diffs {diffs}"));
        tab(ui, Tab::Terminal, "Terminal".into());
        tab(ui, Tab::Chief, "Chief".into());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| layout_menu(ui, view));
    });
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

/// `add`, `gutter` away from both sides, below what the ui holds already.
fn padded<R>(ui: &mut egui::Ui, gutter: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    if gutter <= 0.0 {
        return add(ui);
    }
    let rect = ui.available_rect_before_wrap().shrink2(vec2(gutter, 0.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
    let inner = add(&mut child);
    ui.advance_cursor_after_rect(child.min_rect());
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

fn body(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    let p = client.palette.clone();
    // A phone keeps text off the screen's sides; the panes that reach them keep to the gutter themselves.
    let gutter = gutter(client);
    let has_board = driver.remote.with_mirror(|m| !m.plans.is_empty() || !m.sessions.is_empty());
    match driver.status().clone() {
        Status::Pairing { code } => {
            return padded(ui, gutter, |ui| {
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
            });
        }
        Status::ConfirmRepair => {
            return padded(ui, gutter, |ui| {
                centred(ui, |ui| {
                    ui.label(RichText::new("Pair this browser again?").size(18.0));
                    ui.add_space(16.0);
                    ui.label(
                        "This browser is already paired with this computer, under another key. Pairing again \
                         replaces it. Go on only if you started this pairing yourself, just now, from Remote devices \
                         in ATOMICS on the computer.",
                    );
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        if ui.button("Pair again").clicked() {
                            driver.repair(true);
                        }
                        if ui.button("Keep the current pairing").clicked() {
                            driver.repair(false);
                        }
                    });
                })
            });
        }
        Status::Stopped(stop) if !has_board => {
            return padded(ui, gutter, |ui| {
                centred(ui, |ui| {
                    ui.label(stop.message());
                })
            });
        }
        Status::Stopped(stop) => {
            padded(ui, gutter, |ui| ui.label(RichText::new(stop.message()).color(p.error)));
        }
        Status::Reconnecting { .. } if has_board => {
            let stale = "Reconnecting. The board may be out of date, and nothing can be changed until the computer \
                         is back.";
            padded(ui, gutter, |ui| ui.label(RichText::new(stale).color(p.warning)));
        }
        Status::Waiting | Status::Connecting | Status::Reconnecting { .. } if !has_board => {
            return padded(ui, gutter, |ui| {
                centred(ui, |ui| {
                    ui.label(RichText::new("Connecting to your computer…").color(p.muted));
                })
            });
        }
        _ => {}
    }
    padded(ui, gutter, |ui| notice(ui, driver, client.touch));
    let backend: &dyn Backend = &driver.remote;
    match view.tab {
        Tab::Board => board_tab(ui, backend, client, view),
        Tab::Plans => plans_tab(ui, backend, client),
        Tab::Approvals => approvals_tab(ui, driver, client),
        Tab::Diffs => diffs_tab(ui, driver, client, view),
        Tab::Terminal => terminal_tab(ui, driver, client, view),
        Tab::Chief => chief_tab(ui, driver, client, view),
    }
}

/// An agent's terminal, picked from the agents on the computer.
fn terminal_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let backend: &dyn Backend = &driver.remote;
    let agents: Vec<_> = backend.session_list().into_iter().filter(|s| !s.chief).collect();
    if agents.is_empty() {
        return empty(ui, client, "No agents are running on this computer.");
    }
    let session =
        view.session.clone().filter(|id| agents.iter().any(|s| &s.id == id)).unwrap_or_else(|| agents[0].id.clone());
    let p = client.palette.clone();
    let gutter = gutter(client);
    if gutter > 0.0 {
        ui.add_space(6.0);
    }
    padded(ui, gutter, |ui| {
        ui.horizontal(|ui| {
            let title = |s: &atomics_ui::SessionRow| {
                if s.pending_approvals > 0 { format!("◆ {}", s.title) } else { s.title.clone() }
            };
            let current = agents.iter().find(|s| s.id == session).map(title).unwrap_or_default();
            let width = if gutter > 0.0 { ui.available_width() } else { ui.available_width().min(320.0) };
            egui::ComboBox::from_id_salt("terminal-session")
                .selected_text(RichText::new(current).color(p.foreground))
                .width(width)
                .show_ui(ui, |ui| {
                    for agent in &agents {
                        if ui.selectable_label(agent.id == session, title(agent)).clicked() {
                            view.session = Some(agent.id.clone());
                        }
                    }
                });
        });
    });
    let session = view.session.clone().filter(|id| agents.iter().any(|s| &s.id == id)).unwrap_or(session);
    let now = view.now_ms;
    let term = &mut view.term;
    fill(ui, |ui, rect| term.draw(ui, rect, &session, &driver.remote, client, now));
}

fn chief_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let backend: &dyn Backend = &driver.remote;
    let Some(chief) = backend.session_list().into_iter().find(|s| s.chief) else {
        return empty(ui, client, "No chief is running on this computer.");
    };
    let now = view.now_ms;
    let term = &mut view.term;
    fill(ui, |ui, rect| term.draw(ui, rect, &chief.id, &driver.remote, client, now));
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

fn board_tab(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, view: &mut View) {
    let plans = backend.plan_list();
    if plans.is_empty() {
        return empty(ui, client, "No plans on this computer yet.");
    }
    view.plan = view.plan.min(plans.len() - 1);
    if plans.len() > 1 {
        let p = client.palette.clone();
        let gutter = gutter(client);
        padded(ui, gutter, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (index, row) in plans.iter().enumerate() {
                    let label = RichText::new(row.state.label(&row.tab_name)).color(row.state.color(&p));
                    if ui.selectable_label(index == view.plan, label).clicked() {
                        client.send(Command::ShowPlan(index));
                    }
                }
            })
        });
    }
    fill(ui, |ui, rect| board::draw(ui, rect, view.plan, backend, client));
}

fn plans_tab(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let plans = backend.plan_list();
    if plans.is_empty() {
        return empty(ui, client, "No plans on this computer yet.");
    }
    let p = client.palette.clone();
    if client.touch.is_some() {
        return padded(ui, gutter(client), |ui| {
            list(ui, |ui| {
                for (index, row) in plans.iter().enumerate() {
                    let tapped = list_row(ui, &p, ("plan", index), |ui| {
                        ui.label(RichText::new(row.state.label(&row.goal)).color(row.state.color(&p)).size(13.5));
                        ui.label(RichText::new(plan_facts(row)).color(p.muted).small());
                    });
                    if tapped {
                        client.send(Command::ShowPlan(index));
                    }
                }
            })
        });
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (index, row) in plans.iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                let label = RichText::new(row.state.label(&row.goal)).color(row.state.color(&p)).size(14.0);
                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    client.send(Command::ShowPlan(index));
                }
                ui.label(RichText::new(plan_facts(row)).color(p.muted).small());
            });
            ui.separator();
        }
    });
}

/// A plan's progress: its tasks done, and those waiting for a review.
fn plan_facts(row: &PlanRow) -> String {
    let mut facts = format!("{}/{} done", row.done, row.total);
    if row.attention > 0 {
        facts.push_str(&format!(" · {} need review", row.attention));
    }
    facts
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

fn diffs_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let backend: &dyn Backend = &driver.remote;
    let roots = backend.diff_roots();
    if let Some(root) = view.diff.clone().filter(|root| roots.contains(root)) {
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
    if client.touch.is_some() {
        return padded(ui, gutter(client), |ui| {
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
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (worktree, files, added, removed) in rows {
            ui.horizontal_wrapped(|ui| {
                let name = name(&worktree);
                if ui.add(egui::Button::new(RichText::new(name).size(14.0)).frame(false)).clicked() {
                    client.send(Command::ShowRoot(PathBuf::from(&worktree)));
                }
                ui.label(RichText::new(format!("+{added}")).color(p.added).small());
                ui.label(RichText::new(format!("−{removed}")).color(p.removed).small());
                ui.label(RichText::new(format!("{files} files · {worktree}")).color(p.muted).small());
            });
            ui.separator();
        }
    });
}

/// Carry out what the draw recorded, in order: a view command here, any other through the backend.
fn carry_out(driver: &mut Driver, client: &mut Client, view: &mut View) {
    for command in std::mem::take(&mut client.commands) {
        if command.is_view() {
            view_command(driver, view, command);
        } else {
            driver.remote.send(command);
        }
    }
    driver.flush();
}

/// The view commands this frame carries out ([`view_command`]); the views draw no action for the others.
pub(crate) fn carries_out(command: &Command) -> bool {
    matches!(
        command,
        Command::Activate { open_diff: true, .. }
            | Command::OpenTaskSession(_)
            | Command::ShowTaskDiff { .. }
            | Command::ShowRoot(_)
            | Command::OpenDiff
            | Command::ToggleDiff
            | Command::CloseDiff
            | Command::SetPanel(PanelMode::Diff | PanelMode::Plan)
            | Command::TogglePlan
            | Command::ShowPlan(_)
            | Command::ViewPlanHistory(_)
            | Command::EditingPlan
            | Command::Toast(_)
    )
}

fn view_command(driver: &mut Driver, view: &mut View, command: Command) {
    let remote = &driver.remote;
    let session_root = |id: &str| remote.session_list().into_iter().find(|s| s.id == id).map(|s| s.root);
    let open = match command {
        Command::Activate { session, open_diff: true } | Command::OpenTaskSession(session) => session_root(&session),
        Command::ShowTaskDiff { plan, task } => remote
            .board(plan)
            .and_then(|b| b.plan.tasks.get(task).and_then(|t| t.session.clone()))
            .and_then(|session| session_root(&session)),
        Command::ShowRoot(root) => Some(root),
        Command::OpenDiff | Command::ToggleDiff | Command::SetPanel(PanelMode::Diff) => {
            view.tab = Tab::Diffs;
            None
        }
        Command::CloseDiff => {
            if let Some(old) = view.diff.take() {
                remote.unwant_diff(&old);
            }
            None
        }
        Command::SetPanel(PanelMode::Plan) | Command::TogglePlan => {
            view.tab = Tab::Board;
            None
        }
        Command::ShowPlan(index) | Command::ViewPlanHistory(index) => {
            view.plan = index;
            view.tab = Tab::Board;
            None
        }
        Command::Toast(text) => {
            remote.say(text);
            None
        }
        _ => None,
    };
    if let Some(root) = open {
        if let Some(old) = view.diff.replace(root.clone())
            && old != root
        {
            remote.unwant_diff(&old);
        }
        remote.want_diff(&root);
        view.tab = Tab::Diffs;
    }
}

#[cfg(test)]
mod tests;
