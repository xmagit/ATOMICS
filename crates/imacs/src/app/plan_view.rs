//! Shared plan presentation and client navigation. Selection is independent of activity.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanState {
    Running(usize),
    NeedsYou,
    Idle,
    Completed,
    Archived,
}

impl PlanState {
    pub(super) fn urgency(self) -> u8 {
        match self {
            Self::NeedsYou => 2,
            Self::Running(_) => 1,
            _ => 0,
        }
    }

    pub(super) fn color(self, palette: &Palette) -> Color32 {
        match self {
            Self::Running(_) => palette.accent,
            Self::NeedsYou => palette.warning,
            Self::Idle => palette.foreground,
            Self::Completed | Self::Archived => palette.muted,
        }
    }

    pub(super) fn label(self, goal: &str) -> String {
        let name: String = goal.chars().take(28).collect();
        match self {
            Self::Running(count) => format!("● {name} · {count} running"),
            Self::NeedsYou => format!("◆ {name} · needs you"),
            Self::Idle => format!("{name} · idle"),
            Self::Completed => format!("✓ {name}"),
            Self::Archived => format!("✓ {name} · archived"),
        }
    }
}

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
            task.status == TaskStatus::Blocked
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
        self.runtime.panel = PanelMode::Plan;
        self.runtime.diff_open = true;
        self.runtime.active = None;
        self.runtime.diff_root = None;
    }

    pub(super) fn close_plan_tab(&mut self, index: usize) {
        if self.runtime.plans[index].status == PlanStatus::Open {
            self.client.confirm_plan_close = Some((self.runtime.plans[index].id.clone(), PlanStatus::Completed));
        } else {
            self.post_plan(index, |state, index| {
                if state.plans[index].status != PlanStatus::Open
                    && let Err(error) = state.close_plan(index, PlanStatus::Archived)
                {
                    state.toast(error);
                }
            });
        }
    }
}

fn history_date(plan: &Plan) -> String {
    let path = paths::imacs_home().join("plans").join(format!("{}.json", plan.id));
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

impl Desktop<'_> {
    pub(super) fn history_menu(&mut self, ui: &mut egui::Ui) {
        let mut view = None;
        let mut reopen = None;
        let response = ui
            .menu_button("History", |ui| {
                for (index, plan) in self
                    .runtime
                    .plans
                    .iter()
                    .enumerate()
                    .filter(|(_, plan)| !plan.id.is_empty() && plan.status != PlanStatus::Open)
                {
                    ui.push_id(&plan.id, |ui| {
                        ui.horizontal(|ui| {
                            let state = self.runtime.plan_state(index);
                            ui.label(RichText::new(&plan.goal).color(state.color(&self.runtime.palette)));
                            let status = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
                            ui.label(
                                RichText::new(format!("{status} · {}", history_date(plan)))
                                    .color(self.runtime.palette.muted),
                            )
                            .on_hover_text(
                                "Date the plan was last saved, or its last task completion when no saved file exists",
                            );
                            let response = ui.button("View");
                            record_history_action(ui, &plan.id, "view", &response);
                            if response.clicked() {
                                view = Some(index);
                                ui.close();
                            }
                            let response = ui.button("Reopen");
                            record_history_action(ui, &plan.id, "reopen", &response);
                            if response.clicked() {
                                reopen = Some(index);
                                ui.close();
                            }
                        });
                    });
                }
            })
            .response;
        super::topbar::record_item(ui, "History", &response);
        if let Some(index) = view {
            self.view_plan_history(index);
        }
        if let Some(index) = reopen {
            self.post_plan(index, |state, index| {
                if let Err(error) = state.reopen_plan(index) {
                    state.toast(error);
                }
            });
        }
    }
}

fn record_history_action(_ui: &egui::Ui, _plan: &str, _action: &str, _response: &egui::Response) {
    #[cfg(test)]
    if !_ui.is_sizing_pass() {
        _ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::NULL.with(("plan-history", _plan, _action)), _response.rect));
    }
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
            plan.save_in(&paths::imacs_home()).unwrap();
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

    fn close_tab(app: &mut Desktop<'_>, ctx: &egui::Context, index: usize) {
        let label = app.runtime.plan_state(index).label(&app.runtime.plans[index].goal);
        click(app, ctx, &label);
        render(app, ctx, vec![]).drop_without_applying_deltas();
        let id = egui::Id::NULL.with(("plan-close", app.runtime.plans[index].id.as_str()));
        let rect = ctx.data(|data| data.get_temp::<Rect>(id)).expect("selected plan must have a close button");
        click_at(app, ctx, rect.center());
    }

    fn history_action(app: &mut Desktop<'_>, ctx: &egui::Context, plan: &str, action: &str) {
        click(app, ctx, "History");
        render(app, ctx, vec![]).drop_without_applying_deltas();
        let id = egui::Id::NULL.with(("plan-history", plan, action));
        let rect = ctx.data(|data| data.get_temp::<Rect>(id)).expect("History action must be reachable");
        click_at(app, ctx, rect.center());
    }

    #[test]
    fn completed_tabs_archive_open_tabs_confirm_and_history_views_and_reopens() {
        with_app(
            "app::plan_view::tests::completed_tabs_archive_open_tabs_confirm_and_history_views_and_reopens",
            |app, ctx| {
                fixture(app);
                let sessions = app.runtime.sessions.len();
                click(app, ctx, "✓ Completed one");
                assert!(app.runtime.panel == PanelMode::Plan && app.runtime.plan_view);
                assert!(app.runtime.creating.is_empty());
                assert_eq!(app.runtime.sessions.len(), sessions);
                close_tab(app, ctx, 3);
                assert_eq!(app.runtime.plans[3].status, PlanStatus::Archived);
                assert!(app.client.confirm_plan_close.is_none());
                close_tab(app, ctx, 2);
                assert!(app.client.confirm_plan_close.is_some());
                let output = render(app, ctx, vec![]);
                let labels = text(&output).iter().map(|shape| shape.galley.text()).collect::<Vec<_>>();
                for label in ["Complete plan", "Archive plan", "Cancel"] {
                    assert!(labels.contains(&label));
                }
                output.drop_without_applying_deltas();
                click(app, ctx, "Cancel");
                assert_eq!(app.runtime.plans[2].status, PlanStatus::Open);
                close_tab(app, ctx, 2);
                click(app, ctx, "Complete plan");
                assert_eq!(app.runtime.plans[2].status, PlanStatus::Completed);
                close_tab(app, ctx, 1);
                click(app, ctx, "Archive plan");
                assert_eq!(app.runtime.plans[1].status, PlanStatus::Archived);
                click(app, ctx, "History");
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
                assert_eq!(app.runtime.sessions.len(), sessions);
                assert_eq!(Plan::load(&paths::imacs_home().join("plans/one.json")).unwrap().status, PlanStatus::Open);
            },
        );
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
    fn plan_activity_colors_and_fill_are_independent_of_selection() {
        with_app("app::plan_view::tests::plan_activity_colors_and_fill_are_independent_of_selection", |app, ctx| {
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
            let output = render(app, ctx, vec![]);
            let labels = text(&output);
            for index in 0..5 {
                let state = app.runtime.plan_state(index);
                let expected = state.label(&app.runtime.plans[index].goal);
                let label = labels.iter().find(|shape| shape.galley.text() == expected).unwrap();
                assert!(
                    label
                        .galley
                        .job
                        .sections
                        .iter()
                        .any(|section| section.format.color == state.color(&app.runtime.palette)),
                    "{expected}: wrong state color"
                );
            }
            let items =
                ctx.data(|data| data.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items"))).unwrap();
            let idle = items.iter().find(|(name, _)| name == "Idle · idle").unwrap().1;
            assert!(output.shapes.iter().any(|shape|matches!(&shape.shape,egui::Shape::Rect(rect) if rect.rect.center().distance(idle.center())<1.0&&rect.stroke.color==app.runtime.palette.accent&&rect.fill==Color32::TRANSPARENT)),"selection should be an outline");
            assert!(output.shapes.iter().any(|shape|matches!(&shape.shape,egui::Shape::Rect(rect) if rect.fill==app.runtime.palette.accent.gamma_multiply(0.35))),"running should be filled");
            let running_tab = items.iter().find(|(name, _)| name == "● Running · 2 running").unwrap().1;
            let close_id = egui::Id::NULL.with(("plan-close", "running"));
            assert!(
                ctx.data(|data| data.get_temp::<Rect>(close_id)).is_none(),
                "unselected close button stays hidden until hover"
            );
            let running = text(&output)
                .into_iter()
                .find(|shape| shape.galley.text() == "● Running · 2 running")
                .unwrap()
                .galley
                .job
                .sections[0]
                .format
                .color;
            output.drop_without_applying_deltas();
            let later = render(app, ctx, vec![]);
            let animated = text(&later)
                .into_iter()
                .find(|shape| shape.galley.text() == "● Running · 2 running")
                .unwrap()
                .galley
                .job
                .sections[0]
                .format
                .color;
            assert_ne!(running, animated, "running dot should animate");
            later.drop_without_applying_deltas();
            render(app, ctx, vec![egui::Event::PointerMoved(running_tab.center())]).drop_without_applying_deltas();
            assert!(ctx.data(|data| data.get_temp::<Rect>(close_id)).is_some(), "hover reveals the close button");
            app.runtime.plans[1].tasks[0].status = TaskStatus::Running;
            app.runtime.plans[1].tasks[0].report =
                Some(Report { status: "question".into(), summary: "Question".into(), at: 0 });
            assert_eq!(app.runtime.plan_state(1), PlanState::NeedsYou);
            app.runtime.plans[1].tasks[0].report = None;
            app.runtime.sessions[2].approval_hold = true;
            assert_eq!(app.runtime.plan_state(1), PlanState::NeedsYou);
            app.runtime.sessions[2].approval_hold = false;
            app.runtime.plans[1].tasks[0].status = TaskStatus::Blocked;

            for index in 0..6 {
                app.runtime.selected_plan = index;
                let output = render(app, ctx, vec![]);
                let title = text(&output)
                    .into_iter()
                    .find(|shape| shape.galley.text() == app.runtime.plans[index].goal)
                    .unwrap();
                assert!(
                    title.galley.job.sections.iter().any(
                        |section| section.format.color == app.runtime.plan_state(index).color(&app.runtime.palette)
                    )
                );
                output.drop_without_applying_deltas();
            }
        });
    }
}
