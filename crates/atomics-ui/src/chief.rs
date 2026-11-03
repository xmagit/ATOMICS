//! The chief: its pill in the top bar, its window, docked beside the agents or floating over them, with
//! the chief's terminal, and the steer bar at the bottom of the window, which types a message into the
//! active session, every agent or the sessions picked in the sidebar.

use std::time::Duration;

use atomics_core::config::{AgentKind, ChiefDock};
use atomics_core::layout::{LayoutConfig, MIN_DOCK_WIDTH};
use egui::{Color32, Key, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::keys::shortcut_label;
use crate::status::status_dot;
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::theme::{Palette, lerp};
use crate::topbar::record_item;
use crate::{Backend, ChiefView, Client, Command, touch};

/// Who the steer bar's message goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerTarget {
    /// The active session.
    Active,
    /// Every agent but the chief.
    Agents,
    /// The sessions picked in the sidebar.
    Selected,
}

/// What the microphone does, on a platform that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    Idle,
    Opening,
    /// Recording, for this many seconds so far.
    Recording(u64),
    Transcribing,
}

/// The chief's pill: its mark, its status, the selected plan's progress and the last heartbeat; `compact`
/// leaves out the words. `voice` is what the microphone does, `None` on a platform without one. A click
/// shows or hides the chief ([`Command::ToggleChief`]); its menu floats or docks it. Where the backend can
/// do neither, the pill only shows.
pub fn pill(ui: &mut egui::Ui, compact: bool, voice: Option<VoiceState>, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let chief = backend.chief();
    let status = chief.session.and_then(|index| backend.session_row(index)).map(|row| row.status);
    let plan = if compact { None } else { backend.plan_row(backend.view_hints().selected_plan) };
    let mut toggle = false;
    let mut dock = chief.dock;
    let stroke = if chief.open { p.accent } else { lerp(p.border, p.accent, 0.5) };
    let response = egui::Frame::new()
        .fill(lerp(p.raised, p.accent, 0.08))
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(13)
        .inner_margin(egui::Margin::symmetric(14, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (icon, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                orbit_icon(ui.painter(), icon, p.accent, p.foreground);
                ui.label(RichText::new("CHIEF").strong().color(p.accent).size(13.0));
                match status {
                    Some(status) => {
                        let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        let time = ui.input(|i| i.time);
                        ui.painter().circle_filled(dot.center(), 3.5, status_dot(status, &p, time));
                        if !compact {
                            ui.label(RichText::new(status.label()).color(p.muted).size(11.5));
                        }
                    }
                    None if !compact => {
                        ui.label(RichText::new("start").color(p.muted).size(11.5));
                    }
                    None => {}
                }
                if let Some(plan) = plan.as_ref().filter(|plan| plan.total > 0) {
                    ui.label(RichText::new(format!("plan {}/{}", plan.done, plan.total)).color(p.muted).size(11.5));
                }
                if !compact && let Some(heartbeat) = &chief.heartbeat {
                    if heartbeat.findings > 0 {
                        let (dot, _) = ui.allocate_exact_size(vec2(8.0, 8.0), Sense::hover());
                        ui.painter().circle_filled(dot.center(), 3.0, p.error);
                    }
                    let age = heartbeat.age;
                    let age_text = match age {
                        0..60 => "now".into(),
                        60..3600 => format!("{}m", age / 60),
                        3600..86400 => format!("{}h", age / 3600),
                        _ => format!("{}d", age / 86400),
                    };
                    ui.ctx().request_repaint_after(Duration::from_secs(60 - age % 60));
                    ui.label(RichText::new(format!("♥ {age_text}")).color(p.muted).size(11.5));
                }
                match voice {
                    Some(VoiceState::Recording(secs)) => {
                        let text = format!("● REC {}:{:02}", secs / 60, secs % 60);
                        ui.label(RichText::new(text).color(p.error).strong().size(11.5));
                    }
                    Some(VoiceState::Opening | VoiceState::Transcribing) => {
                        ui.add(egui::Spinner::new().size(12.0).color(p.accent));
                    }
                    Some(VoiceState::Idle) | None => {}
                }
            });
        })
        .response
        .interact(Sense::click());
    record_item(ui, "chief", &response);
    let keys = backend.ui_config().keys;
    let hover = match voice {
        Some(VoiceState::Opening) => "Opening microphone: Esc cancels".to_string(),
        Some(VoiceState::Recording(_)) => {
            format!("Recording: {} sends, Esc cancels", shortcut_label(ui.ctx(), &keys.voice))
        }
        Some(VoiceState::Transcribing) => "Transcribing".to_string(),
        Some(VoiceState::Idle) => format!(
            "The chief agent ({}) · voice ({})",
            shortcut_label(ui.ctx(), &keys.toggle_chief),
            shortcut_label(ui.ctx(), &keys.voice)
        ),
        None => format!("The chief agent ({})", shortcut_label(ui.ctx(), &keys.toggle_chief)),
    };
    if backend.available(&Command::SetChiefDock(chief.dock)) {
        response.context_menu(|ui| {
            if ui.selectable_value(&mut dock, ChiefDock::Floating, "Float chief").clicked()
                || ui.selectable_value(&mut dock, ChiefDock::Docked, "Dock chief").clicked()
            {
                ui.close();
            }
        });
    }
    if response
        .on_hover_ui(|ui| {
            ui.label(hover);
            if let Some(status) = status {
                ui.label(status.label());
            }
            if let Some(heartbeat) = &chief.heartbeat {
                ui.label(format!("Last heartbeat: {} (local)\n{} findings", heartbeat.time, heartbeat.findings));
            }
        })
        .clicked()
    {
        toggle = true;
    }
    if dock != chief.dock {
        client.send(Command::SetChiefDock(dock));
    }
    if toggle && backend.available(&Command::ToggleChief) {
        client.send(Command::ToggleChief);
    }
}

/// The chief's mark: a planet with a tilted orbit and a moon.
fn orbit_icon(painter: &egui::Painter, rect: Rect, accent: Color32, light: Color32) {
    let c = rect.center();
    let r = rect.width() / 2.0;
    painter.circle_filled(c, r * 0.32, light);
    let tilt = -23f32.to_radians();
    let points: Vec<Pos2> = (0..=48)
        .map(|k| {
            let t = k as f32 / 48.0 * std::f32::consts::TAU;
            let (x, y) = (r * 0.95 * t.cos(), r * 0.38 * t.sin());
            pos2(c.x + x * tilt.cos() - y * tilt.sin(), c.y + x * tilt.sin() + y * tilt.cos())
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.4, accent)));
    let moon = pos2(c.x + r * 0.95 * tilt.cos(), c.y + r * 0.95 * tilt.sin());
    painter.circle_filled(moon, r * 0.14, accent);
}

/// The width the docked chief asks for in a window `area_width` wide: the width the user gave it, else a
/// third of what the sidebar leaves, and none while it is hidden or floats. The layout gives it less when
/// the window is too narrow (`atomics_core::layout::compute`).
pub fn dock_width(chief: &ChiefView, area_width: f32, config: &LayoutConfig) -> f32 {
    if !chief.open || chief.dock != ChiefDock::Docked {
        return 0.0;
    }
    chief.width.filter(|w| w.is_finite()).unwrap_or_else(|| {
        let sidebar = if area_width >= config.sidebar_min_window { config.sidebar_width } else { 0.0 };
        ((area_width - sidebar) * 0.34).max(MIN_DOCK_WIDTH)
    })
}

/// The docked chief: a panel `width` wide on the left of what the window has left, with `add_contents`
/// ([`content`]) in it. Returns the panel's rect, for the [`divider`] drawn after the panels beside it.
pub fn dock(ui: &mut egui::Ui, width: f32, palette: &Palette, add_contents: impl FnOnce(&mut egui::Ui)) -> Rect {
    egui::Panel::left("chief-dock")
        .exact_size(width)
        .resizable(false)
        .frame(egui::Frame::new().fill(palette.terminal).inner_margin(egui::Margin::same(6)))
        .show(ui, add_contents)
        .response
        .rect
}

/// The floating chief: a window over the agents, at first half the screen wide and two thirds high in its
/// middle, with `add_contents` ([`content`]) in it.
pub fn window(ctx: &egui::Context, palette: &Palette, add_contents: impl FnOnce(&mut egui::Ui)) {
    let screen = ctx.content_rect();
    egui::Window::new("CHIEF AGENT")
        .id(egui::Id::NULL.with("chief-window"))
        .title_bar(false)
        .resizable(true)
        .default_size(vec2(screen.width() * 0.5, screen.height() * 0.66))
        .default_pos(screen.center())
        .pivot(egui::Align2::CENTER_CENTER)
        .frame(
            egui::Frame::window(&ctx.global_style())
                .fill(palette.terminal)
                .stroke(Stroke::new(1.5, palette.accent.gamma_multiply(0.8))),
        )
        .show(ctx, add_contents);
}

/// The chief's window, docked or floating: a header with minimize ([`Command::HideChief`]) and dock or
/// float, then the chief's terminal, drawn through `terminals` while the chief runs inside ATOMICS. `lane`
/// says how high a strip the platform wants under the terminal, given the height there is; the strip is
/// returned for the platform to draw in (the desktop's trace lane). Nothing is drawn while there is no
/// chief.
pub fn content(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    terminals: &mut dyn TerminalSurface,
    lane: impl FnOnce(f32) -> f32,
) -> Option<Rect> {
    let chief = backend.chief();
    let index = chief.session?;
    let session = backend.session_row(index)?;
    let p = client.palette.clone();
    let touch = client.touch;
    // A phone keeps the header off the screen's sides; the terminal keeps to them itself.
    let gutter = touch.map_or(0.0, |touch| touch.gutter);
    ui.horizontal(|ui| {
        if gutter > 0.0 {
            ui.add_space(gutter);
        }
        ui.label(RichText::new("◆ CHIEF AGENT").color(p.accent).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if gutter > 0.0 {
                ui.add_space(gutter);
            }
            if backend.available(&Command::HideChief)
                && touch::small_button(ui, touch, "minimize")
                    .on_hover_text(shortcut_label(ui.ctx(), &backend.ui_config().keys.toggle_chief))
                    .clicked()
            {
                client.send(Command::HideChief);
            }
            let label = if chief.dock == ChiefDock::Floating { "dock" } else { "float" };
            if backend.available(&Command::SetChiefDock(chief.dock)) && touch::small_button(ui, touch, label).clicked()
            {
                client.send(Command::SetChiefDock(if chief.dock == ChiefDock::Floating {
                    ChiefDock::Docked
                } else {
                    ChiefDock::Floating
                }));
            }
        });
    });
    ui.separator();
    let metrics = Metrics::new(ui, &p, backend.ui_config().font.size);
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let lane_height = lane(rect.height());
    let lane = (lane_height > 0.0).then(|| rect.with_min_y(rect.bottom() - lane_height));
    let rect = lane.map_or(rect, |lane| rect.with_max_y(lane.top()));
    if !session.outside {
        let id = egui::Id::NULL.with(("chief-term", &session.id));
        // Focus before the terminal reads this frame's events.
        if client.focus_terminal.as_deref() == Some(session.id.as_str()) {
            ui.memory_mut(|m| m.request_focus(id));
            client.focus_terminal = None;
        }
        terminals.show(ui, index, &session.id, rect, id, &metrics, &p);
    }
    lane
}

/// The docked chief's right edge, `rect` being its panel. Dragging it resizes the chief
/// ([`Command::ResizeChief`]), a double click gives it its default width back, and both are saved when
/// they end ([`Command::SaveChiefPreferences`]). Draw it after the panels beside the chief, so that it wins
/// the pointer over them.
pub fn divider(ui: &mut egui::Ui, rect: Rect, client: &mut Client) {
    let p = client.palette.clone();
    let handle = Rect::from_center_size(rect.right_center(), vec2(8.0, rect.height()));
    let response = ui.interact(handle, egui::Id::NULL.with("chief-divider"), Sense::click_and_drag());
    ui.painter().line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, p.border));
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        ui.painter().rect_filled(handle.shrink2(vec2(3.0, 0.0)), 0.0, p.accent.gamma_multiply(0.6));
    }
    if (response.dragged() || response.drag_stopped())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        client.send(Command::ResizeChief(Some((pointer.x - rect.left()).max(MIN_DOCK_WIDTH))));
    }
    if response.double_clicked() {
        client.send(Command::ResizeChief(None));
    }
    if response.drag_stopped() || response.double_clicked() {
        client.send(Command::SaveChiefPreferences);
    }
}

/// The steer bar: who to send to, the message and Send. Enter sends, Shift+Enter starts a new line
/// ([`send_steer`]).
pub fn steer_bar(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    ui.horizontal(|ui| {
        let sessions = backend.session_list();
        let active = backend.view_hints().active;
        let active_title = active
            .and_then(|id| sessions.iter().find(|s| s.id == id))
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "none".into());
        let agents = sessions.iter().filter(|s| s.kind != AgentKind::Shell && s.can_receive).count();
        let selected = client.selected.len();
        let label = match client.steer_target {
            SteerTarget::Active => format!("→ {active_title}"),
            SteerTarget::Agents => format!("→ all agents ({agents})"),
            SteerTarget::Selected => format!("→ selected ({selected})"),
        };
        egui::ComboBox::from_id_salt("steer-target").selected_text(label).width(190.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut client.steer_target, SteerTarget::Active, format!("Active: {active_title}"));
            ui.selectable_value(&mut client.steer_target, SteerTarget::Agents, format!("All agents ({agents})"));
            ui.selectable_value(&mut client.steer_target, SteerTarget::Selected, format!("Selected ({selected})"));
        });

        let id = ui.scope_id().with("steer-input");
        let focused = ui.memory(|m| m.has_focus(id));
        let mut send = false;
        if focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            send = true;
        }
        let width = ui.available_width() - 70.0;
        let edit = egui::TextEdit::multiline(&mut client.steer_text)
            .id(id)
            .desired_rows(1)
            .desired_width(width)
            .hint_text(format!(
                "Steer · Enter sends · Shift+Enter newline · {} focuses",
                shortcut_label(ui.ctx(), &backend.ui_config().keys.focus_steer)
            ))
            .font(p.ui(13.5));
        let response = ui.add(edit);
        if std::mem::take(&mut client.focus_steer) {
            response.request_focus();
        }
        let button = egui::Button::new(RichText::new("Send").color(p.on_accent)).fill(p.accent);
        if ui.add(touch::sized(button, client.touch)).clicked() {
            send = true;
        }
        if send {
            send_steer(backend, client);
        }
    });
}

/// Sends the steer bar's message, trimmed, as one [`Command::Steer`] to every target that can take it,
/// and empties the bar. With no such target it says so ([`Command::Toast`]) and keeps the message.
pub fn send_steer(backend: &dyn Backend, client: &mut Client) {
    let text = client.steer_text.trim().to_string();
    if text.is_empty() {
        return;
    }
    let active = backend.view_hints().active;
    let sessions: Vec<String> = backend
        .session_list()
        .into_iter()
        .filter(|row| match client.steer_target {
            SteerTarget::Active => active.as_deref() == Some(row.id.as_str()),
            SteerTarget::Agents => row.kind != AgentKind::Shell && !row.chief,
            SteerTarget::Selected => client.selected.contains(&row.id),
        })
        .filter(|row| row.can_receive)
        .map(|row| row.id)
        .collect();
    if sessions.is_empty() {
        client.send(Command::Toast("No session to send to".into()));
        return;
    }
    client.steer_text.clear();
    client.send(Command::Steer { sessions, text });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_drags_and_double_click_resets() {
        let ctx = egui::Context::default();
        let mut client = Client::default();
        let mut width = Some(720.0);
        let mut time = 0.0;
        let mut frame = |events| {
            time += 0.05;
            let mut save = false;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(2000.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let output = ctx.run_ui(input, |ui| {
                egui::Panel::top("top").exact_size(34.0).show(ui, |_| {});
                egui::Panel::left("sidebar").exact_size(250.0).resizable(false).show(ui, |_| {});
                let rect = egui::Panel::left("chief-dock")
                    .exact_size(width.unwrap_or(595.0))
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::same(6)))
                    .show(ui, |ui| {
                        ui.interact(ui.max_rect(), egui::Id::NULL.with("chief-term"), Sense::click_and_drag());
                        ui.take_available_space();
                    })
                    .response
                    .rect;
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.interact(ui.max_rect(), egui::Id::NULL.with("agent-term"), Sense::click_and_drag());
                });
                divider(ui, rect, &mut client);
            });
            output.drop_without_applying_deltas();
            for command in std::mem::take(&mut client.commands) {
                match command {
                    Command::ResizeChief(resized) => width = resized,
                    Command::SaveChiefPreferences => save = true,
                    other => panic!("the divider records {other:?}"),
                }
            }
            (width, save)
        };
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let start = pos2(970.0, 400.0);
        let end = pos2(1100.0, 400.0);
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(start), button(start, true)]);
        assert_eq!(frame(vec![egui::Event::PointerMoved(end)]).0, Some(850.0));
        assert!(frame(vec![button(end, false)]).1);
        frame(vec![]);
        frame(vec![button(end, true)]);
        frame(vec![button(end, false)]);
        frame(vec![button(end, true)]);
        assert_eq!(frame(vec![button(end, false)]), (None, true));
    }

    #[test]
    fn the_docked_chief_asks_for_its_width_only_while_it_shows() {
        let config = LayoutConfig::default();
        let docked = ChiefView { session: None, open: true, dock: ChiefDock::Docked, width: None, heartbeat: None };
        let default = ((2000.0 - config.sidebar_width) * 0.34).max(MIN_DOCK_WIDTH);
        assert_eq!(dock_width(&docked, 2000.0, &config), default);
        assert_eq!(dock_width(&ChiefView { width: Some(700.0), ..docked.clone() }, 2000.0, &config), 700.0);
        assert_eq!(dock_width(&ChiefView { width: Some(f32::NAN), ..docked.clone() }, 2000.0, &config), default);
        assert_eq!(dock_width(&ChiefView { open: false, ..docked.clone() }, 2000.0, &config), 0.0);
        assert_eq!(dock_width(&ChiefView { dock: ChiefDock::Floating, ..docked }, 2000.0, &config), 0.0);
    }
}
