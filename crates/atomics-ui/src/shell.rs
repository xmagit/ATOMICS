//! The window's frame: the top bar over the steer bar, the sidebar, and between them the fleet's tiles, the
//! right panel (the plan board or a worktree's diff) and the chief, docked beside the agents or floating over
//! them. The platform draws it through a [`Platform`]: it lends each piece the backend and its client, and
//! carries out what that piece recorded before the next one draws, as the desktop did before the frame moved.
//! The items only a host has (the desktop's Settings, Tickets, GitHub, Remote and load items, USAGE, the
//! Tickets panel, a pull request's diff, the editor) are the trait's hooks; a platform that leaves a hook as
//! it is does without that item. Everything else asks [`Backend::available`] whether to draw an action.

use std::path::{Path, PathBuf};

use atomics_core::layout::{self, DiffPlacement, Layout, LayoutConfig};
use atomics_core::plan::PlanStatus;
use atomics_core::session::{AgentStatus, pick_tiled};
use egui::{Rect, RichText, Sense, Stroke, vec2};

use crate::chief::{self, VoiceState};
use crate::terminal_surface::TerminalSurface;
use crate::topbar::{self, measure, record_item, slot};
use crate::{Backend, Client, Command, PanelMode, board, diff_panel, diff_view, fleet, sidebar, tabs};

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

    /// The host's load gauge, after the Project menu while the bar has room for it.
    fn top_bar_load(&mut self, _ui: &mut egui::Ui) {}

    /// The host's settings, last in the top bar.
    fn top_bar_settings(&mut self, _ui: &mut egui::Ui) {}

    /// The host's section at the top of the sidebar, with its separator.
    fn sidebar_usage(&mut self, _ui: &mut egui::Ui) {}

    /// The host's section above the sidebar's agents.
    fn sidebar_tickets(&mut self, _ui: &mut egui::Ui) {}

    /// The chief's window, docked or floating: its header and its terminal ([`chief::content`]). The desktop
    /// adds the trace lane under the terminal.
    fn chief_window(&mut self, ui: &mut egui::Ui) {
        self.draw(|backend, client, terminals| chief::content(ui, backend, client, terminals, |_| 0.0));
    }

    /// The right panel in [`PanelMode::Tickets`], which only the host has.
    fn panel_tickets(&mut self, _ui: &mut egui::Ui, _rect: Rect) {}

    /// A pull request's diff in the right panel instead of the worktree's, while the host shows one; true
    /// when it drew one.
    fn panel_pull_request(&mut self, _ui: &mut egui::Ui, _rect: Rect) -> bool {
        false
    }

    /// The file open in the editor of the worktree at `root`, which the changed-files column marks in
    /// [`PanelMode::Editor`].
    fn panel_editor_file(&self, _root: &Path) -> Option<String> {
        None
    }

    /// The right panel in [`PanelMode::Editor`]: the editor of the worktree at `root`.
    fn panel_editor(&mut self, _ui: &mut egui::Ui, _rect: Rect, _root: &Path) {}
}

/// Reads from the backend and the client, recording nothing.
fn read<R>(platform: &mut impl Platform, read: impl FnOnce(&dyn Backend, &Client) -> R) -> R {
    platform.draw(|backend, client, _| read(backend, client))
}

/// The whole window: the top bar, the steer bar, the sidebar, the chief's dock, the tiles with the right
/// panel, and the floating chief.
pub fn frame(ui: &mut egui::Ui, platform: &mut impl Platform) {
    let p = read(platform, |_, client| client.palette.clone());
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
        let diff_open = !history_board
            && hints.diff_open
            && (hints.diff_target.is_some() || matches!(hints.panel, PanelMode::Plan | PanelMode::Tickets));
        let config = backend.ui_config().layout;
        let sidebar_width = config.sidebar_width;
        let layout_config = LayoutConfig { diff_width: client.split, ..config };
        let visible = if history_board { Vec::new() } else { backend.visible_sessions() };
        let completed = if history_board { 0 } else { backend.completed_tasks().len() };
        let dock_width = chief::dock_width(&backend.chief(), area.width(), &layout_config);
        let compute = |height| {
            layout::compute(area.width(), height, visible.len(), completed, diff_open, dock_width, &layout_config)
        };
        let mut layout = compute(area.height());
        if layout.tabs {
            layout = compute(area.height() - TAB_HEIGHT);
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

    let chief_rect =
        (layout.dock_width > 0.0).then(|| chief::dock(ui, layout.dock_width, &p, |ui| platform.chief_window(ui)));

    egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
        let mut rect = ui.max_rect();
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

    if let Some(rect) = chief_rect {
        platform.draw(|_, client, _| chief::divider(ui, rect, client));
    }
    if read(platform, |backend, _| backend.chief().open) && layout.dock_width == 0.0 {
        chief::window(ui.ctx(), &p, |ui| platform.chief_window(ui));
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

/// The right panel in `rect`: the plan board, the host's Tickets panel, or the diff target's diff or editor
/// with the changed-files column beside it when there is room.
pub fn right_panel(ui: &mut egui::Ui, rect: Rect, platform: &mut impl Platform) {
    platform.draw(|backend, client, _| diff_panel::sync(backend, client));
    let hints = read(platform, |backend, _| backend.view_hints());
    match hints.panel {
        PanelMode::Plan => return plan_board(ui, rect, platform),
        PanelMode::Tickets => return platform.panel_tickets(ui, rect),
        PanelMode::Diff if platform.panel_pull_request(ui, rect) => return,
        _ => {}
    }
    let Some(root) = hints.diff_target else { return };
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
            true => platform.panel_editor_file(&root),
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
        return platform.panel_editor(ui, rect, &root);
    }
    platform.draw(|backend, client, _| diff_panel::draw(ui, rect, &root, backend, client));
}

/// The sidebar: the host's USAGE, its TICKETS and the AGENTS over the EXPLORER, which takes the rest while a
/// worktree's files show.
pub fn sidebar(ui: &mut egui::Ui, platform: &mut impl Platform) {
    let files_root = read(platform, |backend, _| backend.view_hints().diff_target);
    platform.sidebar_usage(ui);
    let total = ui.available_height();
    let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
    ui.add_space(6.0);
    egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(
        ui,
        |ui| {
            platform.sidebar_tickets(ui);
            platform.draw(|backend, client, _| sidebar::sessions(ui, backend, client));
        },
    );

    ui.add_space(4.0);
    ui.separator();
    platform.draw(|backend, client, _| sidebar::explorer(ui, backend, client));
}

/// The top bar: the brand, the navigation with the notice line, the chief's pill (centered while there is
/// room) and the actions. As the bar narrows the brand drops the agent count, the load gauge goes, the pill
/// drops its words, and then the actions move into a menu; every item keeps its natural width.
pub fn top_bar(ui: &mut egui::Ui, platform: &mut impl Platform) {
    let rect = ui.available_rect_before_wrap();
    let gap = ui.spacing().item_spacing.x;
    let controls = measure(ui, |ui| top_bar_controls(ui, platform, false, false));
    let controls_with_load = measure(ui, |ui| top_bar_controls(ui, platform, true, false));
    let full_chief = measure(ui, |ui| chief_pill(ui, platform, false));
    let compact_chief = measure(ui, |ui| chief_pill(ui, platform, true));
    let full_brand = measure(ui, |ui| brand(ui, platform, true));
    let compact_brand = measure(ui, |ui| brand(ui, platform, false));
    let tabs = measure(ui, |ui| platform.draw(|backend, client, _| tabs::view_tabs(ui, backend, client)));
    let (urgent_label, urgent_color) = platform.draw(|backend, client, _| tabs::urgent_tabs_label(backend, client));
    let min_tabs = measure(ui, |ui| {
        ui.menu_button(RichText::new(&urgent_label).color(urgent_color), |_| {});
    });
    let protected_navigation =
        platform.draw(|backend, client, _| tabs::urgent_navigation_width(ui, backend, client)).max(min_tabs);

    // Once even the minimum navigation cannot fit, put the actions in a
    // menu. All four slots then retain their measured natural minimums.
    let actions_menu = compact_brand + protected_navigation + compact_chief + controls + gap * 3.0 > rect.width();
    let show_load = !actions_menu && full_brand + tabs + full_chief + controls_with_load + gap * 3.0 <= rect.width();
    let controls_width = if actions_menu {
        measure(ui, |ui| {
            ui.menu_button("Actions", |_| {});
        })
    } else if show_load {
        controls_with_load
    } else {
        controls
    };
    let show_agents = full_brand + protected_navigation + compact_chief + controls_width + gap * 3.0 <= rect.width();
    let brand_width = if show_agents { full_brand } else { compact_brand };
    let compact = brand_width + protected_navigation + full_chief + controls_width + gap * 3.0 > rect.width();
    let chief_width = if compact { compact_chief } else { full_chief };
    let controls_left = rect.right() - controls_width;
    let tabs_left = rect.left() + brand_width + gap;
    // Keep the wide-screen center when navigation fits. Otherwise use the
    // free space up to the actions before deciding which tabs overflow.
    let chief_limit = controls_left - gap - chief_width;
    let desired_navigation = tabs_left + tabs + gap;
    let navigation_goal = desired_navigation.min(chief_limit);
    let chief_left =
        (rect.center().x - chief_width / 2.0).max(navigation_goal).max(tabs_left + min_tabs + gap).min(chief_limit);
    let navigation = Rect::from_x_y_ranges(tabs_left..=chief_left - gap, rect.y_range());
    slot(ui, "brand", rect.with_max_x(rect.left() + brand_width), |ui| brand(ui, platform, show_agents));
    slot(ui, "chief", rect.with_min_x(chief_left).with_max_x(chief_left + chief_width), |ui| {
        chief_pill(ui, platform, compact);
    });
    slot(ui, "actions", rect.with_min_x(controls_left), |ui| {
        if actions_menu {
            let response = ui.menu_button("Actions", |ui| top_bar_controls(ui, platform, false, true)).response;
            record_item(ui, "Actions", &response);
        } else {
            top_bar_controls(ui, platform, show_load, false);
        }
    });
    slot(ui, "navigation", navigation, |ui| {
        platform.draw(|backend, client, _| tabs::view_tabs(ui, backend, client));
        platform.draw(|backend, client, _| topbar::notice(ui, backend, client));
    });
    ui.allocate_rect(rect, Sense::hover());
}

fn brand(ui: &mut egui::Ui, platform: &mut impl Platform, show_agents: bool) {
    platform.draw(|backend, client, _| topbar::brand(ui, show_agents, backend, client));
}

fn chief_pill(ui: &mut egui::Ui, platform: &mut impl Platform, compact: bool) {
    let voice = platform.voice_state();
    platform.draw(|backend, client, _| chief::pill(ui, compact, voice, backend, client));
}

/// The bar's actions, in the order a row shows them: diff, `+ New`, Plan, the host's items, Project, the
/// host's load gauge with `show_load`, and its settings. Each button is a draw of its own, so a click on one
/// reaches the buttons drawn after it in the same frame.
fn top_bar_controls(ui: &mut egui::Ui, platform: &mut impl Platform, show_load: bool, in_menu: bool) {
    platform.draw(|backend, client, _| topbar::diff_button(ui, in_menu, backend, client));
    platform.draw(|backend, client, _| topbar::new_button(ui, in_menu, backend, client));
    platform.draw(|backend, client, _| topbar::plan_button(ui, in_menu, backend, client));
    platform.top_bar_items(ui, in_menu);
    if read(platform, |backend, _| backend.available(&Command::PickProject)) {
        let response = ui
            .menu_button("Project", |ui| platform.draw(|backend, client, _| topbar::project_menu(ui, backend, client)))
            .response;
        record_item(ui, "Project", &response);
    }
    if show_load {
        platform.top_bar_load(ui);
    }
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
        ui.label(RichText::new("Click an agent's title to see its worktree's diff against main.\nComment on any line and send the review straight to the agent.").color(p.muted).size(12.0));
    });
}
