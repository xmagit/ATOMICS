use super::*;

impl State {
    pub(super) fn toggle_plan(&mut self) {
        self.ctx.request_repaint();
        if self.panel == PanelMode::Plan && self.diff_open {
            self.panel = PanelMode::Diff;
        } else {
            self.panel = PanelMode::Plan;
            self.diff_open = true;
        }
    }
}

fn task_color(status: TaskStatus, p: &Palette) -> Color32 {
    match status {
        TaskStatus::Planned => p.muted,
        TaskStatus::Running => p.accent,
        TaskStatus::Blocked => p.warning,
        TaskStatus::Review => p.info,
        TaskStatus::Done => p.success,
    }
}

impl Desktop<'_> {
    pub(super) fn plan_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let p = self.runtime.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.background);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("plan-panel", &self.runtime.plans[self.runtime.selected_plan].id))
                .max_rect(rect.shrink2(vec2(12.0, 8.0))),
        );
        let ui = &mut child;
        let mut approve = false;
        let mut dispatch_all = false;
        let mut close_status = None;
        let mut close = false;
        let mut clean_plan = false;
        let ready = self.runtime.plans[self.runtime.selected_plan].ready().len();
        if self
            .runtime
            .integration_checks
            .iter()
            .any(|(plan, _)| plan == &self.runtime.plans[self.runtime.selected_plan].id)
        {
            ui.label(RichText::new("Checking integration freshness…").color(p.muted));
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("PLAN").color(p.accent).strong().size(13.0));
            let total = self.runtime.plans[self.runtime.selected_plan].tasks.len();
            if total > 0 {
                let done = self.runtime.plans[self.runtime.selected_plan].count(TaskStatus::Done);
                ui.label(RichText::new(format!("{done}/{total} done")).color(p.muted).size(12.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.runtime.plans[self.runtime.selected_plan].status == PlanStatus::Completed {
                    clean_plan = ui.small_button("Clean up plan…").clicked();
                }
                if self.runtime.plans[self.runtime.selected_plan].is_open() {
                    if ui.small_button("Archive plan").clicked() {
                        close_status = Some(PlanStatus::Archived);
                    }
                    if ui.small_button("Complete plan").clicked() {
                        close_status = Some(PlanStatus::Completed);
                    }
                }
                if ui.small_button("×").on_hover_text("Back to the diff").clicked() {
                    close = true;
                }
                if total == 0 {
                    return;
                }
                if self.runtime.plans[self.runtime.selected_plan].status == plan::PlanStatus::Completed {
                    if ui.small_button("Archive plan").clicked() {
                        close_status = Some(PlanStatus::Archived);
                    }
                } else if !self.runtime.plans[self.runtime.selected_plan].approved {
                    let button = egui::Button::new(RichText::new("Approve plan").color(Color32::WHITE))
                        .fill(p.accent.gamma_multiply(0.7));
                    if ui.add(button).on_hover_text("Let the orchestrator dispatch these tasks").clicked() {
                        approve = true;
                    }
                } else {
                    let button = egui::Button::new(format!("Dispatch ready ({ready})"));
                    if ui
                        .add_enabled(ready > 0, button)
                        .on_hover_text("Start every task whose dependencies are done")
                        .clicked()
                    {
                        dispatch_all = true;
                    }
                    ui.label(RichText::new("approved ✓").color(p.success).size(12.0));
                }
            });
        });
        if !self.runtime.plans[self.runtime.selected_plan].goal.is_empty() {
            ui.label(
                RichText::new(&self.runtime.plans[self.runtime.selected_plan].goal).color(p.foreground).size(13.0),
            );
        }
        let integration = self.runtime.plans[self.runtime.selected_plan].integration_branch();
        if !self.runtime.plans[self.runtime.selected_plan].tasks.is_empty() {
            ui.label(RichText::new(format!("integration branch: {integration}")).color(p.muted).size(11.5))
                .on_hover_text("Integrated tasks are merged into it, and tasks with dependencies branch from it");
        }
        if self.runtime.plans[self.runtime.selected_plan].status == PlanStatus::Completed {
            ui.label(
                RichText::new("Completed · preview cleanup to check what is merged and safe to remove.").color(p.muted),
            );
            if clean_plan {
                self.runtime.open_plan_cleanup(self.runtime.selected_plan);
            }
        }
        for (task, dep) in self.runtime.plans[self.runtime.selected_plan].unknown_dependencies() {
            ui.label(RichText::new(format!("{task} depends on unknown task {dep}")).color(p.error).size(11.5));
        }
        ui.add_space(6.0);
        if self.runtime.plans[self.runtime.selected_plan].tasks.is_empty() {
            ui.label(
                RichText::new(
                    "No plan yet. Ask an orchestrator (a Claude session with the IMACS MCP server) to plan a goal: \
                     its tasks appear here for you to review, edit and approve before any agent starts.",
                )
                .color(p.muted),
            );
        }

        let mut actions: Vec<(String, &str)> = Vec::new();
        let mut open_session: Option<String> = None;
        let editable = !self.runtime.plans[self.runtime.selected_plan].approved;
        let ready_ids: HashSet<String> =
            self.runtime.plans[self.runtime.selected_plan].ready().iter().map(|t| t.id.clone()).collect();
        let approved = self.runtime.plans[self.runtime.selected_plan].approved;
        let sessions: HashMap<String, (String, AgentStatus)> =
            self.runtime.sessions.iter().map(|s| (s.id.clone(), (s.title.clone(), s.status))).collect();
        let expanded = &mut self.client.expanded_tasks;
        let integrating = &self.runtime.integrating;
        let plan_id = self.runtime.plans[self.runtime.selected_plan].id.clone();
        let tasks = &mut self.runtime.plans[self.runtime.selected_plan].tasks;
        if !tasks.is_empty() {
            egui::ScrollArea::horizontal().id_salt(("plan-board", &plan_id)).auto_shrink([false, false]).show(
                ui,
                |ui| {
                    let columns = TaskStatus::ALL.len();
                    let spacing = ui.spacing().item_spacing.x * (columns - 1) as f32;
                    ui.set_width(ui.available_width().max(280.0 * columns as f32 + spacing));
                    ui.columns(TaskStatus::ALL.len(), |columns| {
                        for (column, status) in columns.iter_mut().zip(TaskStatus::ALL) {
                            let count = tasks.iter().filter(|t| t.status == status).count();
                            let color = task_color(status, &p);
                            column.label(
                                RichText::new(format!("{} · {count}", status.label().to_uppercase()))
                                    .color(color)
                                    .strong()
                                    .size(11.0),
                            );
                            column.add_space(4.0);
                            egui::ScrollArea::vertical()
                                .id_salt(("plan-column", &plan_id, status.label()))
                                .auto_shrink([false, false])
                                .show(column, |ui| {
                                    for task in tasks.iter_mut().filter(|t| t.status == status) {
                                        let open = expanded.contains(&(plan_id.clone(), task.id.clone()));
                                        egui::Frame::new()
                                            .fill(p.raised)
                                            .stroke(Stroke::new(
                                                1.0,
                                                if open { color.gamma_multiply(0.6) } else { p.border },
                                            ))
                                            .corner_radius(3)
                                            .inner_margin(egui::Margin::same(8))
                                            .show(ui, |ui| {
                                                // Columns justify their children; card text needs ordinary word spacing.
                                                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                                                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                                                    ui.set_width(ui.available_width());
                                                    let title = ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&task.title).color(p.foreground).strong(),
                                                        )
                                                        .sense(Sense::click()),
                                                    );
                                                    if title.clicked() {
                                                        actions.push((task.id.clone(), "toggle"));
                                                    }
                                                    let mut meta = format!(
                                                        "{} · {}",
                                                        task.id,
                                                        if task.agent.is_empty() { "claude" } else { &task.agent }
                                                    );
                                                    if let Some(model) = task
                                                        .args
                                                        .windows(2)
                                                        .find(|w| w[0] == "-m" || w[0] == "--model")
                                                        .map(|w| w[1].clone())
                                                    {
                                                        meta.push_str(&format!(" {model}"));
                                                    }
                                                    ui.label(RichText::new(meta).color(p.muted).size(11.0));
                                                    if let Some(branch) = &task.branch {
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(branch).color(p.muted).size(11.0),
                                                            )
                                                            .truncate(),
                                                        );
                                                    }
                                                    if !task.depends_on.is_empty() {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "after {}",
                                                                task.depends_on.join(", ")
                                                            ))
                                                            .color(p.muted)
                                                            .size(11.0),
                                                        );
                                                    }
                                                    if let Some((title, agent_status)) =
                                                        task.session.as_ref().and_then(|s| sessions.get(s))
                                                    {
                                                        ui.label(
                                                            RichText::new(format!(
                                                                "● {title} · {}",
                                                                agent_status.label()
                                                            ))
                                                            .color(status_color(*agent_status, &p))
                                                            .size(11.0),
                                                        );
                                                    }
                                                    let status = task
                                                        .session
                                                        .as_ref()
                                                        .and_then(|s| sessions.get(s))
                                                        .map(|(_, s)| *s);
                                                    if let Some(action) =
                                                        task_result(ui, task, status, open, &self.runtime.config, &p)
                                                    {
                                                        actions.push((task.id.clone(), action));
                                                    }
                                                    if let Some(note) = task.notes.last() {
                                                        ui.label(
                                                            RichText::new(note).color(p.muted).italics().size(11.0),
                                                        );
                                                    }
                                                    if open {
                                                        ui.add_space(4.0);
                                                        if editable && task.status == TaskStatus::Planned {
                                                            let edit = egui::TextEdit::multiline(&mut task.brief)
                                                                .desired_rows(4)
                                                                .desired_width(f32::INFINITY);
                                                            if ui.add(edit).changed() {
                                                                actions.push((task.id.clone(), "edited"));
                                                            }
                                                        } else {
                                                            ui.label(
                                                                RichText::new(&task.brief)
                                                                    .color(p.foreground)
                                                                    .size(12.0),
                                                            );
                                                        }
                                                        for check in &task.acceptance {
                                                            ui.label(
                                                                RichText::new(format!("✓ {check}"))
                                                                    .color(p.muted)
                                                                    .size(11.5),
                                                            );
                                                        }
                                                    }
                                                    ui.horizontal_wrapped(|ui| {
                                                        if task
                                                            .session
                                                            .as_ref()
                                                            .is_some_and(|s| sessions.contains_key(s))
                                                            && ui.small_button("open").clicked()
                                                        {
                                                            open_session = task.session.clone();
                                                        }
                                                        match task.status {
                                                            TaskStatus::Planned
                                                                if approved && ready_ids.contains(&task.id) =>
                                                            {
                                                                if ui.small_button("start").clicked() {
                                                                    actions.push((task.id.clone(), "start"));
                                                                }
                                                            }
                                                            TaskStatus::Review => {
                                                                if ui.small_button("accept").clicked() {
                                                                    actions.push((task.id.clone(), "done"));
                                                                }
                                                                if ui
                                                                    .small_button("send back")
                                                                    .on_hover_text(
                                                                        "Back to running; steer it in the steer bar",
                                                                    )
                                                                    .clicked()
                                                                {
                                                                    actions.push((task.id.clone(), "running"));
                                                                }
                                                            }
                                                            TaskStatus::Blocked => {
                                                                if ui.small_button("unblock").clicked() {
                                                                    actions.push((task.id.clone(), "running"));
                                                                }
                                                            }
                                                            TaskStatus::Done
                                                                if task
                                                                    .session
                                                                    .as_ref()
                                                                    .is_some_and(|s| sessions.contains_key(s))
                                                                    && ui.small_button("reopen").clicked() =>
                                                            {
                                                                actions.push((task.id.clone(), "review"));
                                                            }
                                                            _ => {}
                                                        }
                                                        if task.id != plan::REVIEW_TASK
                                                            && task.branch.is_some()
                                                            && matches!(
                                                                task.status,
                                                                TaskStatus::Review | TaskStatus::Done
                                                            )
                                                        {
                                                            let busy = integrating
                                                                .contains(&(plan_id.clone(), task.id.clone()));
                                                            let label =
                                                                if busy { "integrating…" } else { "integrate" };
                                                            if ui
                                                                .add_enabled(!busy, egui::Button::new(label).small())
                                                                .on_hover_text(format!(
                                                                    "Commit its changes and merge it into {integration}"
                                                                ))
                                                                .clicked()
                                                            {
                                                                actions.push((task.id.clone(), "integrate"));
                                                            }
                                                        }
                                                    });
                                                });
                                            });
                                        ui.add_space(6.0);
                                    }
                                });
                        }
                    });
                },
            );
        }

        let mut changed = false;
        for (id, action) in actions {
            match action {
                "toggle" => {
                    let key = (plan_id.clone(), id);
                    if !self.client.expanded_tasks.remove(&key) {
                        self.client.expanded_tasks.insert(key);
                    }
                }
                "edited" => changed = true,
                "start" => {
                    if let Err(err) = self.runtime.dispatch(self.runtime.selected_plan, Some(&id)) {
                        self.runtime.toast(err);
                    }
                }
                "integrate" => self.runtime.integrate(self.runtime.selected_plan, &id, None),
                "diff" | "resume" => {
                    if let Some(task) =
                        self.runtime.plans[self.runtime.selected_plan].tasks.iter().position(|t| t.id == id)
                    {
                        self.runtime.task_action(self.runtime.selected_plan, task, action);
                    }
                }
                status => {
                    if let (Some(task), Some(status)) =
                        (self.runtime.plans[self.runtime.selected_plan].task_mut(&id), TaskStatus::parse(status))
                    {
                        task.set_status(status);
                        changed = true;
                    }
                }
            }
        }
        if approve {
            self.runtime.plans[self.runtime.selected_plan].approved = true;
            changed = true;
            self.runtime.toast("Plan approved: the orchestrator can dispatch it now");
        }
        if dispatch_all && let Err(err) = self.runtime.dispatch(self.runtime.selected_plan, None) {
            self.runtime.toast(err);
        }
        if changed {
            self.runtime.finish_plan_agents();
            self.runtime.save_plan(self.runtime.selected_plan);
        }
        if let Some(session) = open_session {
            self.runtime.activate(&session, true);
            self.runtime.panel = PanelMode::Diff;
        }
        if close {
            self.runtime.panel = PanelMode::Diff;
        }
        if let Some(status) = close_status {
            self.client.confirm_plan_close = Some((self.runtime.plans[self.runtime.selected_plan].id.clone(), status));
        }
    }
}
