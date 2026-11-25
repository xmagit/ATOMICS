//! The view switcher: one segmented control for what the panel beside the agents shows, the plan's board, the
//! diff or the tickets. Each segment has its icon, its name and, where it helps, a count: the board's tasks
//! that wait for the user, the diff's changed files, the ticket system's sign-in (signin's state) or the tickets
//! no task follows yet. The view on screen is tinted with the accent; picking it again hides the panel
//! ([`Command::ToggleView`]), and the keys that toggle a view do the same. A phone draws the same segments as its
//! pane bar's tabs ([`segments`], [`icon`]).

use egui::{Color32, Rect, Sense, Stroke, StrokeKind, pos2, vec2};

use crate::keys::shortcut_label;
use crate::theme::{Palette, lerp};
use crate::topbar::record_item;
use crate::{Backend, Client, Command, PanelMode, ViewHints};

/// A count on a segment, in the color that says how much it asks of the user.
#[derive(Debug, Clone, PartialEq)]
pub struct Badge {
    pub text: String,
    pub color: Color32,
}

/// One view the switcher offers.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub mode: PanelMode,
    pub label: &'static str,
    /// What the label adds while there is room, e.g. the board's progress.
    pub detail: Option<String>,
    pub badge: Option<Badge>,
    /// What the tooltip says beyond the name and the shortcut.
    pub hover: String,
}

/// The view a segment names: the editor is the diff's.
pub fn view_of(mode: PanelMode) -> PanelMode {
    if mode == PanelMode::Editor { PanelMode::Diff } else { mode }
}

/// The view on screen, if the panel shows.
pub fn active(hints: &ViewHints) -> Option<PanelMode> {
    hints.diff_open.then(|| view_of(hints.panel))
}

/// The views this backend can show, in the switcher's order.
pub fn segments(backend: &dyn Backend, p: &Palette) -> Vec<Segment> {
    let hints = backend.view_hints();
    let mut segments = Vec::new();
    if backend.available(&Command::ToggleView(PanelMode::Plan))
        && let Some(plan) = backend.plan_row(hints.selected_plan)
    {
        let waiting = plan.attention;
        segments.push(Segment {
            mode: PanelMode::Plan,
            label: "Board",
            detail: (plan.total > 0).then(|| format!("{}/{}", plan.done, plan.total)),
            badge: (waiting > 0).then(|| Badge { text: waiting.to_string(), color: p.warning }),
            hover: match (plan.total, waiting) {
                (0, _) => "The plan's board".into(),
                (total, 0) => format!("The plan's board: {} of {total} tasks done", plan.done),
                (total, waiting) => {
                    format!("The plan's board: {} of {total} tasks done, {waiting} in review or blocked", plan.done)
                }
            },
        });
    }
    if backend.available(&Command::ToggleView(PanelMode::Diff)) {
        let files = hints
            .diff_target
            .as_deref()
            .and_then(|root| backend.diff(root))
            .and_then(|diff| diff.prepared)
            .map(|prepared| prepared.diff.files.len());
        segments.push(Segment {
            mode: PanelMode::Diff,
            label: "Diff",
            detail: None,
            badge: files.filter(|&n| n > 0).map(|n| Badge { text: n.to_string(), color: p.muted }),
            hover: match (&hints.diff_target, files) {
                (None, _) => "The active agent's changes".into(),
                (Some(root), Some(n)) => {
                    format!("{n} changed file{} in {}", if n == 1 { "" } else { "s" }, backend.display(root))
                }
                (Some(root), None) => format!("The changes in {}", backend.display(root)),
            },
        });
    }
    if let Some(tickets) = backend.tickets_panel()
        && backend.available(&Command::ToggleView(PanelMode::Tickets))
    {
        use atomics_core::projection::SignInState;
        let untracked = backend.board(hints.selected_plan).and_then(|board| board.board_counts).map(|(_, n)| n);
        // The sign-in's state, as the host's sign-in says it, else the panel's own word for it.
        let sign_in = backend.sign_in().map(|view| view.info);
        let wants = sign_in.as_ref().filter(|info| {
            matches!(
                info.state,
                SignInState::SignedOut | SignInState::Expired | SignInState::Partial | SignInState::Unavailable
            )
        });
        let signing = sign_in.as_ref().is_some_and(|info| info.state == SignInState::SigningIn);
        let badge = if signing {
            Some(Badge { text: "…".into(), color: p.muted })
        } else if wants.is_some() || tickets.signed_out {
            Some(Badge { text: "●".into(), color: p.warning })
        } else {
            untracked.filter(|&n| n > 0).map(|n| Badge { text: n.to_string(), color: p.info })
        };
        let mut hover = String::from("Find and read tickets");
        match (wants, &sign_in) {
            (Some(info), _) if info.state == SignInState::Expired => {
                hover.push_str(&format!("\n{} needs you to sign in again: Reconnect", info.system));
            }
            (Some(info), _) => hover.push_str(&format!("\n{} needs you to sign in", info.system)),
            (None, Some(info)) if signing => hover.push_str(&format!("\nSigning in to {}…", info.system)),
            _ if tickets.signed_out => hover.push_str("\nThe ticket system wants you to sign in"),
            _ => {}
        }
        if let Some(reason) = wants.and_then(|info| info.reason.as_ref()) {
            hover.push_str(&format!(" ({reason})"));
        }
        if let Some(n) = untracked.filter(|&n| n > 0) {
            let plural = if n == 1 { "" } else { "s" };
            hover.push_str(&format!("\n{n} ticket{plural} on the plan's board that no task follows"));
        }
        segments.push(Segment { mode: PanelMode::Tickets, label: "Tickets", detail: None, badge, hover });
    }
    segments
}

/// The chord that toggles `mode`, from the `[keys]`.
fn shortcut(backend: &dyn Backend, ctx: &egui::Context, mode: PanelMode) -> Option<String> {
    if !backend.has_shortcuts() {
        return None;
    }
    let keys = backend.ui_config().keys;
    let chord = match mode {
        PanelMode::Plan => keys.toggle_plan,
        PanelMode::Tickets => keys.toggle_tickets,
        _ => keys.toggle_diff,
    };
    Some(shortcut_label(ctx, &chord))
}

/// A segment's icon in `rect`: the board, the diff, a ticket ([`crate::icons`]); painted where this frame has no icons.
pub fn icon(painter: &egui::Painter, rect: Rect, mode: PanelMode, color: Color32) {
    let glyph = match view_of(mode) {
        PanelMode::Plan => crate::icons::BOARD,
        PanelMode::Tickets => crate::icons::TICKETS,
        _ => crate::icons::DIFF,
    };
    if crate::icons::paint(painter, rect.center(), glyph, rect.height().max(ICON) + 1.0, color) {
        return;
    }
    let stroke = Stroke::new(1.4, color);
    let r = rect.shrink(1.0);
    match view_of(mode) {
        PanelMode::Plan => {
            let w = (r.width() - 4.0) / 3.0;
            for (i, height) in [1.0, 0.7, 0.45].into_iter().enumerate() {
                let x = r.left() + i as f32 * (w + 2.0);
                let column = Rect::from_min_size(pos2(x, r.top()), vec2(w, r.height() * height));
                painter.rect_filled(column, 1.0, color);
            }
        }
        PanelMode::Tickets => {
            let ticket = Rect::from_center_size(r.center(), vec2(r.width(), r.height() * 0.72));
            painter.rect_stroke(ticket, 2.0, stroke, StrokeKind::Inside);
            let x = ticket.left() + ticket.width() * 0.62;
            for k in 0..3 {
                let y = ticket.top() + 2.0 + k as f32 * (ticket.height() - 4.0) / 2.5;
                painter.line_segment([pos2(x, y), pos2(x, y + 1.5)], stroke);
            }
        }
        _ => {
            let c = r.center();
            let arm = r.width() * 0.3;
            let plus = c - vec2(0.0, r.height() * 0.22);
            painter.line_segment([plus - vec2(arm, 0.0), plus + vec2(arm, 0.0)], stroke);
            painter.line_segment([plus - vec2(0.0, arm), plus + vec2(0.0, arm)], stroke);
            let minus = c + vec2(0.0, r.height() * 0.36);
            painter.line_segment([minus - vec2(arm, 0.0), minus + vec2(arm, 0.0)], stroke);
        }
    }
}

const PAD: f32 = 8.0;
const ICON: f32 = 13.0;
const GAP: f32 = 5.0;
const TEXT: f32 = 12.5;
const BADGE_TEXT: f32 = 10.5;

/// A badge's pill: its width, and painted at `left` when given.
fn badge(painter: &egui::Painter, badge: &Badge, left: Option<f32>, y: f32, p: &Palette) -> f32 {
    let galley = painter.layout_no_wrap(badge.text.clone(), p.ui(BADGE_TEXT), badge.color);
    let size = vec2((galley.size().x + 8.0).max(16.0), 15.0);
    if let Some(left) = left {
        let rect = Rect::from_min_size(pos2(left, y - size.y / 2.0), size);
        let fill = lerp(p.raised, badge.color, 0.2);
        painter.rect(rect, 7.5, fill, Stroke::new(1.0, badge.color.gamma_multiply(0.7)), StrokeKind::Inside);
        painter.galley(rect.center() - galley.size() / 2.0, galley, badge.color);
    }
    size.x
}

/// What a segment draws, left to right, and how wide it is: `compact` keeps the icon and the badge.
fn layout(painter: &egui::Painter, segment: &Segment, compact: bool, p: &Palette) -> (Option<String>, f32) {
    let label = (!compact).then(|| match &segment.detail {
        Some(detail) => format!("{} {detail}", segment.label),
        None => segment.label.to_string(),
    });
    let mut width = PAD + ICON;
    if let Some(label) = &label {
        width += GAP + painter.layout_no_wrap(label.clone(), p.ui(TEXT), p.foreground).size().x;
    }
    if let Some(b) = &segment.badge {
        width += GAP + badge(painter, b, None, 0.0, p);
    }
    (label, width + PAD)
}

/// The switcher: a segment per view the backend can show; `compact` leaves out the names. A click, or Enter
/// or Space on a focused segment, toggles its view.
pub fn draw(ui: &mut egui::Ui, compact: bool, backend: &dyn Backend, client: &mut Client) {
    let p = client.palette.clone();
    let segments = segments(backend, &p);
    if segments.is_empty() {
        return;
    }
    let shown = active(&backend.view_hints());
    let height = ui.spacing().interact_size.y;
    let painter = ui.painter().clone();
    let laid: Vec<_> = segments.iter().map(|segment| layout(&painter, segment, compact, &p)).collect();
    let total: f32 = laid.iter().map(|(_, width)| width).sum();
    let (outline, _) = ui.allocate_exact_size(vec2(total, height), Sense::hover());
    let radius = f32::from(crate::theme::RADIUS);
    painter.rect(outline, radius, p.raised, Stroke::NONE, StrokeKind::Inside);
    let mut left = outline.left();
    for (index, (segment, (label, width))) in segments.iter().zip(laid).enumerate() {
        let rect = Rect::from_min_size(pos2(left, outline.top()), vec2(width, height));
        left += width;
        let id = ui.scope_id().with(("view-switcher", segment.label, compact));
        let response = ui.interact(rect, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = shown == Some(segment.mode);
        let hover = crate::controls::hover_amount(ui, id, response.hovered());
        let corners = egui::CornerRadius {
            nw: if index == 0 { radius as u8 } else { 0 },
            sw: if index == 0 { radius as u8 } else { 0 },
            ne: if index + 1 == segments.len() { radius as u8 } else { 0 },
            se: if index + 1 == segments.len() { radius as u8 } else { 0 },
        };
        // The view on screen is tinted with the accent; the pointer brightens a segment, and a press more.
        let rest = if on { lerp(p.raised, p.accent, 0.2) } else { p.raised };
        let fill = if response.is_pointer_button_down_on() {
            p.control_pressed()
        } else {
            lerp(rest, if on { lerp(p.raised, p.accent, 0.28) } else { p.control_hover() }, hover)
        };
        if fill != p.raised {
            painter.rect_filled(rect, corners, fill);
        }
        if index > 0 && !on && shown != Some(segments[index - 1].mode) {
            let x = rect.left();
            painter
                .line_segment([pos2(x, rect.top() + 4.0), pos2(x, rect.bottom() - 4.0)], Stroke::new(1.0, p.separator));
        }
        let ink = if on { p.foreground } else { lerp(lerp(p.muted, p.foreground, 0.5), p.foreground, hover) };
        let y = rect.center().y;
        let mut x = rect.left() + PAD;
        let mark = if on { p.accent } else { ink };
        icon(&painter, Rect::from_center_size(pos2(x + ICON / 2.0, y), vec2(ICON, ICON)), segment.mode, mark);
        x += ICON;
        if let Some(label) = label {
            let galley = painter.layout_no_wrap(label, p.ui(TEXT), ink);
            x += GAP;
            let w = galley.size().x;
            painter.galley(pos2(x, y - galley.size().y / 2.0), galley, ink);
            x += w;
        }
        if let Some(b) = &segment.badge {
            badge(&painter, b, Some(x + GAP), y, &p);
        }
        if response.has_focus() {
            let ring = ui.visuals().selection.stroke.color;
            painter.rect_stroke(rect.shrink(1.0), corners, Stroke::new(2.0, ring), StrokeKind::Inside);
        }
        let key = shortcut(backend, ui.ctx(), segment.mode);
        let verb = if on { "Hide" } else { "Show" };
        let tip = match key {
            Some(key) => format!("{verb} {} ({key})\n{}", segment.label.to_lowercase(), segment.hover),
            None => format!("{verb} {}\n{}", segment.label.to_lowercase(), segment.hover),
        };
        let response = response.on_hover_text(tip);
        record_item(ui, &format!("view:{}", segment.label), &response);
        if response.clicked() {
            client.send(Command::ToggleView(segment.mode));
        }
    }
    let outline_stroke = Stroke::new(1.0, p.border);
    painter.rect_stroke(outline, radius, outline_stroke, StrokeKind::Inside);
}
