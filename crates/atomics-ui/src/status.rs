//! The colors that say what a session or a task is doing.

use std::time::Duration;

use atomics_core::plan::TaskStatus;
use atomics_core::session::AgentStatus;
use egui::Color32;

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

/// The glyph before a session's title on its tab.
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
    if status == AgentStatus::Working {
        let pulse = 0.55 + 0.45 * ((time * 3.2).sin() * 0.5 + 0.5) as f32;
        lerp(p.background, color, pulse)
    } else {
        color
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
