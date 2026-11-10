//! Shared plan presentation and client navigation. Selection is independent of activity.
use super::*;

pub(super) use atomics_ui::PlanState;

impl State {
    pub(super) fn plan_state(&self, index: usize) -> PlanState {
        let plan = &self.plans[index];
        match plan.status {
            PlanStatus::Completed => return PlanState::Completed,
            PlanStatus::Archived => return PlanState::Archived,
            PlanStatus::Open => {}
        }
        let linked = |session: &&Session| !session.chief && plan.contains_session(&session.id);
        if plan.tasks.iter().any(|task| {
            task.ref_alert.is_some()
                || task.status == TaskStatus::Blocked
                || task.status != TaskStatus::Done
                    && task.report.as_ref().is_some_and(|report| report.status == "question")
        }) || self.sessions.iter().filter(linked).any(|session| {
            session.status == AgentStatus::NeedsInput || session.approval_hold || !session.approvals.is_empty()
        }) {
            return PlanState::NeedsYou;
        }
        let working = self
            .sessions
            .iter()
            .filter(linked)
            .filter(|session| session.is_agent() && session.status == AgentStatus::Working)
            .count();
        if working > 0 { PlanState::Running(working) } else { PlanState::Idle }
    }
}

impl Desktop<'_> {
    pub(super) fn view_plan_history(&mut self, index: usize) {
        self.runtime.selected_plan = index;
        self.runtime.changed_plans.remove(&self.runtime.plans[index].id);
        self.runtime.view = self.runtime.plans[index].project.clone();
        self.runtime.plan_view = true;
        self.runtime.ungrouped = false;
        self.runtime.panel = PanelMode::Plan;
        self.runtime.diff_open = true;
        self.runtime.active = None;
        self.runtime.diff_root = None;
    }

    pub(super) fn close_plan_tab(&mut self, index: usize) {
        if self.runtime.plans[index].status == PlanStatus::Open {
            self.client.confirm_plan_close = Some((self.runtime.plans[index].id.clone(), PlanStatus::Completed));
        } else {
            self.send(Command::ArchiveFinished(index));
        }
    }
}

/// When a plan was last saved, or its last task finished, as the host's local date.
pub(super) fn history_date(plan: &Plan) -> String {
    let path = paths::atomics_home().join("plans").join(format!("{}.json", plan.id));
    let timestamp = modified(&path)
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .or_else(|| plan.tasks.iter().filter_map(|task| task.finished_at).max());
    let Some(timestamp) = timestamp else { return "date unavailable".into() };
    let timestamp = timestamp as libc::time_t;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: localtime_r initializes the supplied storage on success.
    let local = unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()).as_ref() };
    local
        .map(|date| format!("{:04}-{:02}-{:02}", date.tm_year + 1900, date.tm_mon + 1, date.tm_mday))
        .unwrap_or_else(|| "date unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{session, with_app};

    fn fixture(app: &mut Desktop<'_>) {
        app.runtime.repos = vec![PathBuf::from("/project/P")];
        app.runtime.plans = [
            ("running", "Running", PlanStatus::Open),
            ("needs", "Needs", PlanStatus::Open),
            ("idle", "Idle", PlanStatus::Open),
            ("one", "Completed one", PlanStatus::Completed),
            ("two", "Completed two", PlanStatus::Completed),
            ("archived", "Archived", PlanStatus::Archived),
        ]
        .into_iter()
        .map(|(id, goal, status)| Plan {
            id: id.into(),
            goal: goal.into(),
            status,
            approved: true,
            tasks: vec![Task { id: "work".into(), title: "Saved task history".into(), ..Task::default() }],
            ..Plan::default()
        })
        .collect();
        app.runtime.plans[0].tasks = ["a", "b"]
            .into_iter()
            .map(|id| Task { id: id.into(), session: Some(id.into()), status: TaskStatus::Running, ..Task::default() })
            .collect();
        app.runtime.plans[1].tasks[0].session = Some("c".into());
        app.runtime.plans[1].tasks[0].status = TaskStatus::Blocked;
        app.runtime.sessions = ["a", "b", "c"]
            .into_iter()
            .map(|id| session(id, AgentKind::Generic, AgentStatus::Working, Path::new("/project/P")))
            .collect();
        for plan in &app.runtime.plans {
            plan.save_in(&paths::atomics_home()).unwrap();
        }
        app.runtime.selected_plan = 2;
        app.runtime.plan_view = true;
        app.runtime.panel = PanelMode::Plan;
    }

    fn render(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> egui::FullOutput {
        let clock = egui::Id::NULL.with("plan-test-time");
        let time = ctx.data_mut(|data| {
            let value = data.get_temp::<f64>(clock).unwrap_or(0.0) + 0.1;
            data.insert_temp(clock, value);
            value
        });
        ctx.data_mut(|data| data.insert_temp(egui::Id::NULL.with("topbar-items"), Vec::<(String, Rect)>::new()));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(5120.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| {
                egui::Panel::top("plan-test-top").exact_size(34.0).show(ui, |ui| app.top_bar(ui));
                egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
                app.dialogs(ctx);
            },
        );
        app.runtime.drain_actions();
        output
    }

    fn text(output: &egui::FullOutput) -> Vec<&egui::epaint::TextShape> {
        output
            .shapes
            .iter()
            .filter_map(|shape| if let egui::Shape::Text(text) = &shape.shape { Some(text) } else { None })
            .collect()
    }

    fn click_at(app: &mut Desktop<'_>, ctx: &egui::Context, pos: Pos2) {
        for pressed in [true, false] {
            render(
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
            )
            .drop_without_applying_deltas();
        }
    }

    fn click(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) {
        render(app, ctx, vec![]).drop_without_applying_deltas();
        let output = render(app, ctx, vec![]);
        let labels = text(&output);
        let shape = labels.iter().find(|text| text.galley.text() == label).unwrap_or_else(|| {
            panic!("missing {label}: {:?}", labels.iter().map(|text| text.galley.text()).collect::<Vec<_>>())
        });
        let pos = shape.pos + shape.galley.rect.center().to_vec2();
        output.drop_without_applying_deltas();
        click_at(app, ctx, pos);
    }

    /// Opens the Boards dropdown.
    fn open_boards(app: &mut Desktop<'_>, ctx: &egui::Context) {
        render(app, ctx, vec![]).drop_without_applying_deltas();
        let items = ctx.data(|data| data.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items"))).unwrap();
        let button = items.iter().find(|(name, _)| name == "Boards").expect("the Boards dropdown").1;
        click_at(app, ctx, button.center());
        render(app, ctx, vec![]).drop_without_applying_deltas();
    }

    /// The close button of an open board's row.
    fn close_board(app: &mut Desktop<'_>, ctx: &egui::Context, index: usize) {
        open_boards(app, ctx);
        let id = egui::Id::NULL.with(("plan-close", app.runtime.plans[index].id.as_str(), ""));
        let rect = ctx.data(|data| data.get_temp::<Rect>(id)).expect("an open board's row has a close button");
        click_at(app, ctx, rect.center());
    }

    /// Opens History in the dropdown.
    fn open_history(app: &mut Desktop<'_>, ctx: &egui::Context) {
        open_boards(app, ctx);
        click(app, ctx, "History");
        render(app, ctx, vec![]).drop_without_applying_deltas();
    }

    fn history_action(app: &mut Desktop<'_>, ctx: &egui::Context, plan: &str, action: &str) {
        open_history(app, ctx);
        let id = egui::Id::NULL.with(("plan-history", plan, action));
        let rect = ctx.data(|data| data.get_temp::<Rect>(id)).expect("History action must be reachable");
        click_at(app, ctx, rect.center());
    }

    #[test]
    fn history_views_archives_and_reopens_and_an_open_boards_close_confirms() {
        with_app(
            "app::plan_view::tests::history_views_archives_and_reopens_and_an_open_boards_close_confirms",
            |app, ctx| {
                fixture(app);
                let sessions = app.runtime.sessions.len();
                history_action(app, ctx, "one", "view");
                assert!(app.runtime.panel == PanelMode::Plan && app.runtime.plan_view);
                assert_eq!(app.runtime.selected_plan, 3);
                assert!(app.runtime.creating.is_empty());
                assert_eq!(app.runtime.sessions.len(), sessions);
                history_action(app, ctx, "one", "archive");
                assert_eq!(app.runtime.plans[3].status, PlanStatus::Archived);
                assert!(app.client.confirm_plan_close.is_none());
                close_board(app, ctx, 2);
                assert!(app.client.confirm_plan_close.is_some());
                let output = render(app, ctx, vec![]);
                let labels = text(&output).iter().map(|shape| shape.galley.text()).collect::<Vec<_>>();
                for label in ["Complete plan", "Archive plan", "Cancel"] {
                    assert!(labels.contains(&label));
                }
                output.drop_without_applying_deltas();
                click(app, ctx, "Cancel");
                assert_eq!(app.runtime.plans[2].status, PlanStatus::Open);
                close_board(app, ctx, 2);
                click(app, ctx, "Complete plan");
                assert_eq!(app.runtime.plans[2].status, PlanStatus::Completed);
                close_board(app, ctx, 1);
                click(app, ctx, "Archive plan");
                assert_eq!(app.runtime.plans[1].status, PlanStatus::Archived);
                assert!(app.runtime.find("c").is_none(), "archiving closes the plan's task session");
                open_history(app, ctx);
                let output = render(app, ctx, vec![]);
                let labels = text(&output).iter().map(|shape| shape.galley.text()).collect::<Vec<_>>();
                for name in ["Completed one", "Completed two", "Archived"] {
                    assert!(labels.contains(&name));
                }
                assert!(labels.iter().any(|label| label.starts_with("completed · ") && !label.contains("unavailable")));
                assert!(labels.iter().any(|label| label.starts_with("archived · ") && !label.contains("unavailable")));
                output.drop_without_applying_deltas();
                let id = egui::Id::NULL.with(("plan-history", "one", "view"));
                click_at(app, ctx, ctx.data(|data| data.get_temp::<Rect>(id)).unwrap().center());
                assert_eq!(app.runtime.selected_plan, 3);
                assert!(app.runtime.panel == PanelMode::Plan);
                assert_eq!(app.runtime.plans[3].status, PlanStatus::Archived);
                let tasks = app.runtime.plans[3].tasks.clone();
                history_action(app, ctx, "one", "reopen");
                assert_eq!(app.runtime.plans[3].status, PlanStatus::Open);
                assert_eq!(app.runtime.plans[3].tasks, tasks);
                assert_eq!(app.runtime.sessions.len(), sessions - 1);
                assert_eq!(Plan::load(&paths::atomics_home().join("plans/one.json")).unwrap().status, PlanStatus::Open);
            },
        );
    }

    #[test]
    fn a_finished_tasks_tile_leads_back_to_its_finished_board() {
        with_app("app::plan_view::tests::a_finished_tasks_tile_leads_back_to_its_finished_board", |app, ctx| {
            fixture(app);
            let root = PathBuf::from("/project/P");
            app.runtime.sessions.clear();
            app.runtime.plans[3].project = Some(root.clone());
            app.runtime.plans[3].tasks[0].finished_at = Some(1);
            app.runtime.plans[3].tasks[0].worktree = Some("/tmp".into());
            app.runtime.plan_view = false;
            app.runtime.view = Some(root);
            let paint = |app: &mut Desktop<'_>, ctx: &egui::Context, events| {
                ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                        events,
                        ..Default::default()
                    },
                    |ui| app.paint(ui),
                )
            };
            paint(app, ctx, vec![]).drop_without_applying_deltas();
            let output = paint(app, ctx, vec![]);
            let button = text(&output)
                .into_iter()
                .find(|shape| shape.galley.text() == "Open board")
                .map(|shape| shape.visual_bounding_rect().center());
            let shown: Vec<String> = text(&output).iter().map(|shape| shape.galley.text().to_string()).collect();
            output.drop_without_applying_deltas();
            let button = button.unwrap_or_else(|| panic!("the tile's Open board: {shown:?}"));
            assert!(
                !shown.iter().any(|text| text == "Completed one"),
                "the finished board is out of the bar: {shown:?}"
            );
            for pressed in [true, false] {
                let event = egui::Event::PointerButton {
                    pos: button,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                paint(app, ctx, vec![egui::Event::PointerMoved(button), event]).drop_without_applying_deltas();
                app.runtime.drain_actions();
            }
            assert_eq!(app.runtime.selected_plan, 3);
            assert!(app.runtime.plan_view && app.runtime.panel == PanelMode::Plan && app.runtime.diff_open);
            assert_eq!(app.runtime.plans[3].status, PlanStatus::Completed, "looking changes nothing");
        });
    }

    #[test]
    fn closed_plan_selection_uses_the_workspace_without_agent_launchers() {
        with_app(
            "app::plan_view::tests::closed_plan_selection_uses_the_workspace_without_agent_launchers",
            |app, ctx| {
                fixture(app);
                for index in [3, 5] {
                    app.view_plan_history(index);
                    let sessions = app.runtime.sessions.len();
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1400.0, 900.0))),
                            ..Default::default()
                        },
                        |ui| app.paint(ui),
                    );
                    let labels = text(&output).iter().map(|text| text.galley.text()).collect::<Vec<_>>();
                    assert!(labels.iter().any(|label| label.contains("read-only")));
                    assert!(
                        !labels.iter().any(|label| label.starts_with("Start ")),
                        "history must not offer agent launchers: {labels:?}"
                    );
                    assert_eq!(app.runtime.sessions.len(), sessions);
                    assert!(app.runtime.creating.is_empty());
                    output.drop_without_applying_deltas();
                }
            },
        );
    }

    #[test]
    fn boards_show_their_state_in_their_rows_whatever_is_selected() {
        with_app("app::plan_view::tests::boards_show_their_state_in_their_rows_whatever_is_selected", |app, ctx| {
            fixture(app);
            assert_eq!(
                (0..6).map(|index| app.runtime.plan_state(index)).collect::<Vec<_>>(),
                vec![
                    PlanState::Running(2),
                    PlanState::NeedsYou,
                    PlanState::Idle,
                    PlanState::Completed,
                    PlanState::Completed,
                    PlanState::Archived
                ]
            );
            open_boards(app, ctx);
            let output = render(app, ctx, vec![]);
            let labels = text(&output);
            let shown: Vec<&str> = labels.iter().map(|shape| shape.galley.text()).collect();
            // The menu is drawn last, after the bar's and the board's own text.
            let index_of = |label: &str| shown.iter().rposition(|text| *text == label);
            // The boards that wait come first, then the others; finished ones are behind History.
            let order = ["NEEDS YOU · 1", "Needs", "ACTIVE · 2", "Running", "Idle", "History"]
                .map(|label| index_of(label).unwrap_or_else(|| panic!("{label}: {shown:?}")));
            assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{order:?}: {shown:?}");
            for finished in ["Completed one", "Completed two", "Archived"] {
                assert!(index_of(finished).is_none(), "{finished}: {shown:?}");
            }
            for label in ["2 running", "needs you", "idle"] {
                assert!(index_of(label).is_some(), "{label}: {shown:?}");
            }
            for (index, goal) in [(0, "Running"), (1, "Needs"), (2, "Idle")] {
                let color = app.runtime.plan_state(index).color(&app.palette());
                let name = labels.iter().rfind(|shape| shape.galley.text() == goal).unwrap();
                assert!(
                    name.galley.job.sections.iter().any(|section| section.format.color == color),
                    "{goal}: wrong state color"
                );
            }
            // A board on screen is the button's name, outlined; every open board's row can close it.
            let items =
                ctx.data(|data| data.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items"))).unwrap();
            let boards = items.iter().find(|(name, _)| name == "Boards").unwrap().1;
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Rect(rect)
                    if rect.rect.center().distance(boards.center()) < 1.0
                        && rect.stroke.color == app.palette().accent)),
                "selection should be an outline"
            );
            assert!(
                labels.iter().any(
                    |shape| shape.galley.text() == "Idle" && boards.contains(shape.visual_bounding_rect().center())
                ),
                "the button names the selected board: {shown:?}"
            );
            for id in ["running", "needs", "idle"] {
                let close = egui::Id::NULL.with(("plan-close", id, ""));
                assert!(ctx.data(|data| data.get_temp::<Rect>(close)).is_some(), "{id}'s close");
            }
            // A running board's mark pulses, while the menu shows it.
            let mark = |output: &egui::FullOutput| {
                let name = text(output).into_iter().rfind(|shape| shape.galley.text() == "Running").unwrap();
                let bounds = name.visual_bounding_rect();
                output.shapes.iter().find_map(|shape| match &shape.shape {
                    egui::Shape::Circle(circle)
                        if (circle.center.y - bounds.center().y).abs() < 2.0 && circle.center.x < bounds.left() =>
                    {
                        Some(circle.fill)
                    }
                    _ => None,
                })
            };
            let before = mark(&output).expect("the running mark");
            output.drop_without_applying_deltas();
            let later = render(app, ctx, vec![]);
            assert_ne!(before, mark(&later).expect("the running mark"), "running mark should pulse");
            later.drop_without_applying_deltas();
            render(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            )
            .drop_without_applying_deltas();
            app.runtime.plans[1].tasks[0].status = TaskStatus::Running;
            app.runtime.plans[1].tasks[0].report =
                Some(Report { status: "question".into(), summary: "Question".into(), at: 0, checks: Vec::new() });
            assert_eq!(app.runtime.plan_state(1), PlanState::NeedsYou);
            app.runtime.plans[1].tasks[0].report = None;
            app.runtime.sessions[2].approval_hold = true;
            assert_eq!(app.runtime.plan_state(1), PlanState::NeedsYou);
            app.runtime.sessions[2].approval_hold = false;
            app.runtime.plans[1].tasks[0].status = TaskStatus::Blocked;

            for index in 0..6 {
                app.runtime.selected_plan = index;
                let output = render(app, ctx, vec![]);
                let color = app.runtime.plan_state(index).color(&app.palette());
                assert!(
                    text(&output).into_iter().any(|shape| shape.galley.text() == app.runtime.plans[index].goal
                        && shape.galley.job.sections.iter().any(|section| section.format.color == color)),
                    "the board's goal in its state's color"
                );
                output.drop_without_applying_deltas();
            }
        });
    }
}
