//! Directional timings of a native client's window thread over a busy host, ignored by default: large plan updates
//! applied as they arrive (decoding, the remote backend and its mirror), and frames drawn over a large board. A
//! measurement, not a check; run with
//! `cargo test -p atomics-web --release client_bench -- --ignored --nocapture --test-threads 1`.

use std::time::{Duration, Instant};

use atomics_core::ops::Grant;
use atomics_core::plan::{PlanStatus, TaskStatus};
use atomics_core::projection::{HostInfo, ManagementInfo, PlanInfo, SessionInfo, TaskInfo, TaskLink};
use atomics_core::session::AgentStatus;
use atomics_core::ticket_ui::{TicketReply, TicketsInfo};
use atomics_core::trace_feed::TraceData;
use atomics_core::wire::{self, Envelope, Limits, Message, ResetReason, Resume, Revisions, Role, Seq, VERSION};
use atomics_ui::Client;

use crate::driver::{Driver, Output, Status};
use crate::views::{self, View};

const EPOCH: &str = "bench";
/// Tasks in the large plan: each with a full brief, about 3 KB of JSON.
const TASKS: usize = 70;
const SESSIONS: usize = 40;
const UPDATES: usize = 200;
const FRAMES: usize = 120;

fn session(n: usize) -> SessionInfo {
    SessionInfo {
        status_hint: None,
        previews: Vec::new(),
        id: format!("s{n}"),
        agent: "claude".into(),
        title: format!("Session {n}"),
        cwd: "/w/repo".into(),
        worktree: Some(format!("/w/repo-t{n}")),
        branch: Some(format!("atomics/t{n}")),
        status: if n.is_multiple_of(3) { AgentStatus::Working } else { AgentStatus::Idle },
        chief: n == 0,
        task: (n > 0).then(|| TaskLink { plan: "big".into(), task: format!("t{n}") }),
        pending_approvals: 0,
        activity: None,
    }
}

/// The large plan as its `revision`th update has it: one task's status moves each time.
fn plan(revision: usize) -> PlanInfo {
    let task = |n: usize| TaskInfo {
        pull_request: None,
        id: format!("t{n}"),
        title: format!("Task {n}: make the parser handle every edge case of the grammar"),
        status: if n == revision % TASKS { TaskStatus::Review } else { TaskStatus::Running },
        agent: "claude".into(),
        reviewer_reason: None,
        depends_on: (n > 0).then(|| format!("t{}", n - 1)).into_iter().collect(),
        session: Some(format!("s{n}")),
        branch: Some(format!("atomics/t{n}")),
        integrated: None,
        additions: n,
        deletions: n / 2,
        report: None,
        ticket: Some(format!("ENG-{n}")),
        acceptance: vec!["The tests pass".into(), "The parser is no slower".into()],
        model: Some("Claude Opus 5.5".into()),
        brief: format!("Revision {revision}. {}", "Read the grammar, write the cases, run them. ".repeat(44)),
        brief_cut: false,
    };
    PlanInfo {
        created_at: 0,
        first_message: Some("Make the parser fast".into()),
        publish: None,
        pull_request: None,
        notebook: Vec::new(),
        coordination_enabled: false,
        coordination: Default::default(),
        rules: Default::default(),
        ticket_tree: None,
        id: "big".into(),
        status: PlanStatus::Open,
        goal: "Parse faster".into(),
        project: Some("/w/repo".into()),
        approved: true,
        integration: None,
        tasks: (0..TASKS).map(task).collect(),
        digest: "digest".into(),
        ticket: Some("ENG-1".into()),
    }
}

/// A host with a large configuration and a full page of ticket replies.
fn host() -> HostInfo {
    let mut values = serde_json::Map::new();
    for n in 0..1500 {
        values.insert(format!("section.key_{n}"), serde_json::json!({"value": format!("value {n}"), "source": "main"}));
    }
    values.insert("agents".into(), serde_json::json!({"value": [{"name": "claude"}], "source": "default"}));
    let reply = |n: usize| TicketReply {
        cards: (0..12).map(|c| crate::fake_host::ticket_card(&format!("ENG-{n}{c}"))).collect(),
        ..TicketReply::new(format!("request-{n}"))
    };
    HostInfo {
        management: Some(ManagementInfo {
            config: serde_json::json!({ "values": values, "files": [] }),
            ..ManagementInfo::default()
        }),
        tickets: Some(TicketsInfo {
            system: "tracker".into(),
            replies: (0..12).map(reply).collect(),
            ..Default::default()
        }),
        ..HostInfo::default()
    }
}

/// The chief's trace at its bounds: 200 calls of about 2.5 KB each.
fn trace() -> TraceData {
    use atomics_core::{hook::HookEvent, tickets::Cleaner, trace::TraceLog};
    let mut log = TraceLog::default();
    for n in 0..200 {
        let event = HookEvent::from_payload(&serde_json::json!({
            "session_id": "chief", "hook_event_name": "PostToolUse", "tool_name": "mcp__atomics__atomics_ticket_search",
            "tool_use_id": format!("call-{n}"), "tool_input": {"query": format!("{n} {}", "parse faster ".repeat(150))},
            "tool_response": {"text": "done"}
        }))
        .unwrap();
        log.ingest(&event, None, &Cleaner::default(), n).unwrap();
    }
    TraceData { session: Some("chief".into()), entries: log.entries.into_iter().collect(), ..Default::default() }
}

/// Tell the client the chief's trace changed to `revision`, and answer its reads until it holds `data`.
fn load_trace(driver: &mut Driver, socket: u64, revision: u64, data: &TraceData) -> Vec<Duration> {
    let mut times = Vec::new();
    driver.socket_message(socket, &frame(Message::TraceChanged { revision: Seq(revision) }));
    loop {
        let asked = driver.take_outputs().into_iter().find_map(|output| match output {
            Output::Send { message, .. } => {
                let envelope = wire::decode(std::str::from_utf8(&message).unwrap()).unwrap();
                match envelope.message {
                    Message::TraceGet { revision, offset, limit } => {
                        Some((revision, offset, limit, envelope.request_id))
                    }
                    _ => None,
                }
            }
            _ => None,
        });
        let Some((revision, offset, limit, id)) = asked else { return times };
        let page = data.page(revision.0, offset, limit, wire::MAX_FRAME_BYTES - 1024).unwrap();
        let reply = wire::encode(&Envelope::host(EPOCH, Message::TracePage(page)).reply_to(id)).unwrap();
        let started = Instant::now();
        driver.socket_message(socket, reply.as_bytes());
        times.push(started.elapsed());
    }
}

fn frame(message: Message) -> Vec<u8> {
    wire::encode(&Envelope::host(EPOCH, message)).unwrap().into_bytes()
}

/// A direct client live on a host whose board holds the large plan, its sessions and the host's state.
fn live() -> (Driver, u64) {
    let mut driver = Driver::direct("bench");
    let [Output::DialDirect { socket }] = driver.take_outputs()[..] else { panic!("one dial") };
    driver.socket_open(socket, wire::SUBPROTOCOL);
    driver.take_outputs();
    let welcome = Message::Welcome {
        version: VERSION,
        capabilities: crate::remote::CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        limits: Limits::default(),
        role: Role::Controller,
        resume: Resume::Reset { cursor: Seq(0), reason: ResetReason::NewClient },
        may: vec![Grant::Type, Grant::Approve, Grant::Session, Grant::Plan, Grant::Review, Grant::Host],
    };
    let revisions = Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(1) };
    let snapshot = Message::StateSnapshot {
        cursor: Seq(0),
        revisions,
        sessions: (0..SESSIONS).map(session).collect(),
        plans: vec![plan(0)],
        approvals: Vec::new(),
        diffs: Vec::new(),
        host: HostInfo::default(),
        part: None,
    };
    // The host's state alone fills most of a frame.
    let host = Message::HostUpsert { cursor: Seq(1), host: host() };
    for message in [welcome, snapshot, host] {
        driver.socket_message(socket, &frame(message));
    }
    driver.take_outputs();
    assert_eq!(driver.status(), &Status::Live);
    (driver, socket)
}

/// The plan's updates as the host would send them, after the host's state.
fn updates() -> Vec<Vec<u8>> {
    (1..=UPDATES).map(|n| frame(Message::PlanUpsert { cursor: Seq(n as u64 + 1), plan: plan(n) })).collect()
}

fn report(what: &str, mut times: Vec<Duration>) {
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let total: Duration = times.iter().sum();
    let at = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
    println!(
        "client-bench {what}: n={} total={:.1}ms mean={:.3}ms p50={:.3}ms p95={:.3}ms max={:.3}ms",
        times.len(),
        ms(total),
        ms(total / times.len() as u32),
        ms(at(0.5)),
        ms(at(0.95)),
        ms(*times.last().unwrap())
    );
}

#[test]
#[ignore = "a measurement, not a check; run with --release --ignored --nocapture"]
fn client_bench_applying_large_plan_updates() {
    let updates = updates();
    println!("client-bench plan update frame: {} bytes", updates[0].len());
    // A browser's frame reads each message whole: the text decoded and applied on its one thread.
    let (mut driver, socket) = live();
    let times = updates
        .iter()
        .map(|update| {
            let started = Instant::now();
            driver.socket_message(socket, update);
            started.elapsed()
        })
        .collect();
    assert_eq!(driver.remote.with_mirror(|m| m.cursor), UPDATES as u64 + 1);
    report("apply one plan update from its text", times);
    // A native window gets each frame decoded by its link's thread, and only applies it.
    let (mut driver, socket) = live();
    let (mut decoding, mut applying) = (Vec::new(), Vec::new());
    for update in &updates {
        let started = Instant::now();
        let frame = wire::decode(std::str::from_utf8(update).unwrap());
        decoding.push(started.elapsed());
        let started = Instant::now();
        driver.socket_frame(socket, frame);
        applying.push(started.elapsed());
    }
    assert_eq!(driver.remote.with_mirror(|m| m.cursor), UPDATES as u64 + 1);
    report("decode one plan update (link thread)", decoding);
    report("apply one decoded plan update (window thread)", applying);
}

#[test]
#[ignore = "a measurement, not a check; run with --release --ignored --nocapture"]
fn client_bench_reading_the_chiefs_trace() {
    let (mut driver, socket) = live();
    let data = trace();
    println!(
        "client-bench trace: {} entries, {} bytes",
        data.entries.len(),
        serde_json::to_string(&data).unwrap().len()
    );
    let first = load_trace(&mut driver, socket, 1, &data);
    assert_eq!(atomics_ui::Backend::trace(&driver.remote).entries.len(), data.entries.len());
    report("apply one trace page, first read", first);
    let mut changed = data.clone();
    changed.entries[199].elapsed_ms = Some(5);
    let again = load_trace(&mut driver, socket, 2, &changed);
    assert_eq!(atomics_ui::Backend::trace(&driver.remote), changed);
    report("apply one trace page, after a change", again);
}

#[test]
#[ignore = "a measurement, not a check; run with --release --ignored --nocapture"]
fn client_bench_drawing_frames_over_a_large_board() {
    for mode in [atomics_core::config::UiMode::Full, atomics_core::config::UiMode::Simple] {
        frames_over_a_large_board(mode);
    }
}

fn frames_over_a_large_board(mode: atomics_core::config::UiMode) {
    let (mut driver, socket) = live();
    load_trace(&mut driver, socket, 1, &trace());
    let ctx = egui::Context::default();
    let (mut client, mut view) = (Client::default(), View::default());
    client.mode = Some(mode);
    let (mut now, shapes) = (0.0, std::cell::Cell::new(0));
    let mut draw = |driver: &mut Driver| {
        now += 16.0;
        view.now_ms = now;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))),
            time: Some(now / 1000.0),
            ..egui::RawInput::default()
        };
        let started = Instant::now();
        let output = ctx.run_ui(input, |ui| views::draw(ui, driver, &mut client, &mut view));
        let took = started.elapsed();
        driver.take_outputs();
        shapes.set(output.shapes.len());
        took
    };
    for _ in 0..5 {
        draw(&mut driver);
    }
    println!("client-bench {mode:?}: a frame draws {} shapes", shapes.get());
    let quiet = (0..FRAMES).map(|_| draw(&mut driver)).collect();
    report(&format!("{mode:?}: draw a frame, board quiet"), quiet);
    let updates = updates();
    let busy = updates[..FRAMES]
        .iter()
        .map(|update| {
            let started = Instant::now();
            driver.socket_message(socket, update);
            started.elapsed() + draw(&mut driver)
        })
        .collect();
    report(&format!("{mode:?}: apply an update and draw a frame"), busy);
}
