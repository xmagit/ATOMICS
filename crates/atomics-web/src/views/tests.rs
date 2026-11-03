//! The views drawn headless over a live fake host: the desktop's frame in a wide window, every pane on a
//! phone, and what the user asks for reaches the right place.

use atomics_core::approvals::Decision;
use atomics_core::theme::ThemeMode;
use atomics_core::wire::Message;
use serde_json::json;

use super::*;
use crate::fake_host::{self, WORKTREE};
use crate::term_pane;
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
    size: (f32, f32),
    events: Vec<egui::Event>,
) -> Vec<String> {
    drawn(ctx, world, client, view, size, events).into_iter().map(|(text, _)| text).collect()
}

/// One frame `size` points large, with `events`; the text it drew and where.
fn drawn(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    (width, height): (f32, f32),
    events: Vec<egui::Event>,
) -> Vec<(String, Rect)> {
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
            egui::Shape::Text(text) => Some((text.galley.text().to_string(), text.visual_bounding_rect())),
            _ => None,
        })
        .collect()
}

/// The wide window the browser test and the desktop's QA screenshots use.
const WIDE: (f32, f32) = (1600.0, 1000.0);
/// A window with room for the sidebar beside the docked chief, and for every column of the board.
const BOARD: (f32, f32) = (2800.0, 1000.0);
/// The phone the browser test is, in the points this headless frame is given.
const PHONE: (f32, f32) = (390.0, 844.0);

/// The middle of the first text starting `label`.
fn on(shown: &[(String, Rect)], label: &str) -> egui::Pos2 {
    let found = shown.iter().find(|(text, _)| text.starts_with(label));
    found.unwrap_or_else(|| panic!("{label} is not drawn: {shown:?}")).1.center()
}

/// A few frames at `size`, for the first frame's measuring to settle; the text of the last.
fn settled(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
) -> Vec<(String, Rect)> {
    for _ in 0..3 {
        drawn(ctx, world, client, view, size, Vec::new());
    }
    drawn(ctx, world, client, view, size, Vec::new())
}

fn live_with(host: fake_host::FakeHost) -> (egui::Context, World, Client, View) {
    let mut world = World::paired(host);
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    (egui::Context::default(), world, Client::default(), View::default())
}

fn has(shown: &[(String, Rect)], label: &str) -> bool {
    shown.iter().any(|(text, _)| text.starts_with(label))
}

/// The fake host's approval on the chief instead of its agent.
fn chief_waits(host: &mut fake_host::FakeHost) {
    use atomics_core::session::AgentStatus;
    host.board.approvals[0].session = "chief".into();
    for session in &mut host.board.sessions {
        session.status = if session.chief { AgentStatus::NeedsInput } else { AgentStatus::Idle };
        session.pending_approvals = u32::from(session.chief);
    }
}

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
    live_with(fake_host::host())
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
        view.desk.plan = Some("gone".into());
        frame(&ctx, &mut world, &mut client, &mut view);
        assert_eq!(view.ticket_key.as_deref(), key, "kept until the board is in");
        world.boot(None);
        frame(&ctx, &mut world, &mut client, &mut view);
        assert_eq!((view.tab, view.ticket_key.as_deref()), (Tab::Board, None), "{key:?}: the board");
        assert_eq!(view.desk.hints(&world.driver.remote).selected_plan, plan, "{key:?}: its plan, else the first");
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
        "+ New",
        "Steer · Enter sends",
        "Send",
        "×",
        "Close plan…",
    ] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    for host_only in ["USAGE", "TICKETS", "Settings", "Tickets", "load ", "Remote", "Project"] {
        assert!(!has(host_only), "{host_only} is the host's: {text:?}");
    }
    assert!(!text.iter().any(|t| t.contains("focuses")), "a page has no chord to focus it: {text:?}");
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

#[test]
fn a_page_of_many_plans_opens_on_the_open_plan_with_its_board() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::many_plans());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let hints = view.desk.hints(&world.driver.remote);
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    assert!(hints.plan_view && hints.panel == PanelMode::Plan && hints.selected_plan == 23, "{hints:?}");
    for wanted in ["◆ Parse faster", "Approve plan", "Fix the parser", "Plan 0/2", "History"] {
        assert!(has(&shown, wanted), "{wanted} is missing: {shown:?}");
    }
    assert!(!has(&shown, "Archived"), "no archived plan shows unless it is picked: {shown:?}");
    assert_eq!(view.term.session(), Some("s1"), "the open plan's agent takes the input");

    // Only History reaches an archived plan, and its tab then shows while it does.
    let opened = drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "History")));
    let opened = if has(&opened, "View") { opened } else { settled(&ctx, &mut world, &mut client, &mut view, WIDE) };
    assert_eq!(view.term.overlay.covered, [view.term.overlay.rect.unwrap()], "an open menu takes the pointer");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&opened, "View")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.plan.as_deref(), Some("a-00"));
    assert!(has(&shown, "Archived · read-only") && !has(&shown, "Approve plan"), "{shown:?}");
}

#[test]
fn a_page_keeps_its_plan_when_another_sorts_in_before_it() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::many_plans());
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.hints(&world.driver.remote).selected_plan, 23);
    let plan = fake_host::closed_plan("b-new", atomics_core::plan::PlanStatus::Open, "Another plan");
    let frames = world.host.event(|cursor| Message::PlanUpsert { cursor, plan });
    world.deliver(frames);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    assert_eq!(view.desk.hints(&world.driver.remote).selected_plan, 24, "the same plan, a place later");
    assert!(has(&shown, "Approve plan"), "{shown:?}");
}

#[test]
fn a_host_with_no_open_plan_shows_its_agents_and_no_finished_plans_board() {
    let mut host = fake_host::many_plans();
    host.board.plans.retain(|plan| plan.id != "p1");
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.plan, None);
    assert!(has(&shown, "Fix the parser") && has(&shown, "History"), "{shown:?}");
    for absent in ["PLAN", "Plan", "Archived"] {
        assert!(!has(&shown, absent), "{absent} shows: {shown:?}");
    }
    // A phone says so over the same picker.
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "No plan is open") && has(&shown, "History"), "{shown:?}");
}

#[test]
fn a_phone_shows_the_open_plans_board_with_the_others_behind_a_picker() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::many_plans());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.compact);
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    for wanted in ["◆ Parse faster", "History", "PLAN", "Approve plan", "Task t1"] {
        assert!(has(&shown, wanted), "{wanted} is missing: {shown:?}");
    }
    assert!(!has(&shown, "✓ Archived"), "the archived plans are behind History: {shown:?}");

    // History scrolls, and picks a finished plan; the strip then has both, and the open plan is one tap away.
    let opened = drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&shown, "History")));
    let opened =
        if has(&opened, "✓ Archived") { opened } else { settled(&ctx, &mut world, &mut client, &mut view, PHONE) };
    assert!(has(&opened, "✓ Archived plan number 0"), "{opened:?}");
    let lowest = opened.iter().map(|(_, rect)| rect.bottom()).fold(0.0, f32::max);
    assert!(lowest <= PHONE.1, "the menu stays on the screen and scrolls: {lowest}");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&opened, "✓ Archived plan number 0")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.desk.plan.as_deref(), Some("a-00"));
    assert!(has(&shown, "Archived · read-only") && !has(&shown, "Approve plan"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&shown, "◆ Parse faster")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));

    // The button is on the screen, and a tap approves that plan.
    let approve = on(&shown, "Approve plan");
    assert!(approve.y < PHONE.1 / 2.0, "right under the picker: {approve:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(approve));
    assert!(
        world.sent().iter().any(|m| matches!(m, Message::PlanCommand(c) if c.plan.as_deref() == Some("p1"))),
        "the open plan is approved"
    );
}

#[test]
fn a_floating_window_takes_its_part_of_the_input_and_a_click_on_it_goes_to_its_terminal() {
    let (ctx, mut world, mut client, mut view) = live();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.term.overlay.covered.is_empty(), "nothing lies over the terminal: {:?}", view.term.overlay);
    client.send(Command::SetChiefDock(atomics_core::config::ChiefDock::Floating));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let overlay = view.term.overlay.clone();
    assert_eq!(view.term.session(), Some("s1"));
    let rect = overlay.rect.expect("the input over the agent's terminal");
    let [window] = overlay.covered[..] else { panic!("the chief's window: {overlay:?}") };
    let (inside, outside) = (egui::pos2(600.0, 500.0), egui::pos2(300.0, 500.0));
    assert!(rect.contains(inside) && rect.contains(outside));
    assert!(window.contains(inside) && !window.contains(outside), "{window:?}");
    assert!(term_pane::clip_path(rect, &overlay.covered, 1.0).starts_with("path("));

    // What the overlay leaves to egui, egui gives the chief's terminal, whose input it then is.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(inside));
    assert_eq!(view.desk.typing_in.as_deref(), Some("chief"));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("chief"));
    assert!(view.term.overlay.covered.is_empty(), "nothing lies over a window on top");
    // Back on the agent's tile, beside the window: the chief's window stays, and takes its part again.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(outside));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("s1"));
    assert_eq!(view.term.overlay.covered.len(), 1);
}

#[test]
fn a_menu_over_a_terminal_acts_and_a_click_away_closes_it_taking_no_lease() {
    let (ctx, mut world, mut client, mut view) = live();
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let dots = on(&shown, "⋯");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(dots));
    let opened = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.term.overlay.covered, [view.term.overlay.rect.unwrap()]);
    assert_eq!(term_pane::clip_path(view.term.overlay.rect.unwrap(), &view.term.overlay.covered, 1.0), "inset(50%)");
    // A click on the terminal closes the menu, and the overlay is whole again.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(800.0, 500.0)));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.term.overlay.covered.is_empty(), "the menu closed: {:?}", view.term.overlay);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(dots));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&opened, "Phone")));
    assert_eq!(view.layout, LayoutMode::Phone, "the menu acted");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { .. })), "no lease was taken");
}

#[test]
fn a_click_into_another_terminal_lets_go_of_the_lease_on_the_last() {
    let (ctx, mut world, mut client, mut view) = live();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(world.host.terminals["s1"].lease.as_ref().is_some_and(|lease| lease.browser), "it typed in the agent");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(200.0, 400.0)));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.term.session(), Some("chief"));
    let released = |world: &World, session: &str| {
        world.sent().iter().any(|m| matches!(m, Message::LeaseRelease { session: s, .. } if s == session))
    };
    assert!(released(&world, "s1"), "the host hears it let go");
    assert!(world.host.terminals["s1"].lease.is_none() && world.host.terminals["s1"].followed);

    // Picking the agent again, by a click on it: the chief's lease goes the same way, and the input is focused.
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(world.host.terminals["chief"].lease.as_ref().is_some_and(|lease| lease.browser));
    client.send(Command::Activate { session: "s1".into(), open_diff: false });
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("s1"));
    assert!(view.term.overlay.focus, "picking an agent types there, as on the desktop");
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(released(&world, "chief"));
    assert!(world.host.terminals["chief"].lease.is_none());
}

#[test]
fn leaving_every_terminal_lets_go_of_the_lease_too() {
    let (ctx, mut world, mut client, mut view) = live();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    // A worktree picked on its own: no session is active, so the input is on no terminal.
    client.send(Command::ShowRoot("/w/other".into()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.term.overlay.rect, None);
    assert!(world.host.terminals["s1"].followed && world.host.terminals["s1"].lease.is_none());
}

#[test]
fn a_chiefs_permission_request_is_answered_in_its_window() {
    let mut host = fake_host::host();
    chief_waits(&mut host);
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let allow = on(&shown, "Allow");
    assert!(has(&shown, "Permission requested") && allow.x < 500.0, "over the docked chief's terminal: {shown:?}");
    // The terminal keeps what the banner leaves it, and the input stays off the banner.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(200.0, 600.0)));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.term.session(), Some("chief"));
    assert!(view.term.overlay.rect.is_some_and(|rect| rect.top() > allow.y), "{:?}", view.term.overlay);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(allow));
    assert!(world.sent().iter().any(|m| matches!(m, Message::ApprovalDecide(d) if d.approval == "a1")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Permission requested"), "answered: {shown:?}");
}

#[test]
fn a_phones_chief_pane_answers_its_permission_request_and_the_bar_says_it_waits() {
    let mut host = fake_host::host();
    chief_waits(&mut host);
    let (ctx, mut world, mut client, mut view) = live_with(host);
    view.tab = Tab::Chief;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "Permission requested"), "{shown:?}");
    assert_eq!(view.term.session(), Some("chief"));
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&shown, "Allow")));
    assert!(world.sent().iter().any(|m| matches!(m, Message::ApprovalDecide(d) if d.approval == "a1")));
}

#[test]
fn a_view_only_browser_sees_a_chiefs_request_and_cannot_answer_it() {
    let mut host = fake_host::host();
    chief_waits(&mut host);
    let mut world = World::paired(host);
    world.host.allowed[0].1 = atomics_core::noise::Access::View;
    world.boot(None);
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Permission requested") && !has(&shown, "Allow"), "{shown:?}");
}

#[test]
fn the_notice_opens_a_chief_that_needs_the_user() {
    let mut host = fake_host::host();
    chief_waits(&mut host);
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::HideChief);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Permission requested"), "the chief's window is minimized: {shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "● 1 needs you")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.desk.chief.open && has(&shown, "Permission requested"), "{shown:?}");
    assert_eq!(view.term.session(), Some("chief"), "the input goes there");
    assert_eq!(view.desk.active.as_deref(), Some("s1"), "and the tab keeps its agent");
}

#[test]
fn a_wide_window_says_when_the_board_may_be_out_of_date_and_why_it_stopped() {
    use atomics_core::tunnel::close;

    use crate::driver::Stop;

    let (ctx, mut world, mut client, mut view) = live();
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Reconnecting"), "{shown:?}");
    world.drop_socket(close::ABNORMAL);
    assert!(matches!(world.driver.status(), Status::Reconnecting { .. }));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Reconnecting. The board may be out of date"), "without hovering: {shown:?}");
    assert!(has(&shown, "Task t1"), "the board stays on view: {shown:?}");

    let (ctx, mut world, mut client, mut view) = live();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    world.drop_socket(close::REVOKED);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::Revoked));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, Stop::Revoked.message()) && has(&shown, "Stopped"), "{shown:?}");
}

/// A secondary click at `at`.
fn right_click(at: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(at), button(true), button(false)]
}

/// `live_with`, with the page's device view-only.
fn viewing(host: fake_host::FakeHost) -> (egui::Context, World, Client, View) {
    let mut world = World::new(host);
    let device = atomics_core::noise::Secret::generate().unwrap();
    world.host.allowed.push((device.public(), atomics_core::noise::Access::View));
    let host_key = world.host.public();
    let id = fake_host::HOST_ID.to_string();
    world.store.insert(id.clone(), crate::device::Device { host_id: id, host_key, secret: device });
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    (egui::Context::default(), world, Client::default(), View::default())
}

/// The middle of the text that is `label`, for a button whose dialog's title starts with it too.
fn exactly(shown: &[(String, Rect)], label: &str) -> egui::Pos2 {
    let found = shown.iter().find(|(text, _)| text == label);
    found.unwrap_or_else(|| panic!("{label} is not drawn: {shown:?}")).1.center()
}

/// A click on the first text starting `label`, and the frames it takes to show.
fn click_on(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
    label: &str,
) -> Vec<(String, Rect)> {
    let shown = settled(ctx, world, client, view, size);
    drawn(ctx, world, client, view, size, click(on(&shown, label)));
    settled(ctx, world, client, view, size)
}

#[test]
fn a_viewer_and_a_page_offline_have_nothing_to_press_and_nothing_to_type_in() {
    let actions = [
        "Approve plan",
        "Dispatch ready",
        "Close plan…",
        "+ New",
        "Send",
        "Steer",
        "start",
        "accept",
        "send back",
        "unblock",
        "integrate",
        "Integrate",
        "×",
    ];
    let (ctx, mut world, mut client, mut view) = viewing(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(has(&shown, "PLAN") && has(&shown, "AGENTS"), "the board shows: {shown:?}");
    for action in actions {
        assert!(!has(&shown, action), "{action} is not a viewer's: {shown:?}");
    }
    // No row menu either, and no way to a close dialog.
    let row = on(&shown, "Fix the parser");
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, right_click(row));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(!has(&shown, "Include in") && !has(&shown, "Close"), "{shown:?}");
    client.confirm_close = Some("s1".into());
    client.confirm_plan_close = Some(("p1".into(), atomics_core::plan::PlanStatus::Completed));
    view.picker = Some(Default::default());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Close session") && !has(&shown, "Archive plan") && !has(&shown, "New session"), "{shown:?}");
    assert!(client.confirm_close.is_none() && client.confirm_plan_close.is_none() && view.picker.is_none());

    let mut host = fake_host::working();
    host.board.diffs[0].comments.clear();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Dispatch ready") && has(&shown, "Send"), "{shown:?}");
    world.drop_socket(atomics_core::tunnel::close::ABNORMAL);
    client.confirm_close = Some("s1".into());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Reconnecting") && has(&shown, "PLAN"), "{shown:?}");
    for action in actions {
        assert!(!has(&shown, action), "{action} cannot be sent now: {shown:?}");
    }
    assert!(client.confirm_close.is_none(), "the dialog goes with what it asks about");
}

#[test]
fn a_steer_from_the_bar_reaches_the_host_and_one_refused_comes_back() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "→ Fix the parser"), "to the active session: {shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Steer · Enter sends")));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("go on".into())]);
    assert_eq!(client.steer_text, "go on");
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter.clone()]);
    assert_eq!(world.host.steered, [("s1".to_string(), "go on".to_string())]);
    assert_eq!(client.steer_text, "", "sent");
    assert!(world.host.terminals["s1"].typed.is_empty() && world.host.terminals["s1"].lease.is_none());

    // Refused: the notice says why, and the text is back in the bar.
    world.host.refuse_next = Some(atomics_core::wire::WireError::new(
        atomics_core::wire::ErrorCode::Rejected,
        "message not delivered: session exited",
    ));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("again".into())]);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.steer_text, "again");
    assert!(has(&shown, "The host refused: message not delivered: session exited"), "whole: {shown:?}");

    // The note lies over the frame, moves nothing, takes no pointer, and goes after a while.
    assert!(view.term.overlay.covered.is_empty(), "{:?}", view.term.overlay);
    let bar = on(&shown, "→ Fix the parser");
    assert_eq!(on(&settled(&ctx, &mut world, &mut client, &mut view, WIDE), "→ Fix the parser"), bar);
    view.now_ms += NOTICE_MS + 1.0;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "The host refused"), "{shown:?}");
    assert_eq!(on(&shown, "→ Fix the parser"), bar);
}

#[test]
fn a_row_can_be_picked_to_steer_together_and_closed() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, right_click(on(&shown, "Fix the parser")));
    let menu = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(has(&menu, "Include in “selected”") && has(&menu, "Close"), "{menu:?}");
    assert!(!has(&menu, "Rename"), "atomics.v1 has no rename");
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(on(&menu, "Include in “selected”")));
    assert!(client.selected.contains("s1"));

    client.steer_target = atomics_ui::chief::SteerTarget::Selected;
    client.steer_text = "both of you".into();
    atomics_ui::chief::send_steer(&world.driver.remote, &mut client);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(world.host.steered, [("s1".to_string(), "both of you".to_string())]);

    // A session that is gone leaves the selection.
    world.send(Command::CloseSession("s1".into()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.selected.is_empty());
}

#[test]
fn closing_an_agent_asks_where_the_desktop_asks() {
    use atomics_core::session::AgentStatus;

    // A running agent's tile asks, and Cancel leaves it running.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "×");
    assert_eq!(client.confirm_close.as_deref(), Some("s1"));
    assert!(has(&shown, "Closing “Fix the parser” stops its process"), "{shown:?}");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::SessionCommand(_))), "not yet");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Cancel");
    assert!(!has(&shown, "Close session") && client.confirm_close.is_none(), "{shown:?}");
    assert!(world.host.board.sessions.iter().any(|s| s.id == "s1"));

    // The dialog's Close session closes it, and the page moves on.
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "×");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(exactly(&shown, "Close session")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!world.host.board.sessions.iter().any(|s| s.id == "s1"));
    assert!(client.confirm_close.is_none() && !has(&shown, "Close session?"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None, "and no refusal of the lease of a terminal now gone");

    // The sidebar's Close asks too, where the window has room for the sidebar.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, right_click(on(&shown, "Fix the parser")));
    let menu = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(exactly(&menu, "Close")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(has(&shown, "Close session"), "{shown:?}");

    // An exited agent in a worktree closes at once, as on the desktop.
    let mut host = fake_host::working();
    host.board.sessions[1].status = AgentStatus::Exited;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Process exited"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "×")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!world.host.board.sessions.iter().any(|s| s.id == "s1"), "no dialog");
    assert!(client.confirm_close.is_none());

    // Exited outside a worktree it may be a scratch folder, deleted with it, which the mirror does not say: it asks.
    let mut host = fake_host::working();
    let session = &mut host.board.sessions[1];
    (session.status, session.worktree, session.task) = (AgentStatus::Exited, None, None);
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::ShowUngrouped);
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "×");
    assert!(world.host.board.sessions.iter().any(|s| s.id == "s1"), "not yet");
    assert!(has(&shown, "Closing “Fix the parser” stops its process, and if it runs in a scratch folder"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(exactly(&shown, "Close session")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!world.host.board.sessions.iter().any(|s| s.id == "s1"), "the dialog's answer does not ask again");
}

#[test]
fn closing_a_plan_offers_what_atomics_v1_has_and_the_page_moves_on() {
    use atomics_core::plan::PlanStatus;

    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Close plan…");
    assert!(has(&shown, "Close “Parse faster”? You can view it from History"), "{shown:?}");
    assert!(has(&shown, "Archive plan") && !has(&shown, "Complete plan"), "atomics.v1 has no complete: {shown:?}");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Cancel");
    assert!(!has(&shown, "Archive plan"), "{shown:?}");
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Open);

    // Its tab's close asks the same.
    client.send(Command::ClosePlanTab(0));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Archive plan"), "{shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Archive plan");
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Archived);
    assert_eq!(view.desk.plan, None, "no open plan is left: every agent shows");
    assert_eq!(world.driver.remote.notice(), None);

    // Refused, it says why, and the plan stays where it was.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Close plan…");
    world.host.refuse_next = Some(atomics_core::wire::WireError::new(
        atomics_core::wire::ErrorCode::Rejected,
        "Closing plan failed: disk full",
    ));
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Archive plan");
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    assert!(has(&shown, "The host refused: Closing plan failed") && has(&shown, "PLAN"), "{shown:?}");
}

#[test]
fn a_closed_plans_history_stays_read_only() {
    use atomics_core::plan::PlanStatus;

    let mut host = fake_host::working();
    host.board.plans[0].status = PlanStatus::Completed;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::ViewPlanHistory(0));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Completed ✓ · read-only"), "{shown:?}");
    assert!(has(&shown, "Archive"), "a finished plan's own Archive: {shown:?}");
    for action in
        ["Dispatch ready", "Close plan…", "start", "accept", "send back", "unblock", "integrate", "Integrate", "Reopen"]
    {
        assert!(!has(&shown, action), "{action} on a read-only board: {shown:?}");
    }
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Archive");
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Archived);

    let mut host = fake_host::working();
    host.board.plans[0].status = PlanStatus::Archived;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::ViewPlanHistory(0));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Archived · read-only") && !has(&shown, "Archive plan"), "{shown:?}");
    assert!(!has(&shown, "Reopen plan") && !has(&shown, "Clean up plan"), "{shown:?}");
}

#[test]
fn a_boards_buttons_change_the_plan_and_a_merge_shows_while_it_runs() {
    use atomics_core::plan::TaskStatus;

    let task = |world: &World, id: &str| world.host.board.plans[0].tasks.iter().find(|t| t.id == id).cloned().unwrap();
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    // The right panel dragged wide enough for every column.
    client.split = Some(1800.0);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    for button in ["Dispatch ready (1)", "start", "accept", "send back", "unblock", "integrate", "Integrate"] {
        assert!(has(&shown, button), "{button} is missing: {:?}", shown.iter().map(|s| &s.0).collect::<Vec<_>>());
    }
    assert!(has(&shown, "1 accepted tasks not integrated yet"), "{shown:?}");

    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "start");
    assert_eq!(task(&world, "t3").status, TaskStatus::Running);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(exactly(&shown, "integrate")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let integrated = |world: &World, id: &str| task(world, id).integrated.is_some();
    assert!(integrated(&world, "t4") ^ integrated(&world, "t5"), "the first card's: {shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "accept");
    assert_eq!(task(&world, "t4").status, TaskStatus::Done);
    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "unblock");
    assert_eq!(task(&world, "t6").status, TaskStatus::Running);
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Integrate");
    assert!(task(&world, "t4").integrated.is_some() && task(&world, "t5").integrated.is_some());
    assert!(!has(&shown, "accepted tasks not integrated"), "nothing stale is left: {shown:?}");
    assert_eq!(world.driver.remote.notice(), None);

    // Refused: the notice says why, and nothing moved.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    world.host.refuse_next =
        Some(atomics_core::wire::WireError::new(atomics_core::wire::ErrorCode::Rejected, "no task is ready"));
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Dispatch ready");
    assert_eq!(task(&world, "t3").status, TaskStatus::Planned);
    assert!(has(&shown, "The host refused: no task is ready") && has(&shown, "Dispatch ready (1)"), "{shown:?}");

    // Dispatch itself.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Dispatch ready");
    assert_eq!(task(&world, "t3").status, TaskStatus::Running);
    assert!(!has(&shown, "Dispatch ready (1)"), "{shown:?}");

    // A merge in flight: its card says so until the outcome.
    world.host.hold_results = true;
    client.split = Some(1800.0);
    client.send(Command::IntegrateTask { plan: 0, task: "t4".into() });
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(has(&shown, "integrating…"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None);
}

/// The fake host's working board with its diff in the right panel.
fn on_the_diff(size: (f32, f32)) -> (egui::Context, World, Client, View, Vec<(String, Rect)>) {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, size);
    client.send(Command::SetPanel(PanelMode::Diff));
    let mut shown = settled(&ctx, &mut world, &mut client, &mut view, size);
    for _ in 0..4 {
        shown = settled(&ctx, &mut world, &mut client, &mut view, size);
    }
    (ctx, world, client, view, shown)
}

#[test]
fn a_comment_and_a_review_reach_the_host_from_the_diff() {
    let (ctx, mut world, mut client, mut view, shown) = on_the_diff(WIDE);
    assert!(has(&shown, "Send review (0)"), "{shown:?}");
    // A click on a line opens the comment box, which takes the keyboard.
    let line = shown.iter().find(|(text, _)| text.contains("step(1)")).expect("the diff's lines").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(line));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Comment for the agent"), "{shown:?}");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("Why this step?".into())]);
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    assert_eq!(world.host.board.diffs[0].comments.len(), 1, "the host has it");
    assert_eq!(world.host.board.diffs[0].comments[0].body, "Why this step?");
    assert_eq!(world.host.board.diffs[0].comments[0].path, "src/a.rs");
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Send review (1)"), "the page shows the host's: {shown:?}");
    assert!(world.host.terminals["s1"].typed.is_empty(), "the terminal got none of it");

    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Send review (1)");
    assert_eq!(world.host.reviews.len(), 1);
    assert_eq!(world.host.reviews[0].1[0].body, "Why this step?");
    assert!(has(&shown, "Send review (0)"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None);

    // Discarded comments leave the host too.
    client.send(Command::EditComments {
        root: WORKTREE.into(),
        before: Vec::new(),
        after: vec![atomics_core::steer::ReviewComment {
            path: "src/a.rs".into(),
            line: Some(2),
            removed: false,
            excerpt: String::new(),
            body: "second".into(),
        }],
    });
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "discard");
    assert!(world.host.board.diffs[0].comments.is_empty(), "{shown:?}");
}

#[test]
fn a_refused_comment_says_why_and_leaves_no_comment_on_screen() {
    let (ctx, mut world, mut client, mut view, shown) = on_the_diff(WIDE);
    let line = shown.iter().find(|(text, _)| text.contains("step(1)")).expect("the diff's lines").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(line));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("Why?".into())]);
    world.host.refuse_next = Some(atomics_core::wire::WireError::new(
        atomics_core::wire::ErrorCode::StaleRevision,
        "the diff is at revision 5; anchor the comment again",
    ));
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "The host refused: the diff is at revision 5"), "{shown:?}");
    assert!(has(&shown, "Send review (0)") && !has(&shown, "Why?"), "{shown:?}");
}

#[test]
fn the_changed_files_column_scrolls_the_diff_to_its_file() {
    // Room for the column: the chief hidden, so the panel is 760 points wide or more.
    let size = (2400.0, 1000.0);
    let (ctx, mut world, mut client, mut view, shown) = on_the_diff(size);
    assert!(has(&shown, "CHANGED"), "{shown:?}");
    let file = shown.iter().find(|(text, _)| text == "b.rs").expect("the column names it").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, size, click(file));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
    let state = &client.diff_views[std::path::Path::new(WORKTREE)];
    assert_eq!(state.focused.as_deref(), Some("src/b.rs"));
    assert_eq!(state.viewing, None, "its hunks, not a file the mirror does not have");
    assert!(has(&shown, "CHANGED"), "{shown:?}");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::SessionCommand(_) | Message::PlanCommand(_))));
}

#[test]
fn the_picker_starts_an_agent_on_the_ungrouped_tab() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "+ New");
    for field in ["New session", "Agent", "claude", "Where", "Scratch folder", "Start", "Cancel"] {
        assert!(has(&shown, field), "{field} is missing: {shown:?}");
    }
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Cancel");
    assert!(!has(&shown, "New session"), "{shown:?}");
    assert_eq!(world.host.board.sessions.len(), 2);

    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "+ New");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Start");
    let started = world.host.board.sessions.iter().find(|s| s.id.starts_with("new-")).expect("a new session");
    assert_eq!((started.agent.as_str(), started.cwd.as_str()), ("claude", "/scratch/new"), "a scratch folder");
    assert!(view.desk.ungrouped && !view.desk.plan_view, "it lands on the Ungrouped tab");
    assert!(has(&shown, "new session"), "{shown:?}");
    assert_eq!(view.desk.active.as_deref(), Some(started.id.as_str()));

    // The host no longer has that agent: it says so, and nothing started.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "+ New");
    world.host.refuse_next = Some(atomics_core::wire::WireError::new(
        atomics_core::wire::ErrorCode::NotFound,
        "no agent claude is configured",
    ));
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Start");
    assert!(has(&shown, "The host refused: no agent claude is configured"), "{shown:?}");
    assert_eq!(world.host.board.sessions.len(), 2);
}

#[test]
fn a_hosts_empty_frame_offers_what_the_browser_does() {
    // No chief and none to start; no agent the mirror can name, so nothing to start: a welcome that promises
    // only what the browser does.
    let mut host = fake_host::host();
    host.board.sessions.clear();
    host.board.approvals.clear();
    host.board.plans.clear();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "CHIEF") && !has(&shown, "start"), "{shown:?}");
    assert!(!has(&shown, "+ New") && !has(&shown, "Start "), "the mirror names no agent: {shown:?}");
    assert!(has(&shown, "AGENTS") && !has(&shown, "EXPLORER"), "no projects, no way to open one: {shown:?}");
    let text: String = shown.iter().map(|(text, _)| text.as_str()).collect();
    assert!(!text.contains("scratch folder"), "nothing to start, nowhere to say it starts: {text}");
    assert!(text.contains("No agents yet.") && !text.contains("+ New"), "{text}");
    assert!(text.contains("worktree's diff against main") && text.contains("Comment on any line"), "{text}");

    // A plan's agent is one the mirror can name; the empty tab offers to start it, in a scratch folder.
    let mut host = fake_host::host();
    host.board.sessions.clear();
    host.board.approvals.clear();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::ShowUngrouped);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Start claude"), "{shown:?}");
    let text: String = shown.iter().map(|(text, _)| text.as_str()).collect();
    assert!(text.contains("scratch folder of its own"), "{text}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Start claude")));
    assert!(world.host.board.sessions.iter().any(|s| s.id.starts_with("new-")));
}

/// The boxes the last frame's shapes draw: the buttons and cards, with what lies behind them.
fn boxes(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    (width, height): (f32, f32),
) -> Vec<Rect> {
    view.now_ms += 16.0;
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
        time: Some(view.now_ms / 1000.0),
        ..egui::RawInput::default()
    };
    let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, client, view));
    output.textures_delta.clear();
    world.pump();
    output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Rect(rect) => Some(rect.rect),
            _ => None,
        })
        .collect()
}

#[test]
fn a_phones_board_keeps_its_buttons_and_cards_on_the_narrowest_screen() {
    // 360 and 390 CSS pixels, in the points the phone's zoom draws them in.
    for css in [360.0, 390.0] {
        let size = (layout::px(css), layout::px(740.0));
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(view.compact);
        let inside = size.0 - layout::GUTTER;
        for (text, rect) in shown.iter().filter(|(text, _)| !text.trim().is_empty()) {
            assert!(rect.right() <= size.0 + 0.5, "{text} runs off {css} CSS pixels: {rect:?}");
        }
        for button in ["Dispatch ready (1)", "Close plan…", "Integrate"] {
            let at = on(&shown, button);
            assert!(at.x > layout::GUTTER && at.x < inside, "{button} at {at:?}");
        }
        assert!(!has(&shown, "Steer") && !has(&shown, "+ New"), "a phone has neither: {shown:?}");
        // Between the bars, every box keeps to the gutter: a row that overflows widens what follows it.
        let (top, bottom) = (layout::TOP_BAR, size.1 - layout::BOTTOM_BAR);
        let drawn = boxes(&ctx, &mut world, &mut client, &mut view, size);
        let controls: Vec<&Rect> =
            drawn.iter().filter(|r| r.left() >= layout::GUTTER - 1.0 && r.top() > top && r.top() < bottom).collect();
        assert!(controls.len() >= 6, "the board's buttons and cards: {controls:?}");
        for control in controls {
            assert!(control.right() <= inside + 1.0, "off the gutter at {css} CSS pixels: {control:?}");
        }
    }
}

#[test]
fn a_phones_board_acts_and_its_dialogs_ask() {
    use atomics_core::plan::{PlanStatus, TaskStatus};

    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "Dispatch ready (1)"), "{shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Dispatch ready");
    assert_eq!(world.host.board.plans[0].tasks[2].status, TaskStatus::Running);

    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Close plan…");
    assert!(has(&shown, "Archive plan") && !has(&shown, "Complete plan"), "{shown:?}");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Archive plan");
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Archived);

    // It was the only plan: History, which the empty board names, reaches it, read-only.
    assert!(has(&shown, "No plan is open") && has(&shown, "History"), "{shown:?}");
    let opened = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "History");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, &opened[opened.len() - 1].0.clone());
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    assert!(has(&shown, "Archived · read-only") && !has(&shown, "Dispatch ready"), "{shown:?}");

    // A tile's close, on the phone's Terminal pane, asks over the terminal.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    view.tab = Tab::Terminal;
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "×");
    assert!(has(&shown, "Close session"), "{shown:?}");
    assert!(!view.term.overlay.covered.is_empty(), "the dialog takes its part of the input");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(exactly(&shown, "Close session")));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(!world.host.board.sessions.iter().any(|s| s.id == "s1"));
    assert!(view.picker.is_none());

    // Its AGENTS list offers no steering: there is no bar to steer from.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    view.tab = Tab::Agents;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, right_click(on(&shown, "Fix the parser")));
    let menu = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&menu, "Close") && !has(&menu, "Include in"), "{menu:?}");
}

#[test]
fn a_terminal_taking_the_keyboard_takes_it_from_a_text_box() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Steer · Enter sends")));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(ctx.memory(|m| m.focused().is_some()), "the bar has the keyboard");
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(ctx.memory(|m| m.focused().is_none()), "and gave it up");
    // What the page forwards goes to the terminal; the bar sees none of it.
    view.term.inputs.push(term_pane::TermInput::Text("ls".into()));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(world.host.terminals["s1"].typed, "ls");
    assert_eq!(client.steer_text, "");
}
