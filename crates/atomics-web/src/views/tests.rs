//! The views drawn headless over a live fake host: the desktop's frame in a wide window, every pane on a
//! phone, and what the user asks for reaches the right place.

use atomics_core::approvals::Decision;
use atomics_core::theme::ThemeMode;
use atomics_core::wire::Message;
use serde_json::json;

use super::*;
use crate::fake_host::{self, WORKTREE};
use crate::world::{PARENT, World};

/// One frame at a phone's size.
fn frame(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View) {
    sized(ctx, world, client, view, (390.0, 844.0), Vec::new());
}

/// One frame `size` points large, with `events`; the text it drew.
fn sized(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    (width, height): (f32, f32),
    events: Vec<egui::Event>,
) -> Vec<String> {
    view.now_ms += 16.0;
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
        time: Some(view.now_ms / 1000.0),
        events,
        ..egui::RawInput::default()
    };
    let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, client, view));
    output.textures_delta.clear();
    world.pump();
    output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect()
}

/// The wide window the browser test and the desktop's QA screenshots use.
const WIDE: (f32, f32) = (1600.0, 1000.0);

fn click(at: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(at), button(true), button(false)]
}

fn live() -> (egui::Context, World, Client, View) {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    (egui::Context::default(), world, Client::default(), View::default())
}

#[test]
fn every_tab_draws_over_a_live_host() {
    let (ctx, mut world, mut client, mut view) = live();
    for tab in [Tab::Agents, Tab::Board, Tab::Approvals, Tab::Diffs, Tab::Terminal, Tab::Chief] {
        view.tab = tab;
        frame(&ctx, &mut world, &mut client, &mut view);
        frame(&ctx, &mut world, &mut client, &mut view);
        assert!(client.commands.is_empty(), "commands are carried out after each frame");
    }
}

#[test]
fn opening_a_diff_fetches_it_and_shows_it_in_the_panel() {
    let (ctx, mut world, mut client, mut view) = live();
    world.host.received.clear();
    client.send(Command::ShowRoot(WORKTREE.into()));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!((view.tab, view.diff.as_deref()), (Tab::Diffs, Some(Path::new(WORKTREE))));
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })));
    frame(&ctx, &mut world, &mut client, &mut view);
    let state = client.diff_views.get(Path::new(WORKTREE)).expect("the diff view's state");
    let prepared = state.prepared.as_ref().expect("the diff");
    assert!(!state.loading && prepared.diff.files.iter().all(|f| !f.hunks.is_empty()));

    client.send(Command::CloseDiff);
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!(view.diff, None);

    // A task card's session opens its worktree's diff too.
    client.send(Command::OpenTaskSession("s1".into()));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!(view.diff.as_deref(), Some(Path::new(WORKTREE)));
}

#[test]
fn a_decision_recorded_in_a_frame_reaches_the_host() {
    let (ctx, mut world, mut client, mut view) = live();
    view.tab = Tab::Approvals;
    frame(&ctx, &mut world, &mut client, &mut view);
    // What the banner's Allow button records.
    client.approval_decisions.insert(("s1".into(), "a1".into()));
    client.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(world.sent().iter().any(|m| matches!(m, Message::ApprovalDecide(d) if d.approval == "a1")));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(client.approval_decisions.is_empty(), "the host answered it, so the banner is gone");
}

#[test]
fn the_theme_follows_the_hosting_page() {
    let mut world = World::paired(fake_host::host());
    let mut data = world.boot_data(None);
    data["scheme"] = json!("light");
    data["theme"] = json!("light");
    world.driver.message(PARENT, true, &data).unwrap();
    world.pump();
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    apply_theme(&mut view, world.driver.boot().unwrap());
    assert_eq!(view.looks.desktop.mode, ThemeMode::Light);
    // A phone's frame draws in the phone's look of the theme: proportional UI text, zoomed, sized for touch.
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!((client.palette.mode, client.palette.mono_ui), (ThemeMode::Light, false));
    assert_eq!(ctx.zoom_factor(), layout::ZOOM);
    assert_eq!(client.touch.map(|t| t.target), Some(layout::TOUCH));
    assert_eq!(ctx.global_style().spacing.interact_size.y, layout::TOUCH);
    // A desktop's in the desktop's.
    view.layout = LayoutMode::Desktop;
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(std::sync::Arc::ptr_eq(&client.palette, &view.looks.desktop) && client.touch.is_none());
    assert_eq!(ctx.zoom_factor(), 1.0);
    assert_eq!(Tab::named("approvals"), Tab::Approvals);
    assert_eq!(Tab::named("TICKET-1"), Tab::Board, "a view this client cannot name opens the board");
}

#[test]
fn the_pairing_screen_and_a_stop_draw() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    frame(&ctx, &mut world, &mut client, &mut view);
    let mut world = World::new(fake_host::host());
    world.boot(None);
    assert!(matches!(world.driver.status(), Status::Stopped(_)));
    frame(&ctx, &mut world, &mut client, &mut view);
}

#[test]
fn a_boots_view_and_ticket_key_open_their_pane_or_plan() {
    for (view, tab) in
        [("chief", Tab::Chief), ("terminal", Tab::Terminal), ("diffs", Tab::Diffs), ("ENG-1234", Tab::Board)]
    {
        assert_eq!(Tab::named(view), tab, "{view}");
    }
    // A second plan, which a task's ticket names; the first plan follows its own ticket.
    let mut host = fake_host::host();
    let mut second = host.board.plans[0].clone();
    second.id = "p2".into();
    second.goal = "Ship the parser".into();
    second.ticket = None;
    second.tasks[1].ticket = Some("ENG-77".into());
    host.board.plans.push(second);
    for (key, plan) in [(Some("eng-77"), 1), (Some("ENG-40"), 0), (Some("ENG-41"), 0), (Some("ENG-999"), 0)] {
        let mut world = World::paired(fake_host::host());
        world.host.board = host.board.clone();
        let (ctx, mut client) = (egui::Context::default(), Client::default());
        let mut view = View { tab: Tab::Chief, ticket_key: key.map(str::to_string), ..View::default() };
        view.desk.selected_plan = 5;
        frame(&ctx, &mut world, &mut client, &mut view);
        assert_eq!(view.ticket_key.as_deref(), key, "kept until the board is in");
        world.boot(None);
        frame(&ctx, &mut world, &mut client, &mut view);
        assert_eq!((view.tab, view.ticket_key.as_deref()), (Tab::Board, None), "{key:?}: the board");
        assert_eq!(view.desk.selected_plan, plan, "{key:?}: its plan, else the first");
    }
}

#[test]
fn a_wide_window_draws_the_desktops_frame_without_the_hosts_own_items() {
    let (ctx, mut world, mut client, mut view) = live();
    let mut text = Vec::new();
    for _ in 0..3 {
        text = sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    assert!(!view.compact && client.touch.is_none());
    assert!(Arc::ptr_eq(&client.palette, &view.looks.desktop) && client.palette.mono_ui, "the theme's monospace UI");
    let has = |label: &str| text.iter().any(|t| t.starts_with(label));
    // The top bar, the chief docked with its terminal, the agent's tile with its approval, the plan's board.
    for shown in [
        "ATOMICS",
        "◆ Parse faster",
        "CHIEF",
        "Hide diff",
        "Plan 0/2",
        "Connected",
        "⋯",
        "◆ CHIEF AGENT",
        "float",
        "minimize",
        "Fix the parser",
        "Permission requested",
        "Allow",
        "PLAN",
        "Approve plan",
        "Click the terminal to type",
    ] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    for host_only in ["USAGE", "TICKETS", "Settings", "Tickets", "load ", "Remote", "Project", "+ New", "Steer"] {
        assert!(!has(host_only), "{host_only} is the host's: {text:?}");
    }
    // Both terminals are followed, the agent's (the active session) with the page's input over it.
    assert!(world.host.terminals["s1"].followed && world.host.terminals["chief"].followed);
    assert_eq!(view.term.session(), Some("s1"));
    let overlay = view.term.overlay.rect.expect("the input over the agent's terminal");
    assert!(overlay.left() > 500.0 && overlay.right() < 1100.0, "in the middle, between the dock and the board");
    assert!(world.sent().iter().all(|m| !matches!(m, Message::TerminalResize { .. })), "nothing resized");
}

#[test]
fn a_click_into_the_chiefs_terminal_types_there() {
    let (ctx, mut world, mut client, mut view) = live();
    for _ in 0..3 {
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    // The dock is on the left, under its header.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(200.0, 400.0)));
    assert_eq!(view.desk.typing_in.as_deref(), Some("chief"));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("chief"));
    let overlay = view.term.overlay.clone();
    assert!(overlay.rect.is_some_and(|r| r.right() < 600.0) && overlay.focus, "the input moves to it, focused");
    // Hiding the chief gives the input back to the agent.
    client.send(Command::HideChief);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("s1"));
    assert!(!world.host.terminals["chief"].followed, "a hidden chief's terminal is not followed");
}

#[test]
fn a_wide_window_over_a_host_without_plans_shows_every_agent_and_no_plan_button() {
    let mut host = fake_host::host();
    host.board.plans.clear();
    let mut world = World::paired(host);
    world.boot(None);
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    let mut text = Vec::new();
    for _ in 0..3 {
        text = sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    assert!(text.iter().any(|t| t == "Fix the parser"), "{text:?}");
    assert!(!text.iter().any(|t| t.starts_with("Plan")), "no Plan button without a plan: {text:?}");
    assert_eq!(view.term.session(), Some("s1"));
}

#[test]
fn a_wide_window_shows_a_diff_and_fetches_only_it() {
    let (ctx, mut world, mut client, mut view) = live();
    for _ in 0..2 {
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    assert!(!world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })), "the board shows, no diff");
    client.send(Command::TogglePlan);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })), "the active session's diff");
    let text = sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(text.iter().any(|t| t.contains("src/a.rs")), "{text:?}");
}

#[test]
fn the_layout_is_the_users_choice_over_the_screens() {
    let (ctx, mut world, mut client, mut view) = live();
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(view.compact, "a phone's width");
    view.layout = LayoutMode::Desktop;
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(!view.compact);
    view.layout = LayoutMode::Auto;
    view.viewport.coarse = true;
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(view.compact);
}
