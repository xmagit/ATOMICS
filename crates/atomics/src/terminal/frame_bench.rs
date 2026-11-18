//! Desktop terminal frame times: the shared painter against the one it replaced (`legacy_paint`),
//! taking turns on the same terminal, and with and without a remote screen stream copying the same
//! emulator. Ignored by default (it takes about a minute); run it with
//! `cargo test -p atomics --bin atomics terminal::frame_bench -- --ignored --nocapture`.
//!
//! A frame is what the desktop does for a terminal pane: `TerminalView::show` (locking the
//! emulator, reading cells, laying out text) and tessellating the shapes. In the `busy` terminal a
//! shell writes coloured lines as fast as it can the whole time, so the PTY thread contends for the
//! same lock; the `tui` terminal holds a full screen of styled cells, wide characters and fills.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use egui::{FontId, Pos2, Rect, vec2};

use super::screen::Tracker;
use super::{Metrics, Terminal, TerminalView};
use crate::protocol::ScreenSource;
use crate::theme::{Palette, Theme};

const FRAMES: usize = 600;
const WARMUP: usize = 30;

const BUSY: &str = "i=0; while :; do printf '\\033[3%dm%06d %s\\033[0m \\033[1mbold\\033[0m 中文\\r\\n' \
                    $((i%7+1)) $i 'the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnopqrstuvwxyz'; \
                    i=$((i+1)); done";

/// A screen full of styles, drawn once the pane has its size: every row alternates colours, fills,
/// dim and underlined text and wide characters.
const TUI: &str = "while [ $(stty size | cut -d' ' -f2) -lt 100 ]; do sleep 0.05; done; r=0; while [ $r -lt 49 ]; do \
                   c=0; while [ $c -lt 14 ]; do \
                   printf '\\033[3%d;4%dm ab%02d \\033[0;%dm中文\\033[0m ' $(((r+c)%8)) $(((r*3+c)%8)) $c $((c%2*2+2)); \
                   c=$((c+1)); done; printf '\\r\\n'; r=$((r+1)); done; exec sleep 600";

fn terminal(script: &str) -> (Terminal, crate::test_support::TempDir) {
    let dir = crate::test_support::tempdir().unwrap();
    let theme = Theme::from_config(&atomics_core::config::ThemeConfig::default());
    let terminal = Terminal::spawn(
        crate::host::Wake::default(),
        theme.terminal_colors(),
        "/bin/sh".into(),
        vec!["-c".into(), script.into()],
        dir.path(),
        HashMap::new(),
        5000,
    )
    .unwrap();
    (terminal, dir)
}

/// What the protocol's screens thread does for one subscribed terminal: copy under the emulator
/// lock, then compare and encode outside it, every `interval`.
fn stream(terminal: &Terminal, interval: Duration, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<(u64, Duration)> {
    let handle = terminal.screen_handle();
    std::thread::spawn(move || {
        let (mut tracker, generations) = (Tracker::default(), AtomicU64::new(0));
        let (mut last, mut copies, mut locked) = (None::<atomics_core::screen::Screen>, 0u64, Duration::ZERO);
        let mut revision = 0;
        while !stop.load(Ordering::Acquire) {
            let started = Instant::now();
            let screen = handle.capture(&mut tracker, &generations);
            locked += started.elapsed();
            copies += 1;
            if let Some(old) = &last
                && let Some(delta) = old.delta(&screen, "s", tracker.generation, revision, revision + 1)
            {
                revision += 1;
                let envelope = atomics_core::wire::Envelope::host("e", atomics_core::wire::Message::ScreenDelta(delta));
                let _ = atomics_core::wire::encode(&envelope);
            }
            last = Some(screen);
            std::thread::sleep(interval);
        }
        (copies, locked)
    })
}

struct Stats {
    p50: Duration,
    p95: Duration,
    p99: Duration,
    max: Duration,
    mean: Duration,
}

fn stats(mut times: Vec<Duration>) -> Stats {
    times.sort();
    let at = |q: f64| times[((times.len() as f64 - 1.0) * q).round() as usize];
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    Stats { p50: at(0.5), p95: at(0.95), p99: at(0.99), max: *times.last().unwrap(), mean }
}

/// Frame times of each painter (`true` for the old one), each with its own view and context.
/// They take turns, frame by frame and in alternating order, so load on a shared machine falls on
/// all of them alike.
fn frames(terminal: &Terminal, painters: &[bool]) -> Vec<Vec<Duration>> {
    let palette = Theme::from_config(&atomics_core::config::ThemeConfig::default()).palette();
    let metrics = Metrics { font: FontId::monospace(12.0), cell: vec2(7.25, 15.0) };
    let rect = Rect::from_min_size(Pos2::ZERO, vec2(1200.0, 760.0));
    let mut panes: Vec<_> =
        painters.iter().map(|&legacy| (legacy, egui::Context::default(), TerminalView::default())).collect();
    let mut times = vec![Vec::with_capacity(FRAMES); painters.len()];
    for i in 0..FRAMES + WARMUP {
        let order: Vec<usize> = if i % 2 == 0 { (0..panes.len()).collect() } else { (0..panes.len()).rev().collect() };
        for at in order {
            let (legacy, ctx, view) = &mut panes[at];
            let started = Instant::now();
            let input = egui::RawInput { screen_rect: Some(rect), ..Default::default() };
            let output = ctx.run_ui(input, |ui| {
                let id = ui.scope_id().with("terminal");
                if *legacy {
                    show_legacy(view, terminal, ui, rect, id, &metrics, &palette);
                } else {
                    view.show(terminal, ui, rect, id, (&metrics, &palette));
                }
            });
            let egui::FullOutput { shapes, pixels_per_point, mut textures_delta, .. } = output;
            let _ = ctx.tessellate(shapes, pixels_per_point);
            textures_delta.clear(); // no GPU here to upload font textures to
            if i >= WARMUP {
                times[at].push(started.elapsed());
            }
        }
        // Roughly a 120 Hz display: the PTY thread gets the lock between frames, as it does live.
        std::thread::sleep(Duration::from_millis(8));
    }
    times
}

/// `TerminalView::show` for an unfocused pane, painting with the old painter under the lock.
fn show_legacy(
    view: &mut TerminalView,
    terminal: &Terminal,
    ui: &mut egui::Ui,
    rect: Rect,
    id: egui::Id,
    metrics: &Metrics,
    palette: &Palette,
) {
    let response = ui.interact(rect, id, egui::Sense::click_and_drag());
    terminal.shown(false);
    let inner = rect.shrink2(vec2(6.0, 4.0));
    view.resize(terminal, inner, metrics);
    view.handle_mouse(terminal, ui, &response, inner, metrics);
    let term = terminal.term.lock();
    view.sync_selection(terminal, &term);
    super::legacy_paint::paint(&term, view.selection.as_ref(), view.grid.1, ui, (rect, inner), metrics, palette, false);
}

fn report(variant: &str, s: &Stats) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "frame-times variant={variant} frames={FRAMES} mean={:.3}ms p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        ms(s.mean),
        ms(s.p50),
        ms(s.p95),
        ms(s.p99),
        ms(s.max)
    );
}

#[test]
#[ignore = "a measurement, not a check; run with --ignored --nocapture"]
fn terminal_frame_times() {
    let (tui, _tui_dir) = terminal(TUI);
    // The size `frames` gives the pane, so the screen is drawn once at it.
    tui.resize(alacritty_terminal::event::WindowSize { num_lines: 50, num_cols: 163, cell_width: 7, cell_height: 15 });
    let (terminal, _dir) = terminal(BUSY);
    let deadline = Instant::now() + Duration::from_secs(20);
    while tui.screen_text(50).lines().count() < 49 {
        assert!(Instant::now() < deadline, "the styled screen was not drawn: {}", tui.screen_text(50));
        std::thread::sleep(Duration::from_millis(50));
    }
    for (name, pane) in [("busy", &terminal), ("tui", &tui)] {
        // Each painter alone, as in the app, in alternating blocks so drift falls on both.
        for round in 1..=2 {
            for (painter, legacy) in [("legacy", true), ("shared", false)] {
                report(&format!("{name}-{painter}-painter-alone-{round}"), &stats(frames(pane, &[legacy]).remove(0)));
            }
        }
        // Taking turns frame by frame. The old painter holds the lock while it paints, so the PTY
        // thread's backlog lands on whichever frame comes next.
        let [legacy, shared] = <[Vec<Duration>; 2]>::try_from(frames(pane, &[true, false])).unwrap();
        report(&format!("{name}-legacy-painter-turns"), &stats(legacy));
        report(&format!("{name}-shared-painter-turns"), &stats(shared));
    }
    tui.shutdown();
    for (name, interval) in [("stream-100ms", Duration::from_millis(100)), ("stream-10ms", Duration::from_millis(10))] {
        let stop = Arc::new(AtomicBool::new(false));
        let streamer = stream(&terminal, interval, stop.clone());
        report(name, &stats(frames(&terminal, &[false]).remove(0)));
        stop.store(true, Ordering::Release);
        let (copies, locked) = streamer.join().unwrap();
        println!(
            "screen-copies variant={name} copies={copies} mean_lock={:.3}ms",
            locked.as_secs_f64() * 1000.0 / copies.max(1) as f64
        );
    }
    terminal.shutdown();
}
