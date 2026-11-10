//! Where a key lands, through the window's own frame ([`Desktop::paint`]) with a live terminal behind it: the
//! Tickets panel, its dialog and the windows over the frame keep what is theirs, and whatever the terminal
//! ends up with is exactly what it was meant to get.

use super::tests::{session, with_app};
use super::*;
use crate::mcp_client::tests::example_profile;
use atomics_core::tickets::{TicketCard, TicketConfig, TicketLink};

const NONE: egui::Modifiers = egui::Modifiers::NONE;
const SCREEN: (f32, f32) = (1500.0, 800.0);

fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(SCREEN.0, SCREEN.1))),
            events,
            ..Default::default()
        },
        |ui| app.paint(ui),
    );
    app.runtime.drain_actions();
    output
}

fn run(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) {
    frame(app, ctx, events).drop_without_applying_deltas();
}

/// A key as the window gets it: pressed in one frame, released in the next.
fn press(app: &mut Desktop<'_>, ctx: &egui::Context, key: Key, modifiers: egui::Modifiers) {
    let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
    run(app, ctx, vec![egui::Event::ModifiersChanged(modifiers), event(true)]);
    run(app, ctx, vec![event(false), egui::Event::ModifiersChanged(NONE)]);
}

fn chord(app: &mut Desktop<'_>, ctx: &egui::Context, binding: &str) {
    let chord = shortcut(binding).expect("a default binding parses");
    press(app, ctx, chord.logical_key, chord.modifiers);
}

fn typed(app: &mut Desktop<'_>, ctx: &egui::Context, text: &str) {
    for character in text.chars() {
        run(app, ctx, vec![egui::Event::Text(character.to_string())]);
    }
}

/// An agent whose terminal writes every byte it is sent to `input.bin` in its folder.
fn agent(app: &mut Desktop<'_>, root: &Path, id: &str) {
    std::fs::create_dir_all(root).unwrap();
    let terminal = Terminal::spawn(
        app.runtime.wake.clone(),
        app.runtime.theme.terminal_colors(),
        "/bin/sh".into(),
        vec!["-c".into(), "stty raw -echo; printf ready; exec cat > input.bin".into()],
        root,
        HashMap::new(),
        100,
    )
    .unwrap();
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
    while !terminal.screen_text(24).contains("ready") || !root.join("input.bin").exists() {
        assert!(Instant::now() < deadline, "terminal did not start");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut agent = session(id, AgentKind::Generic, AgentStatus::Working, root);
    agent.terminal = Some(terminal);
    app.runtime.sessions.push(agent);
}

/// A click on the text `label` where the last frame drew it.
fn click_on(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) {
    let output = frame(app, ctx, vec![]);
    let at = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Text(text) if text.galley.text() == label => Some(text.visual_bounding_rect().center()),
        _ => None,
    });
    output.drop_without_applying_deltas();
    let at = at.unwrap_or_else(|| panic!("{label} is not drawn"));
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: NONE,
    };
    run(app, ctx, vec![egui::Event::PointerMoved(at)]);
    run(app, ctx, vec![button(true)]);
    run(app, ctx, vec![button(false)]);
    run(app, ctx, vec![]);
}

/// The text of the next frame.
fn frame_text(app: &mut Desktop<'_>, ctx: &egui::Context) -> Vec<String> {
    let output = frame(app, ctx, vec![]);
    let text = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    text
}

/// The active agent has the keyboard, as after a click on its terminal.
fn at_the_terminal(app: &mut Desktop<'_>, ctx: &egui::Context) {
    app.client.focus_terminal = app.runtime.active.clone();
    run(app, ctx, vec![]);
    run(app, ctx, vec![]);
}

/// What the terminal read, once the keys of `then` reaching it prove it has read everything sent before.
fn received_before(app: &mut Desktop<'_>, ctx: &egui::Context, root: &Path, then: &str) -> Vec<u8> {
    at_the_terminal(app, ctx);
    typed(app, ctx, then);
    read_up_to(root, then)
}

fn read_up_to(root: &Path, then: &str) -> Vec<u8> {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(3));
    loop {
        let bytes = std::fs::read(root.join("input.bin")).unwrap();
        if let Some(at) = bytes.windows(then.len()).rposition(|window| window == then.as_bytes()) {
            return bytes[..at].to_vec();
        }
        assert!(Instant::now() < deadline, "the terminal never got {then:?}: {bytes:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn panel_focused(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(diff_view::focus_id()))
}

fn query_focused(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(egui::Id::NULL.with("tickets-query")))
}

/// A ticket system with no operations: nothing to wait for, the reader's state is the test's.
fn tickets_configured(app: &mut Desktop<'_>) {
    app.runtime.tickets.config = Some(Arc::new(TicketConfig { system: "demo".into(), ..Default::default() }));
}

/// The fake server's tickets, read through the host as the window reads them.
fn tickets_served(app: &mut Desktop<'_>, dir: &Path) {
    app.runtime.config.tickets = example_profile("meaning-example", &dir.join("fake.json"));
    app.runtime.apply_ticket_config();
}

fn settle(app: &mut Desktop<'_>, ctx: &egui::Context) {
    run(app, ctx, vec![]);
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
    loop {
        app.runtime.ticket_results();
        if app.runtime.tickets.pending == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "ticket jobs did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
    run(app, ctx, vec![]);
    run(app, ctx, vec![]);
}

fn card(number: u32) -> TicketCard {
    let link = TicketLink {
        system: "demo".into(),
        id: format!("id-{number}"),
        key: Some(format!("DEMO-{number}")),
        title: Some(format!("Ticket number {number}")),
        ..Default::default()
    };
    TicketCard { link, ..Default::default() }
}

/// The panel open on `count` results, the keyboard at the terminal.
fn results_open(app: &mut Desktop<'_>, ctx: &egui::Context, count: u32) {
    tickets_configured(app);
    app.send(Command::SetPanel(PanelMode::Tickets));
    app.send(Command::OpenDiff);
    run(app, ctx, vec![]);
    app.client.ui.tickets.found("anything", (1..=count).map(card).collect(), false);
    at_the_terminal(app, ctx);
}

fn top_of(output: &egui::FullOutput, word: &str) -> Option<f32> {
    output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Text(text) if text.galley.text() == word => Some(text.pos.y),
        _ => None,
    })
}

// ── Opening Tickets takes the keyboard ───────────────────────────────────────────────────────────────

#[test]
fn opening_tickets_puts_the_keyboard_in_the_search_field_and_enter_searches_there() {
    with_app(
        "app::key_tests::opening_tickets_puts_the_keyboard_in_the_search_field_and_enter_searches_there",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            agent(app, dir.path(), "agent");
            app.runtime.active = Some("agent".into());
            tickets_served(app, dir.path());
            at_the_terminal(app, ctx);
            let toggle = app.runtime.config.keys.toggle_tickets.clone();

            chord(app, ctx, &toggle);
            assert!(app.runtime.panel == PanelMode::Tickets && app.runtime.diff_open);
            assert!(query_focused(ctx), "the chord leaves the keyboard in the search field");
            typed(app, ctx, "DEMO-2");
            assert_eq!(app.client.ui.tickets.query, "DEMO-2");
            press(app, ctx, Key::Enter, NONE);
            settle(app, ctx);
            assert_eq!(app.client.ui.tickets.results[0].link.label(), "DEMO-2", "Enter searched");
            assert_eq!(received_before(app, ctx, dir.path(), "one"), b"", "query and Enter stayed out of the terminal");

            // Closing gives the terminal the keyboard back, and the switcher opens the panel the way the chord does.
            chord(app, ctx, &toggle);
            assert!(!app.runtime.diff_open);
            typed(app, ctx, "two");
            app.send(Command::ToggleView(PanelMode::Tickets));
            run(app, ctx, vec![]);
            assert!(query_focused(ctx), "the switcher leaves the keyboard in the search field too");
            typed(app, ctx, "x");
            assert_eq!(app.client.ui.tickets.query, "DEMO-2x");
            assert_eq!(received_before(app, ctx, dir.path(), "three"), b"onetwo");
        },
    );
}

// ── The search field's own keys ──────────────────────────────────────────────────────────────────────

#[test]
fn arrows_in_the_search_field_move_its_cursor_and_later_typing_lands_there() {
    with_app("app::key_tests::arrows_in_the_search_field_move_its_cursor_and_later_typing_lands_there", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        agent(app, dir.path(), "agent");
        app.runtime.active = Some("agent".into());
        tickets_configured(app);
        at_the_terminal(app, ctx);
        let toggle = app.runtime.config.keys.toggle_tickets.clone();
        chord(app, ctx, &toggle);

        typed(app, ctx, "abc");
        press(app, ctx, Key::ArrowLeft, NONE);
        assert!(query_focused(ctx), "← stays in the field");
        typed(app, ctx, "X");
        press(app, ctx, Key::ArrowRight, NONE);
        typed(app, ctx, "more");
        assert_eq!(app.client.ui.tickets.query, "abXcmore", "typed at the cursor, nothing lost");
        assert_eq!(received_before(app, ctx, dir.path(), "end"), b"");
    });
}

// ── Escape in a ticket ───────────────────────────────────────────────────────────────────────────────

#[test]
fn escape_in_a_ticket_returns_to_the_list_and_again_to_the_terminal() {
    with_app("app::key_tests::escape_in_a_ticket_returns_to_the_list_and_again_to_the_terminal", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        agent(app, dir.path(), "agent");
        app.runtime.active = Some("agent".into());
        results_open(app, ctx, 3);

        // Shift+↓ brings the keyboard to the list, Enter reads the selected result.
        press(app, ctx, Key::ArrowDown, egui::Modifiers::SHIFT);
        assert_eq!(app.client.ui.tickets.selected, Some(1));
        press(app, ctx, Key::Enter, NONE);
        assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), "DEMO-2");
        assert!(panel_focused(ctx), "an open ticket keeps the keys in the panel");

        press(app, ctx, Key::Escape, NONE);
        assert!(app.client.ui.tickets.read.is_none(), "Escape leaves the ticket");
        assert_eq!(app.client.ui.tickets.selected, Some(1), "with it still selected");
        assert!(panel_focused(ctx), "the list keeps the keys");
        assert!(app.runtime.diff_open);

        press(app, ctx, Key::Escape, NONE);
        assert!(!panel_focused(ctx), "from the list Escape returns to the terminal");
        assert!(app.runtime.diff_open);
        assert_eq!(received_before(app, ctx, dir.path(), "end"), b"", "neither Escape reached the terminal");
    });
}

// ── The Plan with chief dialog ───────────────────────────────────────────────────────────────────────

/// A ticket from the fake server read in the panel, the chief there to get the plan. Its label.
fn plan_ready(app: &mut Desktop<'_>, ctx: &egui::Context, dir: &Path) -> String {
    agent(app, dir, "agent");
    app.runtime.active = Some("agent".into());
    let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir.clone());
    chief.chief = true;
    app.runtime.sessions.push(chief);
    tickets_served(app, dir);
    app.send(Command::SetPanel(PanelMode::Tickets));
    app.send(Command::OpenDiff);
    settle(app, ctx);
    let link = app.client.ui.tickets.results.first().expect("the server lists its tickets").link.clone();
    let label = link.label().to_string();
    app.client.ui.tickets.open(link, false, 0.0, &mut app.client.ui.commands);
    settle(app, ctx);
    assert!(panel_focused(ctx), "the ticket holds the keys");
    label
}

/// Where the dialog draws its field's label, which sits near its middle.
fn dialog_label(output: &egui::FullOutput) -> Rect {
    let found = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Text(text) if text.galley.text().starts_with("Extra instructions") => {
            Some(text.visual_bounding_rect())
        }
        _ => None,
    });
    found.expect("the dialog is open")
}

#[test]
fn the_plan_dialog_is_centred_holds_the_keyboard_and_one_return_only_starts_the_plan() {
    with_app(
        "app::key_tests::the_plan_dialog_is_centred_holds_the_keyboard_and_one_return_only_starts_the_plan",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            plan_ready(app, ctx, dir.path());
            let plans = app.runtime.plans.len();
            let binding = app.runtime.config.keys.plan_with_chief.clone();

            chord(app, ctx, &binding);
            run(app, ctx, vec![]);
            assert!(app.client.ui.tickets.dialog);
            let output = frame(app, ctx, vec![]);
            let rect = dialog_label(&output);
            output.drop_without_applying_deltas();
            let middle = pos2(SCREEN.0 / 2.0, SCREEN.1 / 2.0);
            assert!(
                (rect.left() - middle.x).abs() < 240.0 && (rect.center().y - middle.y).abs() < 120.0,
                "the dialog is not in the middle: {rect:?} around {middle:?}"
            );

            typed(app, ctx, "abc");
            assert_eq!(app.client.ui.tickets.extra, "abc", "the field has the keyboard");
            press(app, ctx, Key::ArrowLeft, NONE);
            typed(app, ctx, "X");
            assert_eq!(app.client.ui.tickets.extra, "abXc", "and its arrows");
            assert_eq!(app.runtime.plans.len(), plans);

            press(app, ctx, Key::Enter, NONE);
            settle(app, ctx);
            assert_eq!(app.runtime.plans.len(), plans + 1, "Return started the one plan");
            let messages = app.runtime.hub.lock().unwrap().outbox.get("chief").cloned().unwrap_or_default();
            assert_eq!(messages.len(), 1, "{messages:?}");
            assert!(messages[0].contains("abXc"), "{messages:?}");
            assert!(!app.client.ui.tickets.dialog);
            // The window shows the new plan now, so the agent is off screen: its terminal is written to directly.
            let terminal = app.runtime.sessions.iter().find(|s| s.id == "agent").and_then(|s| s.terminal.as_ref());
            terminal.unwrap().write(b"end".to_vec());
            assert_eq!(read_up_to(dir.path(), "end"), b"", "none of it reached the terminal");
        },
    );
}

#[test]
fn escape_cancels_the_plan_dialog_and_leaves_the_keyboard_on_the_ticket() {
    with_app("app::key_tests::escape_cancels_the_plan_dialog_and_leaves_the_keyboard_on_the_ticket", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let label = plan_ready(app, ctx, dir.path());
        let plans = app.runtime.plans.len();
        let binding = app.runtime.config.keys.plan_with_chief.clone();

        chord(app, ctx, &binding);
        typed(app, ctx, "abc");
        press(app, ctx, Key::Escape, NONE);
        assert!(!app.client.ui.tickets.dialog, "Escape cancels");
        assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), label, "the ticket stays open");
        assert!(panel_focused(ctx), "and has the keys again");
        assert_eq!(app.runtime.plans.len(), plans);

        // Opened again it is a new draft; the panel behind it can't take the keyboard, nor its keys.
        chord(app, ctx, &binding);
        assert_eq!(app.client.ui.tickets.extra, "", "the old words are gone");
        ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
        typed(app, ctx, "typed");
        assert_eq!(app.client.ui.tickets.extra, "typed");
        press(app, ctx, Key::Escape, NONE);
        assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), label, "Escape was the dialog's");

        assert_eq!(received_before(app, ctx, dir.path(), "end"), b"");
    });
}

#[test]
fn return_with_the_keyboard_in_a_terminal_behind_the_dialog_goes_there_alone() {
    with_app(
        "app::key_tests::return_with_the_keyboard_in_a_terminal_behind_the_dialog_goes_there_alone",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            plan_ready(app, ctx, dir.path());
            let plans = app.runtime.plans.len();
            let binding = app.runtime.config.keys.plan_with_chief.clone();
            chord(app, ctx, &binding);

            // A click on the terminal took the keyboard out of the dialog.
            at_the_terminal(app, ctx);
            typed(app, ctx, "ls");
            press(app, ctx, Key::Enter, NONE);
            settle(app, ctx);
            assert_eq!(app.runtime.plans.len(), plans, "that Return was the terminal's");
            assert!(app.client.ui.tickets.dialog, "and the dialog waits");
            assert_eq!(received_before(app, ctx, dir.path(), "end"), b"ls\r");
        },
    );
}

#[test]
fn the_plan_chord_waits_for_the_panel_to_have_the_keyboard() {
    with_app("app::key_tests::the_plan_chord_waits_for_the_panel_to_have_the_keyboard", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        plan_ready(app, ctx, dir.path());
        let binding = app.runtime.config.keys.plan_with_chief.clone();
        at_the_terminal(app, ctx);
        chord(app, ctx, &binding);
        assert!(!app.client.ui.tickets.dialog, "typing to an agent, the chord doesn't pull the keyboard away");
        assert_eq!(received_before(app, ctx, dir.path(), "end"), b"", "and the terminal never sees it");
    });
}

// ── Escape closes the windows over the frame ─────────────────────────────────────────────────────────

fn window_takes_the_keys(
    app: &mut Desktop<'_>,
    ctx: &egui::Context,
    root: &Path,
    name: &str,
    open: fn(&mut Desktop<'_>),
    shown: fn(&Desktop<'_>) -> bool,
) {
    let start = std::fs::read(root.join("input.bin")).unwrap().len();
    at_the_terminal(app, ctx);
    open(app);
    run(app, ctx, vec![]);
    assert!(shown(app));
    run(app, ctx, vec![]);
    typed(app, ctx, "nope");
    press(app, ctx, Key::Escape, NONE);
    assert!(!shown(app), "Escape closes it");
    // The terminal that had the keyboard has it again, without being asked.
    typed(app, ctx, "back");

    // A click on the terminal behind the window gives it the keys, but Escape stays the window's.
    open(app);
    run(app, ctx, vec![]);
    at_the_terminal(app, ctx);
    typed(app, ctx, "=");
    press(app, ctx, Key::Escape, NONE);
    assert!(!shown(app), "Escape closes it from the terminal too");
    assert_eq!(&received_before(app, ctx, root, name)[start..], b"back=", "{name}");
}

#[test]
fn a_window_opened_by_its_button_gives_the_keyboard_back_to_the_terminal() {
    with_app("app::key_tests::a_window_opened_by_its_button_gives_the_keyboard_back_to_the_terminal", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        agent(app, dir.path(), "agent");
        app.runtime.active = Some("agent".into());
        at_the_terminal(app, ctx);
        // A click on the button takes the keyboard off the terminal.
        click_on(app, ctx, "Settings");
        assert!(app.client.ui.management.settings, "the click opened it");
        typed(app, ctx, "nope");
        press(app, ctx, Key::Escape, NONE);
        assert!(!app.client.ui.management.settings, "Escape closes it");
        // Straight to the keyboard, with no click or request to help.
        typed(app, ctx, "back");
        assert_eq!(read_up_to(dir.path(), "back"), b"", "only what was typed after it closed");
    });
}

#[test]
fn escape_closes_settings_and_the_project_picker_and_the_terminal_sees_neither() {
    with_app(
        "app::key_tests::escape_closes_settings_and_the_project_picker_and_the_terminal_sees_neither",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let root = dir.path();
            agent(app, root, "agent");
            app.runtime.active = Some("agent".into());
            window_takes_the_keys(
                app,
                ctx,
                root,
                "settings",
                |app| app.client.ui.management.settings = true,
                |app| app.client.ui.management.settings,
            );
            window_takes_the_keys(
                app,
                ctx,
                root,
                "picker",
                |app| app.send(Command::PickProject),
                |app| app.client.ui.management.project,
            );
            window_takes_the_keys(
                app,
                ctx,
                root,
                "remote",
                |app| app.client.ui.management.remote = true,
                |app| app.client.ui.management.remote,
            );
        },
    );
}

// ── The reader's other keys ────────────────────────────────────────────────────────────────────────────────

#[test]
fn the_results_page_with_the_page_keys_and_the_selection_stays_in_view() {
    with_app("app::key_tests::the_results_page_with_the_page_keys_and_the_selection_stays_in_view", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        agent(app, dir.path(), "agent");
        app.runtime.active = Some("agent".into());
        results_open(app, ctx, 40);
        press(app, ctx, Key::ArrowDown, egui::Modifiers::SHIFT);
        assert!(panel_focused(ctx));
        let top = |app: &mut Desktop<'_>, word: &str| {
            let output = frame(app, ctx, vec![]);
            let y = top_of(&output, word);
            output.drop_without_applying_deltas();
            y
        };
        // Row by row: the selected row stays on the screen.
        for _ in 0..25 {
            press(app, ctx, Key::ArrowDown, NONE);
        }
        assert_eq!(app.client.ui.tickets.selected, Some(26));
        let y = top(app, "DEMO-27").expect("the selected row is drawn");
        assert!((0.0..SCREEN.1).contains(&y), "the selected row is on screen: {y}");

        press(app, ctx, Key::Home, NONE);
        assert_eq!(app.client.ui.tickets.selected, Some(0));
        let home = top(app, "DEMO-1").expect("the first row is drawn");
        assert!((0.0..SCREEN.1).contains(&home), "Home shows the first row: {home}");

        // A page: the rows move up by most of the panel, the selection stays where it was.
        press(app, ctx, Key::PageDown, NONE);
        let paged = top(app, "DEMO-1");
        assert!(paged.is_none_or(|y| y < home - 300.0), "PageDown scrolls a page: {paged:?} from {home}");
        assert_eq!(app.client.ui.tickets.selected, Some(0), "paging leaves the selection");
        press(app, ctx, Key::PageUp, NONE);
        assert_eq!(top(app, "DEMO-1"), Some(home), "PageUp scrolls back");

        assert_eq!(received_before(app, ctx, dir.path(), "end"), b"");
    });
}

#[test]
fn alt_left_goes_back_only_while_the_panel_has_the_keyboard() {
    with_app("app::key_tests::alt_left_goes_back_only_while_the_panel_has_the_keyboard", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        agent(app, dir.path(), "agent");
        app.runtime.active = Some("agent".into());
        results_open(app, ctx, 3);
        app.client.ui.tickets.open(card(2).link, false, 0.0, &mut app.client.ui.commands);
        run(app, ctx, vec![]);
        assert!(panel_focused(ctx));

        // At the terminal: the key is the terminal's, the ticket stays.
        at_the_terminal(app, ctx);
        press(app, ctx, Key::ArrowLeft, egui::Modifiers::ALT);
        assert!(app.client.ui.tickets.read.is_some(), "the reader leaves a terminal's key alone");
        assert_eq!(received_before(app, ctx, dir.path(), "x"), b"\x1b[1;3D");

        // At the panel: back to the results, and the terminal gets nothing.
        ctx.memory_mut(|m| m.request_focus(diff_view::focus_id()));
        run(app, ctx, vec![]);
        press(app, ctx, Key::ArrowLeft, egui::Modifiers::ALT);
        assert!(app.client.ui.tickets.read.is_none(), "Alt+← goes back in the panel");
        assert_eq!(received_before(app, ctx, dir.path(), "y"), b"\x1b[1;3Dx");
    });
}

#[test]
fn an_agent_chord_shows_that_agents_ticket_and_find_stays_in_find() {
    with_app("app::key_tests::an_agent_chord_shows_that_agents_ticket_and_find_stays_in_find", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = (dir.path().join("first"), dir.path().join("second"));
        agent(app, &first, "first");
        agent(app, &second, "second");
        app.runtime.ungrouped = false;
        app.runtime.view = None;
        app.runtime.active = Some("first".into());
        let link = |key: &str| TicketLink {
            system: "demo".into(),
            id: key.into(),
            key: Some(key.into()),
            ..Default::default()
        };
        let task = |id: &str, session: &str, ticket: &str| Task {
            id: id.into(),
            session: Some(session.into()),
            ticket: Some(link(ticket)),
            ..Default::default()
        };
        app.runtime.plans[0].tasks = vec![task("a", "first", "TASK-1"), task("b", "second", "TASK-2")];
        app.runtime.selected_plan = 0;
        results_open(app, ctx, 3);
        let modifiers = if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL };

        press(app, ctx, Key::Num2, modifiers);
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        assert!(app.client.ui.tickets.read.is_none(), "Find stays in Find");
        assert!(panel_focused(ctx), "the panel has the keys");

        app.client.ui.tickets.open(link("OTHER-1"), false, 0.0, &mut app.client.ui.commands);
        run(app, ctx, vec![]);
        press(app, ctx, Key::Num1, modifiers);
        assert_eq!(app.runtime.active.as_deref(), Some("first"));
        assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), "TASK-1", "the reader follows the agent");
        press(app, ctx, Key::Num2, modifiers);
        assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), "TASK-2");
        assert!(panel_focused(ctx));
        assert!(app.runtime.panel == PanelMode::Tickets, "open Tickets keeps its place: no diff comes up");

        assert_eq!(received_before(app, ctx, &second, "end"), b"", "the chords stayed out of the terminal");
        assert_eq!(std::fs::read(first.join("input.bin")).unwrap(), b"");
    });
}

// ── Ctrl+N: the Nth visible agent and its diff against main ──────────────────────────────────────────────

fn agent_chord() -> egui::Modifiers {
    if cfg!(target_os = "macos") { egui::Modifiers::MAC_CMD } else { egui::Modifiers::CTRL }
}

/// An agent with a diff of its own against main, one file named after it.
fn agent_with_diff(app: &mut Desktop<'_>, root: &Path, id: &str) {
    agent(app, root, id);
    let text = (0..120).map(|n| format!("line {n}\n")).collect::<String>();
    let file = atomics_core::diff::FileDiff::untracked(format!("{id}.rs"), &text);
    app.runtime.diffs.entry(root.to_path_buf()).or_default().prepared = Some(Arc::new(diff_view::Prepared {
        diff: atomics_core::diff::WorktreeDiff {
            base_ref: "origin/main".into(),
            base: "0123456789ab".into(),
            files: vec![file],
            ..Default::default()
        },
        highlights: vec![],
        longest_line: 8,
        additions: 120,
        deletions: 0,
        all_files: Arc::default(),
    }));
    app.runtime.ungrouped = true;
}

/// Two agents with their diffs, the first active; the chief first in the list, which N doesn't count.
pub(super) fn two_agents(app: &mut Desktop<'_>, dir: &Path) -> (PathBuf, PathBuf) {
    let (first, second) = (dir.join("first"), dir.join("second"));
    app.runtime.sessions.clear();
    let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir.clone());
    chief.chief = true;
    app.runtime.sessions.push(chief);
    agent_with_diff(app, &first, "first");
    agent_with_diff(app, &second, "second");
    app.runtime.active = Some("first".into());
    (first, second)
}

/// `id` is active and its diff against main is on screen, independently of what holds the keyboard.
fn draws_the_diff_of(app: &mut Desktop<'_>, ctx: &egui::Context, id: &str, root: &Path) {
    assert_eq!(app.runtime.active.as_deref(), Some(id));
    assert_eq!(app.runtime.diff_target().as_deref(), Some(root));
    assert!(app.runtime.panel == PanelMode::Diff, "the panel shows the diff");
    assert!(app.runtime.diff_open, "the panel is open");
    run(app, ctx, vec![]);
    let output = frame(app, ctx, vec![]);
    let text: Vec<String> = output
        .shapes
        .iter()
        .filter_map(|shape| match &shape.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    assert!(text.iter().any(|t| t.contains(&format!("{id}.rs"))), "{id}'s diff is drawn: {text:?}");
    assert!(text.iter().any(|t| t.contains("vs origin/main")), "against main: {text:?}");
}

/// `id` is active, its diff against main is on screen, and the diff holds the keyboard, its keys included.
fn shows_the_diff_of(app: &mut Desktop<'_>, ctx: &egui::Context, id: &str, root: &Path) {
    draws_the_diff_of(app, ctx, id, root);
    assert!(panel_focused(ctx), "the diff holds the keyboard");
    for key in [Key::PageDown, Key::ArrowDown, Key::End, Key::Home] {
        press(app, ctx, key, NONE);
        assert!(panel_focused(ctx), "{key:?} stays with the diff");
    }
}

/// No byte has reached these terminals: the marker written now is the first thing each reads.
pub(super) fn untouched(app: &Desktop<'_>, agents: &[(&str, &Path)], marker: &str) {
    for (id, root) in agents {
        let terminal = app.runtime.sessions.iter().find(|s| s.id == *id).and_then(|s| s.terminal.as_ref());
        terminal.unwrap().write(marker.as_bytes().to_vec());
        assert_eq!(read_up_to(root, marker), b"", "{id}'s terminal got keys");
    }
}

#[test]
fn an_agent_chord_from_a_terminal_shows_the_nth_agents_diff_and_holds_the_keys_there() {
    with_app(
        "app::key_tests::an_agent_chord_from_a_terminal_shows_the_nth_agents_diff_and_holds_the_keys_there",
        |app, ctx| {
            for (split, open) in [
                (DiffSplit::Never, false),
                (DiffSplit::Never, true),
                (DiffSplit::Always, false),
                (DiffSplit::Always, true),
            ] {
                let dir = crate::test_support::tempdir().unwrap();
                let (first, second) = two_agents(app, dir.path());
                app.client.diff_split = Some(split);
                app.runtime.panel = PanelMode::Diff;
                app.runtime.diff_open = open;
                at_the_terminal(app, ctx);

                press(app, ctx, Key::Num2, agent_chord());
                shows_the_diff_of(app, ctx, "second", &second);
                untouched(app, &[("first", &first), ("second", &second)], "chord");

                // No such agent: the chord does nothing, and is still no terminal's.
                press(app, ctx, Key::Num9, agent_chord());
                assert_eq!(app.runtime.active.as_deref(), Some("second"));

                // Escape hands the keyboard to the agent the chord chose.
                press(app, ctx, Key::Escape, NONE);
                assert!(!panel_focused(ctx));
                typed(app, ctx, "typed");
                assert_eq!(read_up_to(&second, "typed"), b"chord");
                assert_eq!(std::fs::read(first.join("input.bin")).unwrap(), b"chord");
            }
        },
    );
}

#[test]
fn an_agent_chord_works_with_the_boards_dropdown_open_and_after_using_it() {
    with_app("app::key_tests::an_agent_chord_works_with_the_boards_dropdown_open_and_after_using_it", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = two_agents(app, dir.path());
        app.runtime.plans = ["Atlas", "Beacon"]
            .into_iter()
            .map(|goal| Plan {
                id: goal.to_lowercase(),
                goal: goal.into(),
                approved: true,
                tasks: vec![Task { id: "t".into(), title: "A task".into(), ..Task::default() }],
                ..Plan::default()
            })
            .collect();
        app.runtime.selected_plan = 0;
        at_the_terminal(app, ctx);

        // Open: the chord picks the agent, the menu goes, and the diff has the keyboard.
        click_on(app, ctx, "Boards");
        assert!(egui::Popup::is_any_open(ctx), "the menu is open");
        assert!(frame_text(app, ctx).iter().any(|text| text == "ACTIVE · 2"));
        press(app, ctx, Key::Num2, agent_chord());
        assert!(!egui::Popup::is_any_open(ctx), "the chord's diff is not left under a menu");
        shows_the_diff_of(app, ctx, "second", &second);
        untouched(app, &[("first", &first), ("second", &second)], "chord");

        // Past the last agent nothing happens, and the menu stays.
        click_on(app, ctx, "Boards");
        press(app, ctx, Key::Num9, agent_chord());
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        assert!(egui::Popup::is_any_open(ctx), "no agent, nothing moved");
        press(app, ctx, Key::Escape, NONE);

        // After using it: a board picked from the menu, then the chord counts that board's agents, here none.
        click_on(app, ctx, "Boards");
        click_on(app, ctx, "Beacon");
        assert_eq!(app.runtime.selected_plan, 1);
        assert!(app.runtime.plan_view && !egui::Popup::is_any_open(ctx));
        assert!(app.runtime.panel == PanelMode::Plan && app.runtime.diff_open, "a row shows its board");
        press(app, ctx, Key::Num1, agent_chord());
        assert_eq!(app.runtime.active, None, "Beacon has no agents, so the chord has none to pick");

        // And back on All, the first agent.
        app.send(Command::SetView(None));
        run(app, ctx, vec![]);
        press(app, ctx, Key::Num1, agent_chord());
        shows_the_diff_of(app, ctx, "first", &first);
    });
}

#[test]
fn an_agent_chord_from_the_board_shows_the_nth_agents_diff_and_holds_the_keys_there() {
    with_app(
        "app::key_tests::an_agent_chord_from_the_board_shows_the_nth_agents_diff_and_holds_the_keys_there",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            app.send(Command::SetPanel(PanelMode::Plan));
            app.send(Command::OpenDiff);
            run(app, ctx, vec![]);
            assert!(!panel_focused(ctx));

            press(app, ctx, Key::Num2, agent_chord());
            shows_the_diff_of(app, ctx, "second", &second);

            // On a plan's tab N counts that plan's agents: its only one is Ctrl+1.
            let third = dir.path().join("third");
            agent_with_diff(app, &third, "third");
            app.runtime.plans[0].goal = "Ship it".into();
            app.runtime.plans[0].tasks = vec![Task {
                id: "t".into(),
                title: "Do the thing".into(),
                session: Some("third".into()),
                status: TaskStatus::Running,
                ..Task::default()
            }];
            app.runtime.selected_plan = 0;
            app.send(Command::ShowPlan(0));
            app.send(Command::SetPanel(PanelMode::Plan));
            app.send(Command::OpenDiff);
            run(app, ctx, vec![]);
            ctx.memory_mut(|m| m.surrender_focus(diff_view::focus_id()));
            run(app, ctx, vec![]);
            press(app, ctx, Key::Num1, agent_chord());
            shows_the_diff_of(app, ctx, "third", &third);

            untouched(app, &[("first", &first), ("second", &second), ("third", &third)], "end");
        },
    );
}

#[test]
fn an_agent_chord_from_the_diff_shows_the_nth_agents_diff_and_holds_the_keys_there() {
    with_app(
        "app::key_tests::an_agent_chord_from_the_diff_shows_the_nth_agents_diff_and_holds_the_keys_there",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            at_the_terminal(app, ctx);
            press(app, ctx, Key::ArrowDown, egui::Modifiers::SHIFT);
            shows_the_diff_of(app, ctx, "first", &first);

            press(app, ctx, Key::Num2, agent_chord());
            shows_the_diff_of(app, ctx, "second", &second);
            press(app, ctx, Key::Num1, agent_chord());
            shows_the_diff_of(app, ctx, "first", &first);

            untouched(app, &[("first", &first), ("second", &second)], "end");
        },
    );
}

#[test]
fn an_agent_chord_after_closing_tickets_shows_the_diff_and_not_tickets_again() {
    with_app(
        "app::key_tests::an_agent_chord_after_closing_tickets_shows_the_diff_and_not_tickets_again",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            tickets_configured(app);
            at_the_terminal(app, ctx);
            let toggle = app.runtime.config.keys.toggle_tickets.clone();
            chord(app, ctx, &toggle);
            chord(app, ctx, &toggle);
            assert!(!app.runtime.diff_open, "Tickets is closed");

            press(app, ctx, Key::Num2, agent_chord());
            shows_the_diff_of(app, ctx, "second", &second);
            untouched(app, &[("first", &first), ("second", &second)], "end");
        },
    );
}

#[test]
fn an_agent_chord_with_settings_holding_the_keyboard_still_switches_the_agent() {
    with_app(
        "app::key_tests::an_agent_chord_with_settings_holding_the_keyboard_still_switches_the_agent",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            app.runtime.panel = PanelMode::Diff;
            app.runtime.diff_open = true;
            at_the_terminal(app, ctx);
            app.client.ui.management.settings = true;
            run(app, ctx, vec![]);
            run(app, ctx, vec![]);
            click_on(app, ctx, "Advanced");
            for _ in 0..8 {
                run(app, ctx, vec![]);
            }
            click_on(app, ctx, "Find a setting");
            typed(app, ctx, "x");
            assert_eq!(app.client.ui.management.filter, "x", "its field has the keyboard");
            let field = ctx.memory(|m| m.focused()).expect("Settings holds the keyboard");

            press(app, ctx, Key::Num2, agent_chord());
            assert_eq!(app.client.ui.management.filter, "x", "the chord isn't typed into it");
            assert!(app.client.ui.management.settings, "Settings stays open");
            draws_the_diff_of(app, ctx, "second", &second);
            assert!(ctx.memory(|m| m.has_focus(field)), "the open window keeps its field's keyboard");
            typed(app, ctx, "yz");
            run(app, ctx, vec![egui::Event::Paste(" pasted".into())]);
            assert_eq!(app.client.ui.management.filter, "xyz pasted", "typing and paste stay in Settings");
            untouched(app, &[("first", &first), ("second", &second)], "end");

            press(app, ctx, Key::Escape, NONE);
            assert!(!app.client.ui.management.settings, "and Escape still closes it");
        },
    );
}

#[test]
fn an_agent_chord_with_the_plan_dialog_holding_the_keyboard_still_switches_the_agent() {
    with_app(
        "app::key_tests::an_agent_chord_with_the_plan_dialog_holding_the_keyboard_still_switches_the_agent",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let label = plan_ready(app, ctx, dir.path());
            let other = dir.path().join("other");
            agent(app, &other, "other");
            let plans = app.runtime.plans.len();
            let binding = app.runtime.config.keys.plan_with_chief.clone();
            chord(app, ctx, &binding);
            typed(app, ctx, "draft");
            assert_eq!(app.client.ui.tickets.extra, "draft");

            // That agent has no ticket: the agent switches, the dialog and its draft wait with the keyboard.
            press(app, ctx, Key::Num2, agent_chord());
            assert_eq!(app.runtime.active.as_deref(), Some("other"));
            assert!(app.client.ui.tickets.dialog);
            assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), label);
            assert!(app.runtime.panel == PanelMode::Tickets, "open Tickets keeps its place");
            typed(app, ctx, "!");
            assert_eq!(app.client.ui.tickets.extra, "draft!", "the keyboard stayed in the dialog");
            assert_eq!(app.runtime.plans.len(), plans, "no plan started");
            untouched(app, &[("agent", dir.path()), ("other", &other)], "end");

            // That agent has a ticket: the open reader follows it, which ends the dialog over the old one.
            let link = TicketLink { id: "TASK-1".into(), key: Some("TASK-1".into()), ..Default::default() };
            app.runtime.plans.push(Plan::default());
            let at = app.runtime.plans.len() - 1;
            app.runtime.plans[at].status = PlanStatus::Open;
            app.runtime.plans[at].tasks =
                vec![Task { id: "t".into(), session: Some("agent".into()), ticket: Some(link), ..Default::default() }];
            press(app, ctx, Key::Num1, agent_chord());
            assert_eq!(app.runtime.active.as_deref(), Some("agent"));
            assert_eq!(app.client.ui.tickets.read.as_ref().unwrap().link.label(), "TASK-1");
            assert!(!app.client.ui.tickets.dialog);
            assert!(panel_focused(ctx), "the panel has the keys");
        },
    );
}

#[test]
fn an_agent_chord_with_tickets_search_holding_the_keyboard_still_switches_the_agent() {
    with_app(
        "app::key_tests::an_agent_chord_with_tickets_search_holding_the_keyboard_still_switches_the_agent",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            tickets_configured(app);
            at_the_terminal(app, ctx);
            let toggle = app.runtime.config.keys.toggle_tickets.clone();
            chord(app, ctx, &toggle);
            typed(app, ctx, "ab");
            assert!(query_focused(ctx));

            press(app, ctx, Key::Num2, agent_chord());
            assert_eq!(app.runtime.active.as_deref(), Some("second"));
            assert_eq!(app.client.ui.tickets.query, "ab", "the chord isn't typed into the field");
            assert!(app.client.ui.tickets.read.is_none(), "Find stays in Find");
            assert!(app.runtime.panel == PanelMode::Tickets, "open Tickets keeps its place");
            assert!(panel_focused(ctx), "the keyboard moves from the field to the list");
            untouched(app, &[("first", &first), ("second", &second)], "end");
        },
    );
}

#[test]
fn an_agent_chord_in_editor_mode_focuses_that_agents_editor() {
    with_app("app::key_tests::an_agent_chord_in_editor_mode_focuses_that_agents_editor", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = two_agents(app, dir.path());
        app.runtime.panel = PanelMode::Editor;
        app.runtime.diff_open = true;
        app.runtime.config.editor.command =
            vec!["/bin/sh".into(), "-c".into(), "stty raw -echo; printf ready; exec cat > editor.bin".into()];
        app.runtime.config.editor.startup_keys.clear();
        app.runtime.config.editor.open_keys.clear();
        at_the_terminal(app, ctx);

        let started = Instant::now();
        press(app, ctx, Key::Num2, agent_chord());
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        assert!(app.runtime.panel == PanelMode::Editor, "editor mode keeps the editor, not the diff");
        assert!(app.runtime.editors.contains_key(&second), "that agent's editor started");
        assert!(ctx.memory(|m| m.has_focus(app.client.ui.editor_id.unwrap())), "and holds the keyboard");

        let deadline = started + crate::test_support::scaled_timeout(Duration::from_secs(4));
        while started.elapsed() < Duration::from_millis(1500) || !second.join("editor.bin").exists() {
            assert!(Instant::now() < deadline, "the editor did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        typed(app, ctx, "typed");
        let bytes = loop {
            let bytes = std::fs::read(second.join("editor.bin")).unwrap();
            if bytes.ends_with(b"typed") || Instant::now() >= deadline {
                break bytes;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(bytes.ends_with(b"typed"), "the editor got the keys: {bytes:?}");
        untouched(app, &[("first", &first), ("second", &second)], "end");
    });
}

// ── With nothing else drawing a frame ────────────────────────────────────────────────────────────────────
//
// An idle window draws nothing. egui follows a frame that had input with exactly one more, and a widget's focus
// lock only takes hold from its second frame with the focus, before that frame's keys are read. So whatever moves
// the keyboard has to ask for the frames it needs.

/// What a window nobody touches draws: `events`, then only the frames the last one asked for at once. Its text.
fn only_asked(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<String> {
    let mut output = frame(app, ctx, events);
    for _ in 0..240 {
        let asked = output.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.repaint_delay.is_zero());
        if !asked {
            let text = output.shapes.iter().filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            });
            let text = text.collect();
            output.drop_without_applying_deltas();
            return text;
        }
        output.drop_without_applying_deltas();
        output = frame(app, ctx, vec![]);
    }
    panic!("the window never goes idle");
}

/// A key alone: pressed and released in one frame, so no release draws a frame after it.
fn key_alone(app: &mut Desktop<'_>, ctx: &egui::Context, key: Key, modifiers: egui::Modifiers) {
    let event = |pressed| egui::Event::Key { key, physical_key: Some(key), pressed, repeat: false, modifiers };
    only_asked(
        app,
        ctx,
        vec![egui::Event::ModifiersChanged(modifiers), event(true), event(false), egui::Event::ModifiersChanged(NONE)],
    );
}

fn chord_alone(app: &mut Desktop<'_>, ctx: &egui::Context, binding: &str) {
    let chord = shortcut(binding).expect("a default binding parses");
    key_alone(app, ctx, chord.logical_key, chord.modifiers);
}

fn text_alone(app: &mut Desktop<'_>, ctx: &egui::Context, text: &str) {
    only_asked(app, ctx, vec![egui::Event::Text(text.into())]);
}

/// A click on the text `label` right of `left`, where the last frame drew it.
fn click_alone(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str, left: f32) {
    let output = frame(app, ctx, vec![]);
    let at = output.shapes.iter().find_map(|shape| match &shape.shape {
        egui::Shape::Text(text) if text.galley.text() == label => {
            Some(text.visual_bounding_rect().center()).filter(|at| at.x > left)
        }
        _ => None,
    });
    output.drop_without_applying_deltas();
    let at = at.unwrap_or_else(|| panic!("{label} is not drawn right of {left}"));
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: NONE,
    };
    only_asked(app, ctx, vec![egui::Event::PointerMoved(at)]);
    only_asked(app, ctx, vec![button(true)]);
    only_asked(app, ctx, vec![button(false)]);
}

/// The terminal at `root` has the keyboard for good: its keys reach it, the ones egui would otherwise use to move
/// the focus included, and the keyboard stays. What it read before them is `before`.
fn holds_the_next_keys(app: &mut Desktop<'_>, ctx: &egui::Context, root: &Path, before: &[u8]) {
    key_alone(app, ctx, Key::ArrowUp, NONE);
    key_alone(app, ctx, Key::ArrowUp, NONE);
    key_alone(app, ctx, Key::Tab, NONE);
    text_alone(app, ctx, "end");
    let mut expected = before.to_vec();
    expected.extend_from_slice(b"\x1b[A\x1b[A\t");
    assert_eq!(read_up_to(root, "end"), expected, "the terminal lost keys");
}

#[test]
fn the_desktops_settings_offer_edit_config_and_name_this_machine() {
    with_app("app::key_tests::the_desktops_settings_offer_edit_config_and_name_this_machine", |app, ctx| {
        app.client.ui.management.settings = true;
        run(app, ctx, vec![]);
        let draws = |app: &mut Desktop<'_>, ctx: &egui::Context, label: &str| {
            let output = frame(app, ctx, vec![]);
            let found = output
                .shapes
                .iter()
                .any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().starts_with(label)));
            output.drop_without_applying_deltas();
            found
        };
        click_on(app, ctx, "Theme");
        run(app, ctx, vec![]);
        assert!(draws(app, ctx, "This machine only (config.d/"));
        assert!(draws(app, ctx, "claude"), "the themes are listed");
        for _ in 0..3 {
            run(app, ctx, vec![]);
        }
        click_on(app, ctx, "Shortcuts");
        for _ in 0..8 {
            run(app, ctx, vec![]);
        }
        assert!(!draws(app, ctx, "The desktop app on the host also has these"), "this is it");
        click_on(app, ctx, "Shortcuts");
        for _ in 0..8 {
            run(app, ctx, vec![]);
        }
        click_on(app, ctx, "Advanced");
        for _ in 0..8 {
            run(app, ctx, vec![]);
        }
        assert!(draws(app, ctx, "Edit config"));
        assert!(!draws(app, ctx, "Edit config opens the file"), "that is a remote client's note");
    });
}

#[test]
fn settings_opened_by_its_button_leaves_the_terminal_ready_when_escape_closes_it() {
    with_app(
        "app::key_tests::settings_opened_by_its_button_leaves_the_terminal_ready_when_escape_closes_it",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            agent(app, dir.path(), "agent");
            app.runtime.active = Some("agent".into());
            // Remember a static section so discovery's spinner does not keep this keyboard-only test drawing.
            click_on(app, ctx, "Settings");
            click_on(app, ctx, "Theme");
            press(app, ctx, Key::Escape, NONE);
            at_the_terminal(app, ctx);
            only_asked(app, ctx, vec![]);
            click_alone(app, ctx, "Settings", 0.0);
            assert!(app.client.ui.management.settings);
            key_alone(app, ctx, Key::Escape, NONE);
            assert!(!app.client.ui.management.settings);
            holds_the_next_keys(app, ctx, dir.path(), b"");
        },
    );
}

#[test]
fn a_click_on_a_tiles_title_leaves_its_terminal_ready_for_the_next_key() {
    with_app("app::key_tests::a_click_on_a_tiles_title_leaves_its_terminal_ready_for_the_next_key", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = two_agents(app, dir.path());
        only_asked(app, ctx, vec![]);
        // Right of the sidebar, which draws the same name.
        click_alone(app, ctx, "second", 260.0);
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        holds_the_next_keys(app, ctx, &second, b"");
        assert_eq!(std::fs::read(first.join("input.bin")).unwrap(), b"");
    });
}

#[test]
fn a_finished_sign_in_with_no_input_leaves_the_search_field_ready_for_the_next_key() {
    with_app(
        "app::key_tests::a_finished_sign_in_with_no_input_leaves_the_search_field_ready_for_the_next_key",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            agent(app, dir.path(), "agent");
            app.runtime.active = Some("agent".into());
            tickets_configured(app);
            app.send(Command::SetPanel(PanelMode::Tickets));
            app.send(Command::OpenDiff);
            only_asked(app, ctx, vec![]);
            // What the sign-in does when it finishes, on no key or click of anyone's.
            app.runtime.tickets.panel.focus_query = true;
            only_asked(app, ctx, vec![]);
            assert!(query_focused(ctx));
            key_alone(app, ctx, Key::Escape, NONE);
            assert!(panel_focused(ctx), "Escape leaves the box for the list, rather than losing the keyboard");
            assert_eq!(received_before(app, ctx, dir.path(), "end"), b"");
        },
    );
}

#[test]
fn an_activate_with_no_input_asks_for_the_frames_that_hand_over_the_keyboard() {
    with_app(
        "app::key_tests::an_activate_with_no_input_asks_for_the_frames_that_hand_over_the_keyboard",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            only_asked(app, ctx, vec![]);
            app.send(Command::Activate { session: "second".into(), open_diff: false });
            assert!(ctx.has_requested_repaint(), "the terminal takes the keyboard on a frame someone has to ask for");
            only_asked(app, ctx, vec![]);
            holds_the_next_keys(app, ctx, &second, b"");
            assert_eq!(std::fs::read(first.join("input.bin")).unwrap(), b"");
        },
    );
}

#[test]
fn the_plans_reply_shows_the_new_plan_with_no_input_after_the_key() {
    with_app("app::key_tests::the_plans_reply_shows_the_new_plan_with_no_input_after_the_key", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let label = plan_ready(app, ctx, dir.path());
        let plans = app.runtime.plans.len();
        let binding = app.runtime.config.keys.plan_with_chief.clone();
        chord_alone(app, ctx, &binding);
        text_alone(app, ctx, "abc");
        assert_eq!(app.client.ui.tickets.extra, "abc", "the dialog has the keyboard");

        let key = |pressed| egui::Event::Key {
            key: Key::Enter,
            physical_key: Some(Key::Enter),
            pressed,
            repeat: false,
            modifiers: NONE,
        };
        let mut text = only_asked(app, ctx, vec![key(true), key(false)]);
        assert_eq!(app.runtime.plans.len(), plans + 1, "one Return, one plan");
        // A slower reply comes on a frame its request waits for, which no input draws either.
        for _ in 0..200 {
            if !app.client.ui.tickets.dialog {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            app.runtime.ticket_results();
            text = only_asked(app, ctx, vec![]);
        }
        assert!(!app.client.ui.tickets.dialog, "the reply closed the dialog");
        let shown = |t: &String| t.starts_with(&format!("{label}: ")) || t.starts_with(&format!("{label} ·"));
        assert!(text.iter().any(shown), "the new plan is on screen, with no input after the key: {text:?}");
    });
}

#[test]
fn the_shortcut_query_and_enter_and_the_dialog_work_with_nothing_else_drawing() {
    with_app(
        "app::key_tests::the_shortcut_query_and_enter_and_the_dialog_work_with_nothing_else_drawing",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            agent(app, dir.path(), "agent");
            app.runtime.active = Some("agent".into());
            let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir.clone());
            chief.chief = true;
            app.runtime.sessions.push(chief);
            tickets_served(app, dir.path());
            at_the_terminal(app, ctx);
            only_asked(app, ctx, vec![]);
            let toggle = app.runtime.config.keys.toggle_tickets.clone();
            let plan = app.runtime.config.keys.plan_with_chief.clone();

            chord_alone(app, ctx, &toggle);
            assert!(query_focused(ctx), "the shortcut lands in the field straight away");
            for character in ["D", "E", "M", "O", "-", "2"] {
                text_alone(app, ctx, character);
            }
            assert_eq!(app.client.ui.tickets.query, "DEMO-2");
            key_alone(app, ctx, Key::ArrowLeft, NONE);
            assert!(query_focused(ctx), "← stays in the field");
            key_alone(app, ctx, Key::ArrowRight, NONE);
            key_alone(app, ctx, Key::Enter, NONE);
            settle(app, ctx);
            assert_eq!(app.client.ui.tickets.results[0].link.label(), "DEMO-2");

            // Into the list and the ticket, then the dialog, with no frame between that wasn't asked for.
            key_alone(app, ctx, Key::ArrowDown, egui::Modifiers::SHIFT);
            assert!(panel_focused(ctx));
            app.client.ui.tickets.selected = Some(0);
            key_alone(app, ctx, Key::Enter, NONE);
            settle(app, ctx);
            only_asked(app, ctx, vec![]);
            assert!(panel_focused(ctx), "the ticket holds the keys");
            chord_alone(app, ctx, &plan);
            text_alone(app, ctx, "abc");
            assert_eq!(app.client.ui.tickets.extra, "abc", "the dialog's field has the keyboard straight away");
            key_alone(app, ctx, Key::ArrowLeft, NONE);
            text_alone(app, ctx, "X");
            assert_eq!(app.client.ui.tickets.extra, "abXc", "and its arrows");
            let text = {
                key_alone(app, ctx, Key::Escape, NONE);
                only_asked(app, ctx, vec![])
            };
            assert!(!text.iter().any(|t| t.starts_with("Extra instructions")), "Escape closed it at once: {text:?}");
            assert!(panel_focused(ctx), "the ticket has the keys again");

            // Escape in the ticket: the list is on screen with no input after the key.
            let label = app.client.ui.tickets.read.as_ref().unwrap().link.label().to_string();
            let before: Vec<String> = only_asked(app, ctx, vec![]);
            assert!(before.iter().any(|t| t == "← Back"), "reading: {before:?}");
            only_asked(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: Some(Key::Escape),
                    pressed: true,
                    repeat: false,
                    modifiers: NONE,
                }],
            );
            let list = only_asked(app, ctx, vec![]);
            assert!(!list.iter().any(|t| t == "← Back"), "{label} is closed");
            assert!(list.iter().any(|t| t == &label), "and the list is drawn: {list:?}");
            assert!(panel_focused(ctx));
            untouched(app, &[("agent", dir.path())], "end");
        },
    );
}

#[test]
fn an_agent_chord_holds_the_diff_with_nothing_else_drawing() {
    with_app("app::key_tests::an_agent_chord_holds_the_diff_with_nothing_else_drawing", |app, ctx| {
        let dir = crate::test_support::tempdir().unwrap();
        let (first, second) = two_agents(app, dir.path());
        app.runtime.panel = PanelMode::Diff;
        app.runtime.diff_open = true;
        at_the_terminal(app, ctx);
        only_asked(app, ctx, vec![]);

        // From a terminal: the first key after the chord is already the diff's.
        key_alone(app, ctx, Key::Num2, agent_chord());
        assert_eq!(app.runtime.active.as_deref(), Some("second"));
        assert!(panel_focused(ctx), "the diff holds the keyboard");
        for key in [Key::ArrowDown, Key::ArrowUp, Key::PageDown] {
            key_alone(app, ctx, key, NONE);
            assert!(panel_focused(ctx), "{key:?} stays with the diff");
        }
        // Escape is the diff's: it gives the keyboard to the terminal without reaching it.
        key_alone(app, ctx, Key::Escape, NONE);
        assert!(!panel_focused(ctx));
        holds_the_next_keys(app, ctx, &second, b"");

        // From the diff: the chord, then Escape straight away.
        key_alone(app, ctx, Key::ArrowDown, egui::Modifiers::SHIFT);
        assert!(panel_focused(ctx));
        key_alone(app, ctx, Key::Num1, agent_chord());
        assert_eq!(app.runtime.active.as_deref(), Some("first"));
        assert!(panel_focused(ctx), "the diff holds the keyboard");
        key_alone(app, ctx, Key::Escape, NONE);
        assert!(!panel_focused(ctx));
        holds_the_next_keys(app, ctx, &first, b"");
    });
}

#[test]
fn opening_a_project_from_the_picker_does_not_go_back_to_the_session_it_left() {
    with_app(
        "app::key_tests::opening_a_project_from_the_picker_does_not_go_back_to_the_session_it_left",
        |app, ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            let (first, second) = two_agents(app, dir.path());
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            for args in [
                vec!["init", "-q"],
                vec!["-c", "user.email=q@q", "-c", "user.name=q", "commit", "-q", "--allow-empty", "-m", "one"],
            ] {
                assert!(std::process::Command::new("git").args(args).current_dir(&repo).status().unwrap().success());
            }
            let repo = repo.canonicalize().unwrap();
            at_the_terminal(app, ctx);
            assert_eq!(app.runtime.active.as_deref(), Some("first"));

            // Opened from the Project menu, whose click has already taken the keyboard off the terminal.
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            app.send(Command::PickProject);
            let path = repo.to_string_lossy().into_owned();
            app.client.ui.management.folder = Some(path.clone());
            app.send(Command::Manage(atomics_core::ops::HostAction::BrowseFolder { path }));
            only_asked(app, ctx, vec![]);
            only_asked(app, ctx, vec![]);
            click_alone(app, ctx, "Open this project or worktree", 0.0);
            only_asked(app, ctx, vec![]);
            assert!(!app.client.ui.management.project, "the picker closed");
            assert!(app.runtime.open_projects.contains(&repo), "the project opened");
            assert!(app.runtime.active.is_none(), "the window moved to it, and no old session took the keyboard back");
            assert_eq!(app.runtime.diff_root.as_deref(), Some(repo.as_path()));
            untouched(app, &[("first", &first), ("second", &second)], "end");
        },
    );
}
