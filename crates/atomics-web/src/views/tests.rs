//! The views drawn headless over a live fake host: the desktop's frame in a wide window, every pane on a
//! phone, and what the user asks for reaches the right place.

use atomics_core::approvals::Decision;
use atomics_core::ops::HostAction;
use atomics_core::projection::{DiffFileSummary, HostInfo, SignInState};
use atomics_core::theme::ThemeMode;
use atomics_core::wire::{ErrorCode, Message, WireError};
use serde_json::json;

use super::*;
use crate::fake_host::{self, WORKTREE};
use crate::layout::Viewport;
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

/// The middle of the top bar's `label`, such as the Boards dropdown's word or the name of the board on screen.
fn on_bar(shown: &[(String, Rect)], label: &str) -> egui::Pos2 {
    let found = shown.iter().find(|(text, rect)| text == label && rect.bottom() <= 34.0);
    found.unwrap_or_else(|| panic!("{label} is not in the top bar: {shown:?}")).1.center()
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

#[test]
fn explorer_opens_worktree_folders_changed_files_and_whole_files_in_both_layouts() {
    for size in [BOARD, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::explorer());
        view.tab = Tab::Agents;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        for label in ["EXPLORER", "All ·", "Repos", "Steer", "Send", "+ New"] {
            assert!(has(&shown, label), "{label} missing at {size:?}: {shown:?}");
        }
        assert!(has(&shown, "README.md"));
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "README.md");
        assert!(has(&shown, "A project readme"), "unchanged files open whole: {shown:?}");
        assert_eq!(view.desk.diff_target(&world.driver.remote), Some(PathBuf::from(WORKTREE)));
        if view.compact {
            assert_eq!(view.tab, Tab::Diffs);
            view.tab = Tab::Agents;
        }
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "▸ src/");
        assert!(has(&shown, "a.rs") && has(&shown, "unchanged.rs"), "{shown:?}");
        click_on(&ctx, &mut world, &mut client, &mut view, size, "a.rs");
        let diff = client.diff_views.get(Path::new(WORKTREE)).unwrap();
        assert!(diff.viewing.is_none(), "changed files open at their hunks");
        assert_eq!(view.desk.panel, PanelMode::Diff);
        assert!(world.sent().iter().any(
            |m| matches!(m, Message::DiffGet { worktree, path, .. } if worktree == WORKTREE && path == "src/a.rs")
        ));
        if view.compact {
            view.tab = Tab::Agents;
        }
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "↑ Parent folder");
        assert!(has(&shown, "README.md"), "parent folder: {shown:?}");
        click_on(&ctx, &mut world, &mut client, &mut view, size, "▸ worktrees");
        click_on(&ctx, &mut world, &mut client, &mut view, size, "main checkout");
        assert_eq!(view.desk.diff_target(&world.driver.remote), Some(PathBuf::from("/w/repo")));
        if view.compact {
            assert_eq!(view.tab, Tab::Agents, "picking a worktree keeps its files in reach");
        }
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "README.md"));
    }
}

#[test]
fn phone_filters_steering_start_and_completion_work() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::explorer());
    view.tab = Tab::Agents;
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "All ·");
    assert!(!view.desk.plan_view && view.desk.project.is_none());
    let menu = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Repos");
    let other = menu.iter().find(|(text, _)| text.starts_with("other · 0 sessions")).unwrap().1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(other));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.desk.project, Some(PathBuf::from("/w/other")));
    assert!(world.driver.remote.visible_sessions().is_empty());
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "All ·");
    assert_eq!(world.driver.remote.visible_sessions().len(), 1);
    client.steer_text = "Continue the review".into();
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Send");
    assert_eq!(world.host.steered.last(), Some(&("s1".into(), "Continue the review".into())));
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "+ New");
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "New session"), "{shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Start");
    assert_eq!(world.host.board.sessions.len(), 3);
    view.tab = Tab::Board;
    client.send(Command::ShowPlan(0));
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Close plan…");
    click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Complete plan");
    assert_eq!(world.host.board.plans[0].status, PlanStatus::Completed);
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
fn the_chiefs_pill_keeps_the_middle_of_the_phones_top_bar_and_opens_the_chiefs_pane() {
    let (ctx, mut world, mut client, mut view) = live();
    view.tab = Tab::Diffs;
    for size in [PHONE, (360.0, 780.0), (430.0, 932.0)] {
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let width = ctx.content_rect().width();
        let name = shown.iter().find(|(text, _)| text == "CHIEF").expect("the pill is named on a phone").1;
        // The name follows the pill's mark, in the middle of the bar.
        assert!(name.left() > width * 0.4 && name.right() < width * 0.6, "{size:?}: {name:?} in {width}");
        assert!(name.bottom() <= layout::TOP_BAR + 1.0, "{size:?}: in the top bar");
        assert!(!has(&shown, "Chief"), "{size:?}: the pane bar has no Chief of its own");
    }
    let pill = on(&settled(&ctx, &mut world, &mut client, &mut view, PHONE), "CHIEF");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(pill));
    assert_eq!(view.tab, Tab::Chief, "a tap opens the chief's pane");
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(pill));
    assert_eq!(view.tab, Tab::Diffs, "another goes back to the pane it left");
}

#[test]
fn the_location_line_says_where_the_window_is_and_back_and_forward_walk_the_places() {
    use atomics_ui::nav;
    let (ctx, mut world, mut client, mut view) = live();
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    // Plan › task › session, under the top bar.
    let parts: Vec<_> = ["Parse faster", "Task t1", "Fix the parser · needs you"]
        .map(|part| {
            let found = shown.iter().find(|(text, rect)| text == part && rect.top() > 34.0);
            found.unwrap_or_else(|| panic!("{part}: {shown:?}")).1
        })
        .into();
    assert!(parts.windows(2).all(|w| w[0].right() < w[1].left()), "in order: {parts:?}");
    assert!(parts.iter().all(|r| r.top() > 34.0 && r.bottom() < 34.0 + nav::HEIGHT + 2.0), "{parts:?}");
    assert!(!client.history.can_go(-1) && !client.history.can_go(1));

    // Another tab is a step; back returns to the plan's, forward goes to it again.
    // A place is a step once it has shown a while.
    view.now_ms += 1000.0;
    client.send(Command::ShowUngrouped);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    view.now_ms += 1000.0;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.desk.ungrouped && client.history.can_go(-1));
    nav::back(&world.driver.remote, &mut client);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.desk.plan_view && !view.desk.ungrouped, "back on the plan's tab");
    assert_eq!(view.desk.active.as_deref(), Some("s1"));
    assert!(client.history.can_go(1) && !client.history.can_go(-1), "nothing before it, the Ungrouped tab after");
    nav::forward(&world.driver.remote, &mut client);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.desk.ungrouped, "forward again");

    // Its back arrow does the same as the key.
    let arrow = back_arrow(&ctx, &mut world, &mut client, &mut view).expect("the location line");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(arrow));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.desk.plan_view, "the ‹ arrow goes back");
}

/// Where the location line's back arrow is: left of the forward arrow, left of its first part.
fn back_arrow(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View) -> Option<egui::Pos2> {
    let shown = settled(ctx, world, client, view, WIDE);
    let first = shown.iter().find(|(text, _)| text == "Ungrouped" || text == "Parse faster")?.1;
    Some(egui::pos2(first.left() - 8.0 - 24.0 - 11.0, first.center().y))
}

#[test]
fn a_task_cards_click_shows_its_session_and_diff_and_marks_the_card() {
    let (ctx, mut world, mut client, mut view) = live();
    view.desk.active = None;
    view.desk.diff_root = Some("/w/other".into());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    // The card's agent line, which is no button: the card takes the click.
    let at = on(&shown, "t1 · claude");
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(at));
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let hints = view.desk.hints(&world.driver.remote);
    assert_eq!(hints.active.as_deref(), Some("s1"), "{hints:?}");
    assert!(hints.diff_open && hints.panel == PanelMode::Diff, "{hints:?}");
    // A planned task has no session: its card's click does nothing.
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    client.send(Command::ToggleView(PanelMode::Plan));
    let shown =
        if has(&shown, "t2 · claude") { shown } else { settled(&ctx, &mut world, &mut client, &mut view, BOARD) };
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(on(&shown, "t2 · claude")));
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert_eq!(view.desk.hints(&world.driver.remote).panel, PanelMode::Plan);
}

#[test]
fn the_diff_with_nothing_to_show_says_how_to_pick_something() {
    let (ctx, mut world, mut client, mut view) = live();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    (view.desk.active, view.desk.diff_root) = (None, None);
    (view.desk.panel, view.desk.diff_open) = (PanelMode::Diff, true);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "No diff to show") && has(&shown, "Pick an agent in the sidebar"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Show the board")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.panel, PanelMode::Plan, "its button shows the board");
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
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
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
fn a_boards_ticket_keys_show_in_either_layout_and_offer_nothing() {
    for size in [BOARD, PHONE] {
        let (ctx, mut world, mut client, mut view) = live();
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let shows = |label: &str| shown.iter().any(|(text, _)| text == label);
        assert!(shows("ticket") && shows("ENG-40") && shows("ENG-41"), "{size:?}: {shown:?}");
        assert!(!world.driver.remote.available(&Command::OpenTicket(Default::default())), "{size:?}");

        // A click on a key is a click on nothing: the view stays, and the host hears nothing of it.
        world.host.received.clear();
        for key in ["ENG-40", "ENG-41"] {
            let before = view.desk.hints(&world.driver.remote);
            drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, key)));
            settled(&ctx, &mut world, &mut client, &mut view, size);
            assert_eq!(view.desk.hints(&world.driver.remote), before, "{size:?}: {key}");
            assert_eq!(world.driver.remote.notice(), None, "{size:?}: {key}");
        }
        assert!(world.sent().is_empty(), "{size:?}: {:?}", world.sent());
    }
}

#[test]
fn a_wide_window_draws_the_desktops_frame_without_the_hosts_own_items() {
    let mut host = fake_host::management();
    host.trace = fake_host::trace_data();
    let (ctx, mut world, mut client, mut view) = live_with(host);
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
        "Parse faster",
        "CHIEF",
        "Diff",
        "Board 0/2",
        "Connected",
        "◆ CHIEF AGENT",
        "TRACE",
        "Float",
        "Minimize",
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
        "Settings",
        "Remote",
        "Project",
    ] {
        assert!(has(shown), "{shown} is missing: {text:?}");
    }
    // At this width the sidebar yields to the chief and diff; the wider parity test covers USAGE.
    assert!(has("load 4.2"), "{text:?}");
    assert!(!has("Workspace") && !has("No workspace"), "the host has no workspace to pick: {text:?}");
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
fn a_native_window_fits_the_docked_chief_and_every_tile_where_the_host_has_no_desktop() {
    let mut host = fake_host::host();
    host.headless = true;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    view.fit_terminals = true;
    let size = |world: &World, session: &str| {
        let screen = &world.host.terminals[session].screen;
        (screen.cols, screen.rows)
    };
    let fits = |world: &World, session: &str| {
        world.sent().iter().filter(|m| matches!(m, Message::TerminalFit { session: s, .. } if s == session)).count()
    };
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    // The docked chief and the agent's tile have their panes' sizes, and nobody pressed anything or took a lease.
    let (chief, agent) = (size(&world, "chief"), size(&world, "s1"));
    assert!(chief != (80, 12) && agent != (80, 12), "{chief:?} {agent:?}");
    assert!(world.host.terminals.values().all(|t| t.lease.is_none()));
    assert!(!has(&shown, "Fit to this screen"), "{shown:?}");

    // Picking the chief's terminal moves the input there; the room for the strip saying who types was kept for it.
    let sent = fits(&world, "chief");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(200.0, 400.0)));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.term.session(), Some("chief"));
    assert_eq!((size(&world, "chief"), fits(&world, "chief")), (chief, sent), "picking it never resizes it");

    // The window gets shorter: the dock and the tile follow it.
    settled(&ctx, &mut world, &mut client, &mut view, (WIDE.0, WIDE.1 - 300.0));
    assert!(size(&world, "chief").1 < chief.1 && size(&world, "s1").1 < agent.1);

    // Where the host has a desktop, its panes size the terminals: the window shows them as they are.
    let (ctx, mut world, mut client, mut view) = live();
    view.fit_terminals = true;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!world.driver.remote.fits());
    assert_eq!((size(&world, "chief"), size(&world, "s1")), ((80, 12), (80, 12)));
    assert!(world.sent().iter().all(|m| !matches!(m, Message::TerminalFit { .. })));
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
    assert!(!text.iter().any(|t| t.starts_with("Board ")), "no board's segment without a plan: {text:?}");
    assert!(
        text.iter().any(|t| t == "Boards"),
        "the dropdown stays, in its place, and says how a board comes: {text:?}"
    );
    assert_eq!(view.term.session(), Some("s1"));
}

#[test]
fn a_wide_window_shows_a_diff_and_fetches_only_it() {
    let (ctx, mut world, mut client, mut view) = live();
    for _ in 0..2 {
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    assert!(!world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })), "the board shows, no diff");
    client.send(Command::ToggleView(PanelMode::Diff));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })), "the active session's diff");
    let text = sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(text.iter().any(|t| t.contains("src/a.rs")), "{text:?}");
}

#[test]
fn the_shell_follows_the_screen() {
    let (ctx, mut world, mut client, mut view) = live();
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(view.compact, "a phone's width");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(!view.compact, "a computer's");
    // A finger on a phone held sideways, and on a tablet upright.
    view.viewport = Viewport { coarse: true, screen_short: 402.0, ..Viewport::default() };
    sized(&ctx, &mut world, &mut client, &mut view, (874.0, 402.0), Vec::new());
    assert!(view.compact);
    view.viewport.screen_short = 820.0;
    sized(&ctx, &mut world, &mut client, &mut view, (820.0, 1180.0), Vec::new());
    assert!(!view.compact);
}

#[test]
fn a_page_of_many_plans_opens_on_the_open_plan_with_its_board() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::many_plans());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let hints = view.desk.hints(&world.driver.remote);
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    assert!(hints.plan_view && hints.panel == PanelMode::Plan && hints.selected_plan == 23, "{hints:?}");
    for wanted in ["Parse faster", "Approve plan", "Fix the parser", "Board 0/2"] {
        assert!(has(&shown, wanted), "{wanted} is missing: {shown:?}");
    }
    assert!(!has(&shown, "Archived"), "no archived plan shows unless it is picked: {shown:?}");
    assert_eq!(view.term.session(), Some("s1"), "the open plan's agent takes the input");

    // Only History, in the Boards dropdown, reaches an archived plan, and the button names it while it shows.
    let menu = drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on_bar(&shown, "Parse faster")));
    let menu = if has(&menu, "History") { menu } else { settled(&ctx, &mut world, &mut client, &mut view, WIDE) };
    let opened = drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&menu, "History")));
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
    assert!(has(&shown, "Fix the parser") && has(&shown, "Boards"), "{shown:?}");
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
    let boards = on_bar(&shown, "Parse faster");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(boards));
    let opened = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.term.overlay.covered, [view.term.overlay.rect.unwrap()]);
    assert_eq!(term_pane::clip_path(view.term.overlay.rect.unwrap(), &view.term.overlay.covered, 1.0), "inset(50%)");
    // A click on the terminal closes the menu, and the overlay is whole again.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(egui::pos2(800.0, 500.0)));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.term.overlay.covered.is_empty(), "the menu closed: {:?}", view.term.overlay);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(boards));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let row = opened.iter().find(|(text, rect)| text == "Parse faster" && rect.top() > 34.0).expect("the board's row");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, click(row.1.center()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(view.term.overlay.covered.is_empty(), "the menu acted, and closed: {:?}", view.term.overlay);
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
fn a_phones_chief_opens_on_its_terminal_with_no_header_and_the_traces_calls_folded() {
    let mut host = fake_host::host();
    host.trace = fake_host::trace_data();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let pill = on(&settled(&ctx, &mut world, &mut client, &mut view, PHONE), "CHIEF");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(pill));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.tab, Tab::Chief);
    for absent in ["◆ CHIEF AGENT", "Minimize", "Float"] {
        assert!(!has(&shown, absent), "{absent}: the chief's button said whose pane this is: {shown:?}");
    }
    assert!(client.lane.collapsed && has(&shown, "TRACE") && has(&shown, "Show"), "folded, with Show: {shown:?}");
    // The terminal right under the top bar and the strip over it.
    let cells = view.term.overlay.rect.expect("the chief's terminal");
    assert!(cells.top() <= layout::TOP_BAR + layout::TOUCH + 8.0 + 1.0, "{cells:?}");
    // Shown, the trace stays so when the user comes back to the chief.
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Show");
    assert!(!client.lane.collapsed && has(&shown, "Hide"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(pill));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_ne!(view.tab, Tab::Chief);
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(pill));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.tab == Tab::Chief && !client.lane.collapsed);
}

#[test]
fn a_phones_board_scrolls_as_one_surface_its_header_with_its_cards() {
    // More tasks than an iPhone SE's screen holds.
    let mut host = fake_host::working();
    let more: Vec<_> = (7..=12)
        .map(|n| {
            let mut task = host.board.plans[0].tasks[2].clone();
            (task.id, task.title) = (format!("t{n}"), format!("Task t{n}"));
            task
        })
        .collect();
    host.board.plans[0].tasks.extend(more);
    let (ctx, mut world, mut client, mut view) = live_with(host);
    view.tab = Tab::Board;
    // An iPhone SE's screen, in the points this headless frame is given.
    const SE: (f32, f32) = (375.0, 667.0);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, SE);
    let plan = on(&shown, "PLAN");
    let cards = shown.iter().filter(|(text, _)| text.starts_with("Task ")).count();
    assert!(cards < 12, "more cards than the screen holds: {cards} of 12 drawn");
    let wheel = |at: egui::Pos2| {
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                delta: vec2(0.0, -200.0),
                modifiers: egui::Modifiers::default(),
            },
        ]
    };
    // Scrolled from over the plan's header, the whole board goes up: the header and the cards together.
    drawn(&ctx, &mut world, &mut client, &mut view, SE, wheel(plan));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, SE);
    let moved = plan.y - shown.iter().find(|(text, _)| text == "PLAN").map_or(f32::NEG_INFINITY, |(_, r)| r.center().y);
    let task = |shown: &[(String, Rect)]| shown.iter().find(|(text, _)| text == "Task t3").map(|(_, r)| r.center().y);
    assert!(moved > 100.0, "the header scrolled up by {moved}: {shown:?}");
    let before = task(&settled(&ctx, &mut world, &mut client, &mut view, SE)).expect("a card");
    drawn(&ctx, &mut world, &mut client, &mut view, SE, wheel(pos2(SE.0 / 2.0, before)));
    let after = task(&settled(&ctx, &mut world, &mut client, &mut view, SE));
    assert!(after.is_none_or(|after| after < before - 100.0), "from over a card too: {before} to {after:?}");

    // A finger's drag, as the page hands it over: pressed on a card, moved up, lifted. Back to the top first.
    for _ in 0..6 {
        let shown = settled(&ctx, &mut world, &mut client, &mut view, SE);
        if has(&shown, "PLAN") {
            break;
        }
        let mut up = wheel(pos2(SE.0 / 2.0, SE.1 / 2.0));
        if let egui::Event::MouseWheel { delta, .. } = &mut up[1] {
            *delta = vec2(0.0, 400.0);
        }
        drawn(&ctx, &mut world, &mut client, &mut view, SE, up);
    }
    let start = task(&settled(&ctx, &mut world, &mut client, &mut view, SE)).expect("a card at the top again");
    // eframe hands a finger over as the pointer and as a touch, which is what lets a scroll area be dragged.
    let finger = |pos, phase| {
        let pointer = match phase {
            egui::TouchPhase::Start => egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::TouchPhase::End => egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
            _ => egui::Event::PointerMoved(pos),
        };
        let touch =
            egui::Event::Touch { device_id: egui::TouchDeviceId(1), id: egui::TouchId(1), phase, pos, force: None };
        vec![egui::Event::PointerMoved(pos), pointer, touch]
    };
    // egui lets a finger drag a list once it has seen a touch: the page's first touch only taps, as on the Board pane.
    let board = egui::pos2(SE.0 * 3.0 / 8.0, SE.1 - layout::BOTTOM_BAR / 2.0);
    drawn(&ctx, &mut world, &mut client, &mut view, SE, finger(board, egui::TouchPhase::Start));
    drawn(&ctx, &mut world, &mut client, &mut view, SE, finger(board, egui::TouchPhase::End));
    settled(&ctx, &mut world, &mut client, &mut view, SE);
    assert_eq!(view.tab, Tab::Board);
    let from = pos2(SE.0 / 2.0, start);
    let to = from - vec2(0.0, 120.0);
    drawn(&ctx, &mut world, &mut client, &mut view, SE, finger(from, egui::TouchPhase::Start));
    for step in 1..=6 {
        let at = from + (to - from) * (step as f32 / 6.0);
        drawn(&ctx, &mut world, &mut client, &mut view, SE, finger(at, egui::TouchPhase::Move));
    }
    drawn(&ctx, &mut world, &mut client, &mut view, SE, finger(to, egui::TouchPhase::End));
    let dragged = task(&settled(&ctx, &mut world, &mut client, &mut view, SE));
    assert!(dragged.is_none_or(|y| y < start - 80.0), "a drag scrolls the board: {start} to {dragged:?}");
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

/// A native client's word on why its dial failed shows while it waits for the host: on the status screen
/// before there is a board, under the reconnecting note over one, and never once the host is back.
#[test]
fn why_a_native_client_cannot_reach_its_host_shows_while_it_waits() {
    use atomics_core::tunnel::close;

    let why = "cannot connect to 127.0.0.1:47800: Connection refused (os error 61)";
    let mut world = World::new(fake_host::host());
    world.driver = crate::driver::Driver::direct("op");
    world.pump();
    assert!(matches!(world.driver.status(), Status::Reconnecting { .. }));
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    view.unreachable = Some(why.into());
    for size in [WIDE, PHONE] {
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Connecting to your computer") && has(&shown, why), "{size:?}: {shown:?}");
    }

    let (ctx, mut world, mut client, mut view) = live();
    view.unreachable = Some(why.into());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, why), "connected: {shown:?}");
    world.drop_socket(close::ABNORMAL);
    for size in [WIDE, PHONE] {
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Reconnecting. The board may be out of date") && has(&shown, why), "{size:?}: {shown:?}");
    }

    let (ctx, mut world, mut client, mut view) = live();
    view.unreachable = Some(why.into());
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    world.drop_socket(close::REVOKED);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, why), "stopped, which says why itself: {shown:?}");
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

/// Opens the phone's ⋯ menu, at the top bar's end, whose dots are painted; the text the menu shows.
fn open_more(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View) -> Vec<(String, Rect)> {
    settled(ctx, world, client, view, PHONE);
    let at = egui::pos2(ctx.content_rect().right() - layout::GUTTER - layout::TOUCH / 2.0, layout::TOP_BAR / 2.0);
    drawn(ctx, world, client, view, PHONE, click(at));
    settled(ctx, world, client, view, PHONE)
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
        "Start",
        "Accept",
        "Send back",
        "Unblock",
        "Integrate",
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
    assert!(has(&shown, "To: Fix the parser"), "to the active session: {shown:?}");
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

    // The note lies over the frame, moves nothing, and goes after a while.
    let bar = on(&shown, "To: Fix the parser");
    assert_eq!(on(&settled(&ctx, &mut world, &mut client, &mut view, WIDE), "To: Fix the parser"), bar);
    view.now_ms += NOTICE_MS + 1.0;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "The host refused"), "{shown:?}");
    assert_eq!(on(&shown, "To: Fix the parser"), bar);
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
    assert!(has(&shown, "Archive plan") && has(&shown, "Complete plan"), "atomics.v1 offers both ways: {shown:?}");
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
    assert!(has(&shown, "Completed · read-only"), "{shown:?}");
    assert!(has(&shown, "Archive"), "a finished plan's own Archive: {shown:?}");
    for action in
        ["Dispatch ready", "Close plan…", "Start", "Accept", "Send back", "Unblock", "Integrate", "Integrate", "Reopen"]
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
    for button in ["Dispatch ready (1)", "Start", "Accept", "Send back", "Unblock", "Integrate", "Integrate"] {
        assert!(has(&shown, button), "{button} is missing: {:?}", shown.iter().map(|s| &s.0).collect::<Vec<_>>());
    }
    assert!(has(&shown, "1 accepted task not integrated yet"), "{shown:?}");

    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Start");
    assert_eq!(task(&world, "t3").status, TaskStatus::Running);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(exactly(&shown, "Integrate")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let integrated = |world: &World, id: &str| task(world, id).integrated.is_some();
    assert!(integrated(&world, "t4") ^ integrated(&world, "t5"), "the first card's: {shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Accept");
    assert_eq!(task(&world, "t4").status, TaskStatus::Done);
    click_on(&ctx, &mut world, &mut client, &mut view, BOARD, "Unblock");
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
    assert!(has(&shown, "Integrating…"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None);
}

/// The fake host's working board with its diff in the right panel.
fn on_the_diff(size: (f32, f32)) -> (egui::Context, World, Client, View, Vec<(String, Rect)>) {
    diff_of(live_with(fake_host::working()), size)
}

/// That page, on the diff.
fn diff_of(
    (ctx, mut world, mut client, mut view): (egui::Context, World, Client, View),
    size: (f32, f32),
) -> (egui::Context, World, Client, View, Vec<(String, Rect)>) {
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
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Discard");
    assert!(world.host.board.diffs[0].comments.is_empty(), "{shown:?}");
}

#[test]
fn a_refused_comment_says_why_and_comes_back_in_its_box() {
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
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter.clone()]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "The host refused: the diff is at revision 5"), "{shown:?}");
    assert!(has(&shown, "Send review (0)") && has(&shown, "Why?"), "not added, and not lost: {shown:?}");
    assert!(world.host.board.diffs[0].comments.is_empty());

    // Nothing else had the keyboard, so the box has it, and Enter tries again.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.board.diffs[0].comments[0].body, "Why?");
    assert_eq!(world.host.board.diffs[0].comments[0].path, "src/a.rs");
    assert!(has(&shown, "Send review (1)") && !has(&shown, "Comment for the agent"), "{shown:?}");
}

#[test]
fn a_refused_comment_takes_the_keyboard_from_nobody_and_stays_in_the_note_without_a_box() {
    use atomics_core::ops::{CommandResult, Outcome};
    use atomics_core::wire::{Envelope, ErrorCode, WireError};

    let comment = |body: &str, line| atomics_core::steer::ReviewComment {
        path: "src/a.rs".into(),
        line: Some(line),
        removed: false,
        excerpt: String::new(),
        body: body.into(),
    };
    let refused = |world: &World, op: &str| {
        let error = WireError::new(ErrorCode::Rejected, "not now");
        let result = CommandResult { op: op.into(), outcome: Outcome::refused(error) };
        world.driver.remote.receive(Envelope::host("epoch-1", Message::CommandResult(result)));
    };
    let op = |world: &World| {
        let found = world.host.received.iter().rev().find_map(|m| match m {
            Message::ReviewCommand(command) => Some(command.op.clone()),
            _ => None,
        });
        found.expect("a comment sent")
    };

    // The user went on to a terminal while it was on its way.
    let (ctx, mut world, mut client, mut view, _) = on_the_diff(WIDE);
    world.host.hold_results = true;
    client.send(Command::EditComments { root: WORKTREE.into(), before: Vec::new(), after: vec![comment("Why?", 2)] });
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    refused(&world, &op(&world));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Why?"), "in its box: {shown:?}");
    assert!(ctx.memory(|m| m.focused().is_none()) && view.term.typing(), "the terminal keeps the keyboard");

    // A line no longer in the diff has no box: the note has the text.
    client.send(Command::EditComments {
        root: WORKTREE.into(),
        before: Vec::new(),
        after: vec![comment("Gone?", 999)],
    });
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    refused(&world, &op(&world));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "The host refused: not now Your comment was: Gone?"), "{shown:?}");
}

/// The fake host's working board, with a comment on the host.
fn with_a_comment() -> fake_host::FakeHost {
    let mut host = fake_host::working();
    host.board.diffs[0].comments.push(atomics_core::steer::ReviewComment {
        path: "src/a.rs".into(),
        line: Some(2),
        removed: false,
        excerpt: "step(1)".into(),
        body: "On the host".into(),
    });
    host
}

/// A comment's ×: a × left of `row`, on its line.
fn comment_cross(shown: &[(String, Rect)], row: egui::Pos2) -> bool {
    shown.iter().any(|(text, rect)| text == "×" && (rect.center().y - row.y).abs() < 8.0 && rect.center().x < row.x)
}

#[test]
fn a_viewer_has_no_comment_actions_and_never_a_box() {
    // A controller has them.
    let (ctx, mut world, mut client, mut view, shown) = diff_of(live_with(with_a_comment()), WIDE);
    let row = on(&shown, "» On the host");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::PointerMoved(row)]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(comment_cross(&shown, row), "{shown:?}");

    let (ctx, mut world, mut client, mut view, shown) = diff_of(viewing(with_a_comment()), WIDE);
    let row = on(&shown, "» On the host");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::PointerMoved(row)]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!comment_cross(&shown, row), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(row));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "» On the host"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None, "it did not try");

    let line = shown.iter().find(|(text, _)| text.contains("step(1)")).expect("the diff's lines").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(line));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Comment for the agent"), "{shown:?}");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::ReviewCommand(_))));
}

#[test]
fn a_box_open_when_the_connection_drops_keeps_its_draft_until_it_can_go() {
    use atomics_core::tunnel::close;

    let (ctx, mut world, mut client, mut view, shown) = diff_of(live_with(with_a_comment()), WIDE);
    let line = shown.iter().find(|(text, _)| text.contains("step(1)")).expect("the diff's lines").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(line));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("draft".into())]);

    world.drop_socket(close::ABNORMAL);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Reconnecting") && has(&shown, "draft"), "{shown:?}");

    // Enter neither closes it nor loses it, and the keyboard stays there.
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter.clone()]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "draft"), "{shown:?}");
    assert!(ctx.memory(|m| m.focused().is_some()), "the box keeps the keyboard");
    assert_eq!(world.driver.remote.notice(), None, "nothing was tried");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text(" more".into())]);

    // The comments already there have no × meanwhile, and a line starts no other box.
    let row = on(&shown, "» On the host");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::PointerMoved(row)]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!comment_cross(&shown, row) && has(&shown, "draft more"), "{shown:?}");

    // Back: Enter sends it.
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(world.host.board.diffs[0].comments.iter().any(|c| c.body == "draft more"));
    assert!(!has(&shown, "Comment for the agent"), "the box closed: {shown:?}");
}

#[test]
fn a_draft_outlasts_the_host_sending_the_whole_board_again() {
    use atomics_core::wire::{Envelope, Limits, ResetReason, Resume, Revisions, Role, Seq, VERSION};

    let (ctx, mut world, mut client, mut view, shown) = on_the_diff(WIDE);
    let line = shown.iter().find(|(text, _)| text.contains("step(1)")).expect("the diff's lines").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(line));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("draft".into())]);

    // The channel comes back on a new board: nothing shows of it until its snapshot lands, a frame or more on.
    world.driver.remote.disconnected();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let capabilities: Vec<String> = crate::remote::CAPABILITIES.iter().map(|c| c.to_string()).collect();
    let may = Role::Controller.may(&capabilities);
    let resume = Resume::Reset { cursor: Seq(9), reason: ResetReason::CursorTooOld };
    let welcome = Message::Welcome {
        version: VERSION,
        capabilities,
        limits: Limits::default(),
        role: Role::Controller,
        resume,
        may,
    };
    world.driver.remote.receive(Envelope::host("epoch-1", welcome));
    let gap = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&gap, "draft"), "{gap:?}");

    let board = world.host.board.clone();
    let snapshot = Message::StateSnapshot {
        cursor: Seq(9),
        revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1), host: Seq(0) },
        sessions: board.sessions,
        plans: board.plans,
        approvals: board.approvals,
        diffs: board.diffs,
        host: board.host,
        part: None,
    };
    world.driver.remote.receive(Envelope::host("epoch-1", snapshot));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "draft"), "the box is back with its text: {shown:?}");

    // It went for those frames, so the keyboard takes a click; then Enter sends it.
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "draft")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(world.host.board.diffs[0].comments.iter().any(|c| c.body == "draft"));
}

#[test]
fn the_steer_bar_names_no_all_agents_and_shift_enter_makes_a_new_line() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let list = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "To: Fix the parser");
    assert!(has(&list, "Active: Fix the parser") && has(&list, "Selected (0)"), "{list:?}");
    assert!(!has(&list, "All agents"), "a shell can run under any name: {list:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&list, "Active: Fix the parser")));

    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Steer · Enter sends")));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("one".into())]);
    let enter = |modifiers| egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    };
    let shift = egui::Modifiers::SHIFT;
    let held = vec![egui::Event::ModifiersChanged(shift), enter(shift)];
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, held);
    assert_eq!(client.steer_text, "one\n");
    assert!(world.host.steered.is_empty(), "as its hint says");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("two".into())]);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter(egui::Modifiers::NONE)]);
    assert_eq!(world.host.steered, [("s1".to_string(), "one\ntwo".to_string())]);
    assert_eq!(client.steer_text, "");
}

#[test]
fn a_steer_one_target_missed_names_it_and_stays_out_of_the_bar() {
    let mut host = fake_host::working();
    let mut other = host.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
    (other.id, other.title, other.task, other.worktree) = ("c2".into(), "Second agent".into(), None, None);
    host.board.sessions.push(other);
    host.terminals.insert("c2".into(), fake_host::FakeTerminal::new(80, 12, "$ "));
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.selected.extend(["c2".to_string(), "s1".to_string()]);
    client.steer_target = atomics_ui::chief::SteerTarget::Selected;
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Steer · Enter sends")));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("go on".into())]);
    let enter = egui::Event::Key {
        key: egui::Key::Enter,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    };
    world.host.refuse_next = Some(atomics_core::wire::WireError::new(
        atomics_core::wire::ErrorCode::Rejected,
        "message not delivered: session exited",
    ));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter.clone()]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.steered, [("s1".to_string(), "go on".to_string())]);
    assert_eq!(client.steer_text, "", "Enter would reach the agent that has it again");
    assert!(has(&shown, "Missed by Second agent. The host refused: message not delivered"), "{shown:?}");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![enter]);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.steered.len(), 1);
}

#[test]
fn a_note_takes_the_pointer_and_a_click_on_it_dismisses_it() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    world.driver.remote.say("A note that goes on. ".repeat(70));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let note = shown.iter().find(|(text, _)| text.starts_with("A note")).expect("the note").1;

    // As for any window, the input over a terminal gives way to it.
    let covered = &view.term.overlay.covered;
    assert!(covered.iter().any(|part| part.intersects(note)), "{covered:?} under {note:?}");

    // A click on what lies under it reaches only the note.
    let close = shown.iter().find(|(text, rect)| text == "×" && note.contains(rect.center()));
    let close = close.expect("a tile's close under the note").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(close));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.confirm_close.is_none(), "{shown:?}");
    assert!(!has(&shown, "A note"), "{shown:?}");
    assert_eq!(world.driver.remote.notice(), None);
    assert!(view.term.overlay.covered.is_empty(), "{:?}", view.term.overlay);

    // With the note gone it does.
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(close));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.confirm_close.as_deref(), Some("s1"));
}

#[test]
fn the_keyboard_in_a_text_box_lets_go_of_the_lease_and_a_click_back_asks_again() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let bar = on(&shown, "Steer · Enter sends");
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "You are typing") && world.host.terminals["s1"].lease.is_some(), "{shown:?}");

    // The keyboard left for the window behind, say: nothing else has it, so the lease stays until it runs out.
    view.term.inputs.push(term_pane::TermInput::Focus(false));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "You are typing") && world.host.terminals["s1"].lease.is_some(), "{shown:?}");

    view.term.inputs.push(term_pane::TermInput::Focus(true));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    view.term.inputs.push(term_pane::TermInput::Focus(false));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(bar));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(world.host.terminals["s1"].lease.is_none(), "another browser sees it free");
    assert!(world.host.terminals["s1"].followed);
    assert!(has(&shown, "Click the terminal to type"), "{shown:?}");
    let released = world.sent().iter().filter(|m| matches!(m, Message::LeaseRelease { .. })).count();
    assert_eq!(released, 1);

    // Typing on in the bar lets nothing go twice, and a click back asks again.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("x".into())]);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.sent().iter().filter(|m| matches!(m, Message::LeaseRelease { .. })).count(), 1);
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "You are typing") && world.host.terminals["s1"].lease.is_some(), "{shown:?}");
    view.term.inputs.push(term_pane::TermInput::Text("ls".into()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.terminals["s1"].typed, "ls");
}

#[test]
fn a_cards_open_types_in_its_agent_in_a_wide_window() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.send(Command::OpenTaskSession("s1".into()));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(view.term.overlay.focus, "as picking it from the sidebar does");

    // A finger taps the terminal itself, which opens the keyboard.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    client.send(Command::OpenTaskSession("s1".into()));
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, Vec::new());
    assert!(!view.term.focus_next && !view.term.overlay.focus);
    assert_eq!(view.tab, Tab::Diffs);
}

#[test]
fn a_finished_plan_picked_from_history_shows_in_the_phones_strip() {
    use atomics_core::plan::PlanStatus;

    let mut host = fake_host::many_plans();
    for plan in host.board.plans.iter_mut().filter(|plan| ("a-01".."a-08").contains(&plan.id.as_str())) {
        plan.status = PlanStatus::Open;
    }
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    let opened = drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&shown, "History")));
    let opened =
        if has(&opened, "✓ Archived") { opened } else { settled(&ctx, &mut world, &mut client, &mut view, PHONE) };
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(on(&opened, "✓ Archived plan number 0")));
    let mut shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    for _ in 0..60 {
        shown = drawn(&ctx, &mut world, &mut client, &mut view, PHONE, Vec::new());
    }
    assert_eq!(view.desk.plan.as_deref(), Some("a-00"));
    let name = shown.iter().find(|(text, _)| text.starts_with("✓ Archived plan number 0")).expect("its name").1;
    let history = on(&shown, "History");
    assert!(name.left() >= layout::TOUCH && name.left() < history.x, "{name:?} left of {history:?}");

    // The plan before it is still a tap away, at the strip's left end.
    let before = egui::pos2(layout::GUTTER + 10.0, name.center().y);
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(before));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_ne!(view.desk.plan.as_deref(), Some("a-00"), "it picked the plan before");
    let open = world.driver.remote.plan_list().into_iter().find(|row| Some(&row.id) == view.desk.plan.as_ref());
    assert_eq!(open.map(|row| row.status), Some(atomics_core::plan::PlanStatus::Open));
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

    let picker = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "+ New");
    // The picker's Start, drawn over the board's cards and their own.
    let start = picker.iter().rfind(|(text, _)| text == "Start").expect("the picker's Start").1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(start));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
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
    assert!(view.desk.plan_view && !view.desk.ungrouped, "the page stays where it was");
}

#[test]
fn a_hosts_empty_frame_offers_what_the_browser_does() {
    // The host can start its chief without a mirrored agent. The welcome offers no other agent
    // until the mirror names one.
    let mut host = fake_host::host();
    host.board.sessions.clear();
    host.board.approvals.clear();
    host.board.plans.clear();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "CHIEF") && has(&shown, "start"), "the pill says it can start: {shown:?}");
    assert!(!has(&shown, "+ New") && !has(&shown, "Start "), "the mirror names no agent: {shown:?}");
    assert!(has(&shown, "AGENTS") && !has(&shown, "EXPLORER"), "no projects, no way to open one: {shown:?}");
    let text: String = shown.iter().map(|(text, _)| text.as_str()).collect();
    assert!(!text.contains("scratch folder"), "nothing to start, nowhere to say it starts: {text}");
    assert!(text.contains("No agents yet.") && !text.contains("+ New"), "{text}");
    assert!(text.contains("worktree's diff against main") && text.contains("Comment on any line"), "{text}");

    client.send(Command::ToggleChief);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(world.driver.remote.session_list().iter().any(|s| s.chief));
    assert!(
        world.sent().iter().any(|m| matches!(m, Message::HostCommand(c) if matches!(c.action, HostAction::StartChief)))
    );

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
    assert!(has(&shown, "Archive plan") && has(&shown, "Complete plan"), "{shown:?}");
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

    // Its AGENTS list offers selecting sessions for the steer bar.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    view.tab = Tab::Agents;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, right_click(on(&shown, "Fix the parser")));
    let menu = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&menu, "Close") && has(&menu, "Include in"), "{menu:?}");
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

/// What the views draw, as PNGs for a person to look at beside the desktop's QA screenshots: the wide window
/// and the phone at the browser test's sizes and narrower, over the fake host, through the desktop's CPU
/// rasterizer. Ignored by default; `ATOMICS_WEB_HEADLESS_SHOTS=<folder>` says where they go.
#[test]
#[ignore]
fn headless_shots() {
    let Some(dir) = std::env::var_os("ATOMICS_WEB_HEADLESS_SHOTS").map(PathBuf::from) else { return };
    std::fs::create_dir_all(&dir).unwrap();
    // The last ones over a host whose ticket system needs the user to sign in again.
    let shots: [(&str, (f32, f32), Tab, bool); 16] = [
        ("phone-402-board", (402.0, 874.0), Tab::Board, false),
        ("phone-402-agents", (402.0, 874.0), Tab::Agents, false),
        ("phone-874-sideways", (874.0, 402.0), Tab::Board, false),
        ("wide-2800-explorer", BOARD, Tab::Agents, false),
        ("phone-390-explorer", PHONE, Tab::Agents, false),
        ("phone-360-explorer", (360.0, 780.0), Tab::Agents, false),
        ("wide-1600", WIDE, Tab::Board, false),
        ("wide-1100", (1100.0, 800.0), Tab::Board, false),
        ("wide-700", (700.0, 800.0), Tab::Board, false),
        ("phone-390-board", PHONE, Tab::Board, false),
        ("phone-390-diffs", PHONE, Tab::Diffs, false),
        ("phone-390-chief", PHONE, Tab::Chief, false),
        ("phone-360-agents", (360.0, 780.0), Tab::Agents, false),
        ("wide-1600-reconnect", WIDE, Tab::Board, true),
        ("wide-1100-reconnect", (1100.0, 800.0), Tab::Board, true),
        ("phone-390-reconnect", PHONE, Tab::Board, true),
    ];
    for (name, (width, height), tab, reconnect) in shots {
        let (ctx, mut world, mut client, mut view) = if name.contains("explorer") {
            live_with(fake_host::explorer())
        } else if reconnect {
            live_with(fake_host::tickets(SignInState::Expired))
        } else {
            live()
        };
        view.tab = tab;
        let mut raster = crate::test_raster::Raster::default();
        let mut last = None;
        for _ in 0..6 {
            view.now_ms += 16.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
                time: Some(view.now_ms / 1000.0),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, &mut client, &mut view));
            raster.feed(&output);
            output.textures_delta.clear();
            world.pump();
            last = Some(output);
        }
        let output = last.unwrap();
        let ppp = output.pixels_per_point;
        let size = [(width * ppp).round() as usize, (height * ppp).round() as usize];
        let image = raster.paint(&ctx, output, size, client.palette.background);
        let path = dir.join(format!("{name}.png"));
        crate::test_raster::save_png(&path, &image).unwrap();
        println!("saved {}", path.display());
    }
}

/// A finger on a phone's page: where a tap takes the keyboard, and whether the page's word for it is right.
#[test]
fn a_fingers_tap_takes_the_keyboard_on_a_text_box_and_on_a_line_that_opens_one() {
    let (ctx, mut world, mut client, mut view) = live();
    view.viewport.coarse = true;
    view.tab = Tab::Agents;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    let steer = *view.text_boxes.first().expect("the steer bar");
    assert!(ctx.memory(|m| m.focused()).is_none());
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, click(steer.center()));
    assert!(
        ctx.memory(|m| m.focused()).is_some_and(|id| egui::TextEdit::load_state(&ctx, id).is_some()),
        "the app focuses what the page took the keyboard for"
    );

    // Nothing to type in, and nothing marked.
    view.tab = Tab::Board;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.text_boxes, []);

    // What lies over a text box takes its taps.
    view.tab = Tab::Agents;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    let steer = *view.text_boxes.first().expect("the steer bar");
    for _ in 0..3 {
        view.now_ms += 16.0;
        let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(PHONE.0, PHONE.1));
        let input = egui::RawInput { screen_rect: Some(screen), ..egui::RawInput::default() };
        let mut output = ctx.run_ui(input, |ui| {
            let over = egui::Area::new(egui::Id::NULL.with("over the steer bar")).order(egui::Order::Foreground);
            over.fixed_pos(steer.min).show(ui.ctx(), |ui| ui.allocate_space(steer.size()));
            draw(ui, &mut world.driver, &mut client, &mut view);
        });
        output.textures_delta.clear();
        world.pump();
    }
    assert_eq!(view.text_boxes, [], "with something over it");

    // A mouse needs none of it.
    let (ctx, mut world, mut client, mut view) = live();
    view.tab = Tab::Agents;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(view.text_boxes, []);

    // A line of a diff opens a comment box under the finger, in the same tap; a viewer's opens nothing.
    let show_diff = |view: &mut View| {
        view.viewport.coarse = true;
        view.tab = Tab::Diffs;
        view.diff = Some(PathBuf::from(WORKTREE));
    };
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    show_diff(&mut view);
    for _ in 0..4 {
        settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    }
    assert!(view.text_boxes.len() > 2, "its lines: {:?}", view.text_boxes);
    // The box asks for the keyboard in the frame of the tap: a frame with none would take it from the page's input.
    let line = view.text_boxes[2];
    view.now_ms += 16.0;
    let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(PHONE.0, PHONE.1));
    let input = egui::RawInput { screen_rect: Some(screen), events: click(line.center()), ..egui::RawInput::default() };
    let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, &mut client, &mut view));
    output.textures_delta.clear();
    assert!(output.platform_output.ime.is_some(), "the comment box, at once");
    let (ctx, mut world, mut client, mut view) = viewing(fake_host::working());
    show_diff(&mut view);
    for _ in 0..4 {
        settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    }
    assert_eq!(view.text_boxes, []);
}

/// A finger on a tablet's screen, as the page says it: all of it visible, nothing in the way yet.
fn tablet(view: &mut View, (width, height): (f32, f32)) {
    let visible = Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height));
    view.viewport =
        Viewport { visible: Some(visible), coarse: true, screen_short: width.min(height), ..Viewport::default() };
}

const IPAD: (f32, f32) = (820.0, 1180.0);
const IPAD_SIDEWAYS: (f32, f32) = (1180.0, 820.0);

#[test]
fn a_tablet_draws_the_real_frame_with_a_fingers_sizes_either_way_up() {
    for size in [IPAD, IPAD_SIDEWAYS] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
        tablet(&mut view, size);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!view.compact, "{size:?}");
        assert_eq!(client.touch.map(|t| (t.target, t.stack)), Some((layout::TABLET_TOUCH, false)), "{size:?}");
        assert_eq!(ctx.zoom_factor(), 1.0, "{size:?}");
        assert_eq!(ctx.global_style().spacing.interact_size.y, layout::TABLET_TOUCH, "{size:?}");
        assert!(Arc::ptr_eq(&client.palette, &view.looks.desktop), "the desktop's look of the theme");
        assert!(in_bar_of(&shown, "ATOMICS", 54.0), "the frame's top bar: {shown:?}");
        assert!(!shown.iter().any(|(text, _)| text == "Terminal"), "not the phone's panes: {shown:?}");
        assert!(view.term.touch.is_some(), "the terminal is a finger's too");
    }

    // A mouse at the same size draws as it always has, with no drawer to open.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!view.compact && client.touch.is_none());
    assert_eq!(ctx.global_style().spacing.interact_size.y, atomics_ui::theme::CONTROL_HEIGHT);
}

/// Whether `label` shows in the top `height` of the window.
fn in_bar_of(shown: &[(String, Rect)], label: &str, height: f32) -> bool {
    shown.iter().any(|(text, rect)| text == label && rect.bottom() <= height)
}

#[test]
fn a_tablets_frame_and_its_windows_keep_to_what_the_user_sees() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    tablet(&mut view, IPAD);
    view.viewport.inset_top = 24.0;
    view.viewport.inset_bottom = 20.0;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!in_bar_of(&shown, "ATOMICS", 24.0), "under the status bar: {shown:?}");
    assert!(in_bar_of(&shown, "ATOMICS", 24.0 + 54.0), "{shown:?}");
    let screen = Rect::from_min_size(egui::Pos2::ZERO, vec2(IPAD.0, IPAD.1));
    let safe = safe_area(&view, screen, 1.0);
    assert_eq!((safe.0.top, safe.0.bottom), (24.0, 20.0));

    // The keyboard comes up with the key bar on it: the terminal and everything else stay above them.
    view.viewport.visible = Some(screen.with_max_y(800.0));
    view.viewport.keybar = 92.0;
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(view.term.typing(), "the terminal stays, or the keyboard would go with it: {:?}", view.term.overlay);
    let bottom = 800.0 - 92.0;
    assert!(view.term.overlay.rect.is_some_and(|r| r.bottom() <= bottom), "{:?}", view.term.overlay);
    let under: Vec<_> = shown.iter().filter(|(_, rect)| rect.top() > bottom).collect();
    assert!(under.is_empty(), "nothing under the key bar: {under:?}");
    assert_eq!(safe_area(&view, screen, 1.0).0.bottom, 1180.0 - bottom, "nor any window");

    // A mouse's screen has none of it.
    let (_, _, _, view) = live();
    assert_eq!(safe_area(&view, screen, 1.0), egui::SafeAreaInsets::default());
}

#[test]
fn a_tablets_chief_starts_closed_and_opens_over_the_area_to_type_in_at_once() {
    for size in [IPAD, IPAD_SIDEWAYS] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
        tablet(&mut view, size);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!view.desk.chief.open, "{size:?}: a first look is at the fleet and the board");
        assert!(!has(&shown, "◆ CHIEF AGENT"), "{size:?}: {shown:?}");
        let cells = view.term.overlay.rect.expect("the agent's terminal");

        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "CHIEF");
        let shown = if has(&shown, "◆ CHIEF AGENT") {
            shown
        } else {
            settled(&ctx, &mut world, &mut client, &mut view, size)
        };
        assert!(view.desk.chief.open && has(&shown, "◆ CHIEF AGENT"), "{size:?}: {shown:?}");
        assert!(!has(&shown, "Float"), "{size:?}: a finger has no window to float: {shown:?}");
        // The input is over the chief's terminal, so one tap types there.
        let over = view.term.overlay.rect.expect("the chief's terminal");
        assert_ne!(over, cells, "{size:?}");
        assert_eq!(view.term.session(), Some("chief"), "{size:?}");
        assert!(view.term.overlay.covered.is_empty(), "{size:?}: {:?}", view.term.overlay);
        assert!(over.width() > size.0 * 0.9, "{size:?}: it takes the room: {over:?}");

        // Its button closes it, and the input goes back to the agent.
        click_on(&ctx, &mut world, &mut client, &mut view, size, "Minimize");
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!view.desk.chief.open, "{size:?}");
        assert_eq!(view.term.session(), Some("s1"), "{size:?}");
    }

    // A boot that asked for the chief has it.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    tablet(&mut view, IPAD);
    view.open(Tab::Chief);
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(view.desk.chief.open);
}

#[test]
fn where_the_sidebar_does_not_fit_a_finger_opens_it_as_a_drawer() {
    // The middle of the button that opens it: first in the top bar, a finger's size.
    let button = egui::pos2(10.0 + layout::TABLET_TOUCH / 2.0, 26.0);
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    tablet(&mut view, IPAD);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(client.sidebar_hidden, "no room for it upright");
    assert!(!has(&shown, "AGENTS"), "{shown:?}");

    sized(&ctx, &mut world, &mut client, &mut view, IPAD, click(button));
    let shown = slid(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(client.drawer && has(&shown, "AGENTS"), "{shown:?}");
    assert_eq!(view.term.overlay.covered, [view.term.overlay.rect.unwrap()], "it takes the terminal's pointer");

    // A tap beside it closes it, and reaches nothing under it.
    let before = world.sent().len();
    sized(&ctx, &mut world, &mut client, &mut view, IPAD, click(egui::pos2(700.0, 600.0)));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer && !has(&shown, "AGENTS"), "{shown:?}");
    assert!(!world.sent()[before..].iter().any(|m| matches!(m, Message::LeaseAcquire { .. })));
    assert!(view.term.overlay.covered.is_empty(), "{:?}", view.term.overlay);

    // Picking an agent in it closes it too.
    sized(&ctx, &mut world, &mut client, &mut view, IPAD, click(button));
    let shown = slid(&ctx, &mut world, &mut client, &mut view, IPAD);
    // The last drawn of that name: the tile's lies under the drawer.
    let row = shown.iter().rfind(|(text, _)| text == "Fix the parser").expect("the agent's row");
    sized(&ctx, &mut world, &mut client, &mut view, IPAD, click(row.1.center()));
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer, "it closed, though that agent was the active one already");
    assert_eq!(view.desk.active.as_deref(), Some("s1"));

    // A mouse at this size has no such button: a click there is on the brand.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    sized(&ctx, &mut world, &mut client, &mut view, IPAD, click(button));
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer && client.touch.is_none());
}

/// The drawer's button: first in the top bar, a finger's size.
const DRAWER_BUTTON: egui::Pos2 = egui::pos2(10.0 + layout::TABLET_TOUCH / 2.0, 26.0);

/// A tap on the drawer's button at `size`, and frames until it is all the way in: whether it slid in from the side, its
/// left edge well left of where it ends in its first frames.
fn opens_sliding(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
) -> bool {
    let left = || ctx.memory(|m| m.area_rect(egui::Id::NULL.with("drawer"))).map(|rect| rect.left());
    drawn(ctx, world, client, view, size, click(DRAWER_BUTTON));
    let mut first = left();
    for _ in 0..2 {
        drawn(ctx, world, client, view, size, Vec::new());
        first = first.into_iter().chain(left()).reduce(f32::min);
    }
    let shown = slid(ctx, world, client, view, size);
    assert!(client.drawer && has(&shown, "AGENTS"), "{shown:?}");
    first.zip(left()).is_some_and(|(first, last)| first < last - 100.0)
}

/// Frames for the drawer to slide all the way in or out; the text of the last.
fn slid(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
) -> Vec<(String, Rect)> {
    for _ in 0..15 {
        drawn(ctx, world, client, view, size, Vec::new());
    }
    drawn(ctx, world, client, view, size, Vec::new())
}

fn key(key: egui::Key) -> Vec<egui::Event> {
    vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }]
}

#[test]
fn the_drawer_holds_the_keyboard_closes_on_escape_and_back_and_slides_in_however_it_closed() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    tablet(&mut view, IPAD);
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    // The terminal has the keyboard as the drawer opens: it lets go of it.
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(view.term.typing());
    assert!(opens_sliding(&ctx, &mut world, &mut client, &mut view, IPAD), "the first time");
    assert!(!view.term.typing(), "the terminal let go of the keyboard: {:?}", view.term.overlay);

    // Tab and Shift+Tab walk what is in the drawer and nothing behind it.
    let drawer = egui::Id::NULL.with("drawer");
    let mut walked = Vec::new();
    for shift in [false, false, false, false, true, true] {
        let mut events = key(egui::Key::Tab);
        if let egui::Event::Key { modifiers, .. } = &mut events[0] {
            modifiers.shift = shift;
        }
        drawn(&ctx, &mut world, &mut client, &mut view, IPAD, events);
        drawn(&ctx, &mut world, &mut client, &mut view, IPAD, Vec::new());
        let focused = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id));
        walked.push(focused.map(|r| r.layer_id.id));
    }
    assert!(walked.iter().all(|layer| *layer == Some(drawer)), "the focus stays in the drawer: {walked:?}");

    // Escape closes it.
    drawn(&ctx, &mut world, &mut client, &mut view, IPAD, key(egui::Key::Escape));
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer, "Escape closes it");
    assert!(opens_sliding(&ctx, &mut world, &mut client, &mut view, IPAD), "after Escape");

    // The browser's Back closes it.
    view.back = true;
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer, "Back closes it");
    assert!(opens_sliding(&ctx, &mut world, &mut client, &mut view, IPAD), "after Back");

    // A tap on its button's place closes it, as anywhere beside it does.
    drawn(&ctx, &mut world, &mut client, &mut view, IPAD, click(DRAWER_BUTTON));
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(!client.drawer, "its button closes it");
    assert!(opens_sliding(&ctx, &mut world, &mut client, &mut view, IPAD), "after its button");

    // Turned to where the sidebar fits (a 12.9-inch iPad's width sideways), the drawer goes; back to where it does not,
    // it opens from the side as before.
    let wider = (1366.0, 1024.0);
    tablet(&mut view, wider);
    settled(&ctx, &mut world, &mut client, &mut view, wider);
    assert!(!client.drawer && !client.sidebar_hidden, "the sidebar shows, and the drawer goes");
    tablet(&mut view, IPAD);
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert!(opens_sliding(&ctx, &mut world, &mut client, &mut view, IPAD), "after a rotation");
}

#[test]
fn reduced_motion_takes_the_time_out_of_eguis_own_animations_in_every_look() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let default = egui::Style::default();
    let calm = |ctx: &egui::Context| {
        let style = ctx.global_style();
        (style.animation_time, style.scroll_animation)
    };
    tablet(&mut view, IPAD);
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert_eq!(calm(&ctx), (default.animation_time, default.scroll_animation));
    view.viewport.reduce_motion = true;
    let none = (0.0, egui::style::ScrollAnimation::none());
    settled(&ctx, &mut world, &mut client, &mut view, IPAD);
    assert_eq!(calm(&ctx), none, "a tablet");
    // A phone's look, put on anew, keeps it.
    tablet(&mut view, PHONE);
    view.viewport.reduce_motion = true;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.compact);
    assert_eq!(calm(&ctx), none, "a phone");
    view.viewport.reduce_motion = false;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert_eq!(calm(&ctx), (default.animation_time, default.scroll_animation), "and back");
}

/// What a finger can tap on this frame that is smaller than `target` either way, named by the text in it.
fn too_small(ctx: &egui::Context, shown: &[(String, Rect)], target: f32) -> Vec<(String, Rect)> {
    let screen = ctx.viewport_rect();
    let tappable: Vec<Rect> = ctx.viewport(|viewport| {
        let widgets = viewport.prev_pass.widgets.layers().flat_map(|(_, widgets)| widgets.iter());
        // Clipped away, or scrolled out of the pane.
        // What only takes a click so the card under it does not act on it offers a finger nothing.
        let seen = widgets
            .filter(|w| w.enabled && w.sense.senses_click() && w.sense.is_focusable() && w.interact_rect.is_positive());
        seen.map(|w| w.rect).collect()
    });
    let big = |rect: &Rect| rect.width().min(rect.height()) >= target - 0.5;
    // A finger's target laid around it, no bigger than it has to be ([`touch::reach`]).
    let reached = |small: &Rect| {
        tappable.iter().any(|around| big(around) && around.contains_rect(*small) && around.height() <= target + 0.5)
    };
    // A diff's rows are lines of text to read, and a scroll bar or a divider is dragged along its length.
    let meant = |rect: &Rect| !(rect.width().min(rect.height()) < 26.0 && rect.width().max(rect.height()) > 300.0);
    let small = tappable.iter().filter(|rect| screen.intersects(**rect) && !big(rect) && !reached(rect) && meant(rect));
    small
        .map(|rect| {
            let named =
                shown.iter().find(|(_, text)| rect.intersects(*text)).map_or("(no text)", |(name, _)| name.as_str());
            (named.to_string(), *rect)
        })
        .collect()
}

/// Every pane of the phone, and the tablet's frame, for a finger: a control is 44 CSS pixels each way.
#[test]
fn every_control_a_finger_taps_is_a_fingers_size() {
    let mut small = Vec::new();
    for host in [fake_host::host, fake_host::working] {
        for tab in [Tab::Agents, Tab::Board, Tab::Approvals, Tab::Diffs, Tab::Terminal, Tab::Chief] {
            let (ctx, mut world, mut client, mut view) = live_with(host());
            view.viewport = Viewport { coarse: true, screen_short: PHONE.0, ..Viewport::default() };
            view.open(tab);
            let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
            small.extend(too_small(&ctx, &shown, layout::TOUCH).into_iter().map(|s| (format!("{tab:?}"), s)));
            if tab == Tab::Diffs {
                view.diff = Some(PathBuf::from(WORKTREE));
                for _ in 0..4 {
                    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
                }
                let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
                small.extend(too_small(&ctx, &shown, layout::TOUCH).into_iter().map(|s| ("diff".to_string(), s)));
            }
        }
        for size in [IPAD, IPAD_SIDEWAYS] {
            let (ctx, mut world, mut client, mut view) = live_with(host());
            tablet(&mut view, size);
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            small.extend(too_small(&ctx, &shown, layout::TABLET_TOUCH).into_iter().map(|s| (format!("{size:?}"), s)));
        }
    }
    small.sort_by(|a, b| a.1.0.cmp(&b.1.0));
    small.dedup_by(|a, b| a.1.0 == b.1.0);
    let list: Vec<String> = small
        .iter()
        .map(|(where_, (name, rect))| format!("{where_}: {name} {:.0}x{:.0}", rect.width(), rect.height()))
        .collect();
    assert!(list.is_empty(), "too small for a finger:\n{}", list.join("\n"));
}

/// A comment box under the fold, and then under a keyboard: it comes into sight both times.
#[test]
fn a_fingers_comment_box_stays_in_sight_as_it_opens_and_as_the_keyboard_comes_up() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    view.viewport = Viewport { coarse: true, screen_short: PHONE.0, ..Viewport::default() };
    view.tab = Tab::Diffs;
    view.diff = Some(PathBuf::from(WORKTREE));
    for _ in 0..4 {
        settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    }
    let last = *view.text_boxes.last().expect("the diff's lines");
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, click(last.center()));
    let hint = "Comment for the agent";
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, hint), "the box opened in sight: {:?}", shown.len());
    // The soft keyboard comes up, and takes the lower half.
    let height = PHONE.1 * layout::ZOOM;
    let visible = Rect::from_min_size(egui::Pos2::ZERO, vec2(PHONE.0 * layout::ZOOM, height / 2.0));
    view.viewport.visible = Some(visible);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    let at = shown.iter().find(|(text, _)| text.starts_with(hint)).expect("the box, still drawn");
    assert!(at.1.bottom() <= height / 2.0 / layout::ZOOM, "above the keyboard: {:?}", at.1);
}

#[test]
fn a_phone_with_no_board_yet_offers_only_what_it_has_to_say() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "Pairing this browser") && has(&shown, "Pairing"), "{shown:?}");
    for absent in ["Agents", "Board", "Diffs", "Terminal", "CHIEF", "⋯"] {
        assert!(!shown.iter().any(|(text, _)| text == absent), "{absent}: {shown:?}");
    }
}

/// An iPhone held sideways with the keyboard up: the terminal has what is left, with nothing around it.
#[test]
fn a_keyboard_over_a_phone_held_sideways_leaves_the_terminal_the_room() {
    const SIDEWAYS: (f32, f32) = (874.0 / layout::ZOOM, 402.0 / layout::ZOOM);
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let visible = Rect::from_min_size(egui::Pos2::ZERO, vec2(874.0, 200.0));
    view.viewport =
        Viewport { visible: Some(visible), coarse: true, screen_short: 402.0, keybar: 48.0, ..Viewport::default() };
    view.tab = Tab::Terminal;
    settled(&ctx, &mut world, &mut client, &mut view, SIDEWAYS);
    view.term.inputs.push(term_pane::TermInput::Focus(true));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, SIDEWAYS);
    assert!(view.compact && view.term.typing() && view.term.cramped);
    let cells = view.term.overlay.rect.expect("the terminal");
    assert!(cells.height() * layout::ZOOM >= 150.0, "{cells:?}");
    assert_eq!(cells.top(), 0.0, "{cells:?}");
    assert!(!shown.iter().any(|(text, _)| text == "CHIEF" || text == "You are typing"), "{shown:?}");
    // Typed text still goes there.
    view.term.inputs.push(term_pane::TermInput::Text("ls".into()));
    settled(&ctx, &mut world, &mut client, &mut view, SIDEWAYS);
    assert_eq!(world.host.terminals["s1"].typed, "ls");
    // The keyboard goes, and so does all of that.
    view.viewport.visible = Some(visible.with_max_y(402.0));
    view.term.inputs.push(term_pane::TermInput::Focus(false));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, SIDEWAYS);
    assert!(!view.term.cramped && has(&shown, "CHIEF"), "{shown:?}");
}

#[test]
fn the_page_learns_the_text_boxes_in_css_pixels() {
    let boxes = [Rect::from_min_size(egui::pos2(8.0, 16.0), vec2(80.0, 40.0))];
    assert_eq!(text_boxes_json(&boxes, 1.25), "[[10.0,20.0,100.0,50.0]]");
}

/// 11 CSS pixels of wobble is 8.8 points at the phone's zoom: too far for a mouse's click, not for a finger's tap.
#[test]
fn a_finger_that_wobbles_still_presses_a_button_and_a_mouse_clicks_as_it_did() {
    for (coarse, pressed) in [(true, true), (false, false)] {
        let (ctx, mut world, mut client, mut view) = live();
        view.viewport.coarse = coarse;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
        assert_eq!(view.tab, Tab::Board);
        let at = on(&shown, "Agents");
        let button = |pressed, pos| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        let moved = at + vec2(8.8, 0.0);
        let events = [
            vec![egui::Event::PointerMoved(at), button(true, at)],
            vec![egui::Event::PointerMoved(moved)],
            vec![button(false, moved)],
        ];
        for events in events {
            sized(&ctx, &mut world, &mut client, &mut view, PHONE, events);
        }
        settled(&ctx, &mut world, &mut client, &mut view, PHONE);
        assert_eq!(view.tab == Tab::Agents, pressed, "coarse: {coarse}");
    }
}

/// A first run's host: the setup screen opens by itself, in every layout, and skipping it tells the host; a client that
/// may not change the host never sees it; the question a refused start asks goes the same way.
#[test]
fn a_first_run_opens_the_setup_screen_once_and_skipping_it_tells_the_host() {
    use atomics_core::ops::SettingsTool;
    let first_run = || {
        let mut host = fake_host::management();
        host.board.host.management.as_mut().unwrap().setup.first_run = true;
        host
    };
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(first_run());
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        // Start and the way out stay in view at any size; the rest scrolls.
        for label in ["Your agents", "Where agents run", "Start Claude Code", "Skip for now"] {
            assert!(has(&shown, label), "{label} on {size:?}: {shown:?}");
        }
        if size == WIDE {
            assert!(has(&shown, "A first project") && has(&shown, "Connections"), "{shown:?}");
        }
        assert!(!has(&shown, "Start Codex"), "not found on the host: {shown:?}");
        assert_eq!(tools_sent(&world), [SettingsTool::CheckPrograms], "it looks as it opens, once");
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "Skip for now");
        assert!(!has(&shown, "Your agents"), "{size:?}: {shown:?}");
        assert_eq!(tools_sent(&world).last(), Some(&SettingsTool::FinishSetup), "{size:?}");
        // The host's answer has not come back yet, and it does not open again meanwhile.
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!has(&shown, "Your agents"), "{size:?}: {shown:?}");
    }
    // With nothing running it has the window to itself, and a short window still shows the way on.
    for size in [WIDE, (1100.0, 560.0)] {
        let mut host = first_run();
        host.board.sessions.clear();
        let (ctx, mut world, mut client, mut view) = live_with(host);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Your agents") && has(&shown, "Start Claude Code"), "{size:?}: {shown:?}");
        assert!(!has(&shown, "Set up ATOMICS") && !has(&shown, "Settings"), "no window, no top bar: {shown:?}");
        click_on(&ctx, &mut world, &mut client, &mut view, size, "Skip for now");
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        // Settings may have folded into a menu at this width; the welcome's own Start is there either way.
        assert!(has(&shown, "Start claude") && !has(&shown, "Your agents"), "the usual window: {shown:?}");
    }

    let (ctx, mut world, mut client, mut view) = viewing(first_run());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "Your agents"), "a viewer has nothing to set up: {shown:?}");
    assert!(tools_sent(&world).is_empty());

    for size in [WIDE, PHONE] {
        let mut host = fake_host::management();
        host.board.host.management.as_mut().unwrap().setup.start_choice = Some(atomics_core::projection::StartChoice {
            agent: "claude".into(),
            why: "Docker Sandboxes (sbx) is not installed.".into(),
            sandbox_required: false,
        });
        let (ctx, mut world, mut client, mut view) = live_with(host);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Set up sandboxes"), "{size:?}: {shown:?}");
        click_on(&ctx, &mut world, &mut client, &mut view, size, "Run on this machine");
        assert_eq!(tools_sent(&world).last(), Some(&SettingsTool::AnswerStart { run_here: true }), "{size:?}");
    }
}

/// Settings on any client: what sandboxes need, with a click, a pasted value, a question or an exact command for each,
/// and what the step under way or the last failure says.
#[test]
fn settings_lists_what_sandboxes_need_and_each_step_goes_to_the_host() {
    use atomics_core::ops::SettingsTool;
    use atomics_core::projection::{SetupAction, SetupClick, SetupFailure, SetupJob, SetupStep};
    let steps = || {
        vec![
            SetupStep {
                name: "Docker account".into(),
                clicks: vec![SetupClick::new(SetupAction::DockerLogin, "Sign in")],
                ..SetupStep::default()
            },
            SetupStep {
                name: "Claude in sandboxes".into(),
                clicks: vec![SetupClick {
                    paste: Some("Anthropic API key or token".into()),
                    ..SetupClick::new(SetupAction::ClaudeCredential, "Save")
                }],
                ..SetupStep::default()
            },
            SetupStep {
                name: "A github secret in sbx".into(),
                clicks: vec![SetupClick {
                    confirm: Some("Remove sbx's github secret?".into()),
                    ..SetupClick::new(SetupAction::RemoveGithubSecret, "Remove")
                }],
                ..SetupStep::default()
            },
            SetupStep {
                name: "The kvm group".into(),
                command: Some("sudo usermod -aG kvm \"$USER\"".into()),
                ..SetupStep::default()
            },
        ]
    };
    let host = |setup: &dyn Fn(&mut atomics_core::projection::SetupInfo)| {
        let mut host = fake_host::management();
        let info = &mut host.board.host.management.as_mut().unwrap().setup;
        info.sandbox_steps = steps();
        setup(info);
        host
    };
    let (ctx, mut world, mut client, mut view) = live_with(host(&|_| {}));
    client.management.settings = true;
    section(&ctx, &mut world, &mut client, &mut view, WIDE, "Theme");
    let shown = section(&ctx, &mut world, &mut client, &mut view, WIDE, "Agents");
    for label in ["What they need", "Docker Sandboxes", "Docker account", "Copy: sudo usermod", "Every check"] {
        assert!(has(&shown, label), "{label}: {shown:?}");
    }
    assert!(!has(&shown, "KVM is not available"), "the detail stays behind its row: {shown:?}");

    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Sign in");
    assert_eq!(
        tools_sent(&world).last(),
        Some(&SettingsTool::RunSetup { action: SetupAction::DockerLogin, value: None }),
        "{shown:?}"
    );
    // What cannot be undone asks first, and Keep leaves it alone.
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Remove");
    assert!(has(&shown, "Remove sbx's github secret?"), "{shown:?}");
    let before = tools_sent(&world).len();
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Keep");
    assert!(!has(&shown, "Remove sbx's github secret?"), "{shown:?}");
    assert_eq!(tools_sent(&world).len(), before);
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Remove");
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let yes = shown.iter().rfind(|(text, _)| text == "Remove").unwrap().1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(yes));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(
        tools_sent(&world).last(),
        Some(&SettingsTool::RunSetup { action: SetupAction::RemoveGithubSecret, value: None })
    );

    // While a step runs it says so, and offers a way out; nothing else can start.
    let (ctx, mut world, mut client, mut view) = live_with(host(&|info| {
        info.job = Some(SetupJob { action: SetupAction::DockerLogin, progress: "Finish in your browser…".into() });
    }));
    client.management.settings = true;
    section(&ctx, &mut world, &mut client, &mut view, WIDE, "Theme");
    let shown = section(&ctx, &mut world, &mut client, &mut view, WIDE, "Agents");
    assert!(has(&shown, "Finish in your browser") && has(&shown, "Cancel"), "{shown:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Cancel");
    assert_eq!(tools_sent(&world).last(), Some(&SettingsTool::CancelSetup));

    // A failure says why, beside its step.
    let (ctx, mut world, mut client, mut view) = live_with(host(&|info| {
        info.failure = Some(SetupFailure { action: SetupAction::DockerLogin, why: "the sign-in timed out".into() });
    }));
    client.management.settings = true;
    section(&ctx, &mut world, &mut client, &mut view, WIDE, "Theme");
    let shown = section(&ctx, &mut world, &mut client, &mut view, WIDE, "Agents");
    assert!(has(&shown, "the sign-in timed out"), "{shown:?}");

    // A client that may not change the host reads it all and starts nothing.
    let (ctx, mut world, mut client, mut view) = viewing(host(&|_| {}));
    client.management.settings = true;
    section(&ctx, &mut world, &mut client, &mut view, WIDE, "Theme");
    section(&ctx, &mut world, &mut client, &mut view, WIDE, "Agents");
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Sign in");
    assert!(!tools_sent(&world).iter().any(|tool| matches!(tool, SettingsTool::RunSetup { .. })));
}

/// The setup screen as a first run sees it. Ignored by default; `ATOMICS_WEB_HEADLESS_SHOTS=<folder>` says where it goes.
#[test]
#[ignore]
fn headless_setup_shots() {
    let Some(dir) = std::env::var_os("ATOMICS_WEB_HEADLESS_SHOTS").map(PathBuf::from) else { return };
    std::fs::create_dir_all(&dir).unwrap();
    let shots = [
        ("page-1600", WIDE, true),
        ("page-1100", (1100.0, 800.0), true),
        ("page-short", (1100.0, 560.0), true),
        ("window-1600", WIDE, false),
        ("phone-390", PHONE, false),
    ];
    for (name, (width, height), empty) in shots {
        let mut host = fake_host::management();
        host.board.host.management.as_mut().unwrap().setup.first_run = true;
        if empty {
            host.board.sessions.clear();
        }
        let (ctx, mut world, mut client, mut view) = live_with(host);
        let mut raster = crate::test_raster::Raster::default();
        let mut last = None;
        // Long enough for a window's fade-in to finish.
        for _ in 0..40 {
            view.now_ms += 16.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(width, height))),
                time: Some(view.now_ms / 1000.0),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, &mut client, &mut view));
            raster.feed(&output);
            output.textures_delta.clear();
            world.pump();
            last = Some(output);
        }
        let output = last.unwrap();
        let ppp = output.pixels_per_point;
        let size = [(width * ppp).round() as usize, (height * ppp).round() as usize];
        let image = raster.paint(&ctx, output, size, client.palette.background);
        crate::test_raster::save_png(&dir.join(format!("setup-{name}.png")), &image).unwrap();
    }
}

/// Settings and the ticket reader, each section open, at the wide window's size and on a phone's, through the same
/// rasterizer. Ignored by default; `ATOMICS_WEB_HEADLESS_SHOTS=<folder>` says where they go.
#[test]
#[ignore]
fn headless_settings_shots() {
    let Some(dir) = std::env::var_os("ATOMICS_WEB_HEADLESS_SHOTS").map(PathBuf::from) else { return };
    std::fs::create_dir_all(&dir).unwrap();

    /// A client over a fake host with a rasterizer that has seen every frame, and so the fonts.
    struct Shots {
        ctx: egui::Context,
        world: World,
        client: Client,
        view: View,
        raster: crate::test_raster::Raster,
        size: (f32, f32),
    }
    impl Shots {
        fn new((ctx, world, client, view): (egui::Context, World, Client, View), size: (f32, f32)) -> Self {
            Self { ctx, world, client, view, raster: Default::default(), size }
        }
        fn frame(&mut self, events: Vec<egui::Event>) -> egui::FullOutput {
            self.view.now_ms += 16.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(self.size.0, self.size.1))),
                time: Some(self.view.now_ms / 1000.0),
                events,
                ..egui::RawInput::default()
            };
            let Self { ctx, world, client, view, raster, .. } = self;
            let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, client, view));
            raster.feed(&output);
            output.textures_delta.clear();
            world.pump();
            output
        }
        fn settle(&mut self) -> egui::FullOutput {
            for _ in 0..12 {
                self.frame(Vec::new());
            }
            self.frame(Vec::new())
        }
        /// Opens or closes a section of the Settings window.
        fn toggle(&mut self, label: &str) {
            let output = self.settle();
            let shown: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some((text.galley.text().to_string(), text.visual_bounding_rect())),
                    _ => None,
                })
                .collect();
            self.frame(click(row(&shown, label).center()));
        }
        fn save(&mut self, file: PathBuf) {
            let output = self.settle();
            let ppp = output.pixels_per_point;
            let size = [(self.size.0 * ppp).round() as usize, (self.size.1 * ppp).round() as usize];
            let image = self.raster.paint(&self.ctx, output, size, self.client.palette.background);
            crate::test_raster::save_png(&file, &image).unwrap();
            println!("saved {}", file.display());
        }
    }

    for (name, size) in [("wide-1600", WIDE), ("phone-390", PHONE), ("phone-320", (320.0, 640.0))] {
        let out = |part: &str| dir.join(format!("settings-{name}-{part}.png"));
        let mut shots = Shots::new(live_with(fake_host::management()), size);
        shots.client.management.settings = true;
        shots.save(out("opened"));
        shots.toggle("Theme");
        for (part, label) in [
            ("shortcuts", "Shortcuts"),
            ("agents", "Agents"),
            ("connections", "Connections"),
            ("remote", "Remote devices"),
            ("credentials", "Credentials"),
            ("advanced", "Advanced"),
        ] {
            shots.toggle(label);
            shots.save(out(part));
            shots.toggle(label);
        }

        let mut shots = Shots::new(viewing(fake_host::management()), size);
        shots.client.management.settings = true;
        shots.save(out("view-only"));

        let mut shots = Shots::new(live_with(fake_host::ticket_reader()), size);
        shots.settle();
        shots.client.send(Command::OpenTicket(fake_host::ticket_card("ENG-123").link));
        shots.save(dir.join(format!("reader-{name}.png")));
    }
}

/// The address a browser ends on after signing in, with a code nothing but the frame to the host may hold.
const ENDED_ON: &str = "http://127.0.0.1:53682/callback?code=c0de&state=s1";

#[test]
fn a_board_offers_the_ticket_sign_in_and_reconnect_in_both_layouts() {
    for (state, button, headline) in [
        (SignInState::SignedOut, "Sign in to tracker", "tracker is not signed in"),
        (SignInState::Expired, "Reconnect", "tracker needs you to sign in again"),
    ] {
        for size in [WIDE, PHONE] {
            let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(state));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(has(&shown, headline) && has(&shown, "Gives you a link to open"), "{size:?}: {shown:?}");
            drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, button)));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert_eq!(world.host.host_commands, [HostAction::SignIn], "{size:?}");
            // The host's sign-in waits on its link, which opens in a tab here; where the tab ends is pasted back.
            for wanted in
                ["Waiting for the sign-in", "https://auth.example.com/", "Open link", "Copy link", "Use it", "Cancel"]
            {
                assert!(has(&shown, wanted), "{size:?}: {wanted} is missing: {shown:?}");
            }
        }
    }
}

#[test]
fn a_sign_in_under_way_takes_the_address_the_browser_ended_on_or_is_cancelled() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::SigningIn));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Open this link on any machine") && has(&shown, "Then paste the address"), "{shown:?}");
        // A click into the box, the address, and Use it.
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "http://127.0.0.1:…")));
        sized(&ctx, &mut world, &mut client, &mut view, size, Vec::new());
        sized(&ctx, &mut world, &mut client, &mut view, size, vec![egui::Event::Text(ENDED_ON.into())]);
        assert_eq!(client.sign_in_paste, ENDED_ON, "{size:?}");
        assert!(world.host.terminals["s1"].typed.is_empty(), "the terminal got none of it");
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "Use it");
        assert_eq!(world.host.host_commands, [HostAction::SignInPaste { text: ENDED_ON.into() }], "{size:?}");
        assert!(client.sign_in_paste.is_empty(), "the box keeps none of it");
        assert!(!has(&shown, "Waiting for the sign-in") && !has(&shown, "Sign in to"), "signed in: {shown:?}");
        assert_eq!(world.driver.remote.notice(), None);
    }

    // Cancel ends it, and the host says it is signed out again.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::SigningIn));
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Cancel");
    assert_eq!(world.host.host_commands, [HostAction::SignInCancel]);
    assert!(has(&shown, "tracker is not signed in") && has(&shown, "Sign in to tracker"), "{shown:?}");

    // A paste the host refuses says why on the note, and never what was pasted.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::SigningIn));
    world.host.refuse_next = Some(WireError::new(ErrorCode::Rejected, "that address is from another sign-in"));
    client.sign_in_paste = ENDED_ON.into();
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Use it");
    assert_eq!(world.host.host_commands.len(), 1);
    assert!(has(&shown, "The host refused: that address is from another sign-in"), "{shown:?}");
    assert!(has(&shown, "Waiting for the sign-in"), "it still waits: {shown:?}");
    assert!(!shown.iter().any(|(text, _)| text.contains("c0de")), "{shown:?}");
}

#[test]
fn a_viewer_sees_where_the_sign_in_stands_and_nothing_to_press() {
    let cannot = "This screen cannot start the sign-in now (offline, or view-only); sign in to tracker from ATOMICS";
    for (state, headline, button) in [
        (SignInState::SignedOut, "tracker is not signed in", "Sign in to tracker"),
        (SignInState::Expired, "tracker needs you to sign in again", "Reconnect"),
    ] {
        for size in [WIDE, PHONE] {
            let (ctx, mut world, mut client, mut view) = viewing(fake_host::tickets(state));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(has(&shown, headline) && has(&shown, cannot), "{size:?}: {shown:?}");
            // Neither the banner's button nor the top bar's.
            assert!(!shown.iter().any(|(text, _)| text == button), "{size:?}: no {button}: {shown:?}");
            assert!(!has(&shown, "Workspace") && !has(&shown, "No workspace"), "nor a workspace to open: {shown:?}");
        }
    }
    // The phone's menu has no workspace for it either.
    let (ctx, mut world, mut client, mut view) = viewing(fake_host::tickets(SignInState::SignedIn));
    let menu = open_more(&ctx, &mut world, &mut client, &mut view);
    assert!(!has(&menu, "Workspace") && !has(&menu, "No workspace"), "{menu:?}");

    let (ctx, mut world, mut client, mut view) = viewing(fake_host::tickets(SignInState::SigningIn));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Waiting for the sign-in") && has(&shown, "https://auth.example.com/"), "{shown:?}");
    for action in ["Cancel", "Use it", "Then paste", "Signing in"] {
        assert!(!has(&shown, action), "{action} is not a viewer's: {shown:?}");
    }
}

/// The middle of the text that is `label` under the top bar: the page's button, not the bar's.
fn in_page(shown: &[(String, Rect)], label: &str) -> egui::Pos2 {
    let found = shown.iter().find(|(text, rect)| text == label && rect.top() > NOTE_TOP);
    found.unwrap_or_else(|| panic!("{label} is not drawn under the top bar: {shown:?}")).1.center()
}

/// Whether the top bar has a text that is `label`.
fn in_bar(shown: &[(String, Rect)], label: &str) -> bool {
    shown.iter().any(|(text, rect)| text == label && rect.bottom() < NOTE_TOP)
}

#[test]
fn the_top_bars_reconnect_signs_in_and_opens_the_board_where_the_steps_show() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::Expired));
    // With the right panel closed, only the top bar says so.
    client.send(Command::CloseDiff);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!view.desk.diff_open && !has(&shown, "PLAN") && in_bar(&shown, "Reconnect"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(exactly(&shown, "Reconnect")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.host_commands, [HostAction::SignIn]);
    assert!(view.desk.diff_open && view.desk.panel == PanelMode::Plan, "the board opens");
    assert!(has(&shown, "Waiting for the sign-in") && has(&shown, "Then paste the address"), "{shown:?}");
    // While it waits, the item only shows the way back to the steps.
    assert!(in_bar(&shown, "Signing in…") && !in_bar(&shown, "Reconnect"), "{shown:?}");
    client.send(Command::CloseDiff);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(exactly(&shown, "Signing in…")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.host_commands, [HostAction::SignIn], "nothing more is sent");
    assert!(view.desk.diff_open && has(&shown, "Waiting for the sign-in"), "{shown:?}");
}

#[test]
fn with_no_open_plan_the_sign_in_shows_by_the_agents_and_on_the_phones_board() {
    let no_plan = || {
        let mut host = fake_host::tickets(SignInState::SignedOut);
        host.board.plans.clear();
        host
    };
    // A wide window: above AGENTS in the sidebar, which has room beside the docked chief, and in the top bar.
    let (ctx, mut world, mut client, mut view) = live_with(no_plan());
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let (banner, agents) = (on(&shown, "tracker is not signed in"), on(&shown, "AGENTS"));
    assert!(banner.y < agents.y && (banner.x - agents.x).abs() < 200.0, "above the agents: {shown:?}");
    assert!(in_bar(&shown, "Sign in to tracker") && !has(&shown, "PLAN"), "{shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, BOARD, click(in_page(&shown, "Sign in to tracker")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert_eq!(world.host.host_commands, [HostAction::SignIn]);
    let steps = on(&shown, "Waiting for the sign-in");
    assert!(steps.y < on(&shown, "AGENTS").y && has(&shown, "Then paste the address"), "{shown:?}");

    // A phone: in its Agents pane and on its Board pane.
    for (tab, under) in [(Tab::Agents, "AGENTS"), (Tab::Board, "No plans on this computer yet.")] {
        let (ctx, mut world, mut client, mut view) = live_with(no_plan());
        view.tab = tab;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
        assert!(on(&shown, "tracker is not signed in").y < on(&shown, under).y, "{tab:?}: {shown:?}");
        drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(exactly(&shown, "Sign in to tracker")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
        assert_eq!(world.host.host_commands, [HostAction::SignIn], "{tab:?}");
        assert!(has(&shown, "Waiting for the sign-in") && has(&shown, "Use it"), "{tab:?}: {shown:?}");
    }
}

/// A wide window with the chief docked and no open plan, so no sidebar and no board, signed out of `tracker`.
fn no_board_nor_sidebar() -> (egui::Context, World, Client, View) {
    let mut host = fake_host::tickets(SignInState::SignedOut);
    host.board.plans.clear();
    live_with(host)
}

/// A desktop-sized bar too narrow for its items, which fold into Actions.
const NARROW: (f32, f32) = (720.0, 900.0);

/// A click on the top bar's sign-in item, wide or folded into Actions: the frames after it.
fn sign_in_from_the_bar(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
) -> Vec<(String, Rect)> {
    let shown = settled(ctx, world, client, view, size);
    let docked = view.desk.chief.open && view.desk.chief.dock == atomics_core::config::ChiefDock::Docked;
    assert!(docked && !has(&shown, "AGENTS") && !has(&shown, "PLAN"), "no sidebar, no board: {shown:?}");
    assert!(!has(&shown, "tracker is not signed in"), "{shown:?}");
    if size == NARROW {
        assert!(in_bar(&shown, "Actions") && !in_bar(&shown, "Sign in to tracker"), "{shown:?}");
        click_on(ctx, world, client, view, size, "Actions");
        click_on(ctx, world, client, view, size, "Sign in to tracker")
    } else {
        drawn(ctx, world, client, view, size, click(exactly(&shown, "Sign in to tracker")));
        settled(ctx, world, client, view, size)
    }
}

#[test]
fn the_top_bars_sign_in_shows_its_steps_with_no_board_or_sidebar_on_screen() {
    for size in [WIDE, NARROW] {
        let (ctx, mut world, mut client, mut view) = no_board_nor_sidebar();
        let shown = sign_in_from_the_bar(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(world.host.host_commands, [HostAction::SignIn], "{size:?}");
        // The host says it waits on its link: the item's popup, or its submenu in Actions, holds the steps.
        assert!(has(&shown, "Signing in…"), "{size:?}: {shown:?}");
        for wanted in [
            "Waiting for the sign-in",
            "https://auth.example.com/",
            "Open link",
            "Copy link",
            "Then paste the address",
            "http://127.0.0.1:…",
            "Use it",
            "Cancel",
        ] {
            assert!(has(&shown, wanted), "{size:?}: {wanted} is missing: {shown:?}");
        }
    }
}

#[test]
fn the_top_bars_sign_in_takes_the_address_in_its_popup() {
    for size in [WIDE, NARROW] {
        let (ctx, mut world, mut client, mut view) = no_board_nor_sidebar();
        let shown = sign_in_from_the_bar(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "http://127.0.0.1:…")));
        let shown = drawn(&ctx, &mut world, &mut client, &mut view, size, Vec::new());
        assert!(has(&shown, "Use it"), "{size:?}: a click into the box keeps the popup open: {shown:?}");
        sized(&ctx, &mut world, &mut client, &mut view, size, vec![egui::Event::Text(ENDED_ON.into())]);
        assert_eq!(client.sign_in_paste, ENDED_ON, "{size:?}");
        let shown = click_on(&ctx, &mut world, &mut client, &mut view, size, "Use it");
        let sent = [HostAction::SignIn, HostAction::SignInPaste { text: ENDED_ON.into() }];
        assert_eq!(world.host.host_commands, sent, "{size:?}");
        assert!(!has(&shown, "Signing in") && !has(&shown, "Waiting for the sign-in"), "signed in: {shown:?}");
        assert!(world.host.terminals["s1"].typed.is_empty() && client.sign_in_paste.is_empty());
    }
}

#[test]
fn the_workspace_item_names_the_hosts_workspace_and_opens_another() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::SignedIn));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(in_bar(&shown, "Workspace: team") && !has(&shown, "Project"), "{shown:?}");
    assert!(!has(&shown, "Sign in to") && !has(&shown, "Signing in") && !has(&shown, "tracker"), "{shown:?}");
    let opened = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Workspace: team");
    for wanted in ["team is open", "Open workspace", "daily"] {
        assert!(has(&opened, wanted), "{wanted} is missing: {opened:?}");
    }
    for absent in ["Open project", "Recent projects", "Save workspace"] {
        assert!(!has(&opened, absent), "the host's folders stay its own: {opened:?}");
    }
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "daily");
    assert_eq!(world.host.host_commands, [HostAction::OpenWorkspace { name: "daily".into() }]);
    assert!(in_bar(&shown, "Workspace: daily"), "the host opened it: {shown:?}");

    // In a narrow window the bar's items fold into Actions, the workspace among them as a submenu.
    let narrow = (720.0, 900.0);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, narrow);
    assert!(!view.compact && in_bar(&shown, "Actions") && !has(&shown, "Workspace: daily"), "{shown:?}");
    let menu = click_on(&ctx, &mut world, &mut client, &mut view, narrow, "Actions");
    let submenu = click_on(&ctx, &mut world, &mut client, &mut view, narrow, "Workspace: daily");
    assert!(has(&menu, "Workspace: daily") && has(&submenu, "daily is open"), "{submenu:?}");
    click_on(&ctx, &mut world, &mut client, &mut view, narrow, "team");
    assert_eq!(world.host.host_commands.last(), Some(&HostAction::OpenWorkspace { name: "team".into() }));
    let menu = click_on(&ctx, &mut world, &mut client, &mut view, narrow, "Actions");
    assert!(has(&menu, "Workspace: team"), "{menu:?}");
}

#[test]
fn a_phones_menu_has_the_hosts_workspace() {
    let (ctx, mut world, mut client, mut view) = live();
    let menu = open_more(&ctx, &mut world, &mut client, &mut view);
    assert!(has(&menu, "Machine") && !has(&menu, "Workspace") && !has(&menu, "No workspace"), "none saved: {menu:?}");

    let (ctx, mut world, mut client, mut view) = live_with(fake_host::tickets(SignInState::SignedIn));
    let menu = open_more(&ctx, &mut world, &mut client, &mut view);
    assert!(view.compact && has(&menu, "Machine") && has(&menu, "Workspace: team"), "{menu:?}");
    let submenu = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "Workspace: team");
    assert!(has(&submenu, "team is open") && has(&submenu, "daily"), "{submenu:?}");
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, PHONE, "daily");
    assert_eq!(world.host.host_commands, [HostAction::OpenWorkspace { name: "daily".into() }]);
    assert!(!has(&shown, "Machine"), "the menu closes: {shown:?}");
    let menu = open_more(&ctx, &mut world, &mut client, &mut view);
    assert!(has(&menu, "Workspace: daily"), "{menu:?}");
}

#[test]
fn a_host_that_does_not_offer_its_own_state_leaves_the_page_as_it_was() {
    let mut host = fake_host::tickets(SignInState::SignedOut);
    host.offers_host = false;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let Message::Hello { capabilities, .. } = &world.sent()[0] else { panic!("hello first") };
    assert!(capabilities.iter().any(|c| c == "host"), "the browser asks for it: {capabilities:?}");
    let welcome = world.driver.remote.welcome().expect("welcomed");
    assert!(!welcome.capabilities.iter().any(|c| c == "host") && !welcome.may.contains(&Grant::Host), "{welcome:?}");
    // The host's own state changes, and the browser is sent `skipped` in its place.
    let mut moved = world.host.board.host.clone();
    moved.workspace = Some("daily".into());
    let frames = world.host.publish_host(moved);
    world.deliver(frames);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let mirror = world.driver.remote.with_mirror(|m| (m.host.clone(), m.cursor));
    assert_eq!(mirror, (HostInfo::default(), world.host.head()), "the cursor moves on, and nothing else");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::Resync { .. })), "no gap");
    for absent in ["tracker", "Sign in", "Reconnect", "Project", "Workspace", "No workspace", "USAGE", "load "] {
        assert!(!has(&shown, absent), "{absent} shows: {shown:?}");
    }
    assert!(has(&shown, "PLAN") && has(&shown, "Approve plan"), "the rest as before: {shown:?}");
}

#[test]
fn shared_settings_remote_and_host_folder_picker_work_in_both_layouts() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        settled(&ctx, &mut world, &mut client, &mut view, size);
        section(&ctx, &mut world, &mut client, &mut view, size, "Theme");
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Advanced");
        assert!(has(&shown, "Effective configuration") && has(&shown, "layout.min_tile_width"), "{size:?}: {shown:?}");
        section(&ctx, &mut world, &mut client, &mut view, size, "Advanced");
        section(&ctx, &mut world, &mut client, &mut view, size, "Theme");
        client.management.settings = false;
        client.management.remote = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Pair a device")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "https://example.com/pair#single-use"), "{size:?}: {shown:?}");
        assert!(
            world.sent().iter().any(|m| matches!(m, Message::HostCommand(c) if c.action == HostAction::PairDevice))
        );
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Cancel pairing")));
        client.management.remote = false;
        client.send(Command::PickProject);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Choose a folder on the host."), "{size:?}: {shown:?}");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "▸ repo")));
        view.desk.active = Some("s1".into());
        view.desk.diff_root = Some(PathBuf::from(WORKTREE));
        view.desk.diff_open = false;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Open this project or worktree")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!client.management.project);
        assert!(world.sent().iter().any(|m| matches!(m, Message::HostCommand(c) if c.action == (HostAction::OpenProject { path: "/home/user/repo".into() }))));
        assert!(world.driver.remote.repos().contains(&PathBuf::from("/home/user/repo")));
        assert!(view.desk.active.is_none(), "opening a project navigates in this browser");
        assert_eq!(view.desk.diff_root.as_deref(), Some(Path::new("/home/user/repo")));
        assert!(view.desk.diff_open);
        if view.compact {
            assert_eq!(view.diff.as_deref(), Some(Path::new("/home/user/repo")));
        }
    }
}

#[test]
fn a_client_shows_the_hosts_version_and_a_new_one_and_cannot_start_an_update() {
    use atomics_core::projection::UpdateInfo;
    let update = |latest: Option<&str>| UpdateInfo {
        current: "0.1.6".into(),
        latest: latest.map(str::to_string),
        notes: latest.map(|_| "https://example.com/notes".into()),
        ..UpdateInfo::default()
    };
    for size in [WIDE, PHONE] {
        // With nothing new it still says which version the host runs, and the bar says nothing.
        let mut host = fake_host::management();
        host.board.host.update = Some(update(None));
        let (ctx, mut world, mut client, mut view) = live_with(host);
        assert!(!world.driver.remote.available(&Command::Update), "atomics.v1 has no operation for it");
        assert!(!world.driver.remote.available(&Command::CancelUpdate));
        let button = |shown: &[(String, Rect)]| shown.iter().any(|(text, _)| text == "Update");
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!button(&shown), "{size:?}: {shown:?}");
        client.management.settings = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(shown.iter().any(|(text, _)| text.starts_with("Updates")), "{size:?}: {shown:?}");
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Updates");
        assert!(has(&shown, "The host runs: 0.1.6"), "{size:?}: {shown:?}");
        client.management.settings = false;

        // A new version: a hint, no button, here or in Settings.
        let mut host = fake_host::management();
        host.board.host.update = Some(update(Some("9.0.0")));
        let (ctx, mut world, mut client, mut view) = live_with(host);
        assert!(!atomics_ui::update::offered(&world.driver.remote), "a hint has no dot on Actions");
        if size == WIDE {
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(has(&shown, "9.0.0 available"), "{shown:?}");
            assert!(!button(&shown), "{shown:?}");
        }
        client.management.settings = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        for line in ["The host runs: 0.1.6", "New version: 9.0.0", "Release notes"] {
            assert!(has(&shown, line), "{size:?}: {line}: {shown:?}");
        }
        assert!(!button(&shown), "{size:?}: {shown:?}");

        // The countdown says what is about to happen to the connection, and has nothing to press.
        let mut host = fake_host::management();
        host.board.host.update = Some(UpdateInfo {
            phase: atomics_core::projection::UpdatePhase::RestartsIn { seconds: 7 },
            ..update(Some("9.0.0"))
        });
        let (ctx, mut world, mut client, mut view) = live_with(host);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "The host restarts in 7 s to update"), "{size:?}: {shown:?}");
        assert!(!has(&shown, "Cancel"), "{size:?}: {shown:?}");
    }
}

#[test]
fn view_only_clients_read_settings_and_devices_but_cannot_change_them() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = viewing(fake_host::management());
        client.management.settings = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "midnight")));
        assert!(has(&shown, "View only: settings changes"), "{size:?}: {shown:?}");
        // Theme opens by itself and takes most of a phone's height.
        section(&ctx, &mut world, &mut client, &mut view, size, "Theme");
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Advanced");
        assert!(has(&shown, "Effective configuration"), "{size:?}: {shown:?}");
        section(&ctx, &mut world, &mut client, &mut view, size, "Advanced");
        for opened in ["Agents", "Connections", "Credentials"] {
            let shown = section(&ctx, &mut world, &mut client, &mut view, size, opened);
            assert!(!shown.is_empty());
            section(&ctx, &mut world, &mut client, &mut view, size, opened);
        }
        assert!(
            world.sent().iter().all(|m| !matches!(m, Message::HostCommand(_))),
            "a viewer picks no theme and asks the host for nothing: {:?}",
            world.sent()
        );
        client.management.settings = false;
        client.management.remote = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "View only: pairing") && has(&shown, "Paired devices"), "{size:?}: {shown:?}");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Pair a device")));
        assert!(
            world.sent().iter().all(|m| !matches!(m, Message::HostCommand(_))),
            "a disabled action must send nothing"
        );
        assert!(!world.driver.remote.available(&Command::PickProject));
    }
}

/// The rectangle of the Settings section whose row starts with `label`. The rows come after the window's title: a
/// phone's Agents tab has the same name as its section, drawn behind it.
fn row(shown: &[(String, Rect)], label: &str) -> Rect {
    let title = shown.iter().rposition(|(text, _)| text == "Settings").expect("the window's title is drawn");
    let found = shown[title..].iter().find(|(text, _)| text.starts_with(label));
    found.unwrap_or_else(|| panic!("{label} is not drawn: {shown:?}")).1
}

/// Opens or closes the section of the Settings window whose row starts with `label`; the text once it has moved.
fn section(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    size: (f32, f32),
    label: &str,
) -> Vec<(String, Rect)> {
    let shown = settled(ctx, world, client, view, size);
    drawn(ctx, world, client, view, size, click(row(&shown, label).center()));
    for _ in 0..8 {
        drawn(ctx, world, client, view, size, Vec::new());
    }
    drawn(ctx, world, client, view, size, Vec::new())
}

/// The settings tools sent so far.
fn tools_sent(world: &World) -> Vec<atomics_core::ops::SettingsTool> {
    world
        .host
        .host_commands
        .iter()
        .filter_map(|action| match action {
            HostAction::SettingsTool { tool } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn settings_lead_with_what_people_change_and_each_section_shows_its_content_as_it_opens() {
    use atomics_core::ops::SettingsTool;
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let mut last = 0.0;
        for label in ["Theme", "Shortcuts", "Agents", "Connections", "Remote devices", "Credentials", "Advanced"] {
            let top = row(&shown, label).top();
            assert!(top > last, "{label} comes in order on {size:?}: {shown:?}");
            last = top;
        }
        assert!(has(&shown, "midnight") && has(&shown, "yours"), "the swatches: {shown:?}");
        assert!(has(&shown, "Only this host (config.d/host.toml)"), "a remote client's words: {shown:?}");
        assert!(!has(&shown, "Effective configuration"), "under Advanced: {shown:?}");
        assert!(tools_sent(&world).is_empty(), "nothing is read before it is opened");

        for (label, expected, content) in [
            ("Agents", vec![SettingsTool::CheckSandbox, SettingsTool::CheckPrograms], "Docker Sandboxes"),
            ("Connections", vec![SettingsTool::RefreshConnections], "linear"),
            ("Credentials", vec![SettingsTool::RefreshCredentials], "Claude subscription"),
            ("Remote devices", vec![], "Paired devices"),
            ("Advanced", vec![], "Effective configuration"),
        ] {
            let before = tools_sent(&world).len();
            let shown = section(&ctx, &mut world, &mut client, &mut view, size, label);
            assert!(has(&shown, content), "{label} shows {content} on {size:?}: {shown:?}");
            settled(&ctx, &mut world, &mut client, &mut view, size);
            let sent = tools_sent(&world);
            assert_eq!(&sent[before..], expected.as_slice(), "{label} reads once, by itself, on {size:?}");
            section(&ctx, &mut world, &mut client, &mut view, size, label);
        }

        // Closing Settings forgets what it read; opening it reads again.
        drawn(&ctx, &mut world, &mut client, &mut view, size, press(egui::Key::Escape, egui::Modifiers::NONE));
        assert!(!client.management.settings, "Escape closes it");
        client.management.settings = true;
        section(&ctx, &mut world, &mut client, &mut view, size, "Credentials");
        let reads = tools_sent(&world).iter().filter(|tool| **tool == SettingsTool::RefreshCredentials).count();
        assert_eq!(reads, 2, "{size:?}");
    }
}

#[test]
fn the_shortcut_list_and_edit_config_say_what_a_remote_client_gets() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Shortcuts");
        assert!(has(&shown, "The desktop app on the host also has these"), "{size:?}: {shown:?}");
        assert!(shown.iter().any(|(text, _)| text.ends_with("previous or next changed file")), "{size:?}: {shown:?}");
        assert!(shown.iter().any(|(text, _)| text.ends_with("open a file")), "{size:?}: {shown:?}");
        assert!(shown.iter().any(|(text, _)| text.ends_with("that agent, with its diff")), "{size:?}: {shown:?}");
        section(&ctx, &mut world, &mut client, &mut view, size, "Shortcuts");
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Advanced");
        assert!(has(&shown, "Edit config opens the file in an editor on the host"), "{size:?}: {shown:?}");
        assert!(!shown.iter().any(|(text, _)| text == "Edit config"), "no button for what it cannot do: {shown:?}");
        assert!(has(&shown, "main: ~/.config/atomics/config.toml"), "{size:?}: {shown:?}");
    }
}

#[test]
fn picking_a_theme_applies_at_once_and_only_the_users_own_choice_goes_to_the_host() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "midnight")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "dusk")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "claude")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        let sent: Vec<_> = world.host.host_commands.iter().collect();
        assert_eq!(
            sent,
            [
                &HostAction::SelectTheme { name: "midnight".into(), host: false },
                &HostAction::SelectTheme { name: "dusk".into(), host: false },
            ],
            "two picks, no proposal, and the theme in use is not picked again"
        );
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Only this host")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "midnight")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(
            world.host.host_commands.last(),
            Some(&HostAction::SelectTheme { name: "midnight".into(), host: true })
        );
    }
}

#[test]
fn a_host_whose_own_file_picks_a_theme_gets_the_pick_there_so_that_it_shows() {
    let mut host = fake_host::management();
    host.board.host.management.as_mut().unwrap().themes.host_picks = true;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.management.settings = true;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "config.d/host.toml picks"), "nothing to warn of yet: {shown:?}");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "midnight")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(world.host.host_commands.last(), Some(&HostAction::SelectTheme { name: "midnight".into(), host: true }));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Only this host")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "config.d/host.toml picks"), "unticked, it would not show: {shown:?}");
}

#[test]
fn a_proposal_from_the_chief_opens_above_settings_and_keeps_its_apply() {
    let proposal = atomics_core::projection::ConfigProposalInfo {
        id: "config-1".into(),
        file: "config.toml".into(),
        scope: "main".into(),
        from: "the chief".into(),
        reason: None,
        diff: "+[layout]".into(),
        risks: vec![],
        warnings: vec![],
        preview: None,
    };
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
    client.management.settings = true;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let mut host = world.host.board.host.clone();
    host.management.as_mut().unwrap().proposals = vec![proposal];
    let frames = world.host.publish_host(host);
    world.deliver(frames);
    // A click inside Settings raises it; the proposal stays above it all the same.
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Shortcuts")));
    view.now_ms += 1000.0;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let apply = exactly(&shown, "Apply");
    let layer = ctx.layer_id_at(apply).expect("a window is there");
    assert_eq!(layer.id, egui::Id::NULL.with("config-proposal"), "above Settings");
    assert_eq!(world.host.host_commands, [], "a proposal asks before it writes");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(apply));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(
        world
            .host
            .host_commands
            .iter()
            .any(|action| matches!(action, HostAction::ConfigDecide { id, apply: true, .. } if id == "config-1")),
        "{:?}",
        world.host.host_commands
    );
}

#[test]
fn only_what_cannot_be_undone_asks_first() {
    use atomics_core::ops::SettingsTool;
    // With every control of the section in view, without scrolling.
    for size in [WIDE, (390.0, 2400.0)] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Credentials");
        // The Azure switch can be undone, and goes at once, with no proposal.
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Enable Azure CLI access")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(tools_sent(&world).last(), Some(&SettingsTool::SetAzure { enabled: true }));

        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Remove…")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Remove Claude subscription? The stored value is gone"), "{size:?}: {shown:?}");
        let removing =
            |world: &World| tools_sent(world).iter().any(|tool| matches!(tool, SettingsTool::RemoveCredential { .. }));
        assert!(!removing(&world), "it asks first");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, "Keep")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(!removing(&world), "Keep keeps it");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Remove…")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, "Remove")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(
            tools_sent(&world).last(),
            Some(&SettingsTool::RemoveCredential { kind: "claude".into(), host: None }),
            "{size:?}"
        );

        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Remove unknown entries…")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(tools_sent(&world).last() != Some(&SettingsTool::CleanCredentials), "it asks first");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, "Remove them")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(tools_sent(&world).last(), Some(&SettingsTool::CleanCredentials));

        section(&ctx, &mut world, &mut client, &mut view, size, "Credentials");
        let shown = section(&ctx, &mut world, &mut client, &mut view, size, "Remote devices");
        assert!(has(&shown, "Paired 2026-11-05 00:00 UTC · Last seen 2026-11-05 12:00 UTC"), "device dates: {shown:?}");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Revoke…")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Revoke A phone?"), "{size:?}: {shown:?}");
        let revoking = |world: &World| {
            world.host.host_commands.iter().any(|action| matches!(action, HostAction::RevokeDevice { .. }))
        };
        assert!(!revoking(&world), "it asks first");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(exactly(&shown, "Revoke")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(revoking(&world));
    }
}

#[test]
fn a_typed_credential_stays_masked_replacing_asks_and_closing_settings_forgets_it() {
    use atomics_core::ops::SettingsTool;
    const SECRET: &str = "sk-typed-secret";
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
    client.management.settings = true;
    let shown = section(&ctx, &mut world, &mut client, &mut view, WIDE, "Credentials");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "New value (hidden)")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text(SECRET.into())]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.management.credential_value, SECRET);
    assert!(!shown.iter().any(|(text, _)| text.contains(SECRET)), "masked: {shown:?}");

    // Claude's entry is set, so saving over it asks; the value stays masked in its field meanwhile.
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Save, replacing the old value…")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Replace Claude subscription?"), "{shown:?}");
    assert!(!tools_sent(&world).iter().any(|tool| matches!(tool, SettingsTool::PutCredential { .. })));
    // The question's button comes last: the entry's own row has a Replace too.
    let answer = shown.iter().rev().find(|(text, _)| text == "Replace").unwrap().1.center();
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(answer));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(
        tools_sent(&world).last(),
        Some(&SettingsTool::PutCredential { kind: "claude".into(), value: SECRET.into(), host: None })
    );
    assert_eq!(client.management.credential_value, "", "it travelled once");

    // An unset entry saves at once, and closing Settings takes a value never saved with it.
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(exactly(&shown, "Add")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.management.credential_kind, "openai");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("typed-and-left".into())]);
    assert_eq!(client.management.credential_value, "typed-and-left", "Add gave the field the keyboard");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Escape, egui::Modifiers::NONE));
    assert!(!client.management.settings);
    assert_eq!(client.management.credential_value, "", "gone with the window");
}

#[test]
fn every_settings_section_fits_a_narrow_screen() {
    for size in [PHONE, (320.0, 640.0), (600.0, 500.0)] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        client.management.settings = true;
        let mut shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        for label in ["Theme", "Shortcuts", "Agents", "Connections", "Remote devices", "Credentials", "Advanced"] {
            if label != "Theme" {
                shown = section(&ctx, &mut world, &mut client, &mut view, size, label);
            }
            let window = shown.iter().rposition(|(text, _)| text == "Settings").expect("its title");
            for (text, rect) in shown[window..].iter().filter(|(text, _)| !text.is_empty()) {
                assert!(
                    rect.left() >= 0.0 && rect.right() <= size.0,
                    "{label} on {size:?}: {text} runs off the screen at {rect:?}"
                );
                let font = egui::FontId::proportional(13.0);
                assert!(
                    ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, &text.replace('\n', ""))),
                    "{label}: a browser has only egui's fonts, and they cannot draw {text}"
                );
            }
            section(&ctx, &mut world, &mut client, &mut view, size, label);
            shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        }
    }
}

#[test]
fn applied_host_themes_reach_both_client_layouts() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
        settled(&ctx, &mut world, &mut client, &mut view, size);
        let mut host = world.host.board.host.clone();
        host.management.as_mut().unwrap().config["theme_tokens"] =
            serde_json::to_value(atomics_core::theme::builtin("paper").unwrap()).unwrap();
        let frames = world.host.publish_host(host);
        world.deliver(frames);
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert_eq!(client.palette.mode, ThemeMode::Light, "{size:?}");
    }
}

#[test]
fn trace_lane_draws_and_navigates_in_the_desktop_frame_and_phone_chief() {
    let mut host = fake_host::host();
    host.trace = fake_host::trace_data();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    for size in [WIDE, PHONE] {
        if size == PHONE {
            // A phone's chief opens on its terminal, the calls folded under it until Show.
            view.tab = Tab::Chief;
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(client.lane.collapsed && has(&shown, "TRACE") && !has(&shown, "search"), "{shown:?}");
            click_on(&ctx, &mut world, &mut client, &mut view, size, "Show");
        }
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let text = shown.iter().map(|(text, _)| text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("TRACE") && text.contains("2 calls") && text.contains("search") && text.contains("open ↗"),
            "{text}"
        );
        assert!(!text.contains("hidden-secret"));
        ctx.memory_mut(|m| m.request_focus(atomics_ui::trace_lane::focus_id()));
        let key =
            |key, modifiers| egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers };
        drawn(&ctx, &mut world, &mut client, &mut view, size, vec![key(egui::Key::ArrowDown, egui::Modifiers::SHIFT)]);
        assert_eq!(client.lane.selected, Some(1));
        drawn(&ctx, &mut world, &mut client, &mut view, size, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert!(client.lane.expanded.contains(&1));
        drawn(&ctx, &mut world, &mut client, &mut view, size, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert!(!ctx.memory(|m| m.has_focus(atomics_ui::trace_lane::focus_id())));
        client.lane.selected = None;
        client.lane.expanded.clear();
    }
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    let open = on(&shown, "open ↗");
    drawn(&ctx, &mut world, &mut client, &mut view, PHONE, click(open));
    let preview = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(preview.iter().any(|(text, _)| text == "Example ticket"));
    let messages = world.host.publish_trace(Default::default());
    world.deliver(messages);
    assert!(world.driver.remote.trace().entries.is_empty());
    let text = sized(&ctx, &mut world, &mut client, &mut view, PHONE, Vec::new());
    assert!(!text.iter().any(|t| t.starts_with("TRACE")));
}

#[test]
fn usage_and_load_reach_the_wide_frame_and_the_phones_overflow_and_update_live() {
    for size in [BOARD, PHONE] {
        let (ctx, mut world, mut client, mut view) = live();
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let shown = if size == PHONE {
            assert!(!has(&shown, "USAGE") && !has(&shown, "load "), "under overflow: {shown:?}");
            open_more(&ctx, &mut world, &mut client, &mut view);
            // The menu's scrolling part has its size from the next frame on.
            settled(&ctx, &mut world, &mut client, &mut view, size)
        } else {
            shown
        };
        for label in [
            "USAGE",
            "Claude",
            "Codex",
            "Session  76%",
            "Session  9%",
            "Weekly  24%",
            "Weekly  100%",
            "2m ago",
            "load 4.2",
        ] {
            assert!(has(&shown, label), "{size:?}: {label} missing: {shown:?}");
        }
        if size == PHONE {
            for label in [
                "Load 4.2 on 8 cores",
                "Compilers of all agents share 6 jobs",
                "test slots: 1/2 in use, 3 waiting",
                "qa slots: 0/1 in use, 1 waiting",
            ] {
                assert!(shown.iter().any(|(text, _)| text.contains(label)), "{label} missing: {shown:?}");
            }
        }
        let mut host = world.host.board.host.clone();
        let resources = host.resources.as_mut().unwrap();
        resources.load = Some(7.3);
        resources.claude.session.as_mut().unwrap().remaining = 53.0;
        resources.test.waiting = 0;
        resources.sampled_at += 120;
        let frames = world.host.publish_host(host);
        world.deliver(frames);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Session  53%") && has(&shown, "4m ago") && has(&shown, "load 7.3"), "{size:?}: {shown:?}");
        assert!(world.host.host_commands.is_empty(), "read-only displays");
    }
}

#[test]
fn an_older_host_without_resources_draws_neither_usage_nor_load() {
    let mut host = fake_host::host();
    host.board.host.resources = None;
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(!has(&shown, "USAGE") && !has(&shown, "load "));
    let menu = open_more(&ctx, &mut world, &mut client, &mut view);
    assert!(!has(&menu, "USAGE") && !has(&menu, "load "));
}

#[test]
fn host_resources_explorer_and_trace_share_the_wide_frame() {
    let mut host = fake_host::explorer();
    host.trace = fake_host::trace_data();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    for label in ["USAGE", "Session  76%", "load 4.2", "TRACE", "2 calls", "EXPLORER", "README.md", "All ·", "Repos"] {
        assert!(has(&shown, label), "{label} missing from the shared frame: {shown:?}");
    }
}

#[test]
fn host_panels_draw_in_both_layouts_and_editor_input_uses_its_own_terminal() {
    for size in [BOARD, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::panels_host());
        settled(&ctx, &mut world, &mut client, &mut view, size);
        client.send(Command::Edit { root: WORKTREE.into(), path: Some("src/a.rs".into()), line: None });
        settled(&ctx, &mut world, &mut client, &mut view, size);
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "EDITOR"), "{size:?}: {shown:?}");
        if !view.compact {
            for label in ["USAGE", "Session  76%", "load 4.2", "TRACE"] {
                assert!(has(&shown, label), "{label} must coexist with the editor: {shown:?}");
            }
        }
        let editor = atomics_core::projection::editor_terminal(WORKTREE);
        assert!(world.host.terminals[&editor].followed);
        assert_eq!(view.term.session(), Some(editor.as_str()));
        // Type only after a focus gesture and the lease, as the browser overlay does.
        view.term.inputs.push(term_pane::TermInput::Focus(true));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(view.term.typing());
        view.term.inputs.push(term_pane::TermInput::Text("editor-only".into()));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(world.host.terminals[&editor].typed.contains("editor-only"));
        assert!(!world.host.terminals["s1"].typed.contains("editor-only"));
        client.send(Command::SetPanel(PanelMode::Diff));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        for key in ["pull:example/repo#7", "pull:dev.azure.com/org/project/repo#8"] {
            client.pr_target = None;
            client.pr_view = Some((key.into(), Default::default()));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(
                has(&shown, "Review the parser") || shown.iter().any(|(s, _)| s.contains("Review the parser")),
                "{size:?}: {shown:?}"
            );
            assert!(client.pr_view.as_ref().unwrap().1.prepared.is_some());
            assert!(!has(&shown, "Send review") && !has(&shown, "Edit"), "a PR is read-only: {shown:?}");
        }
        client.pr_view = None;
        if view.compact {
            // A phone's chief opens with the calls folded, Show at hand.
            view.tab = Tab::Chief;
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(has(&shown, "TRACE") && !has(&shown, "example: lookup"), "{size:?}: {shown:?}");
            click_on(&ctx, &mut world, &mut client, &mut view, size, "Show");
        }
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "TRACE") && has(&shown, "example: lookup"), "{size:?}: {shown:?}");
        if !view.compact {
            client.send(Command::SetChiefDock(atomics_core::config::ChiefDock::Floating));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(has(&shown, "TRACE") && has(&shown, "Dock"), "the floating chief: {shown:?}");
        }
    }
}

#[test]
fn settings_tickets_resources_trace_explorer_and_panels_share_one_host() {
    let mut host = fake_host::panels_host();
    let explorer = fake_host::explorer();
    host.board.host.projects = explorer.board.host.projects;
    host.listings = explorer.listings;
    host.contents = explorer.contents;
    host.board.host.management = fake_host::management().board.host.management;
    host.board.host.tickets = fake_host::ticket_reader().board.host.tickets;
    host.board.plans[0].ticket_tree = Some(atomics_core::ticket_ui::TicketTree {
        plan: host.board.plans[0].id.clone(),
        ticket: Some(fake_host::ticket_card("ENG-40").link),
        tasks: vec![fake_host::ticket_card("ENG-41").link],
        unassigned: vec![fake_host::ticket_card("ENG-123").link],
    });
    host.trace = fake_host::trace_data();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    for label in
        ["Settings", "Remote", "Project", "USAGE", "load 4.2", "TRACE", "EXPLORER", "README.md", "TICKETS", "Tickets"]
    {
        assert!(has(&shown, label), "{label} missing from the combined frame: {shown:?}");
    }
    client.management.settings = true;
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let shown = section(&ctx, &mut world, &mut client, &mut view, BOARD, "Advanced");
    assert!(has(&shown, "Effective configuration"));
    client.management.settings = false;
    client.send(Command::OpenTicket(fake_host::ticket_card("ENG-123").link));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    for label in ["Open in browser", "Plan with chief", "USAGE", "TRACE", "EXPLORER", "TICKETS"] {
        assert!(has(&shown, label), "{label} remains available with the ticket reader: {shown:?}");
    }
    client.send(Command::Edit { root: WORKTREE.into(), path: Some("src/a.rs".into()), line: None });
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(has(&shown, "EDITOR"), "the editor remains available: {shown:?}");
    client.send(Command::SetPanel(PanelMode::Diff));
    client.pr_view = Some(("pull:example/repo#7".into(), Default::default()));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert!(
        shown.iter().any(|(text, _)| text.contains("Review the parser")),
        "the PR panel remains available: {shown:?}"
    );
}

#[test]
fn tickets_sidebar_and_panel_search_read_page_link_and_plan_on_wide_and_phone() {
    use atomics_core::ticket_ui::TicketAction;
    let button = |shown: &[(String, Rect)], label: &str| {
        shown
            .iter()
            .rev()
            .find(|(text, _)| text == label)
            .unwrap_or_else(|| panic!("missing {label}: {shown:?}"))
            .1
            .center()
    };
    for size in [BOARD, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
        view.desk.panel = PanelMode::Tickets;
        view.desk.diff_open = true;
        view.tab = Tab::Diffs;
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Tickets"), "{size:?}: {shown:?}");
        if size == BOARD {
            assert!(has(&shown, "TICKETS"), "{shown:?}");
        }
        assert!(has(&shown, "ENG-123"), "the default view lists on {size:?}: {shown:?}");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "More")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "ENG-124"), "{shown:?}");
        client.tickets.query = "parser failure".into();
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(button(&shown, "Search")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(world.host.host_commands.iter().any(|command| matches!(command,
            HostAction::Tickets { ticket, .. } if matches!(ticket.as_ref(), TicketAction::Search { query, .. } if query == "parser failure"))));
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "ENG-123")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        for label in
            ["Open in browser", "Plan with chief", "Link to plan", "Description", "Comments", "Children", "Related"]
        {
            assert!(has(&shown, label), "{size:?}: missing {label}: {shown:?}");
        }
        assert_eq!(client.tickets.read.as_ref().unwrap().link.label(), "ENG-123");
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Link to plan")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(button(&shown, "Parse faster")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(world.host.host_commands.iter().any(|command| matches!(command,
            HostAction::Tickets { ticket, .. } if matches!(ticket.as_ref(), TicketAction::Link { plan, ticket, .. } if plan == "p1" && ticket.id == "ENG-123"))));
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Plan with chief")));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        client.tickets.extra = "Keep the public API stable".into();
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Plan with chief ⏎")));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(world.host.host_commands.iter().any(|command| matches!(command,
            HostAction::Tickets { ticket, .. } if matches!(ticket.as_ref(), TicketAction::Plan { extra, mode: atomics_core::tickets::TicketMode::Board, .. }
                if extra == "Keep the public API stable"))), "{:?}", world.host.host_commands);
        assert!(!client.tickets.dialog);
        if size == PHONE {
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            drawn(&ctx, &mut world, &mut client, &mut view, size, click(button(&shown, "×")));
            let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
            assert!(!has(&shown, "Open in browser"), "the phone can close the reader: {shown:?}");
        }
    }
}

#[test]
fn a_second_ticket_client_keeps_its_query_read_and_history_and_older_hosts_keep_their_frame() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    view.desk.panel = PanelMode::Tickets;
    view.desk.diff_open = true;
    client.tickets.query = "my own query".into();
    client.send(Command::OpenTicket(fake_host::ticket_card("ENG-123").link));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Fix ticket ENG-123"));
    let mut second = Client::default();
    second.send(Command::OpenTicket(fake_host::ticket_card("ENG-124").link));
    let mut second_view = View::default();
    settled(&ctx, &mut world, &mut second, &mut second_view, WIDE);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.tickets.query, "my own query");
    assert_eq!(client.tickets.read.as_ref().unwrap().link.label(), "ENG-123");
    assert_eq!(second.tickets.read.as_ref().unwrap().link.label(), "ENG-124");
    assert!(client.tickets.crumbs.is_empty());
    let (ctx, mut world, mut client, mut view) = live();
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!has(&shown, "TICKETS") && !has(&shown, "Tickets"));
}

#[test]
fn a_late_ticket_read_cannot_reopen_find_after_the_client_goes_back() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    client.send(Command::OpenTicket(fake_host::ticket_card("ENG-123").link));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![]);
    assert!(client.tickets.read.is_some());
    // The host has answered, but the client has not consumed its replies yet.
    client.tickets.show_find(false);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.tickets.read.is_none());
    assert!(has(&shown, "Search"));
}

fn press(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
    vec![event(true), event(false)]
}

fn text(text: &str) -> Vec<egui::Event> {
    vec![egui::Event::Text(text.into())]
}

fn query_focused(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(egui::Id::NULL.with("tickets-query")))
}

#[test]
fn opening_tickets_with_a_keyboard_takes_it_to_the_search_field_and_its_arrows_stay_there() {
    use egui::{Key, Modifiers};
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    client.send(Command::ToggleView(PanelMode::Tickets));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(query_focused(&ctx), "the panel opens with the keyboard in its search field");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, text("ab"));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::ArrowLeft, Modifiers::NONE));
    assert!(query_focused(&ctx), "← stays in the field");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, text("X"));
    assert_eq!(client.tickets.query, "aXb");

    // A phone's keyboard stays down until its field is tapped.
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    client.send(Command::ToggleView(PanelMode::Tickets));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(view.desk.panel == PanelMode::Tickets && view.desk.diff_open);
    assert!(!query_focused(&ctx));
}

#[test]
fn the_ticket_and_its_plan_dialog_take_their_own_keys() {
    use atomics_core::ticket_ui::TicketAction;
    use egui::{Key, Modifiers};
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    view.desk.panel = PanelMode::Tickets;
    view.desk.diff_open = true;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "ENG-123")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.tickets.read.as_ref().unwrap().link.label(), "ENG-123");
    assert!(ctx.memory(|m| m.has_focus(atomics_ui::panel_nav::focus_id())), "an open ticket has the keys");

    // Escape in the dialog is the dialog's: the ticket stays, and has the keys again.
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Plan with chief")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let label = on(&shown, "Extra instructions");
    assert!(
        (label.x - WIDE.0 / 2.0).abs() < 240.0 && (label.y - WIDE.1 / 2.0).abs() < 120.0,
        "the dialog is in the middle: {label:?}"
    );
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, text("draft"));
    assert_eq!(client.tickets.extra, "draft", "the field has the keyboard");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(!client.tickets.dialog && client.tickets.read.is_some());
    assert!(ctx.memory(|m| m.has_focus(atomics_ui::panel_nav::focus_id())));

    // Opened again it is a new draft, and one Return plans once.
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Plan with chief")));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(client.tickets.extra, "");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, text("keep"));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Enter, Modifiers::NONE));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let plans = world.host.host_commands.iter().filter(|command| {
        matches!(command, HostAction::Tickets { ticket, .. }
            if matches!(ticket.as_ref(), TicketAction::Plan { extra, .. } if extra == "keep"))
    });
    assert_eq!(plans.count(), 1, "{:?}", world.host.host_commands);
    assert!(!client.tickets.dialog);
    assert!(client.tickets.read.is_some());

    // Escape in the ticket: back to the list, with it selected.
    client.tickets.selected = None;
    ctx.memory_mut(|m| m.request_focus(atomics_ui::panel_nav::focus_id()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(client.tickets.read.is_none());
    assert_eq!(client.tickets.selected, Some(0));
}

#[test]
fn escape_closes_the_window_on_top_and_a_menu_in_it_first() {
    use egui::{Key, Modifiers};
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
    client.management.settings = true;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.send(Command::PickProject);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.management.project && client.management.settings);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(!client.management.project, "the picker first");
    assert!(client.management.settings, "one window for each press");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(!client.management.settings);

    client.management.settings = true;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    egui::Popup::open_id(&ctx, egui::Id::NULL.with("a menu"));
    assert!(egui::Popup::is_any_open(&ctx));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(client.management.settings, "a menu inside the window takes Escape first");
}

#[test]
fn the_ticket_reader_draws_headings_link_status_the_update_and_keeps_off_the_edge() {
    for size in [WIDE, PHONE] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
        view.desk.panel = PanelMode::Tickets;
        view.desk.diff_open = true;
        settled(&ctx, &mut world, &mut client, &mut view, size);
        client.send(Command::OpenTicket(fake_host::ticket_card("ENG-123").link));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        let at = |label: &str| shown.iter().find(|(text, _)| text.starts_with(label)).map(|(_, rect)| *rect);
        let at = |label: &str| at(label).unwrap_or_else(|| panic!("{label} is not drawn on {size:?}: {shown:?}"));
        assert!(
            at("Description").height() > at("Read this ticket").height() * 1.3,
            "a heading stands out from its text on {size:?}: {shown:?}"
        );
        assert!(has(&shown, "Fix the parser · Merged · Succeeded"), "the link's status on {size:?}: {shown:?}");
        assert!(
            shown.iter().any(|(text, _)| text.contains("updated 2026-11-04T08:50:34Z")),
            "the last update on {size:?}: {shown:?}"
        );
        assert!(has(&shown, "[Todo]"), "the state on {size:?}: {shown:?}");
        assert!(at("Description").left() >= at("Fix ticket ENG-123").left() + 8.0, "sections are set in: {shown:?}");
        if size == PHONE {
            assert!(at("Fix ticket ENG-123").left() >= 10.0, "off the screen's edge: {shown:?}");
        }
    }
}

#[test]
fn the_ticket_search_asks_for_a_query_before_any_search() {
    for size in [WIDE, PHONE] {
        let mut host = fake_host::ticket_reader();
        host.board.host.tickets.as_mut().unwrap().views.clear();
        let (ctx, mut world, mut client, mut view) = live_with(host);
        client.send(Command::ToggleView(PanelMode::Tickets));
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, "Type a query and press Enter."), "{size:?}: {shown:?}");
        assert!(!has(&shown, "0 tickets"), "{size:?}: {shown:?}");
    }
}

#[test]
fn focus_moved_on_a_frame_with_no_input_asks_for_the_frame_that_settles_it() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::ticket_reader());
    view.desk.panel = PanelMode::Tickets;
    view.desk.diff_open = true;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(!query_focused(&ctx));

    client.tickets.take_keyboard();
    view.now_ms += 16.0;
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(WIDE.0, WIDE.1))),
        time: Some(view.now_ms / 1000.0),
        ..egui::RawInput::default()
    };
    let output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, &mut client, &mut view));
    assert!(query_focused(&ctx));
    let asked = output.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.repaint_delay.is_zero());
    output.drop_without_applying_deltas();
    assert!(asked, "the field's lock on its own keys takes hold on the next frame, which no input will draw");
}

// ── The keys every client takes the same way (`atomics_ui::window_keys`) ─────────────────────────────────────────

const CHORD: egui::Modifiers = egui::Modifiers::CTRL;

/// A host with three agents on the All tab: `s0` and `s2` outside plans, each with a worktree and a diff of its own,
/// and `s1` on its plan's task. So the sidebar lists `s1` first, under its plan, though Ctrl+1 is `s0`.
fn agents() -> fake_host::FakeHost {
    let mut host = fake_host::working();
    for (id, root, file) in [("s0", "/w/zero", "src/zero.rs"), ("s2", "/w/two", "src/two.rs")] {
        let base = host.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
        host.board.sessions.push(atomics_core::projection::SessionInfo {
            previews: Vec::new(),
            id: id.into(),
            title: format!("Agent {id}"),
            task: None,
            worktree: Some(root.into()),
            branch: Some(format!("atomics/{id}")),
            cwd: root.into(),
            ..base
        });
        host.terminals.insert(id.into(), fake_host::FakeTerminal::new(80, 12, "$ "));
        let mut diff = host.board.diffs[0].clone();
        diff.worktree = root.into();
        diff.files.truncate(1);
        diff.files[0].old_path = Some(file.into());
        diff.files[0].new_path = Some(file.into());
        host.board.diffs.push(diff);
        host.files.insert((root.into(), file.into()), fake_host::file_diff(file, 6));
    }
    host
}

/// [`agents`] on the All tab of a window `size` large, with the keyboard in the first agent's terminal.
fn at_a_terminal(host: fake_host::FakeHost, size: (f32, f32)) -> (egui::Context, World, Client, View) {
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::SetView(None));
    settled(&ctx, &mut world, &mut client, &mut view, size);
    if !view.compact {
        view.term.inputs.push(term_pane::TermInput::Focus(true));
        settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(view.term.typing(), "the terminal has the keyboard");
    }
    (ctx, world, client, view)
}

fn typed_in(world: &World, session: &str) -> String {
    world.host.terminals[session].typed.clone()
}

fn no_bytes(world: &World) {
    for (id, terminal) in &world.host.terminals {
        assert_eq!(terminal.typed, "", "{id} got keys");
    }
}

/// What the overlay forwards for a key pressed while it has the keyboard, and a frame.
fn forwarded(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    name: &str,
    mods: egui::Modifiers,
) {
    view.term.inputs.push(term_pane::TermInput::Key { name: name.into(), mods });
    let size = if view.compact { PHONE } else { WIDE };
    sized(ctx, world, client, view, size, Vec::new());
}

fn panel_has_the_keys(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(atomics_ui::panel_nav::focus_id()))
}

fn focused_file(client: &Client, root: &str) -> Option<String> {
    client.diff_views.get(Path::new(root)).and_then(|state| state.focused.clone())
}

fn diff_asked(world: &World, root: &str) -> bool {
    world.sent().iter().any(|m| matches!(m, Message::DiffGet { worktree, .. } if worktree == root))
}

#[test]
fn ctrl_n_from_a_terminal_shows_the_nth_agents_diff_and_no_byte_reaches_a_terminal() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    forwarded(ctx, world, client, view, "3", CHORD);
    assert_eq!(view.desk.active.as_deref(), Some("s2"), "the third of s0, s1 and s2");
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(view.desk.diff_open);
    assert_eq!(view.desk.diff_target(&world.driver.remote).as_deref(), Some(Path::new("/w/two")));
    assert!(panel_has_the_keys(ctx), "the diff holds the keyboard");
    assert!(!view.term.typing() && !view.term.focus_next, "and the input does not");
    sized(ctx, world, client, view, WIDE, Vec::new());
    assert!(diff_asked(world, "/w/two"), "its diff against main is fetched");
    no_bytes(world);

    // From the panel too, and through the canvas, as a page's keys arrive with the input elsewhere.
    sized(ctx, world, client, view, WIDE, press(egui::Key::Num1, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s0"));
    assert!(panel_has_the_keys(ctx));

    // Past the last agent nothing changes.
    sized(ctx, world, client, view, WIDE, press(egui::Key::Num9, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s0"));
    no_bytes(world);
}

#[test]
fn the_approval_banner_takes_what_it_holds_and_the_terminal_the_rest() {
    let top_of = |shown: &[(String, Rect)], label: &str| shown.iter().find(|(text, _)| text == label).map(|(_, r)| *r);
    let (ctx, mut world, mut client, mut view) = live();
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let reason = top_of(&shown, "Run the tests").expect("the request's reason");
    let prompt = top_of(&shown, "$").expect("the terminal");
    assert!((reason.bottom()..reason.bottom() + 40.0).contains(&prompt.top()), "no dead space: {reason:?} {prompt:?}");

    // A long request keeps to the banner's limit, its buttons in reach, and scrolls.
    let mut host = fake_host::host();
    host.board.approvals[0].reason = Some(vec!["A very long reason"; 60].join("\n"));
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let asked = top_of(&shown, "Permission requested").expect("the banner");
    let prompt = top_of(&shown, "$").expect("the terminal");
    assert!(prompt.top() - asked.top() <= 230.0, "the terminal keeps its room: {asked:?} {prompt:?}");
    assert!(top_of(&shown, "Allow").is_some_and(|allow| allow.bottom() < prompt.top()), "{shown:?}");
}

#[test]
fn ctrl_n_works_with_the_boards_dropdown_open_and_after_using_it() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    let shown = settled(ctx, world, client, view, WIDE);

    // Open, from a terminal: the chord picks the agent, the menu goes, and the diff has the keyboard.
    drawn(ctx, world, client, view, WIDE, click(on_bar(&shown, "Boards")));
    let menu = settled(ctx, world, client, view, WIDE);
    assert!(egui::Popup::is_any_open(ctx) && has(&menu, "NEEDS YOU · 1"), "{menu:?}");
    forwarded(ctx, world, client, view, "3", CHORD);
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(panel_has_the_keys(ctx), "the diff holds the keyboard");
    assert!(!egui::Popup::is_any_open(ctx), "and no menu covers it");
    no_bytes(world);

    // Open again, through the canvas: past the last agent nothing moves, not even the menu.
    let shown = settled(ctx, world, client, view, WIDE);
    drawn(ctx, world, client, view, WIDE, click(on_bar(&shown, "Boards")));
    settled(ctx, world, client, view, WIDE);
    sized(ctx, world, client, view, WIDE, press(egui::Key::Num9, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert!(egui::Popup::is_any_open(ctx));

    // After using it: a board picked from the menu is a plan's tab, whose first agent is Ctrl+1.
    let menu = settled(ctx, world, client, view, WIDE);
    let row = menu.iter().rfind(|(text, _)| text == "Parse faster").expect("the board's row").1.center();
    drawn(ctx, world, client, view, WIDE, click(row));
    let shown = settled(ctx, world, client, view, WIDE);
    assert!(view.desk.plan_view && !egui::Popup::is_any_open(ctx));
    assert_eq!(view.desk.panel, PanelMode::Plan, "a row shows its board, though the panel was on the diff");
    assert!(view.desk.diff_open && has(&shown, "PLAN") && has(&shown, "Dispatch ready"), "{shown:?}");
    sized(ctx, world, client, view, WIDE, press(egui::Key::Num1, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s1"), "the plan's only agent");
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(panel_has_the_keys(ctx));
    no_bytes(world);
}

#[test]
fn the_boards_dropdown_counts_what_waits_lists_it_first_and_keeps_every_boards_commands() {
    use atomics_core::plan::PlanStatus;

    let mut host = fake_host::working();
    for (id, goal, status) in [
        ("p2", "Another open plan", PlanStatus::Open),
        ("p3", "Not approved yet", PlanStatus::Open),
        ("p4", "A finished plan", PlanStatus::Completed),
    ] {
        let mut plan = fake_host::closed_plan(id, status, goal);
        plan.approved = id != "p3";
        plan.tasks = host.board.plans[0].tasks[..1].to_vec();
        plan.tasks[0].session = None;
        plan.digest = format!("digest-{id}");
        host.board.plans.push(plan);
    }
    let (ctx, mut world, mut client, mut view) = live_with(host);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    let shown = settled(ctx, world, client, view, WIDE);

    // The button names the board on screen and counts the two that wait; finished boards are out of the bar.
    assert_eq!(view.desk.plan.as_deref(), Some("p1"));
    let button = on_bar(&shown, "Parse faster");
    let beside =
        |text: &str, rect: &Rect| text == "2" && rect.bottom() <= 34.0 && (button.x - rect.right()).abs() < 60.0;
    assert!(shown.iter().any(|(text, rect)| beside(text, rect)), "the count comes before the name: {shown:?}");
    assert!(!shown.iter().any(|(text, rect)| text.contains("A finished plan") && rect.bottom() <= 34.0), "{shown:?}");

    // The ones that wait first, under a heading of their own, then the others, then History.
    drawn(ctx, world, client, view, WIDE, click(button));
    let menu = settled(ctx, world, client, view, WIDE);
    let y = |label: &str| {
        menu.iter().rfind(|(text, _)| text == label).unwrap_or_else(|| panic!("{label}: {menu:?}")).1.top()
    };
    let order = ["NEEDS YOU · 2", "Parse faster", "Not approved yet", "ACTIVE · 1", "Another open plan", "History"];
    assert!(order.windows(2).all(|pair| y(pair[0]) < y(pair[1])), "{order:?}: {menu:?}");
    assert!(!has(&menu, "A finished plan"), "behind History: {menu:?}");
    for (name, state) in
        [("Parse faster", "needs you"), ("Not approved yet", "needs approval"), ("Another open plan", "idle")]
    {
        assert!((y(name) - y(state)).abs() < 3.0, "{name} says {state}: {menu:?}");
    }

    // A row shows its board, whose button approves it.
    let row = menu.iter().rfind(|(text, _)| text == "Not approved yet").unwrap().1.center();
    drawn(ctx, world, client, view, WIDE, click(row));
    let shown = settled(ctx, world, client, view, WIDE);
    assert_eq!(view.desk.plan.as_deref(), Some("p3"));
    assert!(!egui::Popup::is_any_open(ctx), "the menu closes");
    let approve = on(&shown, "Approve plan");
    drawn(ctx, world, client, view, WIDE, click(approve));
    let approved = |world: &World| world.host.board.plans.iter().find(|plan| plan.id == "p3").unwrap().approved;
    assert!(approved(world), "the host approved it");

    // A row's close asks first, as the board's Close plan… does.
    let shown = settled(ctx, world, client, view, WIDE);
    drawn(ctx, world, client, view, WIDE, click(on_bar(&shown, "Not approved yet")));
    let menu = settled(ctx, world, client, view, WIDE);
    let row = menu.iter().rfind(|(text, _)| text == "Another open plan").unwrap().1;
    let state =
        menu.iter().rfind(|(text, rect)| text == "idle" && (rect.center().y - row.center().y).abs() < 3.0).unwrap().1;
    drawn(ctx, world, client, view, WIDE, click(egui::pos2(state.right() + 17.0, row.center().y)));
    let shown = settled(ctx, world, client, view, WIDE);
    assert!(has(&shown, "Close “Another open plan”?"), "{shown:?}");
    assert_eq!(world.host.board.plans.iter().find(|plan| plan.id == "p2").unwrap().status, PlanStatus::Open);
    click_on(ctx, world, client, view, WIDE, "Cancel");

    // History reads a finished plan's board.
    let shown = settled(ctx, world, client, view, WIDE);
    drawn(ctx, world, client, view, WIDE, click(on_bar(&shown, "Not approved yet")));
    let menu = settled(ctx, world, client, view, WIDE);
    drawn(ctx, world, client, view, WIDE, click(on(&menu, "History")));
    let opened = settled(ctx, world, client, view, WIDE);
    assert!(has(&opened, "A finished plan") && has(&opened, "View"), "{opened:?}");
    for absent in ["Reopen", "Archive"] {
        assert!(!has(&opened, absent), "atomics.v1 carries no {absent} from here: {opened:?}");
    }
    assert!(!has(&opened, "completed ·"), "a page has no date to give: {opened:?}");
    drawn(ctx, world, client, view, WIDE, click(on(&opened, "View")));
    let shown = settled(ctx, world, client, view, WIDE);
    assert_eq!(view.desk.plan.as_deref(), Some("p4"));
    assert!(has(&shown, "Completed") && !has(&shown, "Approve plan"), "{shown:?}");
    assert!(in_bar(&shown, "A finished plan"), "the button names it while it shows: {shown:?}");
}

#[test]
fn every_digit_with_ctrl_stays_out_of_a_terminal_and_plain_ctrl_with_a_letter_reaches_it() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(fake_host::working(), WIDE);
    assert_eq!(view.term.session(), Some("s1"));
    for digit in 0..=9 {
        // Ctrl+3 would be Escape, which interrupts an agent. A digit that picks an agent takes the keyboard away, so it
        // comes back for the next.
        view.term.inputs.push(term_pane::TermInput::Focus(true));
        view.term.inputs.push(term_pane::TermInput::Key { name: digit.to_string(), mods: CHORD });
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    }
    no_bytes(&world);

    view.term.inputs.push(term_pane::TermInput::Focus(true));
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    view.term.inputs.push(term_pane::TermInput::Key { name: "d".into(), mods: CHORD });
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(typed_in(&world, "s1"), "\x04", "Ctrl+D is the terminal's");
    assert!(!panel_has_the_keys(&ctx));
}

#[test]
fn shift_arrows_walk_the_files_from_a_terminal_and_page_keys_walk_the_hunks_in_the_diff() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(fake_host::working(), WIDE);
    let root = fake_host::WORKTREE;
    let shift = egui::Modifiers::SHIFT;
    // The page keys are the terminal's while it has the keyboard, for its programs' own scrolling.
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    forwarded(ctx, world, client, view, "PageDown", egui::Modifiers::NONE);
    settled(ctx, world, client, view, WIDE);
    assert_eq!(typed_in(world, "s1"), "\x1b[6~");
    assert!(!panel_has_the_keys(ctx));

    // Shift+↓ takes it to the diff, which shows, at its first file.
    let typed_before = typed_in(world, "s1");
    forwarded(ctx, world, client, view, "ArrowDown", shift);
    settled(ctx, world, client, view, WIDE);
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(panel_has_the_keys(ctx), "the diff holds the keyboard");
    assert!(!view.term.typing());
    assert_eq!(view.desk.active.as_deref(), Some("s1"), "the agent stays");
    assert_eq!(focused_file(client, root).as_deref(), Some("src/a.rs"));
    sized(ctx, world, client, view, WIDE, press(egui::Key::ArrowDown, shift));
    assert_eq!(focused_file(client, root).as_deref(), Some("src/b.rs"));
    sized(ctx, world, client, view, WIDE, press(egui::Key::ArrowUp, shift));
    assert_eq!(focused_file(client, root).as_deref(), Some("src/a.rs"));

    // The page keys go on to the next file's hunk from here, and the arrows only scroll.
    sized(ctx, world, client, view, WIDE, press(egui::Key::PageDown, egui::Modifiers::NONE));
    sized(ctx, world, client, view, WIDE, press(egui::Key::PageDown, egui::Modifiers::NONE));
    assert_eq!(focused_file(client, root).as_deref(), Some("src/b.rs"));
    sized(ctx, world, client, view, WIDE, press(egui::Key::PageUp, egui::Modifiers::NONE));
    assert_eq!(focused_file(client, root).as_deref(), Some("src/a.rs"));
    sized(ctx, world, client, view, WIDE, press(egui::Key::ArrowDown, egui::Modifiers::NONE));
    assert_eq!(focused_file(client, root).as_deref(), Some("src/a.rs"));
    assert!(panel_has_the_keys(ctx));
    assert_eq!(typed_in(world, "s1"), typed_before, "no walking key reached the terminal");
}

#[test]
fn shift_arrows_stay_in_a_text_box_and_an_agent_chord_leaves_it_with_its_text() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), WIDE);
    client.focus_steer = true;
    client.steer_text = "half a thought".into();
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(ctx.text_edit_focused(), "the steer bar has the keyboard");
    let panel = view.desk.panel;
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::ArrowDown, egui::Modifiers::SHIFT));
    assert_eq!(view.desk.panel, panel, "it stays with the text box");
    assert!(!panel_has_the_keys(&ctx));

    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num3, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert_eq!(client.steer_text, "half a thought");
    assert!(panel_has_the_keys(&ctx));
    no_bytes(&world);
}

#[test]
fn typing_or_escape_in_the_diff_hands_the_keyboard_back_to_the_agents_terminal() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    forwarded(ctx, world, client, view, "3", CHORD);
    settled(ctx, world, client, view, WIDE);
    assert!(panel_has_the_keys(ctx));
    no_bytes(world);

    // The first key goes to the terminal, and the input takes the keyboard for the next.
    sized(ctx, world, client, view, WIDE, vec![egui::Event::Text("y".into())]);
    assert!(!panel_has_the_keys(ctx), "the panel let go");
    assert!(view.term.overlay.focus, "the input takes the keyboard");
    settled(ctx, world, client, view, WIDE);
    assert_eq!(typed_in(world, "s2"), "y", "the key that moved the keyboard is not lost");
    assert_eq!(typed_in(world, "s1"), "");

    // Escape goes back the same way, and reaches no terminal: an agent would take it for an interrupt.
    view.term.inputs.push(term_pane::TermInput::Focus(false));
    sized(ctx, world, client, view, WIDE, press(egui::Key::Num2, CHORD));
    settled(ctx, world, client, view, WIDE);
    assert!(panel_has_the_keys(ctx));
    sized(ctx, world, client, view, WIDE, press(egui::Key::Escape, egui::Modifiers::NONE));
    assert!(view.term.overlay.focus);
    assert!(!panel_has_the_keys(ctx));
    settled(ctx, world, client, view, WIDE);
    assert_eq!(typed_in(world, "s1"), "");
}

#[test]
fn a_viewer_moves_around_with_the_keys_and_types_nothing() {
    let (ctx, mut world, mut client, mut view) = viewing(agents());
    client.send(Command::SetView(None));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num3, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("y".into())]);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    no_bytes(&world);
    assert!(!view.term.overlay.focus);
}

#[test]
fn with_tickets_open_the_reader_follows_the_agent_and_no_diff_comes_up() {
    let mut host = fake_host::ticket_reader();
    host.board.plans[0].tasks[1].session = Some("s2".into());
    host.board.plans[0].tasks[1].ticket = Some("ENG-42".into());
    let base = host.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
    host.board.sessions.push(atomics_core::projection::SessionInfo { id: "s2".into(), ..base });
    host.terminals.insert("s2".into(), fake_host::FakeTerminal::new(80, 12, "$ "));
    let (ctx, mut world, mut client, mut view) = live_with(host);
    client.send(Command::SetView(None));
    client.send(Command::ToggleView(PanelMode::Tickets));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.tickets.active(), "the reader shows");
    let link = |key: &str| atomics_core::tickets::TicketLink {
        system: "tracker".into(),
        id: key.into(),
        key: Some(key.into()),
        ..Default::default()
    };
    // Find stays in Find.
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num2, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert!(client.tickets.read.is_none());
    assert_eq!(view.desk.panel, PanelMode::Tickets, "open Tickets keeps its place");

    client.tickets.open(link("OTHER-1"), false, 0.0, &mut client.commands);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    fn reads(client: &Client) -> Option<String> {
        client.tickets.read.as_ref().map(|read| read.link.label().to_string())
    }
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num1, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s1"));
    assert_eq!(reads(&client).as_deref(), Some("ENG-41"), "the reader follows the agent");
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num2, CHORD));
    assert_eq!(reads(&client).as_deref(), Some("ENG-42"));
    assert_eq!(view.desk.panel, PanelMode::Tickets, "no diff comes up");
    assert!(panel_has_the_keys(&ctx), "the panel has the keys");
    no_bytes(&world);
}

#[test]
fn in_editor_mode_the_nth_agents_editor_takes_the_keyboard() {
    let mut host = fake_host::panels_host();
    let base = host.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
    host.board.sessions.push(atomics_core::projection::SessionInfo {
        previews: Vec::new(),
        id: "s2".into(),
        task: None,
        worktree: Some("/w/two".into()),
        cwd: "/w/two".into(),
        ..base
    });
    host.terminals.insert("s2".into(), fake_host::FakeTerminal::new(80, 12, "$ "));
    let (ctx, mut world, mut client, mut view) = at_a_terminal(host, WIDE);
    client.send(Command::Edit { root: fake_host::WORKTREE.into(), path: None, line: None });
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert_eq!(view.desk.panel, PanelMode::Editor);
    let asked = |world: &World, root: &str| {
        world.sent().iter().any(|m| {
            matches!(m, Message::HostCommand(c)
                if matches!(&c.action, HostAction::Edit { root: r, .. } if r == root))
        })
    };

    // The second agent's editor starts on demand, and the keyboard goes there, not to the panel.
    view.term.inputs.push(term_pane::TermInput::Key { name: "2".into(), mods: CHORD });
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert_eq!(view.desk.panel, PanelMode::Editor, "it stays in the editor");
    assert!(asked(&world, "/w/two"), "its editor starts");
    assert!(!panel_has_the_keys(&ctx));
    assert!(view.term.overlay.focus || view.term.focus_next, "typing goes to the editor");
    no_bytes(&world);

    // An editor that runs keeps Shift with the arrows, to select text.
    view.term.overlay.editor = true;
    view.term.inputs.push(term_pane::TermInput::Key { name: "ArrowDown".into(), mods: egui::Modifiers::SHIFT });
    assert!(view.term.window_keys(ctx.os()).is_empty(), "the editor keeps it");
    assert_eq!(view.term.inputs.len(), 1);
    view.term.inputs.clear();
    view.term.inputs.push(term_pane::TermInput::Key { name: "1".into(), mods: CHORD });
    assert_eq!(view.term.window_keys(ctx.os()).len(), 1, "an agent chord leaves it");
}

#[test]
fn a_phone_with_a_keyboard_takes_the_same_keys_where_it_has_the_agents() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), PHONE);
    view.tab = Tab::Agents;
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "Agent s0"), "{shown:?}");

    // Ctrl+3: the third of every agent, on the Diffs pane.
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, press(egui::Key::Num3, CHORD));
    assert_eq!(view.tab, Tab::Diffs);
    assert_eq!(view.diff.as_deref(), Some(Path::new("/w/two")));
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    assert!(has(&shown, "src/two.rs"), "{shown:?}");

    // Shift+↓ walks the diff on screen, and no key does anything else.
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, press(egui::Key::ArrowDown, egui::Modifiers::SHIFT));
    assert_eq!(focused_file(&client, "/w/two").as_deref(), Some("src/two.rs"));
    assert_eq!(view.tab, Tab::Diffs);
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, vec![egui::Event::Text("y".into())]);
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, press(egui::Key::Escape, egui::Modifiers::NONE));
    assert_eq!(view.tab, Tab::Diffs, "a phone keeps its panes");
    no_bytes(&world);

    // From the terminal's pane, Shift+↓ goes to the worktree of the agent it shows.
    view.tab = Tab::Terminal;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    client.send(Command::Activate { session: "s0".into(), open_diff: false });
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    view.tab = Tab::Terminal;
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
    sized(&ctx, &mut world, &mut client, &mut view, PHONE, press(egui::Key::ArrowDown, egui::Modifiers::SHIFT));
    assert_eq!(view.tab, Tab::Diffs);
    assert_eq!(view.diff.as_deref(), Some(Path::new("/w/zero")));
    settled(&ctx, &mut world, &mut client, &mut view, PHONE);
}

#[test]
fn a_window_with_no_room_for_the_diff_and_the_terminal_gives_the_terminal_back_by_closing_the_diff() {
    // Wide layout, small: the diff covers the terminals.
    const SMALL: (f32, f32) = (640.0, 300.0);
    let (ctx, mut world, mut client, mut view) = live_with(agents());
    client.send(Command::SetView(None));
    settled(&ctx, &mut world, &mut client, &mut view, SMALL);
    sized(&ctx, &mut world, &mut client, &mut view, SMALL, press(egui::Key::Num3, CHORD));
    settled(&ctx, &mut world, &mut client, &mut view, SMALL);
    assert_eq!(view.desk.active.as_deref(), Some("s2"));
    assert!(view.desk.diff_open);
    if view.desk.diff_fills {
        assert!(panel_has_the_keys(&ctx));
        sized(&ctx, &mut world, &mut client, &mut view, SMALL, press(egui::Key::Escape, egui::Modifiers::NONE));
        assert!(!view.desk.diff_open, "or the terminal would not show");
        assert!(!panel_has_the_keys(&ctx));
    }
    settled(&ctx, &mut world, &mut client, &mut view, SMALL);
    no_bytes(&world);
}

#[test]
fn a_rows_number_is_the_key_that_goes_to_it_though_the_groups_order_it_differently() {
    let (ctx, mut world, mut client, mut view) = live_with(agents());
    client.send(Command::SetView(None));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let row_of = |title: &str| on(&shown, title);
    let number_by = |row: egui::Pos2| {
        shown
            .iter()
            .filter(|(text, rect)| {
                text.len() == 1
                    && text.chars().all(|c| c.is_ascii_digit())
                    && rect.left() < row.x
                    && (rect.center().y - row.y).abs() < 28.0
            })
            .map(|(text, rect)| (text.clone(), rect.center().x))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(text, _)| text)
    };
    let (first, planned, last) = (row_of("Agent s0"), row_of("Fix the parser"), row_of("Agent s2"));
    assert!(planned.y < first.y, "the plan's group comes first: {shown:?}");
    assert_eq!(number_by(first).as_deref(), Some("1"), "{shown:?}");
    assert_eq!(number_by(planned).as_deref(), Some("2"));
    assert_eq!(number_by(last).as_deref(), Some("3"));

    sized(&ctx, &mut world, &mut client, &mut view, BOARD, press(egui::Key::Num1, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s0"), "what you see is what you press");
}

#[test]
fn a_changed_files_click_scrolls_the_diff_in_place_and_keeps_the_agent() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert_eq!(view.desk.active.as_deref(), Some("s1"));
    client.send(Command::ToggleView(PanelMode::Diff));
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    let before = world.sent().len();
    let root = fake_host::WORKTREE;
    client.send(Command::OpenFile { root: root.into(), path: "src/b.rs".into() });
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert_eq!(view.desk.active.as_deref(), Some("s1"), "the agent stays the active one");
    assert_eq!(focused_file(&client, root).as_deref(), Some("src/b.rs"));
    assert_eq!(world.sent().len(), before, "and nothing goes to the host: {:?}", &world.sent()[before..]);

    // A file of a worktree that is not on screen shows that worktree, which may be nobody's.
    client.send(Command::OpenFile { root: "/w/other".into(), path: "src/c.rs".into() });
    settled(&ctx, &mut world, &mut client, &mut view, BOARD);
    assert_eq!(view.desk.active, None);
    assert_eq!(view.desk.diff_target(&world.driver.remote).as_deref(), Some(Path::new("/w/other")));
}

#[test]
fn closing_settings_gives_the_keyboard_back_to_the_terminal() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::management());
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.management.settings = true;
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.management.settings = false;
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, Vec::new());
    assert!(client.focus_terminal.is_none());
    assert!(view.term.overlay.focus, "the input takes it back");
}

#[test]
fn agent_chords_with_a_management_window_open_keep_typed_and_pasted_text_out_of_terminals() {
    for window in ["settings", "remote", "project"] {
        let (ctx, mut world, mut client, mut view) = at_a_terminal(fake_host::management(), WIDE);
        match window {
            "settings" => client.management.settings = true,
            "remote" => client.management.remote = true,
            "project" => client.management.project = true,
            _ => unreachable!(),
        }
        settled(&ctx, &mut world, &mut client, &mut view, WIDE);
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num1, CHORD));
        assert_eq!(view.desk.active.as_deref(), Some("s1"));
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Text("private text".into())]);
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Paste("private paste".into())]);
        // Even a panel that already has focus cannot hand the keyboard to a terminal behind a window.
        ctx.memory_mut(|m| m.request_focus(atomics_ui::panel_nav::focus_id()));
        sized(&ctx, &mut world, &mut client, &mut view, WIDE, vec![egui::Event::Paste("still private".into())]);
        settled(&ctx, &mut world, &mut client, &mut view, WIDE);
        no_bytes(&world);
        assert!(!view.term.typing(), "{window}");
        assert!(client.focus_terminal.is_none(), "{window}");
    }
}

#[test]
fn selecting_the_active_agent_with_a_chord_replaces_a_pull_request_with_its_diff_against_main() {
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::panels_host());
    client.send(Command::SetView(None));
    client.send(Command::Activate { session: "s1".into(), open_diff: true });
    client.send(Command::SetPanel(PanelMode::Diff));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    client.pr_view = Some(("pull:example/repo#7".into(), Default::default()));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.pr_view.as_ref().unwrap().1.prepared.is_some());
    assert!(client.pr_target.is_some());
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::Num1, CHORD));
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(client.pr_view.is_none());
    assert!(client.pr_target.is_none());
    assert_eq!(view.desk.active.as_deref(), Some("s1"));
    assert_eq!(view.desk.diff_target(&world.driver.remote).as_deref(), Some(Path::new(WORKTREE)));
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(diff_asked(&world, WORKTREE));
    assert!(panel_has_the_keys(&ctx));
    no_bytes(&world);
}

#[test]
fn preview_menu_lists_ports_and_records_open_and_stop_actions() {
    use atomics_core::preview::Preview;
    use atomics_core::wire::{Envelope, Limits, Resume, Role, Seq, VERSION};
    let mut host = fake_host::host();
    let session = host.board.sessions.iter_mut().find(|s| !s.chief).unwrap();
    session.previews = vec![
        Preview { port: 5173, path: "/demo".into(), label: "Web".into() },
        Preview { port: 8080, path: String::new(), label: "API".into() },
    ];
    let id = session.id.clone();
    let (ctx, mut world, mut client, mut view) = live_with(host);
    world.driver.remote.enable_native_previews();
    let mut welcome = world.driver.remote.welcome().unwrap();
    welcome.capabilities.push(atomics_core::wire::TUNNEL.into());
    let epoch = world.host.epoch.clone();
    let after = world.driver.remote.with_mirror(|m| m.resume_point().1.unwrap());
    world.driver.remote.receive(Envelope::host(
        &epoch,
        Message::Welcome {
            version: VERSION,
            capabilities: welcome.capabilities,
            limits: Limits::default(),
            role: Role::Controller,
            resume: Resume::Replay { after: Seq(after) },
            may: welcome.may,
        },
    ));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    let at = on(&shown, "Preview");
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(at));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Web · 5173"));
    assert!(has(&shown, "API · 8080"));
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Web · 5173")));
    assert!(
        matches!(world.driver.remote.take_preview_actions().as_slice(), [Command::OpenPreview { session: Some(session), preview }] if session == &id && preview.port == 5173 && preview.path == "/demo")
    );
    world.driver.remote.set_active_previews(vec![5173]);
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Preview")));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    drawn(&ctx, &mut world, &mut client, &mut view, WIDE, click(on(&shown, "Stop port 5173")));
    assert_eq!(world.driver.remote.take_preview_actions(), vec![Command::StopPreview { port: 5173 }]);
}

#[test]
fn machine_panel_draws_resources_containers_ports_and_agent_previews_at_narrow_width() {
    use atomics_core::machine::{Container, Disk, Machine, Memory, Port};
    use atomics_core::preview::Preview;
    for size in [WIDE, (320.0, 844.0)] {
        let mut host = fake_host::explorer();
        host.board.sessions[0].previews = vec![Preview { port: 5173, path: "/".into(), label: "Web".into() }];
        let (ctx, mut world, mut client, mut view) = live_with(host);
        client.machine_open = true;
        settled(&ctx, &mut world, &mut client, &mut view, size);
        let snapshot = Machine {
            sampled_at: 1,
            truncated: false,
            cores: 8,
            load: Some([1.5, 1.2, 0.9]),
            cpu_busy: Some(25.0),
            memory: Some(Memory { total: 8 << 30, available: 6 << 30 }),
            disks: vec![Disk { path: "/srv/work".into(), total: 64 << 30, free: 32 << 30 }],
            containers: Some(vec![Container {
                name: "web-1".into(),
                image: "app.example/web:1".into(),
                state: "running".into(),
                status: "Up 1 hour".into(),
                ports: "0.0.0.0:8080->80/tcp".into(),
                cpu_percent: Some(2.0),
                memory: Some("32MiB / 2GiB".into()),
            }]),
            ports: vec![Port {
                address: "127.0.0.1".into(),
                port: 8080,
                pid: Some(123),
                process: Some("web".into()),
                previewable: true,
                ..Default::default()
            }],
        };
        world
            .driver
            .remote
            .receive(atomics_core::wire::Envelope::host(&world.host.epoch, Message::Machine { snapshot }));
        let mut shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        for _ in 0..5 {
            let pointer = on(&shown, "Resources") + vec2(60.0, 100.0);
            shown.extend(drawn(
                &ctx,
                &mut world,
                &mut client,
                &mut view,
                size,
                vec![
                    egui::Event::PointerMoved(pointer),
                    egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        phase: egui::TouchPhase::Move,
                        source: egui::MouseWheelSource::Wheel,
                        delta: vec2(0.0, -220.0),
                        modifiers: egui::Modifiers::default(),
                    },
                ],
            ));
            shown.extend(settled(&ctx, &mut world, &mut client, &mut view, size));
        }
        for label in ["Machine", "Resources", "Containers", "web-1", "Ports", "8080", "Agent previews", "Preview"] {
            assert!(has(&shown, label), "{label} missing at {size:?}: {shown:?}");
        }
        // The window stays inside the screen, with horizontal table scrolling at narrow widths.
        let title = shown.iter().find(|(label, _)| label == "Resources").unwrap().1;
        assert!(title.left() >= 0.0 && title.right() <= size.0, "{title:?}");
        assert!(world.driver.remote.machine().is_some());
        client.machine_open = false;
        settled(&ctx, &mut world, &mut client, &mut view, size);
    }
}

#[test]
fn machine_opens_from_the_load_gauge_and_browser_preview_stays_disabled() {
    use atomics_core::machine::{Machine, Port};
    let (ctx, mut world, mut client, mut view) = live_with(fake_host::working());
    let shown = click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "load 4.2");
    assert!(client.machine_open);
    assert!(has(&shown, "Waiting for machine data"));
    let snapshot = Machine {
        cores: 4,
        sampled_at: 1,
        ports: vec![Port { address: "127.0.0.1".into(), port: 8080, previewable: true, ..Default::default() }],
        ..Default::default()
    };
    world.driver.remote.receive(atomics_core::wire::Envelope::host(&world.host.epoch, Message::Machine { snapshot }));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    assert!(has(&shown, "Docker not available"));
    assert!(has(&shown, "Preview"));
    assert!(!world.driver.remote.can_preview());
    click_on(&ctx, &mut world, &mut client, &mut view, WIDE, "Preview");
    assert!(world.driver.remote.take_preview_actions().is_empty());
}

#[test]
fn an_unreadable_pairing_offers_try_again_instead_of_pairing() {
    use crate::driver::{DeviceReadError, Stop};
    for size in [WIDE, PHONE] {
        let mut world = World::paired(fake_host::host());
        let pairing = world.host.open_pairing();
        let data = world.boot_data(Some(&pairing));
        world.driver.message(PARENT, true, &data).unwrap();
        world.driver.take_outputs(); // Simulate the asynchronous storage read before the world answers it.
        world.driver.device_loaded(Err(DeviceReadError));
        let ctx = egui::Context::default();
        let (mut client, mut view) = (Client::default(), View::default());
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(has(&shown, Stop::DeviceReadFailed.message()), "{shown:?}");
        assert!(has(&shown, "Reason: could not read the stored key."), "{shown:?}");
        assert!(has(&shown, "Try again"), "{shown:?}");
        assert!(!has(&shown, "Pair again") && !has(&shown, "Pairing this browser"), "{shown:?}");
        assert!(world.dials.is_empty());
        drawn(&ctx, &mut world, &mut client, &mut view, size, click(on(&shown, "Try again")));
        assert_eq!(world.driver.status(), &Status::Live, "the retry reads the existing pairing");
    }
}

/// A host whose worktree changed eight files, of `lines` added lines each.
fn many_files(lines: u32) -> fake_host::FakeHost {
    let mut host = fake_host::working();
    let template = host.board.diffs[0].files[0].clone();
    let name = |i: usize| format!("src/f{i}.rs");
    host.board.diffs[0].files = (0..8)
        .map(|i| DiffFileSummary { old_path: Some(name(i)), new_path: Some(name(i)), ..template.clone() })
        .collect();
    for i in 0..8 {
        host.files.insert((WORKTREE.into(), name(i)), fake_host::file_diff(&name(i), lines));
    }
    host
}

/// The host's diff moves on a revision, with what `change` makes of its files.
fn diff_moves_on(world: &mut World, change: impl FnOnce(&mut Vec<DiffFileSummary>, &mut fake_host::FakeHost)) {
    let mut summary = world.host.board.diffs[0].clone();
    summary.revision += 1;
    change(&mut summary.files, &mut world.host);
    let frames = world.host.event(|cursor| Message::DiffSummary { cursor, summary });
    world.deliver(frames);
}

/// Where a click lands on the first text starting `label`.
fn on_line(shown: &[(String, Rect)], label: &str) -> egui::Pos2 {
    shown.iter().find(|(text, _)| text.starts_with(label)).unwrap_or_else(|| panic!("{label}: {shown:?}")).1.center()
}

/// That a row stands where it stood. egui rounds the top of what scrolls to a whole pixel, so a row whose index
/// changed can stand part of a pixel off.
#[track_caller]
fn stays(now: Option<f32>, then: f32, what: &str) {
    let now = now.unwrap_or_else(|| panic!("{what}: not on screen, at {then} before"));
    assert!((now - then).abs() < 1.0, "{what}: at {now}, at {then} before");
}

/// Where the first text starting `label` is drawn, from the top of the window.
fn top_of(shown: &[(String, Rect)], label: &str) -> Option<f32> {
    shown.iter().find(|(text, _)| text.starts_with(label)).map(|(_, rect)| rect.top())
}

#[test]
fn a_new_revision_keeps_the_place_at_the_nth_file_by_its_path() {
    let shift_down = || press(egui::Key::ArrowDown, egui::Modifiers::SHIFT);
    let (ctx, mut world, mut client, mut view, _) = diff_of(live_with(many_files(120)), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    for _ in 0..5 {
        drawn(ctx, world, client, view, WIDE, shift_down());
        settled(ctx, world, client, view, WIDE);
    }
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f4.rs"));
    let shown = settled(ctx, world, client, view, WIDE);
    let header = top_of(&shown, "src/f4.rs").expect("the file at the top of the diff");
    let line = top_of(&shown, "    step(7);").expect("a line of it");

    // The file right above it grows, the body it shows comes back the same: nothing moves in any frame of the refresh.
    diff_moves_on(world, |_, host| {
        host.files.insert((WORKTREE.into(), "src/f3.rs".into()), fake_host::file_diff("src/f3.rs", 300));
    });
    for frame in 0..6 {
        let shown = drawn(ctx, world, client, view, WIDE, Vec::new());
        stays(top_of(&shown, "src/f4.rs"), header, &format!("its header, frame {frame}"));
        stays(top_of(&shown, "    step(7);"), line, &format!("its line, frame {frame}"));
    }
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f4.rs"));
    let state = &client.diff_views[Path::new(WORKTREE)];
    let lines = |path: &str| {
        let file = state.prepared.as_ref().unwrap().diff.files.iter().find(|f| f.path() == path).unwrap();
        file.hunks.iter().map(|hunk| hunk.lines.len()).sum::<usize>()
    };
    assert_eq!(lines("src/f3.rs"), 301, "the new body did arrive");

    // Shift+↓ goes on from there, and not from the first file.
    drawn(ctx, world, client, view, WIDE, shift_down());
    settled(ctx, world, client, view, WIDE);
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f5.rs"));
}

#[test]
fn a_new_revision_keeps_the_scroll_inside_an_unchanged_file() {
    use egui::{Key, Modifiers};
    let (ctx, mut world, mut client, mut view, _) = diff_of(live_with(many_files(120)), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    for _ in 0..4 {
        drawn(ctx, world, client, view, WIDE, press(Key::ArrowDown, Modifiers::SHIFT));
        settled(ctx, world, client, view, WIDE);
    }
    for _ in 0..30 {
        drawn(ctx, world, client, view, WIDE, press(Key::ArrowDown, Modifiers::NONE));
    }
    let shown = settled(ctx, world, client, view, WIDE);
    assert_eq!(top_of(&shown, "src/f3.rs"), None, "scrolled past its header");
    let line = top_of(&shown, "    step(60);").expect("a line further down");

    diff_moves_on(world, |files, host| {
        // One file above goes, another grows.
        files.remove(0);
        host.files.insert((WORKTREE.into(), "src/f1.rs".into()), fake_host::file_diff("src/f1.rs", 30));
    });
    for frame in 0..6 {
        let shown = drawn(ctx, world, client, view, WIDE, Vec::new());
        stays(top_of(&shown, "    step(60);"), line, &format!("the line, frame {frame}"));
    }
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f3.rs"));

    // The file itself goes: the one that took its place shows, from its start.
    diff_moves_on(world, |files, _| files.retain(|f| f.new_path.as_deref() != Some("src/f3.rs")));
    let shown = settled(ctx, world, client, view, WIDE);
    let next = top_of(&shown, "src/f4.rs").expect("the next file");
    assert!(next < line, "at the top of the diff, under its header: {next}");
}

fn pointer(at: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
    let button = egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(at), button]
}

/// The diff at its third file with the keyboard in the panel, and the frame it shows.
fn at_the_third_file() -> (egui::Context, World, Client, View, Vec<(String, Rect)>) {
    let (ctx, mut world, mut client, mut view, _) = diff_of(live_with(many_files(120)), WIDE);
    for _ in 0..3 {
        drawn(&ctx, &mut world, &mut client, &mut view, WIDE, press(egui::Key::ArrowDown, egui::Modifiers::SHIFT));
        settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    }
    assert_eq!(focused_file(&client, WORKTREE).as_deref(), Some("src/f2.rs"));
    let shown = settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    (ctx, world, client, view, shown)
}

/// The text of a line of the fake diff, as far as it tells the lines apart.
fn step(n: u32) -> String {
    format!("    step({n});")
}

/// A person's click, its press and release some frames apart, with the frames it takes to show.
fn slow_click(
    ctx: &egui::Context,
    world: &mut World,
    client: &mut Client,
    view: &mut View,
    at: egui::Pos2,
) -> Vec<(String, Rect)> {
    drawn(ctx, world, client, view, WIDE, pointer(at, true));
    settled(ctx, world, client, view, WIDE);
    drawn(ctx, world, client, view, WIDE, pointer(at, false));
    settled(ctx, world, client, view, WIDE)
}

const COMMENT_BOX: &str = "Comment for the agent";

#[test]
fn a_click_on_a_line_leaves_it_and_what_is_above_it_where_they_are() {
    let (ctx, mut world, mut client, mut view, shown) = at_the_third_file();
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    let header = top_of(&shown, "src/f2.rs").expect("the file's header");
    let line = top_of(&shown, &step(9)).expect("a line of it");
    let at = |shown: &[(String, Rect)], n: u32| on_line(shown, &step(n));

    let after = slow_click(ctx, world, client, view, at(&shown, 9));
    assert_eq!(top_of(&after, "src/f2.rs"), Some(header), "the scroll stays, to the pixel");
    assert_eq!(top_of(&after, &step(9)), Some(line), "the line stays, to the pixel");
    let row = top_of(&shown, &step(10)).unwrap() - line;
    assert_eq!(top_of(&after, COMMENT_BOX).map(|y| ((y - line) / row).round()), Some(1.0), "the box is under it");
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f2.rs"));

    // Empty, it gives way to a click further down: the line that was clicked gets the box and stays put, however
    // long the press takes.
    let target = top_of(&after, &step(20)).unwrap();
    let press = at(&after, 20);
    drawn(ctx, world, client, view, WIDE, pointer(press, true));
    for frame in 0..4 {
        let held = drawn(ctx, world, client, view, WIDE, Vec::new());
        assert_eq!(top_of(&held, &step(20)), Some(target), "nothing moves during the press: frame {frame}");
    }
    drawn(ctx, world, client, view, WIDE, pointer(press, false));
    let after = settled(ctx, world, client, view, WIDE);
    stays(top_of(&after, &step(20)), target, "the clicked line");
    assert_eq!(top_of(&after, COMMENT_BOX).map(|y| ((y - target) / row).round()), Some(1.0), "the box is under it");
    assert_eq!(after.iter().filter(|(text, _)| text.starts_with(COMMENT_BOX)).count(), 1);

    // And to a click further up: nothing above the new box moves.
    let target = top_of(&after, &step(5)).unwrap();
    let header = top_of(&after, "src/f2.rs");
    let after = slow_click(ctx, world, client, view, at(&after, 5));
    assert_eq!(top_of(&after, &step(5)), Some(target));
    assert_eq!(top_of(&after, "src/f2.rs"), header);
    assert_eq!(top_of(&after, COMMENT_BOX).map(|y| ((y - target) / row).round()), Some(1.0), "the box is under it");
}

#[test]
fn an_empty_comment_box_closes_when_the_press_elsewhere_is_over() {
    let (ctx, mut world, mut client, mut view, shown) = at_the_third_file();
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    let after = slow_click(ctx, world, client, view, on_line(&shown, &step(9)));
    assert!(top_of(&after, COMMENT_BOX).is_some());
    // The file's header: what a click does there is its own.
    let empty = on_line(&after, "@@ -1,1 +1,9 @@");
    let after = slow_click(ctx, world, client, view, empty);
    assert_eq!(top_of(&after, COMMENT_BOX), None);
}

#[test]
fn ctrl_n_reaches_the_nth_agents_diff_from_a_comment_box_opened_by_a_click() {
    let (ctx, mut world, mut client, mut view) = at_a_terminal(agents(), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    forwarded(ctx, world, client, view, "2", CHORD);
    assert_eq!(view.desk.active.as_deref(), Some("s1"));
    let shown = settled(ctx, world, client, view, WIDE);
    let after = slow_click(ctx, world, client, view, on_line(&shown, &step(2)));
    assert!(top_of(&after, COMMENT_BOX).is_some());
    assert!(!panel_has_the_keys(ctx), "the box has them");

    sized(ctx, world, client, view, WIDE, press(egui::Key::Num3, CHORD));
    assert_eq!(view.desk.active.as_deref(), Some("s2"), "the third of s0, s1 and s2");
    assert_eq!(view.desk.diff_target(&world.driver.remote).as_deref(), Some(Path::new("/w/two")));
    assert_eq!(view.desk.panel, PanelMode::Diff);
    assert!(panel_has_the_keys(ctx), "the diff holds the keyboard");
    no_bytes(world);
}

#[test]
fn a_new_title_keeps_the_place() {
    // The scroll area goes by the title, which an agent's session changes as it works.
    let (ctx, mut world, mut client, mut view, shown) = at_the_third_file();
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    let line = top_of(&shown, &step(9)).expect("a line of the file");
    let header = top_of(&shown, "src/f2.rs").expect("the file's header");
    let mut session = world.host.board.sessions.iter().find(|s| s.id == "s1").expect("the agent").clone();
    session.title = "Another title".into();
    let frames = world.host.event(|cursor| Message::SessionUpsert { cursor, session });
    world.deliver(frames);
    for frame in 0..4 {
        let shown = drawn(ctx, world, client, view, WIDE, Vec::new());
        stays(top_of(&shown, "src/f2.rs"), header, &format!("its header, frame {frame}"));
        stays(top_of(&shown, &step(9)), line, &format!("its line, frame {frame}"));
    }
    assert_eq!(focused_file(client, WORKTREE).as_deref(), Some("src/f2.rs"));
}

#[test]
fn typing_a_comment_leaves_the_code_scrolled_sideways() {
    let mut host = many_files(120);
    for file in host.files.values_mut() {
        for line in &mut file.hunks[0].lines {
            line.text.push_str(&"y".repeat(300));
        }
    }
    let (ctx, mut world, mut client, mut view, _) = diff_of(live_with(host), WIDE);
    let (ctx, world, client, view) = (&ctx, &mut world, &mut client, &mut view);
    drawn(ctx, world, client, view, WIDE, press(egui::Key::ArrowDown, egui::Modifiers::SHIFT));
    let shown = settled(ctx, world, client, view, WIDE);
    let left = |shown: &[(String, Rect)]| shown.iter().find(|(text, _)| text.starts_with(&step(9))).unwrap().1.left();
    let start = left(&shown);
    // The line runs on past the window, so not the middle of its text.
    let at = egui::pos2(start + 40.0, on_line(&shown, &step(9)).y);

    let wheel = egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: vec2(-400.0, 0.0),
        phase: egui::TouchPhase::Move,
        source: egui::MouseWheelSource::Unknown,
        modifiers: egui::Modifiers::NONE,
    };
    drawn(ctx, world, client, view, WIDE, vec![egui::Event::PointerMoved(at)]);
    drawn(ctx, world, client, view, WIDE, vec![wheel]);
    for _ in 0..40 {
        drawn(ctx, world, client, view, WIDE, Vec::new());
    }
    let shown = settled(ctx, world, client, view, WIDE);
    let scrolled = left(&shown);
    assert!(scrolled < start - 100.0, "scrolled sideways: {start} to {scrolled}");

    let shown = slow_click(ctx, world, client, view, at);
    assert_eq!(left(&shown), scrolled, "the click leaves it");
    let hint = top_of(&shown, COMMENT_BOX);
    assert!(hint.is_some(), "the box is on screen: {shown:?}");
    drawn(ctx, world, client, view, WIDE, text("n"));
    for _ in 0..40 {
        drawn(ctx, world, client, view, WIDE, Vec::new());
    }
    let shown = settled(ctx, world, client, view, WIDE);
    assert!(shown.iter().any(|(drawn, _)| drawn == "n"), "the box took the text: {shown:?}");
    assert_eq!(left(&shown), scrolled, "typing leaves it");
}

#[test]
fn client_pull_requests_open_from_terminal_chord_group_rows_and_load_with_arrows_and_enter() {
    use atomics_core::github::Group;
    use egui::{Key, Modifiers};
    for size in [WIDE, PHONE] {
        let mut host = fake_host::panels_host();
        host.board.host.pull_requests[0].group = Some(Group::Mine);
        host.board.host.pull_requests[1].group = Some(Group::Assigned);
        host.board.host.pull_requests[1].draft = Some(true);
        let (ctx, mut world, mut client, mut view) = at_a_terminal(host, size);
        settled(&ctx, &mut world, &mut client, &mut view, size);
        // A wide window routes the overlay's key; a phone on its board routes the hardware key.
        if view.compact {
            sized(&ctx, &mut world, &mut client, &mut view, size, press(Key::G, Modifiers::CTRL));
        } else {
            view.term.inputs.push(term_pane::TermInput::Key { name: "g".into(), mods: Modifiers::CTRL });
        }
        let shown = settled(&ctx, &mut world, &mut client, &mut view, size);
        assert!(client.pull_requests.open, "{size:?}: overlay {:?}", view.term.overlay);
        sized(&ctx, &mut world, &mut client, &mut view, size, text("list-only-input"));
        assert!(!world.host.terminals["s1"].typed.contains("list-only-input"));
        for label in ["Assigned to me", "My pull requests", "Other", "View diff", "Open"] {
            assert!(has(&shown, label), "{size:?}: {label}: {shown:?}");
        }
        assert!(shown.iter().any(|(s, _)| s.contains("contoso") && s.contains("feature → main")));
        assert!(shown.iter().any(|(s, _)| s.contains("Review the parser · Draft")));
        sized(&ctx, &mut world, &mut client, &mut view, size, press(Key::ArrowDown, Modifiers::NONE));
        sized(&ctx, &mut world, &mut client, &mut view, size, press(Key::ArrowUp, Modifiers::NONE));
        sized(&ctx, &mut world, &mut client, &mut view, size, press(Key::Enter, Modifiers::NONE));
        assert!(!client.pull_requests.open);
        assert_eq!(client.pr_view.as_ref().map(|p| p.0.as_str()), Some("pull:dev.azure.com/org/project/repo#8"));
        assert!(!world.host.terminals["s1"].typed.contains('g'));
    }
}

#[test]
fn client_pull_request_title_and_view_diff_button_both_request_the_host_diff() {
    for label in ["Review the parser", "View diff"] {
        let (ctx, mut world, mut client, mut view) = live_with(fake_host::panels_host());
        settled(&ctx, &mut world, &mut client, &mut view, WIDE);
        client.pull_requests.open = true;
        settled(&ctx, &mut world, &mut client, &mut view, WIDE);
        click_on(&ctx, &mut world, &mut client, &mut view, WIDE, label);
        assert!(client.pr_view.is_some());
        assert!(!client.pull_requests.open);
        assert!(
            world
                .host
                .host_commands
                .iter()
                .any(|a| matches!(a, HostAction::PullRequestDiff { key } if key == "pull:example/repo#7"))
        );
    }
}

#[test]
fn client_pull_request_shortcut_uses_host_configuration_and_escape_closes() {
    use egui::{Key, Modifiers};
    let mut host = fake_host::panels_host();
    host.board.host.management = fake_host::management().board.host.management;
    host.board.host.management.as_mut().unwrap().config["values"]["keys.github"] =
        json!({"value": "alt+p", "source": "main"});
    let (ctx, mut world, mut client, mut view) = live_with(host);
    settled(&ctx, &mut world, &mut client, &mut view, WIDE);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::G, Modifiers::CTRL));
    assert!(!client.pull_requests.open);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::P, Modifiers::ALT));
    assert!(client.pull_requests.open);
    sized(&ctx, &mut world, &mut client, &mut view, WIDE, press(Key::Escape, Modifiers::NONE));
    assert!(!client.pull_requests.open);
}
