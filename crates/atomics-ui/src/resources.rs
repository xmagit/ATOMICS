//! The host's account limits and machine-wide governor, drawn identically by every client.

use atomics_core::projection::ResourceInfo;
use atomics_core::usage::{Usage, Window, age_label, reset_label};
use egui::{Color32, RichText, Sense, vec2};

use crate::Backend;
use crate::theme::Palette;

/// USAGE at the top of a wide sidebar, or in the phone's overflow menu.
pub fn section(ui: &mut egui::Ui, backend: &dyn Backend, p: &Palette) {
    let Some(state) = backend.resources() else { return };
    crate::sidebar::section(ui, p, "USAGE");
    usage(ui, &state, p);
    ui.separator();
}

/// Remaining account limits, reset labels, source age and proportional bars.
pub fn usage(ui: &mut egui::Ui, state: &ResourceInfo, p: &Palette) {
    let now = state.sampled_at;
    for (name, usage) in [("Claude", &state.claude), ("Codex", &state.codex)] {
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).font(p.ui(12.0)).color(p.foreground));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if usage.observed_at > 0 {
                    ui.label(RichText::new(age_label(usage.observed_at, now)).font(p.ui(10.0)).color(p.muted));
                }
            });
        });
        for (label, weekly, window) in [("Session", false, &usage.session), ("Weekly", true, &usage.weekly)] {
            let missing = missing_label(usage, window);
            let window = window.as_ref().filter(|window| window.resets_at > now);
            let text = match window {
                Some(window) => format!("{label}  {:.0}% · {}", window.remaining, reset_label(window, weekly, now)),
                None => format!("{label}  {missing}"),
            };
            ui.label(RichText::new(text).font(p.ui(10.5)).color(p.muted))
                .on_hover_text("Limits are shared by this provider's account. Weekly reset days are UTC.");
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 3.0), Sense::hover());
            ui.painter().rect_filled(rect, 1.0, p.border);
            if let Some(window) = window {
                let filled = egui::Rect::from_min_size(
                    rect.min,
                    vec2(rect.width() * window.remaining as f32 / 100.0, rect.height()),
                );
                ui.painter().rect_filled(filled, 1.0, remaining_color(window, p));
            }
            ui.add_space(3.0);
        }
    }
}

fn missing_label(usage: &Usage, window: &Option<Window>) -> &'static str {
    if window.is_none() && usage.observed_at > 0 { "not reported by this account" } else { "no data yet" }
}

fn remaining_color(window: &Window, p: &Palette) -> Color32 {
    if window.remaining < 10.0 {
        p.error
    } else if window.remaining < 30.0 {
        p.warning
    } else {
        p.success
    }
}

/// Load per core, with compiler jobs and the slot queue on hover. Menus show the details directly,
/// where a phone has no hover.
pub fn load(ui: &mut egui::Ui, backend: &dyn Backend, p: &Palette, in_menu: bool) -> bool {
    let Some(state) = backend.resources() else { return false };
    let color = match state.load.map(|load| load / state.cores.max(1) as f64) {
        Some(load) if load >= 1.0 => p.error,
        Some(load) if load >= 0.7 => p.warning,
        _ => p.muted,
    };
    let label = state.load.map_or_else(|| "load —".into(), |load| format!("load {load:.1}"));
    let details = load_details(&state);
    let sense = if backend.machine_available() { Sense::click() } else { Sense::hover() };
    let response = ui.add(egui::Label::new(RichText::new(label).color(color).size(12.0)).sense(sense)).on_hover_text(
        if backend.machine_available() { format!("{details}\nClick to open Machine") } else { details.clone() },
    );
    crate::topbar::record_item(ui, "load", &response);
    if in_menu {
        ui.label(RichText::new(details).font(p.ui(10.5)).color(p.muted));
    }
    backend.machine_available() && response.clicked()
}

fn load_details(state: &ResourceInfo) -> String {
    let mut details = state.load.map_or_else(
        || format!("Load unavailable on {} cores", state.cores),
        |load| format!("Load {load:.1} on {} cores", state.cores),
    );
    match state.compiler_jobs {
        Some(jobs) => details.push_str(&format!("\nCompilers of all agents share {jobs} jobs")),
        None => details.push_str("\nNo shared compiler job pool"),
    }
    for (name, pool) in [("test", &state.test), ("qa", &state.qa)] {
        details.push_str(&format!("\n{name} slots: {}/{} in use, {} waiting", pool.running, pool.slots, pool.waiting));
    }
    details
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_accepts_latest_account_reports_and_draws_expired_windows_as_missing() {
        let ctx = egui::Context::default();
        let now = 1_800_000_000;
        let mut state = ResourceInfo { sampled_at: now, ..Default::default() };
        for observed_at in [now - 1, now, now - 2] {
            state.claude.update(Usage {
                observed_at,
                session: Some(Window { remaining: 73.0, resets_at: now - 1 }),
                ..Usage::default()
            });
        }
        assert_eq!(state.claude.observed_at, now);
        let p = Palette::new(&Default::default(), Default::default());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                ..Default::default()
            },
            |ui| super::usage(ui, &state, &p),
        );
        let text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape { Some(text.galley.job.text.as_str()) } else { None }
            })
            .collect();
        assert!(text.contains(&"Claude") && text.contains(&"Codex"));
        assert_eq!(text.iter().filter(|text| **text == "Session  no data yet").count(), 2);
        assert_eq!(text.iter().filter(|text| **text == "Weekly  no data yet").count(), 1);
        assert_eq!(text.iter().filter(|text| **text == "Weekly  not reported by this account").count(), 1);
        assert!(text.contains(&"just now"));
        assert!(
            !text.iter().any(|text| text.contains('%')),
            "expired and missing windows must never appear as percentages"
        );
        output.drop_without_applying_deltas();
    }

    #[test]
    fn weekly_only_reports_replace_the_initial_session_placeholder() {
        let ctx = egui::Context::default();
        let now = 1_800_000_000;
        let mut state = ResourceInfo { sampled_at: now, ..Default::default() };
        let p = Palette::new(&Default::default(), Default::default());
        let mut usage =
            Usage::codex(include_str!("../../atomics-core/tests/fixtures/codex-token-count-weekly-only.json")).unwrap();
        usage.observed_at = now;
        usage.weekly.as_mut().unwrap().resets_at = now + 86400;
        for reported in [false, true] {
            if reported {
                state.codex.update(usage.clone());
                // Missing windows must be explicit even when no plan name is supplied.
                usage.plan = None;
                state.claude.update(usage.clone());
            }
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                    ..Default::default()
                },
                |ui| super::usage(ui, &state, &p),
            );
            let text: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                    _ => None,
                })
                .collect();
            let session = if reported { "Session  not reported by this account" } else { "Session  no data yet" };
            assert_eq!(text.iter().filter(|text| **text == session).count(), 2);
            if reported {
                assert!(!text.iter().any(|text| text.contains("no data yet")));
                assert_eq!(text.iter().filter(|text| text.starts_with("Weekly  90% · resets ")).count(), 2);
            }
            output.drop_without_applying_deltas();
        }
    }
    #[test]
    fn populated_panel_renders_remaining_reset_age_and_proportional_bars() {
        let ctx = egui::Context::default();
        let now = 1_800_000_000;
        let window = |remaining| Some(Window { remaining, resets_at: now + 8040 });
        let state = ResourceInfo {
            claude: Usage { session: window(76.0), weekly: window(24.0), observed_at: now - 120, plan: None },
            codex: Usage { session: window(9.0), weekly: window(100.0), observed_at: now - 7200, plan: None },
            sampled_at: now,
            ..Default::default()
        };
        let p = Palette::new(&Default::default(), Default::default());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                ..Default::default()
            },
            |ui| usage(ui, &state, &p),
        );
        let text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains(&"Session  76% · resets in 2h 14m"));
        assert!(text.contains(&"Session  9% · resets in 2h 14m"));
        for remaining in [24, 100] {
            let weekly =
                format!("Weekly  {remaining}% · {}", reset_label(state.claude.weekly.as_ref().unwrap(), true, now));
            assert!(text.contains(&weekly.as_str()));
        }
        assert!(text.contains(&"2m ago") && text.contains(&"2h ago"));
        let bars: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) if rect.rect.height() == 3.0 => Some(rect),
                _ => None,
            })
            .collect();
        assert_eq!(bars.len(), 8);
        for (pair, (remaining, color)) in bars.as_chunks::<2>().0.iter().zip([
            (76.0, p.success),
            (24.0, p.warning),
            (9.0, p.error),
            (100.0, p.success),
        ]) {
            assert_eq!(pair[0].fill, p.border);
            assert_eq!(pair[1].fill, color);
            assert!((pair[1].rect.width() / pair[0].rect.width() * 100.0 - remaining).abs() < 0.001);
        }
        output.drop_without_applying_deltas();
    }

    #[test]
    fn colors_use_strict_remaining_thresholds_and_missing_is_explicit() {
        let p = Palette::new(&Default::default(), Default::default());
        for (remaining, expected) in [(30.0, p.success), (29.9, p.warning), (10.0, p.warning), (9.9, p.error)] {
            assert_eq!(remaining_color(&Window { remaining, resets_at: 1 }, &p), expected);
        }
        assert_eq!(missing_label(&Usage::default(), &None), "no data yet");
        assert_eq!(
            missing_label(&Usage { observed_at: 1, plan: Some("free".into()), ..Usage::default() }, &None),
            "not reported by this account"
        );
    }
}
