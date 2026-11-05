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

    /// Additional client preferences, such as the browser's layout chooser.
    fn top_bar_settings(&mut self, _ui: &mut egui::Ui) {}

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
    egui::Panel::top("top")
        .exact_size(34.0)
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
        let compute = |height| {
            layout::compute(area.width(), height, visible.len(), completed, diff_open, dock_width, &layout_config)
        };
        // The location line takes the top of the agents' area.
        let mut layout = compute(area.height() - nav::HEIGHT);
        if layout.tabs {
            layout = compute(area.height() - nav::HEIGHT - TAB_HEIGHT);
            layout.tabs = visible.len() > layout.tiled || layout.tiled == 0;
        }
        (layout, visible, completed, history_board, diff_open, sidebar_width)
    });

    platform.laid_out(&layout);
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
        let line = rect.with_max_y(rect.top() + nav::HEIGHT);
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
            let strip = rect.with_max_y(rect.top() + TAB_HEIGHT);
            platform.draw(|backend, client, _| fleet::tab_strip(ui, strip, &tiled, &visible, backend, client));
            rect.min.y += TAB_HEIGHT;
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
                platform.draw(|backend, client, _| welcome(ui, term_rect, byline, backend, client));
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
        chief::window(ui.ctx(), &p, |ui| {
            platform.draw(|backend, client, terminals| chief::draw(ui, backend, client, terminals))
        });
    }
}

/// The divider between the tiles and the right panel `rect`, `width` wide: dragging it gives the panel more
/// or less room, a double click its default width again.
fn split_divider(ui: &mut egui::Ui, rect: Rect, width: f32, client: &mut Client) {
    let handle = Rect::from_center_size(rect.left_center(), vec2(8.0, rect.height()));
    let response = ui.interact(handle, ui.scope_id().with("divider"), Sense::click_and_drag());
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        ui.painter().rect_filled(handle.shrink2(vec2(3.0, 0.0)), 0.0, client.palette.accent.gamma_multiply(0.6));
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
/// under [`FULL_PILL_BAR`], its name under [`NAMED_PILL_BAR`]. Around it the load gauge goes first, the
/// brand's agent count gives way to the plans that need the user, the actions fold into a menu and the brand
/// keeps only its mark. Every item keeps
/// its natural width; the navigation folds the plans it has no room for into its own menu.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TopBarPlan {
    pub brand: BrandSize,
    pub chief: chief::PillSize,
    /// The view switcher without its names.
    pub compact_switcher: bool,
    pub actions_menu: bool,
    pub show_load: bool,
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
    /// The navigation with every plan that needs the user or runs in the row.
    pub urgent: f32,
    /// The navigation's least width: its menu, which says how many plans need the user.
    pub navigation: f32,
    /// The view switcher with its names, and without.
    pub switcher: [f32; 2],
    pub controls: f32,
    pub controls_with_load: f32,
    pub actions_menu: f32,
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
        // size: a plan that starts needing the user, or an item the host adds, moves nothing but its neighbours.
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
        let brand = if left(BrandSize::Full, w.urgent, chief) {
            BrandSize::Full
        } else if left(BrandSize::Name, w.navigation, chief) {
            BrandSize::Name
        } else {
            BrandSize::Mark
        };
        let show_load = chief == PillSize::Full && right(w.controls_with_load, w.switcher[0]) <= side(chief);
        TopBarPlan { brand, chief, compact_switcher, actions_menu, show_load }
    }
}

/// The top bar: the brand and the navigation with the notice line on the left, the chief's pill in the
/// middle and the actions on the right ([`TopBarPlan`]).
pub fn top_bar(ui: &mut egui::Ui, platform: &mut impl Platform) {
    use chief::PillSize;
    let rect = ui.available_rect_before_wrap();
    let gap = ui.spacing().item_spacing.x;
    let palette = read(platform, |_, client| client.palette.clone());
    let (urgent_label, urgent_color) = platform.draw(|backend, client, _| tabs::urgent_tabs_label(backend, client));
    let navigation = measure(ui, |ui| {
        ui.menu_button(RichText::new(&urgent_label).color(urgent_color), |_| {});
    });
    let widths = TopBarWidths {
        width: rect.width(),
        gap,
        chief: [PillSize::Full, PillSize::Name, PillSize::Mark].map(|size| chief::pill_width(ui, &palette, size)),
        brand: [BrandSize::Full, BrandSize::Name, BrandSize::Mark]
            .map(|size| measure(ui, |ui| brand(ui, platform, size))),
        urgent: platform.draw(|backend, client, _| tabs::urgent_navigation_width(ui, backend, client)).max(navigation),
        navigation,
        switcher: [false, true].map(|compact| {
            measure(ui, |ui| platform.draw(|backend, client, _| switcher::draw(ui, compact, backend, client)))
        }),
        controls: measure(ui, |ui| top_bar_controls(ui, platform, false, false)),
        controls_with_load: measure(ui, |ui| top_bar_controls(ui, platform, true, false)),
        actions_menu: measure(ui, |ui| {
            ui.menu_button("Actions", |_| {});
        }),
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
        } else if plan.show_load {
            widths.controls_with_load
        } else {
            widths.controls
        };
    slot(ui, "actions", actions.with_min_x((actions.right() - used).max(actions.left())), |ui| {
        platform.draw(|backend, client, _| switcher::draw(ui, plan.compact_switcher, backend, client));
        if plan.actions_menu {
            let response = ui.menu_button("Actions", |ui| top_bar_controls(ui, platform, false, true)).response;
            record_item(ui, "Actions", &response);
            if read(platform, |backend, _| crate::update::offered(backend)) {
                crate::update::paint_dot(ui, &response, &palette);
            }
        } else {
            top_bar_controls(ui, platform, plan.show_load, false);
        }
    });
    slot(ui, "navigation", navigation, |ui| {
        platform.draw(|backend, client, _| tabs::view_tabs(ui, backend, client));
        platform.draw(|backend, client, _| topbar::notice(ui, backend, client));
    });
    ui.allocate_rect(rect, Sense::hover());
}

fn brand(ui: &mut egui::Ui, platform: &mut impl Platform, size: BrandSize) {
    platform.draw(|backend, client, _| topbar::brand(ui, size, backend, client));
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
/// they fold into the Actions menu.
fn top_bar_controls(ui: &mut egui::Ui, platform: &mut impl Platform, show_load: bool, in_menu: bool) {
    platform.draw(|backend, client, _| topbar::new_button(ui, in_menu, backend, client));
    platform.draw(|backend, client, _| topbar::explorer_menu(ui, backend, client));
    platform.top_bar_items(ui, in_menu);
    platform.draw(|backend, client, _| crate::update::top_bar_item(ui, backend, client));
    platform.draw(|backend, client, _| crate::pull_request::menu(ui, backend, client));
    if read(platform, |backend, _| backend.management().is_some() || backend.available(&Command::PickProject)) {
        let response = ui
            .menu_button("Project", |ui| platform.draw(|backend, client, _| topbar::project_menu(ui, backend, client)))
            .response;
        record_item(ui, "Project", &response);
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
    platform.top_bar_settings(ui);
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

    /// What a bar holds, measured roughly as the desktop draws it, with plans that need the user.
    fn widths(width: f32) -> TopBarWidths {
        TopBarWidths {
            width,
            gap: 7.0,
            chief: [214.0, 98.0, 56.0],
            brand: [164.0, 99.0, 33.0],
            urgent: 453.0,
            navigation: 138.0,
            switcher: [190.0, 110.0],
            controls: 226.0,
            controls_with_load: 445.0,
            actions_menu: 65.0,
        }
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
                show_load: true
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
                TopBarWidths { urgent: 0.0, navigation: 40.0, controls: 100.0, switcher: [0.0; 2], ..widths(width) };
            assert_eq!(TopBarPlan::choose(&empty).chief, plan.chief, "{width}");
            // What is drawn beside the pill fits its half.
            let side = (width - widths(width).chief[plan.chief as usize]) / 2.0 - 7.0;
            let switcher = if plan.compact_switcher { 110.0 } else { 190.0 };
            let actions = switcher + 7.0 + if plan.actions_menu { 65.0 } else { 226.0 };
            assert!(actions <= side, "{width}: actions {actions} in {side}");
            assert!(widths(width).brand[plan.brand as usize] + 7.0 + 138.0 <= side, "{width}: {plan:?}");
            last = plan;
        }
        assert_eq!(last.chief, PillSize::Mark);
        assert!(last.actions_menu && last.brand == BrandSize::Mark);
    }
}
