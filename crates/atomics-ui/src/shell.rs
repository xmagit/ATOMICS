//! The window's frame: the top bar over the steer bar, the sidebar, and between them the location line
//! ([`nav`]) over the fleet's tiles, the right panel (the plan board or a worktree's diff) and the chief,
//! docked beside the agents or floating over them. The platform draws it through a [`Platform`]: it lends
//! each piece the backend and its client, and carries out what that piece recorded before the next one
//! draws, as the desktop did before the frame moved.
//! The remaining platform items (ticket integration, GitHub and VM controls, and the
//! Tickets panel) are the trait's hooks; a platform that leaves a hook as
//! it is does without that item. Everything else asks [`Backend::available`] whether to draw an action.

use std::path::{Path, PathBuf};

use atomics_core::layout::{self, DiffPlacement, Layout, LayoutConfig};
use atomics_core::plan::PlanStatus;
use atomics_core::session::{AgentStatus, pick_tiled};
use egui::{Rect, RichText, Sense, Stroke, vec2};

use crate::chief::{self, VoiceState};
use crate::terminal_surface::TerminalSurface;
use crate::topbar::{self, measure, record_item, slot};
use crate::{Backend, Client, Command, PanelMode, board, diff_panel, diff_view, fleet, nav, sidebar, switcher, tabs};

/// The height of the tab strip over the tiles, when the sessions do not all fit as tiles.
pub const TAB_HEIGHT: f32 = 30.0;

/// What the frame asks of the platform it is drawn on. Only [`draw`](Platform::draw) is required; every
/// other method is a hook for an item of the host's own, which a platform without it leaves out.
pub trait Platform {
    /// Draws `draw` over the host's backend and this UI's client, with the platform's terminals, and then
    /// carries out the commands it recorded, so that what is drawn after it sees their effect.
    fn draw<R>(&mut self, draw: impl FnOnce(&dyn Backend, &mut Client, &mut dyn TerminalSurface) -> R) -> R;

    /// What the microphone does, for the chief's pill; `None` on a platform without one.
    fn voice_state(&self) -> Option<VoiceState> {
        None
    }

    /// The frame's layout, once it is worked out and before the sidebar draws.
    fn laid_out(&mut self, _layout: &Layout) {}

    /// Who makes this build, for the welcome under the name.
    fn byline(&self) -> Option<&'static str> {
        None
    }

    /// The host's items after the top bar's Plan button; in the Actions menu when `in_menu`.
    fn top_bar_items(&mut self, _ui: &mut egui::Ui, _in_menu: bool) {}

    /// The host's section above the sidebar's agents.
    fn sidebar_tickets(&mut self, _ui: &mut egui::Ui) {}

    /// The right panel in [`PanelMode::Tickets`], which only the host has.
    fn panel_tickets(&mut self, _ui: &mut egui::Ui, _rect: Rect) {}
}

/// Reads from the backend and the client, recording nothing.
fn read<R>(platform: &mut impl Platform, read: impl FnOnce(&dyn Backend, &Client) -> R) -> R {
    platform.draw(|backend, client, _| read(backend, client))
}

/// The whole window: the top bar, the steer bar, the sidebar, the chief's dock, the tiles with the right
/// panel, and the floating chief.
pub fn frame(ui: &mut egui::Ui, platform: &mut impl Platform) {
    let p = read(platform, |_, client| client.palette.clone());
    let time = ui.input(|i| i.time);
    platform.draw(|backend, client, _| nav::track(backend, client, time));
    if read(platform, |backend, client| crate::management::setup::fills_window(backend, client)) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(p.background).inner_margin(egui::Margin::same(16)))
            .show(ui, |ui| platform.draw(|backend, client, _| crate::management::setup::page(ui, backend, client)));
        return;
    }
    let touch = read(platform, |_, client| client.touch);
    egui::Panel::top("top")
        .exact_size(touch.map_or(34.0, |touch| touch.target + 10.0))
        .frame(egui::Frame::new().fill(p.topbar).inner_margin(egui::Margin::symmetric(10, 4)))
        .show(ui, |ui| top_bar(ui, platform));
    let steers =
        read(platform, |backend, _| backend.available(&Command::Steer { sessions: Vec::new(), text: String::new() }));
    if steers {
        egui::Panel::bottom("steer")
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 7)))
            .show(ui, |ui| platform.draw(|backend, client, _| chief::steer_bar(ui, backend, client)));
    }

    let area = ui.available_rect_before_wrap();
    let nav_height = nav::height(touch);
    let tab_height = touch.map_or(TAB_HEIGHT, |touch| touch.target + 6.0);
    let (layout, visible, completed, history_board, diff_open, sidebar_width) = read(platform, |backend, client| {
        let hints = backend.view_hints();
        let history_board = hints.plan_view
            && hints.panel == PanelMode::Plan
            && backend.plan_row(hints.selected_plan).is_some_and(|plan| plan.status != PlanStatus::Open);
        let visible = if history_board { Vec::new() } else { backend.visible_sessions() };
        let completed = if history_board { 0 } else { backend.completed_tasks().len() };
        // The diff with nothing to show still shows beside agents, saying how to pick one; an empty window keeps
        // its welcome.
        let diff_open = !history_board
            && hints.diff_open
            && (hints.diff_target.is_some()
                || client.pr_view.is_some()
                || matches!(hints.panel, PanelMode::Plan | PanelMode::Tickets)
                || !visible.is_empty()
                || completed > 0);
        let config = backend.ui_config().layout;
        let sidebar_width = config.sidebar_width;
        let layout_config = LayoutConfig { diff_width: client.split, ..config };
        let dock_width = chief::dock_width(&backend.chief(), area.width(), &layout_config);
        let compute = |height, diff_open| {
            layout::compute(area.width(), height, visible.len(), completed, diff_open, dock_width, &layout_config)
        };
        let placed = |diff_open| {
            // The location line takes the top of the agents' area.
            let mut layout = compute(area.height() - nav_height, diff_open);
            if layout.tabs {
                layout = compute(area.height() - nav_height - tab_height, diff_open);
                layout.tabs = visible.len() > layout.tiled || layout.tiled == 0;
            }
            layout
        };
        let mut layout = placed(diff_open);
        // A keyboard leaves room for one of the panel and the terminal: a finger typing keeps the terminal, or the
        // keyboard would go with it, and the terminal come back without it.
        if client.typing && layout.diff == DiffPlacement::Full {
            layout = placed(false);
        }
        (layout, visible, completed, history_board, diff_open, sidebar_width)
    });

    platform.laid_out(&layout);
    platform.draw(|_, client, _| {
        client.sidebar_hidden = !layout.sidebar;
        client.drawer &= client.sidebar_hidden;
    });
    if layout.sidebar {
        egui::Panel::left("sidebar")
            .exact_size(sidebar_width)
            .resizable(false)
            .frame(egui::Frame::new().fill(p.sidebar).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| self::sidebar(ui, platform));
    }

    let chief_rect = (layout.dock_width > 0.0).then(|| {
        chief::dock(ui, layout.dock_width, &p, |ui| {
            platform.draw(|backend, client, terminals| chief::draw(ui, backend, client, terminals))
        })
    });

    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
        let mut rect = ui.max_rect();
        let line = rect.with_max_y(rect.top() + nav_height);
        platform.draw(|backend, client, _| nav::location(ui, line, backend, client));
        rect.min.y = line.bottom();
        if history_board {
            plan_board(ui, rect, platform);
            return;
        }
        let tiled: Vec<usize> = read(platform, |backend, _| {
            let sessions = backend.session_list();
            let statuses: Vec<AgentStatus> = visible.iter().map(|&i| sessions[i].status).collect();
            let active = backend.view_hints().active.and_then(|id| sessions.iter().position(|s| s.id == id));
            let active = active.and_then(|a| visible.iter().position(|&i| i == a));
            pick_tiled(&statuses, active, layout.tiled).into_iter().map(|k| visible[k]).collect()
        });
        if layout.tabs {
            let strip = rect.with_max_y(rect.top() + tab_height);
            platform.draw(|backend, client, _| fleet::tab_strip(ui, strip, &tiled, &visible, backend, client));
            rect.min.y += tab_height;
        }
        let byline = platform.byline();
        if visible.is_empty() && completed == 0 && !diff_open {
            platform.draw(|backend, client, _| welcome(ui, rect, byline, backend, client));
            return;
        }
        let (diff_rect, term_rect) = match layout.diff {
            DiffPlacement::Right { width } => {
                (Some(rect.with_min_x(rect.right() - width)), Some(rect.with_max_x(rect.right() - width)))
            }
            DiffPlacement::Top { height } => {
                (Some(rect.with_max_y(rect.top() + height)), Some(rect.with_min_y(rect.top() + height)))
            }
            DiffPlacement::Full => (Some(rect), None),
            DiffPlacement::Hidden => (None, Some(rect)),
        };
        if let Some(term_rect) = term_rect {
            if visible.is_empty() && layout.completed == 0 {
                // The board may be dragged over the welcome screen; a sliver too narrow for it stays empty.
                if term_rect.width() >= WELCOME_MIN_WIDTH {
                    platform.draw(|backend, client, _| welcome(ui, term_rect, byline, backend, client));
                }
            } else {
                platform.draw(|backend, client, terminals| {
                    fleet::tiles(ui, term_rect, &layout, &tiled, backend, client, terminals)
                });
            }
        }
        if let Some(diff_rect) = diff_rect {
            right_panel(ui, diff_rect, platform);
            if let DiffPlacement::Right { width } = layout.diff {
                platform.draw(|_, client, _| split_divider(ui, diff_rect, width, client));
            }
            let edge = match layout.diff {
                DiffPlacement::Right { .. } => Some([diff_rect.left_top(), diff_rect.left_bottom()]),
                DiffPlacement::Top { .. } => Some([diff_rect.left_bottom(), diff_rect.right_bottom()]),
                _ => None,
            };
            if let Some(edge) = edge {
                ui.painter().line_segment(edge, Stroke::new(1.0, p.separator));
            }
        }
    });

    platform.draw(|backend, client, _| crate::machine::window(ui.ctx(), backend, client));
    if let Some(rect) = chief_rect {
        platform.draw(|_, client, _| chief::divider(ui, rect, client));
    }
    if read(platform, |backend, _| backend.chief().open) && layout.dock_width == 0.0 {
        chief::window(ui.ctx(), &p, touch.map(|_| area), |ui| {
            platform.draw(|backend, client, terminals| chief::draw(ui, backend, client, terminals))
        });
    }
    if read(platform, |_, client| client.drawer) {
        drawer(ui.ctx(), area, platform, &p);
    } else {
        drawer_closed(ui.ctx());
    }
}

/// The widest the sidebar is as a drawer, and how many seconds it takes to come in.
const DRAWER_WIDTH: f32 = 320.0;
const DRAWER_TIME: f32 = 0.16;

/// For a finger, the sidebar where it does not fit: over the rest, opened from the top bar. A tap anywhere beside it
/// closes it (its button's place in the top bar too, under the shade), and so do Escape (a menu open in it takes Escape
/// first), the platform's Back, and going somewhere from it ([`Client::send`]). While it is open the keyboard's focus
/// stays in it: what had the focus behind it lets go as it opens, and nothing behind the shade can take the focus (a
/// modal layer).
fn drawer(ctx: &egui::Context, area: Rect, platform: &mut impl Platform, p: &crate::theme::Palette) {
    let width = DRAWER_WIDTH.min(area.width() * 0.85);
    let opening = ctx.data_mut(|d| !std::mem::replace(d.get_temp_mut_or_default::<bool>(drawer_id("open")), true));
    if opening {
        ctx.memory_mut(|m| {
            if let Some(focused) = m.focused() {
                m.surrender_focus(focused);
            }
        });
    }
    let time = if read(platform, |_, client| client.reduce_motion) { 0.0 } else { DRAWER_TIME };
    let out = ctx.animate_bool_with_time(drawer_id("out"), true, time);
    let screen = ctx.viewport_rect();
    let shade = egui::Area::new(egui::Id::NULL.with("drawer-shade"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            // A tap's, not a stop for Tab.
            let (rect, response) = ui.allocate_exact_size(screen.size(), Sense::CLICK);
            ui.painter().rect_filled(rect, 0.0, p.background.gamma_multiply(0.6 * out));
            response.clicked()
        });
    let at = area.min - vec2(width * (1.0 - out), 0.0);
    // Off the screen's edge while it slides in: egui would otherwise keep the area on screen, and nothing would move.
    let shown = egui::Area::new(egui::Id::NULL.with("drawer"))
        .order(egui::Order::Foreground)
        .fixed_pos(at)
        .constrain(false)
        .show(ctx, |ui| {
            egui::Frame::new().fill(p.sidebar).inner_margin(egui::Margin::symmetric(10, 4)).show(ui, |ui| {
                ui.set_width(width - 20.0);
                ui.set_height(area.height() - 8.0);
                self::sidebar(ui, platform);
            });
        });
    ctx.move_to_top(shown.response.layer_id);
    ctx.memory_mut(|m| m.set_modal_layer(shade.response.layer_id));
    let escape =
        !egui::Popup::is_any_open(ctx) && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
    if shade.inner || escape {
        platform.draw(|_, client, _| client.drawer = false);
    }
    if !read(platform, |_, client| client.drawer) {
        drawer_closed(ctx);
    }
}

fn drawer_id(what: &str) -> egui::Id {
    egui::Id::NULL.with("drawer").with(what)
}

/// The drawer is closed, however that came about (a tap beside it, Escape or Back, its button, going somewhere from it,
/// or room for the sidebar after a rotation): the next time it opens, it comes in from the side again.
fn drawer_closed(ctx: &egui::Context) {
    ctx.animate_bool_with_time(drawer_id("out"), false, 0.0);
    ctx.data_mut(|d| d.remove::<bool>(drawer_id("open")));
}

/// What opens the [`drawer`], with a dot while something in it waits for the user.
fn drawer_button(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if client.touch.is_none() || !client.sidebar_hidden {
        return;
    }
    let p = client.palette.clone();
    let size = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let color = if client.drawer { p.accent } else { p.foreground };
    let c = rect.center();
    for line in [-5.0, 0.0, 5.0] {
        ui.painter().hline((c.x - 8.0)..=(c.x + 8.0), c.y + line, Stroke::new(1.5, color));
    }
    if backend.session_list().iter().any(|s| s.status.needs_attention() || s.pending_approvals > 0) {
        ui.painter().circle_filled(c + vec2(9.0, -8.0), 3.5, p.warning);
    }
    if response.on_hover_text("Agents").clicked() {
        client.drawer = !client.drawer;
    }
}

/// The least width the welcome screen is drawn in.
const WELCOME_MIN_WIDTH: f32 = 360.0;

/// The divider between the tiles and the right panel `rect`, `width` wide: dragging it gives the panel more
/// or less room, a double click its default width again.
fn split_divider(ui: &mut egui::Ui, rect: Rect, width: f32, client: &mut Client) {
    let reach = client.touch.map_or(8.0, |_| 24.0);
    let handle = Rect::from_center_size(rect.left_center(), vec2(reach, rect.height()));
    let response = ui.interact(handle, ui.scope_id().with("divider"), Sense::click_and_drag());
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        let line = Rect::from_center_size(handle.center(), vec2(2.0, handle.height()));
        ui.painter().rect_filled(line, 0.0, client.palette.accent.gamma_multiply(0.6));
    }
    if response.dragged() {
        client.split = Some(width - response.drag_delta().x);
    }
    if response.double_clicked() {
        client.split = None;
    }
}

/// The selected plan's board in `rect`.
pub fn plan_board(ui: &mut egui::Ui, rect: Rect, platform: &mut impl Platform) {
    platform.draw(|backend, client, _| {
        let index = backend.view_hints().selected_plan;
        board::draw(ui, rect, index, backend, client);
    });
}

/// The right panel in `rect`: the plan board, the host's Tickets panel, a pull request, or the diff target's diff or editor
/// with the changed-files column beside it when there is room.
pub fn right_panel(ui: &mut egui::Ui, rect: Rect, platform: &mut impl Platform) {
    platform.draw(|backend, client, _| diff_panel::sync(backend, client));
    let hints = read(platform, |backend, _| backend.view_hints());
    match hints.panel {
        PanelMode::Plan => return plan_board(ui, rect, platform),
        PanelMode::Tickets => {
            if read(platform, |backend, _| backend.tickets().is_some()) {
                return platform.draw(|backend, client, _| crate::tickets::panel(ui, rect, backend, client));
            }
            return platform.panel_tickets(ui, rect);
        }
        PanelMode::Diff
            if platform.draw(|backend, client, _| crate::pull_request::panel(ui, rect, backend, client)) =>
        {
            return;
        }
        _ => {}
    }
    let Some(root) = hints.diff_target else {
        return platform.draw(|backend, client, _| empty_panel(ui, rect, backend, client));
    };
    let editor = hints.panel == PanelMode::Editor;
    // What changed sits right next to the diff/editor, in both modes, where a click can open it.
    let open = |path: String, line: Option<u32>| match editor {
        true => Command::Edit { root: root.clone(), path: Some(path), line },
        false => Command::OpenFile { root: root.clone(), path },
    };
    let opens = read(platform, |backend, _| backend.available(&open(String::new(), None)));
    let rect = if rect.width() >= 760.0 && opens {
        let column = rect.with_max_x(rect.left() + 250.0);
        let current = match editor {
            true => read(platform, |backend, _| {
                backend.editors().into_iter().find(|e| Path::new(&e.root) == root).and_then(|e| e.file)
            }),
            false => read(platform, |_, client| client.diff_views.get(&root).and_then(|d| d.focused.clone())),
        };
        platform.draw(|_, client, _| {
            let p = client.palette.clone();
            let state = client.diff_views.entry(root.clone()).or_default();
            if let Some((path, line)) = diff_view::changed_files(ui, column, state, current.as_deref(), &p) {
                client.send(open(path, line));
            }
        });
        rect.with_min_x(column.right())
    } else {
        rect
    };
    if editor {
        return platform
            .draw(|backend, client, terminals| crate::editor::draw(ui, rect, &root, backend, client, terminals));
    }
    platform.draw(|backend, client, _| diff_panel::draw(ui, rect, &root, backend, client));
}

/// The panel's diff while no session is active and no worktree is picked: what would show there, and how
/// to get it.
fn empty_panel(ui: &mut egui::Ui, rect: Rect, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("empty-panel")
            .max_rect(Rect::from_center_size(rect.center(), vec2((rect.width() - 32.0).clamp(0.0, 380.0), 160.0))),
    );
    child.set_clip_rect(rect);
    child.vertical_centered(|ui| {
        ui.label(RichText::new("No diff to show").color(p.foreground).size(14.0));
        ui.add_space(6.0);
        let mut ways = vec!["Pick an agent in the sidebar or a tile", "click a task's card on the board"];
        if backend.available(&Command::ShowRoot(PathBuf::new())) && !backend.repos().is_empty() {
            ways.push("or a worktree in the EXPLORER");
        }
        let text = format!("{}: its changes against the base show here.", ways.join(", "));
        ui.label(RichText::new(text).color(p.muted).size(12.0));
        if backend.available(&Command::ToggleView(PanelMode::Plan))
            && backend.plan_row(backend.view_hints().selected_plan).is_some()
        {
            ui.add_space(8.0);
            if ui.button("Show the board").clicked() {
                client.send(Command::ToggleView(PanelMode::Plan));
            }
        }
    });
}

/// The sidebar: the host's USAGE, its TICKETS and the AGENTS over the EXPLORER, which takes the rest while a
/// worktree's files show. A backend with no projects and no way to open one has no EXPLORER.
pub fn sidebar(ui: &mut egui::Ui, platform: &mut impl Platform) {
    let (files_root, explorer) = read(platform, |backend, _| {
        let explorer = !backend.repos().is_empty() || backend.available(&Command::PickProject);
        (backend.view_hints().diff_target, explorer)
    });
    platform.draw(|backend, client, _| crate::resources::section(ui, backend, &client.palette));
    let total = ui.available_height();
    let sessions_height = if explorer && files_root.is_some() { (total * 0.46).max(150.0) } else { total };
    ui.add_space(6.0);
    egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(
        ui,
        |ui| {
            if read(platform, |backend, _| backend.tickets().is_some()) {
                platform.draw(|backend, client, _| crate::tickets::sidebar(ui, backend, client));
            } else {
                platform.sidebar_tickets(ui);
            }
            platform.draw(|backend, client, _| sidebar::sessions(ui, backend, client));
        },
    );

    if explorer {
        ui.add_space(4.0);
        ui.separator();
        platform.draw(|backend, client, _| sidebar::explorer(ui, backend, client));
    }
}

/// How much of the brand the top bar shows, widest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrandSize {
    /// The mark, the name, how many agents run and what the sandbox VMs use.
    Full,
    /// The mark and the name.
    Name,
    /// The mark alone.
    Mark,
}

/// Where the top bar puts its items in `width` (the bar's inner width): the chief's pill is always in the
/// middle, at one of its fixed widths ([`chief::pill_width`]), so that it never moves while the window keeps
/// its size, whatever the chief, the plans or the notices do. The brand and the navigation share the half on
/// its left, the actions the half on its right. The pill's size follows the bar's width alone: its words go
/// under [`FULL_PILL_BAR`], its name under [`NAMED_PILL_BAR`]. Around it the load gauge leaves the row of
/// actions first, the brand's agent count gives way to the Boards dropdown, the actions fold into a menu, beside
/// which the gauge comes back wherever it fits, and the brand keeps only its mark. Every item keeps its natural width; the navigation folds the tabs it has no room for into the
/// dropdown's menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TopBarPlan {
    pub brand: BrandSize,
    pub chief: chief::PillSize,
    /// The view switcher without its names.
    pub compact_switcher: bool,
    pub actions_menu: bool,
    /// The load gauge, which opens Machine, in the bar: among the actions, or beside their folded menu.
    pub show_load: bool,
    /// An update under way stays in the row, beside the folded Actions menu.
    pub update_beside: bool,
}

/// The bar's least inner width for the full pill, and for the pill with its name.
pub const FULL_PILL_BAR: f32 = 980.0;
pub const NAMED_PILL_BAR: f32 = 600.0;

/// The widths [`TopBarPlan::choose`] decides by, measured with the fonts the bar draws with.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TopBarWidths {
    pub width: f32,
    pub gap: f32,
    /// The pill at each [`chief::PillSize`], widest first.
    pub chief: [f32; 3],
    /// The brand at each [`BrandSize`], widest first.
    pub brand: [f32; 3],
    /// The Boards dropdown as it wants to be, naming the board on screen.
    pub boards: f32,
    /// The navigation's least width: the dropdown with no more than its word and how many boards need the user.
    pub navigation: f32,
    /// The view switcher with its names, and without.
    pub switcher: [f32; 2],
    pub controls: f32,
    pub controls_with_load: f32,
    pub actions_menu: f32,
    /// The load gauge alone, nothing while the host reports no load.
    pub load: f32,
    /// An update under way, which stays beside the folded menu where it fits. Nothing while none is.
    pub update: f32,
}

impl TopBarPlan {
    pub fn choose(w: &TopBarWidths) -> TopBarPlan {
        use chief::PillSize;
        // Each side of the pill has the same room: half the bar less half the pill and a gap.
        let side = |chief: PillSize| (w.width - w.chief[chief as usize]) / 2.0 - w.gap;
        let left = |brand: BrandSize, navigation: f32, chief: PillSize| {
            w.brand[brand as usize] + w.gap + navigation <= side(chief)
        };
        // The pill's size follows the bar's width alone, so that it never changes while the window keeps its
        // size: a board that starts needing the user, or an item the host adds, moves nothing but its neighbours.
        let chief = if w.width >= FULL_PILL_BAR {
            PillSize::Full
        } else if w.width >= NAMED_PILL_BAR {
            PillSize::Name
        } else {
            PillSize::Mark
        };
        // The switcher stays in the bar before the actions; they fold first, then its names go.
        let right = |rest: f32, switcher: f32| switcher + if switcher > 0.0 { w.gap } else { 0.0 } + rest;
        let actions_menu = right(w.controls, w.switcher[0]) > side(chief);
        let compact_switcher = actions_menu && right(w.actions_menu, w.switcher[0]) > side(chief);
        let brand = if left(BrandSize::Full, w.boards, chief) {
            BrandSize::Full
        } else if left(BrandSize::Name, w.navigation, chief) {
            BrandSize::Name
        } else {
            BrandSize::Mark
        };
        let switcher = w.switcher[usize::from(compact_switcher)];
        // The gauge is a few characters wide and the way to Machine: once the actions fold it stays beside their
        // menu wherever it fits, before an update under way does.
        let show_load = if actions_menu {
            w.load > 0.0 && right(w.actions_menu + w.gap + w.load, switcher) <= side(chief)
        } else {
            right(w.controls_with_load, w.switcher[0]) <= side(chief)
        };
        let beside = w.actions_menu + if actions_menu && show_load { w.gap + w.load } else { 0.0 };
        let update_beside = actions_menu && w.update > 0.0 && right(beside + w.gap + w.update, switcher) <= side(chief);
        TopBarPlan { brand, chief, compact_switcher, actions_menu, show_load, update_beside }
    }
}

/// The top bar: the brand and the navigation with the notice line on the left, the chief's pill in the
/// middle and the actions on the right ([`TopBarPlan`]).
pub fn top_bar(ui: &mut egui::Ui, platform: &mut impl Platform) {
    use chief::PillSize;
    let rect = ui.available_rect_before_wrap();
    let gap = ui.spacing().item_spacing.x;
    crate::update::keep_keyboard(ui.ctx());
    let palette = read(platform, |_, client| client.palette.clone());
    let (boards, navigation) = read(platform, |backend, client| crate::boards::widths(ui, backend, &client.palette));
    let widths = TopBarWidths {
        width: rect.width(),
        gap,
        chief: [PillSize::Full, PillSize::Name, PillSize::Mark].map(|size| chief::pill_width(ui, &palette, size)),
        brand: [BrandSize::Full, BrandSize::Name, BrandSize::Mark]
            .map(|size| measure(ui, |ui| brand(ui, platform, size))),
        boards,
        navigation,
        switcher: [false, true].map(|compact| {
            measure(ui, |ui| platform.draw(|backend, client, _| switcher::draw(ui, compact, backend, client)))
        }),
        controls: measure(ui, |ui| top_bar_controls(ui, platform, false, false, true)),
        controls_with_load: measure(ui, |ui| top_bar_controls(ui, platform, true, false, true)),
        actions_menu: measure(ui, |ui| {
            ui.menu_button("Actions", |_| {});
        }),
        load: measure(ui, |ui| {
            platform.draw(|backend, client, _| {
                crate::resources::load(ui, backend, &client.palette, false);
            })
        }),
        update: measure(ui, |ui| platform.draw(|backend, client, _| crate::update::status(ui, backend, client))),
    };
    let plan = TopBarPlan::choose(&widths);
    let brand_width = widths.brand[plan.brand as usize];
    let chief_width = widths.chief[plan.chief as usize];
    let chief_left = (rect.center().x - chief_width / 2.0).round();
    let chief_rect = Rect::from_x_y_ranges(chief_left..=chief_left + chief_width, rect.y_range());
    let actions = Rect::from_x_y_ranges(chief_rect.right() + gap..=rect.right(), rect.y_range());
    let tabs_left = rect.left() + brand_width + gap;
    let navigation = Rect::from_x_y_ranges(tabs_left..=(chief_rect.left() - gap).max(tabs_left), rect.y_range());
    slot(ui, "brand", rect.with_max_x(rect.left() + brand_width), |ui| brand(ui, platform, plan.brand));
    slot(ui, "chief", chief_rect, |ui| chief_pill(ui, platform, plan.chief));
    // The actions keep to the bar's right end, and so does their menu.
    let switcher = widths.switcher[usize::from(plan.compact_switcher)];
    let used = switcher
        + if switcher > 0.0 { gap } else { 0.0 }
        + if plan.actions_menu {
            widths.actions_menu
                + if plan.update_beside { widths.update + gap } else { 0.0 }
                + if plan.show_load { widths.load + gap } else { 0.0 }
        } else if plan.show_load {
            widths.controls_with_load
        } else {
            widths.controls
        };
    slot(ui, "actions", actions.with_min_x((actions.right() - used).max(actions.left())), |ui| {
        platform.draw(|backend, client, _| switcher::draw(ui, plan.compact_switcher, backend, client));
        if plan.actions_menu {
            if plan.update_beside {
                platform.draw(|backend, client, _| crate::update::status(ui, backend, client));
            }
            if plan.show_load {
                platform.draw(|backend, client, _| {
                    if crate::resources::load(ui, backend, &client.palette, false) {
                        client.machine_open = !client.machine_open;
                    }
                });
            }
            let update_inside = !plan.update_beside;
            let response =
                ui.menu_button("Actions", |ui| top_bar_controls(ui, platform, false, true, update_inside)).response;
            record_item(ui, "Actions", &response);
            if update_inside {
                read(platform, |backend, _| crate::update::mark_menu(ui, &response, backend, &palette));
            }
        } else {
            top_bar_controls(ui, platform, plan.show_load, false, true);
        }
    });
    slot(ui, "navigation", navigation, |ui| {
        platform.draw(|backend, client, _| tabs::view_tabs(ui, backend, client));
        platform.draw(|backend, client, _| topbar::notice(ui, backend, client));
    });
    ui.allocate_rect(rect, Sense::hover());
    platform.draw(|backend, client, _| crate::update::countdown(ui.ctx(), backend, client));
}

fn brand(ui: &mut egui::Ui, platform: &mut impl Platform, size: BrandSize) {
    platform.draw(|backend, client, _| {
        drawer_button(ui, backend, client);
        topbar::brand(ui, size, backend, client);
    });
}

fn chief_pill(ui: &mut egui::Ui, platform: &mut impl Platform, size: chief::PillSize) {
    let voice = platform.voice_state();
    platform.draw(|backend, client, _| {
        chief::pill(ui, size, voice, backend, client);
    });
}

/// The bar's actions, in the order a row shows them: `+ New`, the host's items, Project, the host's load
/// gauge with `show_load`, and its settings. Each button is a draw of its own, so a click on one reaches the
/// buttons drawn after it in the same frame. The view switcher stands before them, and stays in the bar when
/// they fold into the Actions menu. `update_item` is false when an update under way stays beside that menu.
fn top_bar_controls(
    ui: &mut egui::Ui,
    platform: &mut impl Platform,
    show_load: bool,
    in_menu: bool,
    update_item: bool,
) {
    platform.draw(|backend, client, _| topbar::new_button(ui, in_menu, backend, client));
    platform.draw(|backend, client, _| topbar::explorer_menu(ui, backend, client));
    platform.top_bar_items(ui, in_menu);
    if update_item {
        platform.draw(|backend, client, _| crate::update::top_bar_item(ui, backend, client));
    }
    platform.draw(|backend, client, _| crate::pull_request::menu(ui, backend, client));
    if read(platform, |backend, _| backend.management().is_some() || backend.available(&Command::PickProject)) {
        let response = ui
            .menu_button("Project", |ui| platform.draw(|backend, client, _| topbar::project_menu(ui, backend, client)))
            .response;
        record_item(ui, "Project", &response);
        response.on_hover_text("Open a project or a workspace");
    }
    if show_load || in_menu {
        platform.draw(|backend, client, _| {
            if crate::resources::load(ui, backend, &client.palette, in_menu) {
                client.machine_open = !client.machine_open;
                if in_menu {
                    ui.close();
                }
            }
        });
    }
    if in_menu {
        platform.draw(|backend, client, _| crate::machine::button(ui, backend, client));
    }
    platform.draw(|backend, client, _| crate::management::buttons(ui, backend, client));
}

/// What an empty frame shows in `rect`: the name with the platform's `byline`, and a button for each
/// configured agent where the backend can start one: in the folder the host started in, or on the Ungrouped
/// tab in a scratch folder of its own.
pub fn welcome(ui: &mut egui::Ui, rect: Rect, byline: Option<&str>, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("welcome")
            .max_rect(Rect::from_center_size(rect.center(), vec2(rect.width().min(460.0), 260.0))),
    );
    child.vertical_centered(|ui| {
        ui.label(RichText::new("ATOMICS").size(28.0).strong().color(p.accent));
        ui.label(RichText::new("Emacs, but for agents").color(p.muted));
        if let Some(byline) = byline {
            ui.label(RichText::new(byline).color(p.muted).size(12.0));
        }
        ui.add_space(14.0);
        let folder: Option<PathBuf> = if backend.view_hints().ungrouped { None } else { backend.launch_dir() };
        let start = |agent: String| Command::Start { agent, folder: folder.clone() };
        let can_start = backend.available(&start(String::new()));
        ui.horizontal(|ui| {
            if !can_start {
                return;
            }
            for agent in backend.ui_config().agents {
                if ui.button(format!("Start {agent}")).clicked() {
                    client.send(start(agent));
                }
            }
        });
        ui.add_space(6.0);
        if can_start {
            let place = match &folder {
                Some(folder) => format!("in {}", backend.display(folder)),
                None => "each in a scratch folder of its own, outside any project".into(),
            };
            ui.label(RichText::new(place).color(p.muted).size(12.0));
        }
        ui.add_space(14.0);
        let reviews = backend.available(&Command::SendReview(PathBuf::new()));
        let hint = if reviews {
            "Click an agent's title to see its worktree's diff against main.\nComment on any line and send the review straight to the agent."
        } else {
            "Click an agent's title to see its worktree's diff against main."
        };
        ui.label(RichText::new(hint).color(p.muted).size(12.0));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chief::PillSize;

    /// What a bar holds, measured roughly as the desktop draws it, with boards that need the user.
    fn widths(width: f32) -> TopBarWidths {
        TopBarWidths {
            width,
            gap: 7.0,
            chief: [214.0, 98.0, 56.0],
            brand: [164.0, 99.0, 33.0],
            boards: 453.0,
            navigation: 138.0,
            switcher: [190.0, 110.0],
            controls: 226.0,
            controls_with_load: 445.0,
            actions_menu: 65.0,
            load: 52.0,
            update: 0.0,
        }
    }

    /// A busy bar at a laptop's width, as a user with four agents and a board open had it: the actions fold
    /// into their menu, and the gauge that opens Machine stays beside it, before an update under way.
    #[test]
    fn the_load_gauge_stays_beside_the_folded_actions_in_a_busy_bar() {
        let busy = TopBarWidths {
            width: 1500.0,
            gap: 7.0,
            chief: [214.0, 98.0, 56.0],
            // ATOMICS with four agents, and the Boards button at its 360-point cap.
            brand: [190.0, 99.0, 33.0],
            boards: 360.0,
            navigation: 138.0,
            switcher: [190.0, 110.0],
            // + New, the explorer, the host's items, pull requests, Project and settings.
            controls: 520.0,
            controls_with_load: 579.0,
            actions_menu: 65.0,
            load: 52.0,
            update: 0.0,
        };
        let plan = TopBarPlan::choose(&busy);
        assert_eq!(plan.chief, PillSize::Full);
        assert!(plan.actions_menu && plan.show_load, "{plan:?}");
        // At any pill size, wherever the gauge fits beside the menu it is there.
        for width in (460..1500).step_by(5).map(|w| w as f32) {
            let plan = TopBarPlan::choose(&TopBarWidths { width, ..busy });
            let side = (width - busy.chief[plan.chief as usize]) / 2.0 - 7.0;
            let switcher = if plan.compact_switcher { 110.0 } else { 190.0 };
            if plan.actions_menu {
                assert_eq!(plan.show_load, switcher + 7.0 + 65.0 + 7.0 + 52.0 <= side, "{width}: {plan:?}");
            }
        }
        // Short of room for both, the gauge outranks an update under way, which the menu's mark then shows.
        let short = TopBarPlan::choose(&TopBarWidths { width: 1000.0, update: 130.0, ..busy });
        assert!(short.actions_menu && short.show_load && !short.update_beside, "{short:?}");
        let roomy = TopBarPlan::choose(&TopBarWidths { update: 130.0, ..busy });
        assert!(roomy.show_load && roomy.update_beside, "{roomy:?}");
        // A host that reports no load has no gauge to keep.
        assert!(!TopBarPlan::choose(&TopBarWidths { load: 0.0, ..busy }).show_load);
    }

    #[test]
    fn an_update_under_way_stays_beside_the_folded_actions_only_where_it_fits() {
        // No gauge to keep: it would come before the update (`the_load_gauge_stays_beside_the_folded_actions…`).
        let with = |width, update| TopBarPlan::choose(&TopBarWidths { update, load: 0.0, ..widths(width) });
        // Nothing under way, and nothing to keep.
        assert!(with(1000.0, 0.0).actions_menu && !with(1000.0, 0.0).update_beside);
        // Folded with room for it.
        assert!(with(1000.0, 100.0).actions_menu && with(1000.0, 100.0).update_beside);
        // Folded without: the menu's mark says it instead, and the rest of the plan does not change for it.
        let narrow = with(700.0, 100.0);
        assert!(narrow.actions_menu && !narrow.update_beside);
        assert_eq!(narrow, with(700.0, 0.0));
        // Unfolded it is one of the controls.
        assert!(!with(6000.0, 100.0).actions_menu && !with(6000.0, 100.0).update_beside);
    }

    #[test]
    fn the_bar_folds_in_one_order_as_it_narrows_and_the_pill_follows_the_width_alone() {
        let rank = |plan: &TopBarPlan| {
            (plan.chief as u8, plan.brand as u8, u8::from(plan.actions_menu), u8::from(plan.compact_switcher))
        };
        let mut last = TopBarPlan::choose(&widths(6000.0));
        assert_eq!(
            last,
            TopBarPlan {
                brand: BrandSize::Full,
                chief: PillSize::Full,
                compact_switcher: false,
                actions_menu: false,
                show_load: true,
                update_beside: false
            }
        );
        for width in (460..6000).rev().step_by(5).map(|w| w as f32) {
            let plan = TopBarPlan::choose(&widths(width));
            let (before, now) = (rank(&last), rank(&plan));
            // The pill and the brand only ever shrink. The actions and the switcher fold in order while the
            // pill keeps its size; where it shrinks, the room it leaves may bring the actions back.
            assert!(now.0 >= before.0 && now.1 >= before.1, "{width}: {last:?} -> {plan:?}");
            assert!(now.0 != before.0 || (now.2, now.3) >= (before.2, before.3), "{width}: {last:?} -> {plan:?}");
            // Whatever the bar holds, the pill's size is the width's.
            let empty =
                TopBarWidths { boards: 0.0, navigation: 40.0, controls: 100.0, switcher: [0.0; 2], ..widths(width) };
            assert_eq!(TopBarPlan::choose(&empty).chief, plan.chief, "{width}");
            // What is drawn beside the pill fits its half.
            let side = (width - widths(width).chief[plan.chief as usize]) / 2.0 - 7.0;
            let switcher = if plan.compact_switcher { 110.0 } else { 190.0 };
            let actions = switcher
                + 7.0
                + match (plan.actions_menu, plan.show_load) {
                    (true, true) => 65.0 + 7.0 + 52.0,
                    (true, false) => 65.0,
                    (false, true) => 445.0,
                    (false, false) => 226.0,
                };
            assert!(actions <= side, "{width}: actions {actions} in {side}");
            assert!(widths(width).brand[plan.brand as usize] + 7.0 + 138.0 <= side, "{width}: {plan:?}");
            last = plan;
        }
        assert_eq!(last.chief, PillSize::Mark);
        assert!(last.actions_menu && last.brand == BrandSize::Mark);
    }
}
