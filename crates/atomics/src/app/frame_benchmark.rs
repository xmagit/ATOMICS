//! Headless desktop frame benchmark. Run on an isolated test home with an optimized, symbolized build:
//! `ATOMICS_FRAME_BENCH=/tmp/frames.csv CARGO_PROFILE_RELEASE_STRIP=false cargo test --release
//! -p atomics --bin atomics app::frame_benchmark::desktop_frames -- --nocapture`.
//! Set `ATOMICS_FRAME_BENCH_SAMPLE` to a report path to sample the Simple workload on macOS.
//! This runs the native desktop's layout, paint and tessellation at a 60 Hz input cadence without a GPU.

use super::*;
use atomics_core::config::UiMode;
use atomics_core::hook::HookEvent;
use atomics_core::projection::{ChiefConversation, ChiefTurn};
use atomics_core::trace::TraceLog;
use std::fmt::Write;

const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);
const ACTIVE: Duration = Duration::from_secs(6);
const IDLE_FRAMES: u32 = 180;
const IDLE_SETTLE_FRAMES: u32 = 60;
const SIZE: [f32; 2] = [1440.0, 900.0];

#[test]
fn desktop_frames() {
    let Some(report) = std::env::var_os("ATOMICS_FRAME_BENCH") else { return };
    tests::with_state("app::frame_benchmark::desktop_frames", |mut state, ctx| {
        let mut csv = String::from("mode,action,frames,fps,work_p95_ms\n");
        seed(&mut state);
        let mut app = desktop(state, ctx, UiMode::Full);
        let clock = Instant::now();
        wait_for_trace(&app, 200);
        frame(ctx, &mut app, clock, vec![]);
        measure(ctx, &mut app, clock, "full", "output", &mut csv, |_, _| vec![]);
        // Reuse the isolated runtime, with a fresh client for the Simple view.
        let repaint = track_repaints(ctx);
        {
            let mut state = app.host.state();
            seed_trace(&mut state, true);
        }
        app.host.wake();
        wait_for_trace(&app, 221);
        app.client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
        app.client.ui.mode = Some(UiMode::Simple);
        app.client.ui.simple.focus_composer = true;
        for _ in 0..5 {
            frame(ctx, &mut app, clock, vec![]);
            std::thread::sleep(FRAME);
        }
        let shown: Vec<(String, egui::Rect)> =
            ctx.data(|data| data.get_temp(egui::Id::NULL.with("simple-shown")).unwrap_or_default());
        let composer = shown.iter().find(|(name, _)| name == "composer").expect("composer was drawn").1;
        let tracks: Vec<_> = shown
            .iter()
            .filter(|(name, _)| name.starts_with("row:"))
            .map(|(_, rect)| {
                let y =
                    if rect.height() >= atomics_ui::flow::NARROW_ROW { rect.bottom() - 18.0 } else { rect.center().y };
                egui::pos2(rect.left() + rect.width() * 0.8, y)
            })
            .collect();
        assert!(tracks.len() > 4, "the flow graph is visible alongside the conversation: {shown:?}");
        let mut sampler = std::env::var_os("ATOMICS_FRAME_BENCH_SAMPLE").map(|output| {
            std::process::Command::new("/usr/bin/sample")
                .args([std::process::id().to_string().as_str(), "5", "1", "-file"])
                .arg(output)
                .spawn()
                .expect("start sampling this benchmark process")
        });
        let transcript = egui::pos2(composer.center().x, 400.0);
        measure(ctx, &mut app, clock, "simple", "scroll", &mut csv, |_, elapsed| {
            vec![
                egui::Event::PointerMoved(transcript),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, if elapsed < ACTIVE / 2 { 84.0 } else { -84.0 }),
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Wheel,
                },
            ]
        });
        app.client.ui.simple.focus_composer = true;
        frame(ctx, &mut app, clock, Vec::new());
        measure(ctx, &mut app, clock, "simple", "type", &mut csv, |i, _| {
            if i == 0 {
                vec![egui::Event::PointerMoved(composer.center()), egui::Event::Text("Review ".into())]
            } else {
                vec![egui::Event::Text("a".into())]
            }
        });
        assert!(app.client.ui.simple.composer.starts_with("Review "), "typed into the composer");
        ctx.memory_mut(|memory| {
            if let Some(focus) = memory.focused() {
                memory.surrender_focus(focus);
            }
        });
        measure(ctx, &mut app, clock, "simple", "hover", &mut csv, |i, _| {
            vec![egui::Event::PointerMoved(tracks[i / 4 % tracks.len()])]
        });
        if let Some(mut sampler) = sampler.take() {
            assert!(sampler.wait().unwrap().success());
        }
        {
            let mut state = app.host.state();
            for session in state.sessions.iter_mut().filter(|session| !session.chief) {
                session.terminal.take();
                session.status = AgentStatus::Idle;
            }
        }
        app.client.ui.simple.traffic = Default::default();
        frame(ctx, &mut app, clock, vec![egui::Event::PointerGone]);
        let start = Instant::now();
        let mut times = Vec::new();
        // Give the hover and scroll animations time to settle, while continuing to service
        // their repaint requests just as the desktop event loop would.
        for i in 0..IDLE_SETTLE_FRAMES + IDLE_FRAMES {
            sleep_until(start + FRAME * i);
            let due = {
                let mut next = repaint.lock().unwrap();
                let due = next.is_some_and(|at| at <= Instant::now());
                if due {
                    *next = None;
                }
                due
            };
            if due {
                let work = frame(ctx, &mut app, clock, Vec::new());
                if i >= IDLE_SETTLE_FRAMES {
                    times.push(work);
                }
            }
        }
        row(&mut csv, "simple", "idle", start.elapsed() - FRAME * IDLE_SETTLE_FRAMES, times);
        std::fs::write(&report, csv).unwrap();
    });
    if std::env::var_os("ATOMICS_APP_TEST_CHILD").is_none() {
        print!("{}", std::fs::read_to_string(report).unwrap());
    }
}

fn desktop(state: State, ctx: &egui::Context, mode: UiMode) -> App {
    let mut client = Client::new(ctx, &mut Options::default(), &crate::load_icon());
    client.ui.mode = Some(mode);
    App::with(client, state.into_host())
}

fn wait_for_trace(app: &App, entries: usize) {
    let projections = app.host.projections();
    let deadline = Instant::now() + Duration::from_secs(20);
    while projections.trace.load().data.entries.len() != entries {
        assert!(Instant::now() < deadline, "host did not publish the {entries}-entry fixture");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn track_repaints(ctx: &egui::Context) -> Arc<Mutex<Option<Instant>>> {
    let next = Arc::new(Mutex::<Option<Instant>>::new(None));
    let pending = next.clone();
    ctx.set_request_repaint_callback(move |info| {
        if let Some(at) = Instant::now().checked_add(info.delay) {
            let mut next = pending.lock().unwrap();
            *next = Some(next.map_or(at, |pending| pending.min(at)));
        }
    });
    next
}

fn sleep_until(at: Instant) {
    std::thread::sleep(at.saturating_duration_since(Instant::now()));
}

fn measure(
    ctx: &egui::Context,
    app: &mut App,
    clock: Instant,
    mode: &str,
    action: &str,
    csv: &mut String,
    events: impl Fn(usize, Duration) -> Vec<egui::Event>,
) {
    let start = Instant::now();
    let mut next = start;
    let mut times = Vec::new();
    while start.elapsed() < ACTIVE {
        sleep_until(next);
        let index = times.len();
        // Keep the scheduled cadence when the OS wakes the thread a little late. If a whole
        // frame was missed, restart the cadence without replaying the missed frames in a burst.
        next = (next + FRAME).max(Instant::now());
        if mode == "simple" && index % 16 == 0 {
            let mut state = app.host.state();
            let task = &mut state.plans[0].tasks[(index / 16) % 4];
            task.report = Some(Report {
                status: "progress".into(),
                summary: "First implementation is running; checking cancellation and repeated submissions.".into(),
                at: atomics_core::store::now(),
                ..Default::default()
            });
            app.host.wake();
        }
        times.push(frame(ctx, app, clock, events(index, start.elapsed())));
    }
    row(csv, mode, action, start.elapsed(), times);
}

fn row(csv: &mut String, mode: &str, action: &str, elapsed: Duration, mut times: Vec<f64>) {
    times.sort_by(f64::total_cmp);
    let frames = times.len();
    let p95 = if times.is_empty() { 0.0 } else { times[(frames - 1) * 95 / 100] };
    writeln!(csv, "{mode},{action},{frames},{:.2},{p95:.3}", frames as f64 / elapsed.as_secs_f64()).unwrap();
}

fn frame(ctx: &egui::Context, app: &mut App, clock: Instant, events: Vec<egui::Event>) -> f64 {
    let start = Instant::now();
    if let Some(stats) = &mut app.frame_stats {
        stats.begin(start);
    }
    let mut raw = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(SIZE[0], SIZE[1]))),
        time: Some(clock.elapsed().as_secs_f64()),
        events,
        ..Default::default()
    };
    app.frame_input(ctx, &mut raw);
    let mut output = ctx.run_ui(raw, |ui| {
        ctx.data_mut(|data| data.remove::<Vec<(String, egui::Rect)>>(egui::Id::NULL.with("simple-shown")));
        app.frame_logic(ctx);
        app.frame_ui(ui);
    });
    output.textures_delta.clear();
    std::hint::black_box(ctx.tessellate(output.shapes, output.pixels_per_point));
    let done = Instant::now();
    if let Some(stats) = &mut app.frame_stats {
        stats.end(done);
    }
    (done - start).as_secs_f64() * 1000.0
}

fn seed(state: &mut State) {
    let mut chief = tests::session("chief", AgentKind::Claude, AgentStatus::Idle, &state.launch_dir);
    chief.chief = true;
    chief.terminal = Some(
        Terminal::spawn(
            state.wake.clone(),
            Vec::new(),
            "/bin/sh".into(),
            vec!["-c".into(), "exec sleep 180".into()],
            &state.launch_dir,
            HashMap::new(),
            600,
        )
        .unwrap(),
    );
    state.sessions.push(chief);
    state.chief_open = true;
    state.selected_plan = 0;
    let mut plan = Plan {
        id: "cleanup".into(),
        goal: "# Clear the import queue\nMake ingestion restartable on a busy host.".into(),
        approved: true,
        ..Default::default()
    };
    for i in 0..18 {
        let id = format!("task-{i}");
        let mut task = Task {
            id: id.clone(),
            title: format!("Check import stage {i} with concurrent requests"),
            status: if i < 4 { TaskStatus::Running } else { TaskStatus::Planned },
            acceptance: vec!["Retries preserve source order".into(), "Cancellation leaves staging clean".into()],
            depends_on: if i >= 16 { vec![format!("task-{}", i - 16)] } else { Vec::new() },
            ..Default::default()
        };
        if i < 4 {
            let mut session = tests::session(&id, AgentKind::Claude, AgentStatus::Working, &state.launch_dir);
            session.terminal = Some(
                Terminal::spawn(
                    state.wake.clone(),
                    Vec::new(),
                    "/bin/sh".into(),
                    vec![
                        "-c".into(),
                        format!("sleep 0.0{i}; while :; do printf 'import: wrote sample output\\n'; sleep 0.25; done"),
                    ],
                    &state.launch_dir,
                    HashMap::new(),
                    600,
                )
                .unwrap(),
            );
            task.session = Some(id);
            state.sessions.push(session);
        }
        plan.tasks.push(task);
    }
    state.plans = vec![plan];
    state.plans.extend((1..10).map(|i| Plan {
        id: format!("maintenance-{i}"),
        goal: format!("Refresh the import examples for dataset {i}"),
        created_at: i,
        ..Default::default()
    }));
    state.store.chief_history.extend((0..65).map(|i| ChiefConversation {
        output: Vec::new(),
        thread: None,
        title: format!("Question about import batch {i}"),
        plan: None,
        agent: ChiefAgent::Claude,
        conversation: format!("fixture-{i}"),
        turns: vec![ChiefTurn { speaker: "user".into(), text: format!("Help with example batch {i}.") }],
    }));
    state.remember_chief_masks(&(0..6).map(|i| format!("fixture-value-{i:02}-opaque-credential")).collect::<Vec<_>>());
    seed_trace(state, false);
}

fn seed_trace(state: &mut State, simple: bool) {
    let now = atomics_core::store::now() * 1000;
    let mut log = TraceLog::default();
    // The native Simple stress fixture goes beyond the rolling host limit to exercise long rendering.
    for i in 0..if simple { 221 } else { 200 } {
        let payload = if !simple {
            json!({
                "hook_event_name":"PostToolUse", "session_id":"chief",
                "tool_name":"mcp__atomics__atomics_ticket_read", "tool_use_id":format!("call-{i}"),
                "tool_input":{"ticket":"DEMO-1"}, "tool_response":{"structuredContent":{"card":{
                    "system":"fixture","id":format!("example-{i}"),"key":format!("DEMO-{i}"),
                    "title":"Review the sample batch", "url":"https://example.test/cards/1",
                    "body":"This fixture explains how the example importer retries a staging write.".repeat(3),
                }}}
            })
        } else if i == 215 {
            json!({
                "hook_event_name":"PostToolUse", "session_id":"chief", "tool_name":"mcp__atomics__atomics_plan_set",
                "tool_use_id":"fixture-plan", "tool_input":{"id":"cleanup"},
                "tool_response":{"structuredContent":{"plan":{"id":"cleanup","goal":"Clear the import queue","tasks":[]}}}
            })
        } else if i % 2 == 0 {
            json!({"hook_event_name":"UserPromptSubmit", "session_id":"chief", "prompt":format!("Can you check item {i}?")})
        } else {
            json!({"hook_event_name":"Stop", "session_id":"chief", "last_assistant_message":format!(
                "### Import result {i}\n\nThe **staging worker** retried the request. Read the [runbook](https://example.test/guide).\n\n- Check the queue\n- Keep the `batch_id`\n\n```rust\nlet result = import(batch_id)?;\nassert!(result.ready());\n```\n\nThe worker is ready for another submission."
            )})
        };
        let event = HookEvent::from_payload(&payload).unwrap();
        let mut single = TraceLog::default();
        single.reserve_order(i + 1);
        let entry = single.ingest_for_plan(&event, None, &Default::default(), now + i, Some("cleanup")).unwrap();
        log.entries.push_back(entry.clone());
    }
    state.traces.insert("chief".into(), log.into());
}
