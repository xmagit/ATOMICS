use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use egui::{Color32, RichText, Sense, vec2};
use imacs_core::usage::{Usage, Window, age_label, reset_label};

use crate::theme::Palette;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const TAIL_BYTES: u64 = 16 * 1024;

pub enum Report {
    Claude(Usage),
    Codex(Usage),
}

pub struct UsagePanel {
    claude: Usage,
    codex: Usage,
    reports: Receiver<Report>,
    stop: Sender<()>,
}

impl UsagePanel {
    pub fn new(ctx: egui::Context) -> (Self, Sender<Report>) {
        let (tx, reports) = channel();
        let (stop, stopped) = channel();
        let home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| imacs_core::paths::home().join(".codex"));
        let poll_tx = tx.clone();
        std::thread::Builder::new()
            .name("imacs-usage".into())
            .spawn(move || poll_codex(&home.join("sessions"), poll_tx, stopped, ctx, POLL_INTERVAL))
            .expect("usage worker");
        (Self { claude: Usage::default(), codex: Usage::default(), reports, stop }, tx)
    }

    pub fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(report) = self.reports.try_recv() {
            changed = true;
            match report {
                Report::Claude(usage) => self.claude.update(usage),
                Report::Codex(usage) => self.codex.update(usage),
            };
        }
        changed
    }

    pub fn show(&self, ui: &mut egui::Ui, p: &Palette) {
        let now = now();
        for (name, usage) in [("Claude", &self.claude), ("Codex", &self.codex)] {
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
}

impl Drop for UsagePanel {
    fn drop(&mut self) {
        let _ = self.stop.send(());
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

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn poll_codex(sessions: &Path, tx: Sender<Report>, stopped: Receiver<()>, ctx: egui::Context, interval: Duration) {
    let mut previous = None;
    loop {
        if let Some(usage) = read_codex(sessions)
            && previous.as_ref() != Some(&usage)
        {
            previous = Some(usage.clone());
            if tx.send(Report::Codex(usage)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
        if stopped.recv_timeout(interval) != Err(std::sync::mpsc::RecvTimeoutError::Timeout) {
            break;
        }
    }
}

fn read_codex(sessions: &Path) -> Option<Usage> {
    let path = newest_rollout(sessions)?;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = length.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(offset.saturating_sub(1))).ok()?;
    let mut preceding = *b"\n";
    if offset > 0 {
        file.read_exact(&mut preceding).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    let tail = if preceding[0] != b'\n' { tail.split_once('\n')?.1 } else { &tail };
    tail.lines().rev().find_map(Usage::codex)
}

fn newest_rollout(sessions: &Path) -> Option<PathBuf> {
    let mut dirs = vec![sessions.to_path_buf()];
    // Codex stores logs under YYYY/MM/DD. Ignore symlinks and unrelated files.
    for _ in 0..3 {
        dirs = dirs
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect();
    }
    dirs.iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter(|entry| {
            entry.file_name().to_str().is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
        })
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max()
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use imacs_core::config::ThemeConfig;
    use serde_json::json;

    #[test]
    fn poller_runs_off_thread_rechecks_and_does_not_repaint_unchanged_data() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join("rollout-test.jsonl");
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }});
        std::fs::write(&path, event.to_string()).unwrap();
        let ctx = egui::Context::default();
        let repaints = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = repaints.clone();
        ctx.set_request_repaint_callback(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        // Clear egui's initial repaint before observing the worker.
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        ctx.run_ui(Default::default(), |_| {}).drop_without_applying_deltas();
        let (tx, rx) = channel();
        let (stop, stopped) = channel();
        let sessions = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || poll_codex(&sessions, tx, stopped, ctx, Duration::from_millis(20)));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), Report::Codex(_)));
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "idle polls must not send data");
        let before = repaints.load(std::sync::atomic::Ordering::SeqCst);
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        assert_eq!(repaints.load(std::sync::atomic::Ordering::SeqCst), before);
        let mut changed = event;
        changed["payload"]["rate_limits"]["primary"]["used_percent"] = json!(25);
        std::fs::write(path, changed.to_string()).unwrap();
        let Report::Codex(usage) = rx.recv_timeout(Duration::from_secs(2)).unwrap() else { panic!("codex report") };
        assert_eq!(usage.session.unwrap().remaining, 75.0);
        stop.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn panel_accepts_latest_account_reports_and_draws_expired_windows_as_missing() {
        let ctx = egui::Context::default();
        let (tx, reports) = channel();
        let (stop, _) = channel();
        let mut panel = UsagePanel { claude: Usage::default(), codex: Usage::default(), reports, stop };
        let now = now();
        for observed_at in [now - 1, now, now - 2] {
            tx.send(Report::Claude(Usage {
                observed_at,
                session: Some(Window { remaining: 73.0, resets_at: now - 1 }),
                ..Usage::default()
            }))
            .unwrap();
        }
        panel.drain();
        assert_eq!(panel.claude.observed_at, now);
        let p = Palette::from_config(&ThemeConfig::default());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                ..Default::default()
            },
            |ui| panel.show(ui, &p),
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
        let (tx, reports) = channel();
        let (stop, _) = channel();
        let mut panel = UsagePanel { claude: Usage::default(), codex: Usage::default(), reports, stop };
        let p = Palette::from_config(&ThemeConfig::default());
        let mut usage =
            Usage::codex(include_str!("../../imacs-core/tests/fixtures/codex-token-count-weekly-only.json")).unwrap();
        usage.observed_at = now();
        usage.weekly.as_mut().unwrap().resets_at = now() + 86400;
        for reported in [false, true] {
            if reported {
                tx.send(Report::Codex(usage.clone())).unwrap();
                // Missing windows must be explicit even when no plan name is supplied.
                usage.plan = None;
                tx.send(Report::Claude(usage.clone())).unwrap();
                panel.drain();
            }
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                    ..Default::default()
                },
                |ui| panel.show(ui, &p),
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
    fn log_reader_uses_newest_rollout_and_bounded_tail() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event = json!({"timestamp":"2026-10-28T15:00:00Z","type":"event_msg","payload":{
            "type":"token_count","rate_limits":{"primary":{"used_percent":15,"window_minutes":300,"resets_at":1793207640}}
        }}).to_string();
        let old = day.join("rollout-old.jsonl");
        std::fs::write(&old, &event).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1))
            .unwrap();
        let newest = day.join("rollout-new.jsonl");
        std::fs::write(&newest, format!("{event}\n{}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(newest_rollout(dir.path()).unwrap(), newest);
        assert!(read_codex(dir.path()).is_none(), "must not read an old event outside the tail");
        std::fs::write(&newest, format!("{}\n{event}\n{{\"partial\":", "x".repeat(TAIL_BYTES as usize))).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().session.unwrap().remaining, 85.0);
        assert!(read_codex(&dir.path().join("missing")).is_none());
    }

    #[test]
    fn log_reader_keeps_a_complete_event_at_the_tail_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/10/28");
        std::fs::create_dir_all(&day).unwrap();
        let event: serde_json::Value =
            serde_json::from_str(include_str!("../../imacs-core/tests/fixtures/codex-token-count-weekly-only.json"))
                .unwrap();
        let event = event.to_string();
        let tail = format!("{event}\n{}", " ".repeat(TAIL_BYTES as usize - event.len() - 1));
        assert_eq!(tail.len(), TAIL_BYTES as usize);
        let path = day.join("rollout-boundary.jsonl");
        std::fs::write(&path, format!("ignored prefix\n{tail}")).unwrap();
        assert_eq!(read_codex(dir.path()).unwrap().weekly.unwrap().remaining, 90.0);
        std::fs::write(&path, format!("ignored prefix{tail}")).unwrap();
        assert!(read_codex(dir.path()).is_none(), "a fragment at the boundary is not an event");
    }

    #[test]
    fn populated_panel_renders_remaining_reset_age_and_proportional_bars() {
        let ctx = egui::Context::default();
        let (_, reports) = channel();
        let (stop, _) = channel();
        let now = now();
        let window = |remaining| Some(Window { remaining, resets_at: now + 8040 });
        let panel = UsagePanel {
            claude: Usage { session: window(76.0), weekly: window(24.0), observed_at: now - 120, plan: None },
            codex: Usage { session: window(9.0), weekly: window(100.0), observed_at: now - 7200, plan: None },
            reports,
            stop,
        };
        let p = Palette::from_config(&ThemeConfig::default());
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(250.0, 400.0))),
                ..Default::default()
            },
            |ui| panel.show(ui, &p),
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
                format!("Weekly  {remaining}% · {}", reset_label(panel.claude.weekly.as_ref().unwrap(), true, now));
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
        let p = Palette::from_config(&ThemeConfig::default());
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
