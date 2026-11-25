use super::*;

impl State {
    /// Whether a session belongs to no plan's task.
    pub(super) fn ungrouped_session(&self, session: &Session) -> bool {
        !session.chief && !self.plans.iter().any(|plan| plan.contains_session(&session.id))
    }

    /// Shows the Ungrouped tab, keeping the active session if it is there.
    pub(super) fn show_ungrouped(&mut self) {
        self.plan_view = false;
        self.view = None;
        self.ungrouped = true;
        if !self.active_index().is_some_and(|i| self.visible_sessions().contains(&i)) {
            match self.visible_sessions().first().map(|&i| self.sessions[i].id.clone()) {
                Some(id) => self.activate(&id, false),
                None => {
                    self.active = None;
                    self.diff_root = None;
                }
            }
        }
    }

    pub(super) fn set_view(&mut self, view: Option<PathBuf>) {
        self.plan_view = false;
        self.ungrouped = false;
        self.view = view.clone();
        let Some(project) = view else { return };
        let in_view = self.active_index().is_some_and(|i| {
            !self.sessions[i].chief && self.project_of(&self.sessions[i].root()).as_ref() == Some(&project)
        });
        if !in_view {
            match self.visible_sessions().first().map(|&i| self.sessions[i].id.clone()) {
                Some(id) => self.activate(&id, false),
                None => {
                    self.active = None;
                    self.diff_root = Some(project);
                }
            }
        }
    }
}

/// What a plan's tab shows: its ticket's key with progress when it has one, else its goal.
pub(super) fn plan_tab_name(plan: &Plan) -> String {
    let Some(ticket) = plan.ticket.as_ref() else { return plan.goal.clone() };
    if plan.status == PlanStatus::Open && plan.tasks.is_empty() {
        return format!("{} · planning…", ticket.label());
    }
    let done = plan.count(TaskStatus::Done);
    format!("{} · {done}/{}", ticket.label(), plan.tasks.len())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn picking_boards_and_approving_them_targets_only_the_selected_plan() {
        with_app(
            "app::sidebar::tests::picking_boards_and_approving_them_targets_only_the_selected_plan",
            |app, ctx| {
                app.runtime.repos = (0..8).map(|i| PathBuf::from(format!("/project/repo-{i}"))).collect();
                for goal in ["Atlas", "Beacon"] {
                    assert!(
                        request(
                            app,
                            json!({"cmd":"plan_set", "goal":goal, "tasks":[
                    {"id":"a", "title":format!("{goal} task"), "brief":format!("{goal} instructions")}]})
                        )
                        .ok
                    );
                }
                let one = app.runtime.plans.iter().position(|p| p.goal == "Atlas").unwrap();
                let two = app.runtime.selected_plan;
                app.runtime.plans.push(Plan {
                    id: "archived".into(),
                    goal: "Archived".into(),
                    status: plan::PlanStatus::Archived,
                    ..Plan::default()
                });
                app.runtime.plans.push(Plan {
                    id: "completed".into(),
                    goal: "Completed".into(),
                    status: plan::PlanStatus::Completed,
                    ..Plan::default()
                });
                for (index, id) in [(one, "atlas-agent"), (two, "beacon-agent")] {
                    app.runtime.plans[index].tasks[0].session = Some(id.into());
                    app.runtime.sessions.push(session(
                        id,
                        AgentKind::Generic,
                        AgentStatus::Working,
                        Path::new("/project"),
                    ));
                }
                let mut time = 0.0;
                let mut frame = |app: &mut Desktop<'_>, width, events| {
                    time += 0.05;
                    ctx.data_mut(|d| d.insert_temp(egui::Id::NULL.with("topbar-items"), Vec::<(String, Rect)>::new()));
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 1000.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::Panel::top("test-top").exact_size(34.0).show(ui, |ui| app.top_bar(ui));
                            egui::CentralPanel::default().show(ui, |ui| {
                                if app.runtime.panel == PanelMode::Plan {
                                    app.plan_panel(ui, ui.max_rect());
                                }
                            });
                            app.dialogs(ctx);
                        },
                    );
                    app.runtime.drain_actions();
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                let button = |pos, pressed| egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                let boards = || {
                    let items = ctx.data(|d| d.get_temp::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items")));
                    items.unwrap().iter().find(|(name, _)| name == "Boards").expect("the Boards dropdown").1.center()
                };
                // A finished plan is done, under DONE until it is archived; its agents keep working.
                frame(app, 2400.0, vec![]);
                let open = boards();
                frame(app, 2400.0, vec![egui::Event::PointerMoved(open), button(open, true)]);
                frame(app, 2400.0, vec![button(open, false)]);
                frame(app, 2400.0, vec![]);
                let text = frame(app, 2400.0, vec![]);
                let done = text.iter().rposition(|(t, _)| t == "DONE · 1").expect("the done boards' heading");
                assert!(text.iter().rposition(|(t, _)| t == "Completed").is_some_and(|at| at > done), "{text:?}");
                assert_eq!(app.runtime.plans.last().unwrap().status, PlanStatus::Completed);
                assert_eq!(app.runtime.selected_plan, two);
                assert_eq!(
                    app.runtime.sessions.iter().map(|s| s.status).collect::<Vec<_>>(),
                    [AgentStatus::Working; 2]
                );
                frame(
                    app,
                    2400.0,
                    vec![egui::Event::Key {
                        key: Key::Escape,
                        physical_key: Some(Key::Escape),
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
                // Give this board-routing fixture enough room for every tab and the wider count.
                let width = 3200.0;
                for (goal, index, visible) in [("Atlas", one, 0), ("Beacon", two, 1)] {
                    let text = frame(app, width, vec![]);
                    assert!(text.iter().any(|(t, _)| t == "Repos · 0"));
                    assert!(!text.iter().any(|(t, _)| t.starts_with("repo-")));
                    assert!(!text.iter().any(|(t, _)| t.contains("Archived")));
                    assert!(
                        !text.iter().any(|(t, pos)| t.contains("Completed") && pos.y < 34.0),
                        "a finished board leaves the bar"
                    );
                    let open = boards();
                    frame(app, width, vec![egui::Event::PointerMoved(open), button(open, true)]);
                    frame(app, width, vec![button(open, false)]);
                    frame(app, width, vec![]);
                    let text = frame(app, width, vec![]);
                    let waiting = if goal == "Atlas" { "NEEDS YOU · 2" } else { "NEEDS YOU · 1" };
                    assert!(text.iter().any(|(t, _)| t == waiting), "unapproved boards wait: {text:?}");
                    // The menu is drawn last, over the bar's own name of the selected board.
                    let pos = text.iter().rfind(|(t, _)| t == goal).unwrap().1;
                    frame(app, width, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                    frame(app, width, vec![button(pos, false)]);
                    assert_eq!(app.runtime.selected_plan, index);
                    assert!(app.runtime.plan_view);
                    assert_eq!(app.runtime.visible_sessions(), [visible]);
                    app.runtime.toggle_plan();
                    let text = frame(app, width, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} task")));
                    let other = if goal == "Atlas" { "Beacon task" } else { "Atlas task" };
                    assert!(!text.iter().any(|(t, _)| t == other));
                    assert!(!text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let task_pos = text.iter().find(|(t, _)| t == &format!("{goal} task")).unwrap().1;
                    frame(app, width, vec![egui::Event::PointerMoved(task_pos), button(task_pos, true)]);
                    frame(app, width, vec![button(task_pos, false)]);
                    let text = frame(app, width, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let approve = text.iter().find(|(t, _)| t == "Approve plan").unwrap().1;
                    frame(app, width, vec![egui::Event::PointerMoved(approve), button(approve, true)]);
                    frame(app, width, vec![button(approve, false)]);
                    assert!(app.runtime.plans[index].approved);
                    if index == one {
                        assert!(!app.runtime.plans[two].approved);
                    }
                }
                assert_eq!(app.runtime.sessions.len(), 2, "closing plans does not terminate their sessions");
            },
        );
    }
}
