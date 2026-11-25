//! The colors and marks that say what a session or a task is doing.

use std::time::Duration;

use atomics_core::plan::TaskStatus;
use atomics_core::session::AgentStatus;
use egui::{Color32, Stroke, vec2};

use crate::theme::{Palette, lerp};

/// How often a pulsing status (a working agent, a running plan) is drawn again.
pub const PULSE_STEP: Duration = Duration::from_millis(250);

pub fn status_color(status: AgentStatus, p: &Palette) -> Color32 {
    match status {
        AgentStatus::Starting => p.muted,
        AgentStatus::Idle => lerp(p.muted, p.foreground, 0.3),
        AgentStatus::Working => p.accent,
        AgentStatus::NeedsInput => p.warning,
        AgentStatus::Done => p.success,
        AgentStatus::Exited => lerp(p.error, p.background, 0.4),
    }
}

/// The icon of a session's state ([`crate::icons`]).
pub fn status_icon(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => crate::icons::PLANNED,
        AgentStatus::Idle => crate::icons::IDLE,
        AgentStatus::Working => crate::icons::WORKING,
        AgentStatus::NeedsInput => crate::icons::NEEDS_YOU,
        AgentStatus::Done => crate::icons::DONE,
        AgentStatus::Exited => crate::icons::FAILED,
    }
}

/// The icon of a task's state.
pub fn task_icon(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Planned => crate::icons::PLANNED,
        TaskStatus::Running => crate::icons::WORKING,
        TaskStatus::Blocked => crate::icons::BLOCKED,
        TaskStatus::Review => crate::icons::REVIEW,
        TaskStatus::Done => crate::icons::DONE,
    }
}

/// The glyph before a session's title on its tab, where this frame has no icons.
pub fn status_glyph(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::NeedsInput => "◆",
        AgentStatus::Done => "●",
        AgentStatus::Working => "◐",
        AgentStatus::Exited => "×",
        _ => "○",
    }
}

/// The status dot; working agents pulse like Claude Code's spinner.
pub fn status_dot(status: AgentStatus, p: &Palette, time: f64) -> Color32 {
    let color = status_color(status, p);
    if status == AgentStatus::Working { pulse(p.background, color, time) } else { color }
}

/// `color` as it pulses over `background` at `time`.
pub fn pulse(background: Color32, color: Color32, time: f64) -> Color32 {
    lerp(background, color, 0.55 + 0.45 * ((time * 3.2).sin() * 0.5 + 0.5) as f32)
}

/// What state a row is in. Painted, not typed: a browser has only egui's own fonts, and they draw none of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Check,
    Cross,
    Dot,
    Ring,
    Diamond,
}

pub fn paint_mark(painter: &egui::Painter, center: egui::Pos2, mark: Mark, color: Color32) {
    let line = Stroke::new(1.6, color);
    match mark {
        Mark::Check => {
            let points = [vec2(-4.0, 0.0), vec2(-1.0, 3.0), vec2(4.5, -3.5)];
            painter.line(points.map(|point| center + point).into(), line);
        }
        Mark::Cross => {
            let arm = vec2(3.5, 3.5);
            painter.line_segment([center - arm, center + arm], line);
            let arm = vec2(3.5, -3.5);
            painter.line_segment([center - arm, center + arm], line);
        }
        Mark::Dot => {
            painter.circle_filled(center, 3.5, color);
        }
        Mark::Ring => {
            painter.circle_stroke(center, 3.5, Stroke::new(1.0, color));
        }
        Mark::Diamond => {
            let points = [vec2(0.0, -4.5), vec2(4.5, 0.0), vec2(0.0, 4.5), vec2(-4.5, 0.0)];
            painter.add(egui::Shape::convex_polygon(points.map(|point| center + point).into(), color, Stroke::NONE));
        }
    }
}

pub fn task_color(status: TaskStatus, p: &Palette) -> Color32 {
    match status {
        TaskStatus::Planned => p.muted,
        TaskStatus::Running => p.accent,
        TaskStatus::Blocked => p.warning,
        TaskStatus::Review => p.info,
        TaskStatus::Done => p.success,
    }
}
