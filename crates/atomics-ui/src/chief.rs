//! The chief: its pill in the top bar, its window, docked beside the agents or floating over them, with
//! the chief's terminal, and the steer bar at the bottom of the window, which types a message into the
//! active session, every agent or the sessions picked in the sidebar.

use std::time::Duration;

use atomics_core::config::{AgentKind, ChiefDock};
use atomics_core::layout::{LayoutConfig, MIN_DOCK_WIDTH};
use atomics_core::session::AgentStatus;
use egui::{Color32, Key, Pos2, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::keys::shortcut_label;
use crate::status::status_dot;
use crate::terminal::Metrics;
use crate::terminal_surface::TerminalSurface;
use crate::theme::{Palette, lerp};
use crate::topbar::{record_item, shortcut_tip};
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

/// The pill's padding inside its outline, the chief's mark and the gap between its parts.
const PILL_PAD: f32 = 12.0;
const PILL_ICON: f32 = 18.0;
const PILL_GAP: f32 = 6.0;
/// The pill's height in the desktop's top bar; on a phone it is a finger's target.
pub const PILL_HEIGHT: f32 = 24.0;

/// The pill's word and the size of its smaller text.
const PILL_NAME: &str = "CHIEF";
const PILL_NAME_SIZE: f32 = 13.0;
const PILL_SMALL: f32 = 11.5;

/// How much the chief's pill shows, widest first. Each size has a fixed width ([`pill_width`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PillSize {
    /// The mark, the name, the status in a word and the heartbeat or the recording.
    Full,
    /// The mark, the name and the status dot.
    Name,
    /// The mark and the status dot.
    Mark,
}

/// The pill's width at `size`: the same whatever the chief does, so that it never moves and nothing beside
/// it moves when its status, its heartbeat or the microphone changes. The full pill has room for the longest
/// status word and the heartbeat, or for the recording.
pub fn pill_width(ui: &egui::Ui, palette: &Palette, size: PillSize) -> f32 {
    let width = |text: &str, font: egui::FontId| {
        ui.fonts_mut(|fonts| fonts.layout_no_wrap(text.to_string(), font, palette.foreground).size().x)
    };
    let mark = PILL_PAD * 2.0 + PILL_ICON + PILL_GAP + 8.0;
    if size == PillSize::Mark {
        return mark.ceil();
    }
    let name = mark + width(PILL_NAME, palette.ui(PILL_NAME_SIZE)) + PILL_GAP;
    if size == PillSize::Name {
        return name.ceil();
    }
    let small = palette.ui(PILL_SMALL);
    let words = [
        "starting",
        "idle",
        "working",
        "needs you",
        "turn done",
        "exited",
        "start",
        "waiting for you",
        "first-run setup",
        "needs sign-in",
    ];
    let word = words.iter().map(|w| width(w, small.clone())).fold(0.0, f32::max);
    let heart = width("♥ 59m", small.clone()) + 8.0 + PILL_GAP;
    let recording = width("● REC 59:59", small);
    (name + PILL_GAP + (word + PILL_GAP + heart).max(recording)).ceil()
}

/// How long ago the heartbeat ran, in its shortest words.
fn heartbeat_age(age: u64) -> String {
    match age {
        0..60 => "now".into(),
        60..3600 => format!("{}m", age / 60),
        3600..86400 => format!("{}h", age / 3600),
        _ => format!("{}d", age / 86400),
    }
}

/// The chief's pill: its mark, its status and the last heartbeat, at a fixed width ([`pill_width`]), so that
/// the top bar can keep it in one place; a smaller `size` leaves out the words. `voice` is what the microphone does,
/// `None` on a platform without one. A click, or Enter and Space while it has the keyboard (it shows a focus
/// ring), shows or hides the chief ([`Command::ToggleChief`]); its menu floats or docks it. Where the backend
/// can do neither, the pill only shows; a platform that shows the chief otherwise (a phone's pane) reads the
/// click from the response.
pub fn pill(
    ui: &mut egui::Ui,
    size: PillSize,
    voice: Option<VoiceState>,
    backend: &dyn Backend,
    client: &mut Client,
) -> egui::Response {
    let compact = size != PillSize::Full;
    let p = client.palette.clone();
    let chief = backend.chief();
    let session = chief.session.and_then(|index| backend.session_row(index));
    let status = session.as_ref().map(|row| row.status);
    let status_label = session.as_ref().map(|row| row.status_label().split(':').next().unwrap_or_default());
    let mut toggle = false;
    let mut dock = chief.dock;
    let height = client.touch.map_or(PILL_HEIGHT, |touch| touch.target);
    let width = pill_width(ui, &p, size);
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let hovered = response.hovered();
        let stroke = if chief.open || hovered { p.accent } else { lerp(p.border, p.accent, 0.5) };
        let fill = lerp(
            p.raised,
            p.accent,
            if chief.open {
                0.16
            } else if hovered {
                0.12
            } else {
                0.08
            },
        );
        let radius = (height / 2.0).min(13.0);
        painter.rect(rect, radius, fill, Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
        if response.has_focus() {
            let ring = ui.visuals().selection.stroke.color;
            painter.rect_stroke(rect.expand(2.0), radius + 2.0, Stroke::new(2.0, ring), egui::StrokeKind::Outside);
        }
        let small = p.ui(PILL_SMALL);
        let mut x = rect.left() + PILL_PAD;
        let y = rect.center().y;
        orbit_icon(
            painter,
            Rect::from_center_size(egui::pos2(x + PILL_ICON / 2.0, y), vec2(PILL_ICON, PILL_ICON)),
            p.accent,
            p.foreground,
        );
        x += PILL_ICON + PILL_GAP;
        if size != PillSize::Mark {
            let name = painter.layout_no_wrap(PILL_NAME.into(), p.ui(PILL_NAME_SIZE), p.accent);
            let name_width = name.size().x;
            painter.galley(egui::pos2(x, y - name.size().y / 2.0), name, p.accent);
            x += name_width + PILL_GAP;
        }
        let right = rect.right() - PILL_PAD;
        match status {
            Some(status) => {
                let time = ui.input(|i| i.time);
                painter.circle_filled(egui::pos2(x + 4.0, y), 3.5, status_dot(status, &p, time));
                if status == AgentStatus::Working {
                    ui.ctx().request_repaint_after(crate::status::PULSE_STEP);
                }
                x += 8.0 + PILL_GAP;
                if !compact && !matches!(voice, Some(VoiceState::Recording(_))) {
                    let word =
                        painter.layout_no_wrap(status_label.unwrap_or(status.label()).into(), small.clone(), p.muted);
                    painter.galley(egui::pos2(x, y - word.size().y / 2.0), word, p.muted);
                }
            }
            None if !compact && backend.available(&Command::ToggleChief) => {
                let word = painter.layout_no_wrap("start".into(), small.clone(), p.muted);
                painter.galley(egui::pos2(x, y - word.size().y / 2.0), word, p.muted);
            }
            None => {}
        }
        if !compact {
            match voice {
                Some(VoiceState::Recording(secs)) => {
                    let text = format!("● REC {}:{:02}", secs / 60, secs % 60);
                    let galley = painter.layout_no_wrap(text, small.clone(), p.error);
                    painter.galley(egui::pos2(right - galley.size().x, y - galley.size().y / 2.0), galley, p.error);
                }
                Some(VoiceState::Opening | VoiceState::Transcribing) => {
                    let spin = Rect::from_center_size(egui::pos2(right - 6.0, y), vec2(12.0, 12.0));
                    egui::Spinner::new().size(12.0).color(p.accent).paint_at(ui, spin);
                }
                Some(VoiceState::Idle) | None => {
                    if let Some(heartbeat) = &chief.heartbeat {
                        let age = heartbeat.age;
                        ui.ctx().request_repaint_after(Duration::from_secs(60 - age % 60));
                        let galley =
                            painter.layout_no_wrap(format!("♥ {}", heartbeat_age(age)), small.clone(), p.muted);
                        let at = right - galley.size().x;
                        painter.galley(egui::pos2(at, y - galley.size().y / 2.0), galley, p.muted);
                        if heartbeat.findings > 0 {
                            painter.circle_filled(egui::pos2(at - PILL_GAP - 3.0, y), 3.0, p.error);
                        }
                    }
                }
            }
        } else if let Some(VoiceState::Recording(_)) = voice {
            painter.circle_filled(egui::pos2(right - 3.0, y), 3.0, p.error);
        }
    }
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
        None if backend.has_shortcuts() => {
            format!("The chief agent ({})", shortcut_label(ui.ctx(), &keys.toggle_chief))
        }
        None => "The chief agent".to_string(),
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
    let response = response.on_hover_ui(|ui| {
        ui.label(hover);
        if let Some(session) = &session {
            ui.label(session.status_label());
        }
        if let Some(heartbeat) = &chief.heartbeat {
            ui.label(format!("Last heartbeat: {} (local)\n{} findings", heartbeat.time, heartbeat.findings));
        }
    });
    if response.clicked() {
        toggle = true;
    }
    if dock != chief.dock {
        client.send(Command::SetChiefDock(dock));
    }
    if toggle && backend.available(&Command::ToggleChief) {
        client.send(Command::ToggleChief);
    }
    response
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
pub fn window(ctx: &egui::Context, palette: &Palette, fills: Option<Rect>, add_contents: impl FnOnce(&mut egui::Ui)) {
    let screen = ctx.content_rect();
    let window = egui::Window::new("CHIEF AGENT").id(egui::Id::NULL.with("chief-window")).title_bar(false).frame(
        egui::Frame::window(&ctx.global_style())
            .fill(palette.terminal)
            .stroke(Stroke::new(1.5, palette.accent.gamma_multiply(0.8))),
    );
    // A finger has no use for a window to drag about: it takes the room it is given.
    let window = match fills {
        Some(rect) => window.resizable(false).fixed_rect(rect),
        None => window
            .resizable(true)
            .default_size(vec2(screen.width() * 0.5, screen.height() * 0.66))
            .default_pos(screen.center())
            .pivot(egui::Align2::CENTER_CENTER),
    };
    window.show(ctx, add_contents);
}

/// The chief's header, terminal and shared trace lane. Nothing is drawn without a chief. A phone has no header: its
/// chief button opened this, which says whose it is, and the terminal takes the room.
pub fn content(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, terminals: &mut dyn TerminalSurface) {
    let chief = backend.chief();
    let Some(index) = chief.session else { return };
    let Some(session) = backend.session_row(index) else { return };
    let p = client.palette.clone();
    let touch = client.touch;
    if !touch::stacks(touch) {
        // A tablet keeps the header off the screen's sides; the terminal keeps to them itself.
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
                    && shortcut_tip(touch::button(ui, touch, "Minimize"), backend, |keys| &keys.toggle_chief).clicked()
                {
                    client.send(Command::HideChief);
                }
                let (label, hover) = if chief.dock == ChiefDock::Floating {
                    ("Dock", "Keep the chief in a column beside the agents")
                } else {
                    ("Float", "Float the chief in a window over the agents")
                };
                // A finger's chief sits beside the agents where it fits, and takes the room where it does not.
                if touch.is_none()
                    && backend.available(&Command::SetChiefDock(chief.dock))
                    && touch::button(ui, touch, label).on_hover_text(hover).clicked()
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
    }
    if let Some(hint) = &session.status_hint {
        ui.label(RichText::new(hint).color(p.warning).size(12.0));
    }
    let metrics = Metrics::new(ui, &p, backend.ui_config().font.size);
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, Sense::hover());
    let data = backend.trace();
    crate::trace_lane::shortcuts(ui.ctx(), client, &data);
    let lane_height = client.lane.height(&data, rect.height(), client.touch);
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
    if let Some(lane) = lane {
        crate::trace_lane::show(ui, lane, chief.dock == ChiefDock::Docked, backend, client, &data);
        crate::trace_lane::lane_divider(ui, lane, &mut client.lane.height, &p);
    }
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
    let narrow = ui.available_width() < 600.0;
    let draw = |ui: &mut egui::Ui| {
        let sessions = backend.session_list();
        let active = backend.view_hints().active;
        let active_session = active.and_then(|id| sessions.iter().find(|s| s.id == id));
        let active_title = active_session.map(|s| s.title.clone()).unwrap_or_else(|| "no agent".into());
        let agents = sessions.iter().filter(|s| s.kind != AgentKind::Shell && s.can_receive).count();
        let selected = client.selected.len();
        let label = match client.steer_target {
            SteerTarget::Active => format!("To: {active_title}"),
            SteerTarget::Agents => format!("To: all agents ({agents})"),
            SteerTarget::Selected => format!("To: selected ({selected})"),
        };
        egui::ComboBox::from_id_salt("steer-target")
            .selected_text(label)
            .width(190.0)
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut client.steer_target, SteerTarget::Active, format!("Active: {active_title}"));
                if backend.tells_agents_from_shells() {
                    ui.selectable_value(
                        &mut client.steer_target,
                        SteerTarget::Agents,
                        format!("All agents ({agents})"),
                    );
                }
                ui.selectable_value(&mut client.steer_target, SteerTarget::Selected, format!("Selected ({selected})"));
            })
            .response
            .on_hover_text(active_session.map(|s| s.hover()).unwrap_or_default());

        if narrow {
            ui.end_row();
        }

        let id = ui.scope_id().with("steer-input");
        let focused = ui.memory(|m| m.has_focus(id));
        let mut send = false;
        // Its match ignores Shift, which is the text box's: a new line.
        if focused && ui.input_mut(|i| !i.modifiers.shift && i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
            send = true;
        }
        let width = ui.available_width() - 70.0;
        let hint = "Steer · Enter sends · Shift+Enter newline";
        let hint = if backend.has_shortcuts() {
            format!("{hint} · {} focuses", shortcut_label(ui.ctx(), &backend.ui_config().keys.focus_steer))
        } else {
            hint.to_string()
        };
        let edit = egui::TextEdit::multiline(&mut client.steer_text)
            .id(id)
            .desired_rows(1)
            .desired_width(width)
            .hint_text(hint)
            .font(p.ui(13.0));
        let response = ui.add(crate::controls::field(edit));
        if std::mem::take(&mut client.focus_steer) {
            response.request_focus();
        }
        // Filled only with something to send, as Send review is: with nothing typed it does nothing.
        let response = if client.steer_text.trim().is_empty() {
            touch::button(ui, client.touch, "Send")
        } else {
            crate::controls::primary(ui, "Send", client.touch, &p)
        };
        if response.on_hover_text("Send this to the agents under To").clicked() {
            send = true;
        }
        if send {
            send_steer(backend, client);
        }
    };
    if narrow {
        ui.horizontal_wrapped(draw);
    } else {
        ui.horizontal(draw);
    }
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
            SteerTarget::Agents => backend.tells_agents_from_shells() && row.kind != AgentKind::Shell && !row.chief,
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

/// Shared chief window body, including approvals and the host's cleaned trace cards.
pub fn draw(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, terminals: &mut dyn TerminalSurface) {
    // Keep the terminal rect until the header and trace are drawn, so approval controls receive focus first.
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
            _p: &Palette,
        ) -> egui::Response {
            self.0 =
                Some((index, session.into(), rect, id, Metrics { font: metrics.font.clone(), cell: metrics.cell }));
            ui.interact(Rect::from_min_size(rect.min, vec2(0.0, 0.0)), id.with("kept"), Sense::hover())
        }
    }
    let mut kept = Kept::default();
    content(ui, backend, client, &mut kept);
    if let Some((index, session, rect, id, metrics)) = kept.0 {
        let p = client.palette.clone();
        let rect = crate::approvals::banner(ui, rect, index, backend, client);
        terminals.show(ui, index, &session, rect, id, &metrics, &p);
    }
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
