//! The calm full window: the start screen and the chief it holds, BOARDS in the sidebar in the dropdown's order, the
//! board chords, New work, and the closed sessions under their board.

use super::tests::{repository, session, with_app};
use super::*;
use atomics_core::config::ChiefDock;
use atomics_core::store::SessionRecord;

/// A closed session's record: what resuming it needs.
fn record(pane: &str, title: &str, cwd: &Path) -> SessionRecord {
    SessionRecord {
        thread: String::new(),
        chief: false,
        pane: Some(pane.into()),
        claude_session_id: format!("conversation-{pane}"),
        agent: "claude".into(),
        cwd: cwd.into(),
        title: title.into(),
        last_prompt: None,
        updated: 0,
        sandbox: None,
        managed_worktree: None,
        named: false,
        unsaved: None,
    }
}

const SIZE: egui::Vec2 = egui::vec2(1440.0, 900.0);

thread_local! {
    /// While [`calm_screens`] renders, every frame's textures, so that what it paints has its glyphs.
    static RASTER: std::cell::RefCell<Option<crate::test_raster::Raster>> = const { std::cell::RefCell::new(None) };
}

/// One frame of the whole window, with `events`.
fn frame(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) {
    let clock = egui::Id::NULL.with("calm-test-time");
    let time = ctx.data_mut(|data| {
        let value = data.get_temp::<f64>(clock).unwrap_or(0.0) + 0.1;
        data.insert_temp(clock, value);
        value
    });
    ctx.data_mut(|data| data.remove::<Vec<(String, Rect)>>(egui::Id::NULL.with("simple-shown")));
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)),
            time: Some(time),
            events,
            ..Default::default()
        },
        |ui| app.paint(ui),
    );
    app.runtime.drain_actions();
    RASTER.with_borrow_mut(|raster| {
        if let Some(raster) = raster {
            raster.feed(&output);
        }
    });
    output.drop_without_applying_deltas();
}

/// Where each named part was drawn in the last frame.
fn shown(ctx: &egui::Context) -> Vec<(String, Rect)> {
    ctx.data(|data| data.get_temp(egui::Id::NULL.with("simple-shown")).unwrap_or_default())
}

/// The window keeps a frame's rows for the frames after it, and reads them anew once the user did something. Where the
/// bar's buttons are, and which they are, is the same on both.
#[test]
fn a_press_and_its_release_hit_the_same_button_of_the_bar_whatever_was_read_anew_between() {
    with_app(
        "app::calm_tests::a_press_and_its_release_hit_the_same_button_of_the_bar_whatever_was_read_anew_between",
        |app, ctx| {
            app.runtime.drawing = true;
            frame(app, ctx, vec![]);
            frame(app, ctx, vec![]);
            let at = |ctx: &egui::Context, name: &str| {
                let items: Vec<(String, Rect)> =
                    ctx.data(|data| data.get_temp(egui::Id::NULL.with("topbar-items")).unwrap_or_default());
                items.into_iter().find(|(item, _)| item == name).unwrap_or_else(|| panic!("{name} is not drawn")).1
            };
            let pos = at(ctx, "Settings").center();
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            assert!(!app.client.ui.management.settings);
            frame(app, ctx, vec![egui::Event::PointerMoved(pos)]);
            // The press: these rows are the last frame's. The release: they were read anew.
            frame(app, ctx, vec![button(true)]);
            let next = app.runtime.shown.get_mut().next();
            *app.runtime.shown.get_mut() = next;
            frame(app, ctx, vec![button(false)]);
            assert!(app.client.ui.management.settings, "the click opens Settings");
        },
    );
}

fn click(app: &mut Desktop<'_>, ctx: &egui::Context, name: &str) {
    frame(app, ctx, vec![]);
    let rect =
        shown(ctx).into_iter().find(|(shown, _)| shown == name).unwrap_or_else(|| panic!("{name} is not drawn")).1;
    for pressed in [true, false] {
        let pos = rect.center();
        frame(
            app,
            ctx,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
    frame(app, ctx, vec![]);
}

fn plan(id: &str, status: PlanStatus, approved: bool, created_at: u64) -> Plan {
    Plan {
        id: id.into(),
        goal: format!("Goal {id}"),
        status,
        approved,
        created_at,
        tasks: vec![Task { id: "work".into(), title: "Work".into(), ..Task::default() }],
        ..Plan::default()
    }
}

#[test]
fn the_start_screen_holds_the_chief_until_it_is_asked_for() {
    with_app("app::calm_tests::the_start_screen_holds_the_chief_until_it_is_asked_for", |app, ctx| {
        let mut chief = session("chief", AgentKind::Claude, AgentStatus::Idle, &app.runtime.launch_dir.clone());
        chief.chief = true;
        app.runtime.sessions.push(chief);
        app.runtime.chief_open = true;
        app.runtime.store.chief_dock = Some(ChiefDock::Docked);
        for _ in 0..4 {
            frame(app, ctx, vec![]);
        }
        let home = &app.client.ui.home;
        assert!(home.shown && home.held, "nothing happens, so the chief waits behind the start screen");
        assert!(
            shown(ctx).iter().any(|(name, _)| name == "home-composer"),
            "the start screen's composer is the way in"
        );
        assert!(!egui::Popup::is_any_open(ctx), "no menu, the Boards dropdown among them, opens by itself");
        // Laid out as though docked: showing a board from here keeps the chief, which then docks beside it.
        assert!(app.runtime.last_layout.is_some_and(|layout| layout.dock_width > 0.0));
        app.send(Command::ToggleChief);
        assert!(app.runtime.chief_open, "the chief toggle shows the held chief rather than hiding it");
        frame(app, ctx, vec![]);
        assert!(!app.client.ui.home.held && app.client.ui.home.summoned);
        app.send(Command::ToggleChief);
        assert!(!app.runtime.chief_open, "once it shows, the toggle hides it as before");
        // A chief at work shows wherever it is.
        app.send(Command::ToggleChief);
        app.client.ui.home.summoned = false;
        app.runtime.sessions[0].status = AgentStatus::Working;
        frame(app, ctx, vec![]);
        assert!(app.client.ui.home.shown && !app.client.ui.home.held);
    });
}

#[test]
fn the_sidebar_lists_the_boards_as_the_dropdown_does_and_the_board_chords_step_through_them() {
    with_app(
        "app::calm_tests::the_sidebar_lists_the_boards_as_the_dropdown_does_and_the_board_chords_step_through_them",
        |app, ctx| {
            app.runtime.plans = vec![
                plan("active", PlanStatus::Open, true, 1),
                plan("waiting", PlanStatus::Open, false, 2),
                plan("done", PlanStatus::Completed, true, 3),
                plan("archived", PlanStatus::Archived, true, 4),
                plan("later", PlanStatus::Open, true, 5),
            ];
            frame(app, ctx, vec![]);
            let listed = app.draw(|backend, _| atomics_ui::boards::listed(backend));
            let order: Vec<usize> = listed.steps().map(|(index, _)| *index).collect();
            assert_eq!(order, [1, 0, 4, 2], "needs you, the other open boards, then the done ones");
            assert_eq!(listed.history.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [3]);
            let mut rows: Vec<(usize, f32)> = shown(ctx)
                .into_iter()
                .filter_map(|(name, rect)| Some((name.strip_prefix("board:")?.parse().ok()?, rect.top())))
                .collect();
            rows.sort_by(|a, b| a.1.total_cmp(&b.1));
            assert_eq!(rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(), order, "the sidebar's order");
            assert!(!shown(ctx).iter().any(|(name, _)| name.starts_with("history:")), "History opens folded");
            // The board chords walk the same order, wrapping round, and a click on a row shows its board.
            let [next, previous] = atomics_ui::window_keys::board_chords(ctx.os());
            let key = |chord: egui::KeyboardShortcut| egui::Event::Key {
                key: chord.logical_key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: chord.modifiers,
            };
            for expected in [1, 0, 4, 2, 1] {
                frame(app, ctx, vec![key(next)]);
                assert!(app.runtime.plan_view && app.runtime.selected_plan == expected, "next to {expected}");
            }
            frame(app, ctx, vec![key(previous)]);
            assert_eq!(app.runtime.selected_plan, 2);
            assert_eq!(app.runtime.panel, PanelMode::Plan, "a finished board shows read-only");
            click(app, ctx, "board:4");
            assert!(app.runtime.plan_view && app.runtime.selected_plan == 4 && app.runtime.panel == PanelMode::Plan);
            // It stands first and in full; the others keep their order below it, a line each.
            let rows: Vec<(usize, egui::Rect)> = shown(ctx)
                .into_iter()
                .filter_map(|(name, rect)| Some((name.strip_prefix("board:")?.parse().ok()?, rect)))
                .collect();
            assert_eq!(rows.iter().map(|(index, _)| *index).collect::<Vec<_>>(), [4, 1, 0, 2]);
            assert!(rows.windows(2).all(|pair| pair[0].1.bottom() <= pair[1].1.top()), "{rows:?}");
            assert!(rows[1..].iter().all(|(_, row)| row.height() < rows[0].1.height() * 0.6), "{rows:?}");
        },
    );
}

#[test]
fn new_work_goes_back_to_the_start_screen_until_the_window_goes_elsewhere() {
    with_app("app::calm_tests::new_work_goes_back_to_the_start_screen_until_the_window_goes_elsewhere", |app, ctx| {
        app.runtime.plans = vec![plan("one", PlanStatus::Open, true, 1), plan("two", PlanStatus::Open, true, 2)];
        let worker = session("worker", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir.clone());
        app.runtime.sessions.push(worker);
        click(app, ctx, "board:0");
        assert!(!app.client.ui.home.shown && app.runtime.plan_view, "a board is on screen");
        click(app, ctx, "new-work");
        assert!(app.client.ui.home.shown && !app.runtime.plan_view, "New work shows the start screen");
        assert!(shown(ctx).iter().any(|(name, _)| name == "home-board:0"), "with the boards to continue");
        // An agent that runs does not take the start screen's place; going to a board does.
        frame(app, ctx, vec![]);
        assert!(app.client.ui.home.shown);
        click(app, ctx, "home-board:1");
        assert!(!app.client.ui.home.shown && app.runtime.plan_view && app.runtime.selected_plan == 1);
    });
}

#[test]
fn the_older_done_boards_fold_away_but_for_the_one_on_screen() {
    with_app("app::calm_tests::the_older_done_boards_fold_away_but_for_the_one_on_screen", |app, ctx| {
        app.runtime.plans = vec![plan("open", PlanStatus::Open, true, 1)];
        app.runtime.plans.extend((0..40).map(|n| plan(&format!("done {n}"), PlanStatus::Completed, true, 2 + n)));
        frame(app, ctx, vec![]);
        let rows = |ctx: &egui::Context, prefix: &str| -> Vec<usize> {
            shown(ctx).into_iter().filter_map(|(name, _)| name.strip_prefix(prefix)?.parse().ok()).collect()
        };
        assert_eq!(rows(ctx, "board:"), [0, 1, 2, 3], "the open board and the latest three done");
        assert!(rows(ctx, "done:").is_empty(), "the rest are folded");
        click(app, ctx, "fold:DONE");
        let listed = rows(ctx, "done:");
        assert!(listed.contains(&4) && listed.len() < 37, "open, only the rows in view are drawn: {listed:?}");
        // The board on screen shows above the others, wherever it stood.
        click(app, ctx, "done:4");
        assert!(app.runtime.plan_view && app.runtime.selected_plan == 4);
        assert_eq!(rows(ctx, "board:"), [4, 0, 1, 2, 3]);
    });
}

#[test]
fn a_frame_takes_the_boards_once_and_outside_one_they_are_read_anew() {
    with_app("app::calm_tests::a_frame_takes_the_boards_once_and_outside_one_they_are_read_anew", |app, _| {
        app.runtime.plans = vec![plan("one", PlanStatus::Open, true, 1)];
        let boards = |app: &mut Desktop<'_>| app.draw(|backend, _| backend.plan_list().len());
        app.runtime.drawing = true;
        assert_eq!(boards(app), 1);
        app.runtime.plans.push(plan("two", PlanStatus::Open, true, 2));
        assert_eq!(boards(app), 1, "one frame shows one state");
        app.runtime.drawing = false;
        assert_eq!(boards(app), 2);
    });
    // What only moves the pointer changes nothing the host holds; everything else may.
    assert!(!super::reaches_the_host(&egui::Event::PointerMoved(pos2(1.0, 1.0))));
    assert!(!super::reaches_the_host(&egui::Event::PointerGone));
    assert!(super::reaches_the_host(&egui::Event::Text("a".into())));
    assert!(super::reaches_the_host(&egui::Event::WindowFocused(true)));
}

#[test]
fn finished_cards_can_be_hidden_and_come_back_and_a_done_board_shows_none_by_itself() {
    with_app(
        "app::calm_tests::finished_cards_can_be_hidden_and_come_back_and_a_done_board_shows_none_by_itself",
        |app, ctx| {
            let (_dir, repo) = repository();
            app.runtime.repos = vec![repo.clone()];
            let finished = |id: &str, status| {
                let mut board = plan(id, status, true, 1);
                let mut task = board.tasks[0].clone();
                (task.finished_at, task.worktree) = (Some(100), Some(repo.clone()));
                board.tasks = ["a", "b"].map(|id| plan::Task { id: id.into(), ..task.clone() }).to_vec();
                board
            };
            app.runtime.plans = vec![finished("open", PlanStatus::Open), finished("done", PlanStatus::Completed)];
            let cards =
                |ctx: &egui::Context| shown(ctx).iter().filter(|(n, _)| n.starts_with("hide-finished:")).count();
            let count = |app: &mut Desktop<'_>, ctx: &egui::Context| {
                app.draw(|backend, _| {
                    (atomics_ui::fleet::finished(ctx, backend).len(), atomics_ui::fleet::hides_finished(ctx, backend))
                })
            };
            click(app, ctx, "board:0");
            app.runtime.panel = PanelMode::Tickets;
            frame(app, ctx, vec![]);
            assert_eq!((cards(ctx), count(app, ctx)), (2, (2, false)));
            click(app, ctx, "hide-finished:0:0");
            frame(app, ctx, vec![]);
            assert_eq!((cards(ctx), count(app, ctx)), (1, (1, true)), "the other moves up");
            atomics_ui::fleet::show_finished(ctx, "open");
            frame(app, ctx, vec![]);
            assert_eq!((cards(ctx), count(app, ctx)), (2, (2, false)));
            // The work of a done board is on it: its agents' cards come when asked for.
            click(app, ctx, "board:1");
            app.runtime.panel = PanelMode::Tickets;
            frame(app, ctx, vec![]);
            assert_eq!((cards(ctx), count(app, ctx)), (0, (0, true)));
            atomics_ui::fleet::show_finished(ctx, "done");
            frame(app, ctx, vec![]);
            assert_eq!((cards(ctx), count(app, ctx)), (2, (2, false)));
        },
    );
}

#[test]
fn a_session_shows_where_it_works_when_the_view_leaves_it_out() {
    with_app("app::calm_tests::a_session_shows_where_it_works_when_the_view_leaves_it_out", |app, ctx| {
        let root = app.runtime.launch_dir.clone();
        let mut board = plan("board", PlanStatus::Open, true, 1);
        board.tasks[0].session = Some("worker".into());
        app.runtime.plans = vec![board, plan("other", PlanStatus::Open, true, 2)];
        app.runtime.sessions.push(session("worker", AgentKind::Generic, AgentStatus::Working, &root));
        app.runtime.sessions.push(session("loose", AgentKind::Shell, AgentStatus::Idle, &root));
        let visible = |app: &Desktop<'_>| -> Vec<String> {
            app.runtime.visible_sessions().into_iter().map(|i| app.runtime.sessions[i].id.clone()).collect()
        };
        click(app, ctx, "board:1");
        assert!(visible(app).is_empty(), "no agent works for the other board");
        // Of no board: with the other sessions of none.
        click(app, ctx, "session:loose");
        assert_eq!(app.runtime.active.as_deref(), Some("loose"));
        assert_eq!(visible(app), ["loose"]);
        // Of a board: on its board.
        click(app, ctx, "session:worker");
        assert_eq!(app.runtime.active.as_deref(), Some("worker"));
        assert!(app.runtime.plan_view && app.runtime.selected_plan == 0);
        assert_eq!(visible(app), ["worker"]);
        // New work, then the session that is active already: it shows again.
        click(app, ctx, "new-work");
        assert!(app.client.ui.home.shown);
        click(app, ctx, "session:worker");
        assert!(!app.client.ui.home.shown, "the start screen gives way");
    });
}

#[test]
fn a_boards_closed_sessions_show_under_it_and_the_rest_fold_away() {
    with_app("app::calm_tests::a_boards_closed_sessions_show_under_it_and_the_rest_fold_away", |app, ctx| {
        let (_dir, repo) = repository();
        app.runtime.repos = vec![repo.clone()];
        let mut board = plan("board", PlanStatus::Open, true, 1);
        board.tasks[0].session = Some("pane-1".into());
        app.runtime.plans = vec![board];
        app.runtime.store.sessions =
            vec![record("pane-1", "Board worker", &repo), record("pane-2", "Earlier work", &repo)];
        frame(app, ctx, vec![]);
        let shown = shown(ctx);
        let at = |name: &str| shown.iter().find(|(shown, _)| shown == name).map(|(_, rect)| *rect);
        let board = at("board:0").expect("the board's row");
        let closed = at("closed:pane-1").expect("its closed session, under it");
        assert!(closed.top() >= board.bottom());
        assert!(at("closed:pane-2").is_none(), "a closed session of no board waits folded");
    });
}

/// Screens of what only a click reaches, which `atomics ctl` cannot make: a closed session resumed from under its
/// board, and an archived board viewed from History and reopened. Rendered on the CPU into `ATOMICS_CALM_SHOTS`.
#[test]
fn calm_screens() {
    let Some(dir) = std::env::var_os("ATOMICS_CALM_SHOTS").map(PathBuf::from) else { return };
    with_app("app::calm_tests::calm_screens", |app, ctx| {
        app.palette().apply(ctx, &crate::theme::SystemFonts);
        let (_repos, repo) = repository();
        app.runtime.repos = vec![repo.clone()];
        if let Some(claude) = app.runtime.config.agents.iter_mut().find(|agent| agent.name == "claude") {
            claude.command = vec![super::tests::fixture_agent("claude").to_string_lossy().into_owned()];
        }
        let now = atomics_core::store::now();
        let mut export = plan("export", PlanStatus::Open, true, now * 1000);
        export.goal = "Add invoice export to the billing API".into();
        export.tasks = ["Add the export schema", "Expose the CSV endpoint", "Document the export"]
            .into_iter()
            .enumerate()
            .map(|(n, title)| Task {
                id: format!("t{n}"),
                title: title.into(),
                session: (n == 0).then(|| "pane-1".to_string()),
                status: if n == 2 { TaskStatus::Done } else { TaskStatus::Planned },
                ..Task::default()
            })
            .collect();
        let mut rename = plan("rename", PlanStatus::Archived, true, now * 1000 - 86_400_000);
        rename.goal = "Rename the payments module".into();
        let mut tidy = plan("tidy", PlanStatus::Completed, true, now * 1000 - 3_600_000);
        tidy.goal = "Tidy the README and contributor guide".into();
        tidy.tasks[0].status = TaskStatus::Done;
        app.runtime.plans = vec![rename, tidy, export];
        for plan in &app.runtime.plans {
            plan.save_in(&paths::atomics_home()).unwrap();
        }
        app.runtime.store.sessions = vec![record("pane-1", "schema · Add the export schema", &repo)];
        RASTER.with_borrow_mut(|raster| *raster = Some(crate::test_raster::Raster::default()));
        let shot = |app: &mut Desktop<'_>, name: &str| {
            for _ in 0..6 {
                frame(app, ctx, vec![]);
            }
            let mut output = ctx.run_ui(
                egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, SIZE)), ..Default::default() },
                |ui| app.paint(ui),
            );
            let pixels = SIZE * output.pixels_per_point;
            let background = app.palette().background;
            RASTER.with_borrow_mut(|raster| {
                let raster = raster.as_mut().unwrap();
                raster.feed(&output);
                output.textures_delta.clear();
                let image = raster.paint(ctx, output, [pixels.x as usize, pixels.y as usize], background);
                crate::test_raster::save_png(&dir.join(format!("{name}.png")), &image).unwrap();
            });
        };
        app.send(Command::ShowPlan(2));
        shot(app, "headless-dark-1440-closed-session-under-its-board");
        click(app, ctx, "closed:pane-1");
        shot(app, "headless-dark-1440-closed-session-resumed");
        // History opens from its fold; its row shows the archived board, whose Reopen brings it back.
        click(app, ctx, "fold:HISTORY");
        shot(app, "headless-dark-1440-history-open");
        click(app, ctx, "history:0");
        shot(app, "headless-dark-1440-archived-board-viewed");
        click(app, ctx, "reopen-plan");
        shot(app, "headless-dark-1440-archived-board-reopened");
        assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
        // Completing an unfinished plan with an agent at work: one short confirmation, then done anyway.
        let mut worker = session("worker", AgentKind::Generic, AgentStatus::Working, &repo);
        worker.title = "endpoint · Expose the CSV endpoint".into();
        app.runtime.sessions.push(worker);
        app.runtime.plans[2].tasks[1].session = Some("worker".into());
        app.runtime.plans[2].tasks[1].status = TaskStatus::Running;
        app.send(Command::ShowPlan(2));
        app.draw(|backend, client| atomics_ui::boards::complete(2, backend, client));
        shot(app, "headless-dark-1440-complete-anyway-confirm");
        click(app, ctx, "close-anyway");
        shot(app, "headless-dark-1440-completed-anyway");
        assert_eq!(app.runtime.plans[2].status, PlanStatus::Completed);
        // Archiving a done board from its row in the panel: at once, with an Undo.
        let row = shown(ctx).into_iter().find(|(name, _)| name == "board:1").unwrap().1;
        frame(app, ctx, vec![egui::Event::PointerMoved(row.center())]);
        shot(app, "headless-dark-1440-panel-row-actions");
        click(app, ctx, "board-archive:1");
        shot(app, "headless-dark-1440-archived-from-panel-with-undo");
        assert_eq!(app.runtime.plans[1].status, PlanStatus::Archived);
    });
}

#[test]
fn complete_and_archive_work_on_any_plan_and_undo_brings_a_clean_one_back() {
    with_app("app::calm_tests::complete_and_archive_work_on_any_plan_and_undo_brings_a_clean_one_back", |app, ctx| {
        let mut board = plan("board", PlanStatus::Open, true, 1);
        board.tasks = vec![
            Task { id: "a".into(), title: "Accepted".into(), status: TaskStatus::Done, ..Task::default() },
            Task {
                id: "b".into(),
                title: "Still running".into(),
                status: TaskStatus::Running,
                session: Some("worker".into()),
                ..Task::default()
            },
        ];
        let mut done = plan("done", PlanStatus::Open, true, 2);
        done.tasks[0].status = TaskStatus::Done;
        app.runtime.plans = vec![board, done];
        app.runtime.sessions.push(session(
            "worker",
            AgentKind::Generic,
            AgentStatus::Working,
            &app.runtime.launch_dir.clone(),
        ));
        // Where the workflow calls a plan done only once its work is integrated, the host completes an unfinished one
        // only when told to anyway.
        app.runtime.config.workflow.done = "integrated".into();
        app.send(Command::ClosePlan { plan: 0, status: PlanStatus::Completed, force: false });
        app.runtime.drain_actions();
        assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
        // Complete asks first, naming what is not accepted and the agent at work, and then completes anyway.
        app.draw(|backend, client| atomics_ui::boards::complete(0, backend, client));
        assert_eq!(app.client.confirm_plan_close, Some(("board".into(), PlanStatus::Completed)));
        // A new window sizes itself before it takes a click.
        frame(app, ctx, vec![]);
        frame(app, ctx, vec![]);
        click(app, ctx, "close-anyway");
        assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
        assert_eq!(app.runtime.plans[0].tasks[1].status, TaskStatus::Running, "its tasks are as they were");
        assert!(app.runtime.find("worker").is_some(), "the agent of a task not accepted keeps running");
        assert!(app.client.ui.undo.is_none(), "an agent was at work: no clean undo");
        // A finished plan with nothing at stake completes at once, with an Undo that reopens it.
        app.runtime.config.workflow.done = "accepted".into();
        app.runtime.plans[1].tasks[0].integrated = Some(app.runtime.plans[1].integration_branch());
        app.draw(|backend, client| atomics_ui::boards::complete(1, backend, client));
        frame(app, ctx, vec![]);
        assert_eq!(app.runtime.plans[1].status, PlanStatus::Completed);
        assert!(app.client.confirm_plan_close.is_none());
        click(app, ctx, "undo");
        assert_eq!(app.runtime.plans[1].status, PlanStatus::Open, "Undo reopens it");
        // Archiving it from its row in the panel, and undoing that, brings it back as it was.
        app.draw(|backend, client| atomics_ui::boards::complete(1, backend, client));
        frame(app, ctx, vec![]);
        let index = 1;
        app.draw(|backend, client| atomics_ui::boards::archive(index, backend, client));
        frame(app, ctx, vec![]);
        assert_eq!(app.runtime.plans[1].status, PlanStatus::Archived);
        click(app, ctx, "undo");
        assert_eq!(app.runtime.plans[1].status, PlanStatus::Completed, "Undo puts the done board back");
    });
}

/// A new line in Simple mode's composer, typed or pasted, grows the box in the frame that takes the key: no frame shows
/// the text scrolled in a box of the old height, and the next one the box grown.
#[test]
fn the_composer_takes_its_new_height_in_the_frame_of_the_key() {
    with_app("app::calm_tests::the_composer_takes_its_new_height_in_the_frame_of_the_key", |app, ctx| {
        app.client.ui.mode = Some(atomics_core::config::UiMode::Simple);
        // The last pass of a frame is the one shown.
        let box_shown = |ctx: &egui::Context| {
            let shown = shown(ctx);
            shown.into_iter().rev().find(|(name, _)| name == "composer-box").expect("the composer is drawn").1
        };
        click(app, ctx, "composer");
        frame(app, ctx, vec![egui::Event::Text("First line".into())]);
        frame(app, ctx, vec![]);
        let one = box_shown(ctx);
        let enter = |pressed| egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::SHIFT,
        };
        frame(app, ctx, vec![enter(true), enter(false)]);
        let typed = box_shown(ctx);
        frame(app, ctx, vec![]);
        assert!(typed.height() > one.height(), "Shift+Enter grows the box: {one:?} then {typed:?}");
        assert_eq!(typed, box_shown(ctx), "the frame of the key shows the box as it stays");
        frame(app, ctx, vec![egui::Event::Paste("two\nthree\nfour".into())]);
        let pasted = box_shown(ctx);
        frame(app, ctx, vec![]);
        assert!(pasted.height() > typed.height(), "a paste of lines grows it: {typed:?} then {pasted:?}");
        assert_eq!(pasted, box_shown(ctx), "the frame of the paste shows the box as it stays");
    });
}
