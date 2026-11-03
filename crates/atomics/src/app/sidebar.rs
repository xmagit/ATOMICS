use atomics_ui::sidebar::section;

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

impl Desktop<'_> {
    /// The TICKETS section: a plan's ticket tree in its view, else Find and the last tickets read.
    pub(super) fn tickets_section(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(config) = self.runtime.ticket_config() else { return };
        let mut open: Option<atomics_core::tickets::TicketLink> = None;
        let mut find = false;
        section(ui, p, "TICKETS");
        let plan = self.runtime.plan_view.then(|| &self.runtime.plans[self.runtime.selected_plan]);
        let row = |ui: &mut egui::Ui,
                   depth: usize,
                   link: &atomics_core::tickets::TicketLink,
                   status: Option<TaskStatus>| {
            let dot = match (&link.state, status) {
                (Some(state), _) => config.states.status_of(state).map_or(p.muted, |s| super::board::task_color(s, p)),
                (None, Some(status)) => super::board::task_color(status, p),
                (None, None) => p.muted,
            };
            let mut text = egui::text::LayoutJob::default();
            text.append("● ", 0.0, egui::TextFormat { font_id: p.ui(10.0), color: dot, ..Default::default() });
            text.append(
                &format!("{} {}", link.label(), link.title.as_deref().unwrap_or("")),
                0.0,
                egui::TextFormat { font_id: p.ui(12.0), color: p.foreground, ..Default::default() },
            );
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                ui.add(egui::Button::selectable(false, text).truncate())
                    .on_hover_text(link.title.as_deref().unwrap_or(""))
            })
            .inner
            .clicked()
        };
        match plan.and_then(|plan| plan.ticket.as_ref().map(|ticket| (plan, ticket))) {
            Some((plan, ticket)) => {
                if row(ui, 0, ticket, None) {
                    open = Some(ticket.clone());
                }
                for task in &plan.tasks {
                    if let Some(link) = &task.ticket
                        && row(ui, 1, link, Some(task.status))
                    {
                        open = Some(link.clone());
                    }
                }
                for item in self.runtime.tickets.boards.get(&plan.id).into_iter().flatten() {
                    let tasked = plan.tasks.iter().any(|t| t.ticket.as_ref().is_some_and(|l| l.same_ticket(item)));
                    if !tasked && row(ui, 1, item, None) {
                        open = Some(item.clone());
                    }
                }
            }
            None => {
                if ui.add(egui::Button::selectable(false, RichText::new("Find tickets…").size(12.0))).clicked() {
                    find = true;
                }
                for link in &self.runtime.tickets.panel.recent {
                    if row(ui, 0, link, None) {
                        open = Some(link.clone());
                    }
                }
            }
        }
        ui.add_space(10.0);
        if find {
            self.runtime.show_find();
        } else if let Some(link) = open {
            self.runtime.open_ticket(link, tickets_panel::Nav::Fresh);
        }
    }
}

impl Desktop<'_> {
    pub(super) fn settings_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Edit config").clicked() {
            let _ = open_path(&self.runtime.config_path);
            ui.close();
        }
        if global_hooks_installed() {
            ui.label(RichText::new("Hooks for all Claude sessions are installed").size(12.0));
        } else if ui
            .button("Install hooks for all Claude sessions")
            .on_hover_text(
                "Adds `atomics hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
            )
            .clicked()
        {
            match install_global_hooks() {
                Ok(path) => self.runtime.toast(format!("Hooks added to {}", paths::display(&path))),
                Err(err) => self.runtime.toast(format!("Installing hooks failed: {err:#}")),
            }
            ui.close();
        }
        ui.separator();
        self.themes_menu(ui);
        self.sandbox_menu(ui);
        self.credentials_menu(ui);
        self.connections_menu(ui);
        ui.separator();
        let keys = &self.runtime.config.keys;
        for (key, what) in [
            (&keys.quick_open, "open file"),
            (&keys.toggle_diff, "show/hide diff"),
            (&keys.next_attention, "next agent that needs you"),
            (&keys.focus_steer, "steer bar"),
            (&keys.new_session, "new session"),
            (&keys.voice, "voice: record, send (Esc cancels)"),
            (&keys.github, "GitHub controls"),
        ] {
            ui.label(RichText::new(format!("{}  {what}", shortcut_label(ui.ctx(), key))).size(12.0).monospace());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan() {
        with_app(
            "app::sidebar::tests::clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan",
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
                let mut frame = |app: &mut Desktop<'_>, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1700.0, 1000.0))),
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
                for (goal, index, visible) in [("Atlas", one, 0), ("Beacon", two, 1)] {
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == "Repos · 0"));
                    assert!(!text.iter().any(|(t, _)| t.starts_with("repo-")));
                    assert!(!text.iter().any(|(t, _)| t.contains("Archived")));
                    assert!(text.iter().any(|(t, _)| t.starts_with("✓ Completed")));
                    let pos = text.iter().find(|(t, _)| t.starts_with(&format!("● {goal} ·"))).unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                    frame(app, vec![button(pos, false)]);
                    assert_eq!(app.runtime.selected_plan, index);
                    assert!(app.runtime.plan_view);
                    assert_eq!(app.runtime.visible_sessions(), [visible]);
                    app.runtime.toggle_plan();
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} task")));
                    let other = if goal == "Atlas" { "Beacon task" } else { "Atlas task" };
                    assert!(!text.iter().any(|(t, _)| t == other));
                    assert!(!text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let task_pos = text.iter().find(|(t, _)| t == &format!("{goal} task")).unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(task_pos), button(task_pos, true)]);
                    frame(app, vec![button(task_pos, false)]);
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let approve = text.iter().find(|(t, _)| t == "Approve plan").unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(approve), button(approve, true)]);
                    frame(app, vec![button(approve, false)]);
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
