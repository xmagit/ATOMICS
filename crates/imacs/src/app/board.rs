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

/// A ticket's key and state. A click opens it in the browser; hovering shows the rest.
/// A failed ticket operation shows beneath it, and a state that means another IMACS
/// status than the task's is pointed out.
fn ticket_chip(
    ui: &mut egui::Ui,
    link: &imacs_core::tickets::TicketLink,
    means: Option<(TaskStatus, TaskStatus)>,
    p: &Palette,
) {
    let color = if link.error.is_some() { p.warning } else { p.info };
    ui.horizontal_wrapped(|ui| {
        let text = match &link.state {
            Some(state) => format!("{} · {state}", link.label()),
            None => link.label().to_string(),
        };
        let mut response = egui::Frame::new()
            .fill(lerp(p.raised, color, 0.12))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(6, 1))
            .show(ui, |ui| ui.add(egui::Label::new(RichText::new(text).color(color).size(11.0)).selectable(false)))
            .response
            .interact(Sense::click());
        if link.url.is_some() {
            response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
        }
        let response = response.on_hover_ui(|ui| {
            if let Some(title) = &link.title {
                ui.label(RichText::new(title).strong());
            }
            let system = if link.system.is_empty() { "ticket" } else { &link.system };
            ui.label(format!("{system} {}", link.id));
            if let Some(updated) = &link.updated_at {
                ui.label(format!("updated {updated}"));
            }
            match link.synced_at {
                Some(at) => ui.label(format!("read {} min ago", imacs_core::store::now().saturating_sub(at) / 60)),
                None => ui.label("not read yet"),
            };
            if let Some(error) = &link.error {
                ui.label(RichText::new(error).color(p.error));
            }
            if let Some(url) = &link.url {
                ui.label(RichText::new(format!("click to open {url}")).color(p.muted));
            }
        });
        if response.clicked()
            && let Some(url) = &link.url
        {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if let Some((ticket, task)) = means.filter(|(ticket, task)| ticket != task) {
            ui.label(RichText::new(format!("ticket says {}", ticket.label())).color(p.warning).size(11.0))
                .on_hover_text(format!("The task is {}; IMACS leaves the ticket as it was set", task.label()));
        }
    });
    if let Some(error) = &link.error {
        ui.label(RichText::new(format!("⚠ {}", truncate(error, 160))).color(p.error).size(11.0)).on_hover_text(error);
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
        let read_only = self.runtime.plans[self.runtime.selected_plan].status != PlanStatus::Open;
        let mut reopen = false;
        let mut archive = false;
        let mut approve = false;
        let mut dispatch_all = false;
        let mut close_status = None;
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
            if read_only {
                let status = if self.runtime.plans[self.runtime.selected_plan].status == PlanStatus::Completed {
                    "Completed ✓ · read-only"
                } else {
                    "Archived · read-only"
                };
                ui.label(RichText::new(status).color(p.muted).size(12.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if read_only {
                    if self.runtime.plans[self.runtime.selected_plan].status == PlanStatus::Completed {
                        archive = ui.small_button("Archive").clicked();
                        clean_plan = ui.small_button("Clean up plan…").clicked();
                    }
                    reopen = ui.small_button("Reopen plan").clicked();
                } else {
                    if ui.small_button("Close plan…").clicked() {
                        close_status = Some(PlanStatus::Completed);
                    }
                    if total == 0 {
                        return;
                    }
                    if !self.runtime.plans[self.runtime.selected_plan].approved {
                        let button = egui::Button::new(RichText::new("Approve plan").color(Color32::WHITE))
                            .fill(p.accent.gamma_multiply(0.7));
                        approve = ui.add(button).on_hover_text("Let the orchestrator dispatch these tasks").clicked();
                    } else {
                        dispatch_all = ui
                            .add_enabled(ready > 0, egui::Button::new(format!("Dispatch ready ({ready})")))
                            .on_hover_text("Start every task whose dependencies are done")
                            .clicked();
                        ui.label(RichText::new("approved ✓").color(p.success).size(12.0));
                    }
                }
            });
        });
        if !self.runtime.plans[self.runtime.selected_plan].goal.is_empty() {
            ui.label(
                RichText::new(&self.runtime.plans[self.runtime.selected_plan].goal)
                    .color(self.runtime.plan_state(self.runtime.selected_plan).color(&p))
                    .size(13.0),
            );
        }
        let integration = self.runtime.plans[self.runtime.selected_plan].integration_branch();
        if !self.runtime.plans[self.runtime.selected_plan].tasks.is_empty() {
            ui.label(RichText::new(format!("integration branch: {integration}")).color(p.muted).size(11.5))
                .on_hover_text("Integrated tasks are merged into it, and tasks with dependencies branch from it");
        }
        let ticket_config = self.runtime.ticket_config();
        let mut sync_tickets = false;
        if let Some(link) = self.runtime.plans[self.runtime.selected_plan].ticket.clone() {
            let plan = &self.runtime.plans[self.runtime.selected_plan];
            ui.horizontal_wrapped(|ui| {
                let system = ticket_config.as_ref().map_or(link.system.as_str(), |c| c.system.as_str());
                let what = match ticket_config.as_ref().map(|c| c.mode) {
                    Some(imacs_core::tickets::TicketMode::Board) => "board",
                    _ => "ticket",
                };
                ui.label(RichText::new(format!("{system} {what}")).color(p.muted).size(11.5));
                let means = ticket_config
                    .as_ref()
                    .filter(|c| c.mode == imacs_core::tickets::TicketMode::Ticket)
                    .and_then(|c| c.states.status_of(link.state.as_deref()?))
                    .map(|ticket| (ticket, imacs_core::tickets::plan_status(plan)));
                ui.vertical(|ui| ticket_chip(ui, &link, means, &p));
                if let Some(title) = &link.title {
                    ui.label(RichText::new(title).color(p.foreground).size(11.5));
                }
                if let Some(items) = self.runtime.tickets.boards.get(&plan.id) {
                    let planned = |item: &imacs_core::tickets::TicketLink| {
                        plan.tasks.iter().any(|t| t.ticket.as_ref().is_some_and(|l| l.same_ticket(item)))
                    };
                    let loose = items.iter().filter(|item| !planned(item)).count();
                    ui.label(
                        RichText::new(format!("{} tickets · {loose} without a task", items.len()))
                            .color(if loose > 0 { p.warning } else { p.muted })
                            .size(11.5),
                    );
                }
                if ticket_config.is_some() {
                    sync_tickets = ui.small_button("sync").on_hover_text("Read the tickets again now").clicked();
                }
            });
        }
        if self.runtime.plans[self.runtime.selected_plan].status == PlanStatus::Completed {
            ui.label(
                RichText::new("Completed · preview cleanup to check what is merged and safe to remove.").color(p.muted),
            );
            if clean_plan {
                self.open_plan_cleanup(self.runtime.selected_plan);
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
        let editable = !read_only && !self.runtime.plans[self.runtime.selected_plan].approved;
        let ready_ids: HashSet<String> =
            self.runtime.plans[self.runtime.selected_plan].ready().iter().map(|t| t.id.clone()).collect();
        let approved = self.runtime.plans[self.runtime.selected_plan].approved;
        let sessions: HashMap<String, (String, AgentStatus)> =
            self.runtime.sessions.iter().map(|s| (s.id.clone(), (s.title.clone(), s.status))).collect();
        let expanded = &mut self.client.expanded_tasks;
        let integrating = &self.runtime.integrating;
        let plan_id = self.runtime.plans[self.runtime.selected_plan].id.clone();
        let ticket_states = ticket_config.as_ref().map(|c| c.states.clone());
        let mut edited_tasks = self.runtime.plans[self.runtime.selected_plan].tasks.clone();
        let tasks = &mut edited_tasks;
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
                                                    if let Some(link) = &task.ticket {
                                                        let means = ticket_states
                                                            .as_ref()
                                                            .and_then(|s| s.status_of(link.state.as_deref()?))
                                                            .map(|ticket| (ticket, task.status));
                                                        ticket_chip(ui, link, means, &p);
                                                    }
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
                                                    if let Some(action) = task_result(
                                                        ui,
                                                        task,
                                                        status,
                                                        open,
                                                        read_only,
                                                        &self.runtime.config,
                                                        &p,
                                                    ) {
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
                                                            let response = ui.add(edit);
                                                            self.runtime.plan_edit_open |= response.has_focus();
                                                            if response.changed() {
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
                                                        if read_only {
                                                            return;
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

        for (id, action) in actions {
            match action {
                "toggle" => {
                    let key = (plan_id.clone(), id);
                    if !self.client.expanded_tasks.remove(&key) {
                        self.client.expanded_tasks.insert(key);
                    }
                }
                "edited" => {
                    if let Some(edited) = edited_tasks.iter().find(|t| t.id == id) {
                        let brief = edited.brief.clone();
                        self.post_plan(self.runtime.selected_plan, move |state, index| {
                            if !state.plans[index].approved
                                && let Some(task) = state.plans[index].task_mut(&id)
                                && task.status == TaskStatus::Planned
                            {
                                task.brief = brief;
                                state.save_plan(index);
                            }
                        });
                    }
                }
                "start" => self.post_plan(self.runtime.selected_plan, move |state, index| {
                    if let Err(err) = state.dispatch(index, Some(&id)) {
                        state.toast(err);
                    }
                }),
                "integrate" => {
                    self.post_plan(self.runtime.selected_plan, move |state, index| state.integrate(index, &id, None))
                }
                "diff" | "resume" => {
                    if let Some(task) =
                        self.runtime.plans[self.runtime.selected_plan].tasks.iter().position(|t| t.id == id)
                    {
                        let index = self.runtime.selected_plan;
                        self.task_action(index, task, action);
                    }
                }
                status => {
                    if let Some(status) = TaskStatus::parse(status) {
                        self.post_plan(self.runtime.selected_plan, move |state, index| {
                            if state.plans[index].status != PlanStatus::Open {
                                return;
                            }
                            let old = state.plans[index].task_mut(&id).map(|task| {
                                let old = task.status;
                                task.set_status(status);
                                old
                            });
                            state.finish_plan_agents();
                            state.save_plan(index);
                            if let Some(old) = old {
                                let author = imacs_core::tickets::Author::User;
                                state.ticket_status_changed(index, &id, old, status, None, author);
                            }
                        });
                    }
                }
            }
        }
        if approve {
            self.post_plan(self.runtime.selected_plan, |state, index| {
                state.plans[index].approved = true;
                state.save_plan(index);
                state.toast("Plan approved: the orchestrator can dispatch it now");
            });
        }
        if sync_tickets {
            self.post_plan(self.runtime.selected_plan, |state, index| {
                let id = state.plans[index].id.clone();
                state.resync_tickets(&id);
            });
        }
        if dispatch_all {
            self.post_plan(self.runtime.selected_plan, |state, index| {
                if let Err(err) = state.dispatch(index, None) {
                    state.toast(err);
                }
            });
        }
        if let Some(session) = open_session {
            self.activate(&session, true);
            self.runtime.panel = PanelMode::Diff;
        }
        if reopen {
            self.post_plan(self.runtime.selected_plan, |state, index| {
                if let Err(error) = state.reopen_plan(index) {
                    state.toast(error);
                }
            });
        }
        if archive {
            self.post_plan(self.runtime.selected_plan, |state, index| {
                if let Err(error) = state.close_plan(index, PlanStatus::Archived) {
                    state.toast(error);
                }
            });
        }
        if let Some(status) = close_status {
            self.client.confirm_plan_close = Some((self.runtime.plans[self.runtime.selected_plan].id.clone(), status));
        }
    }
}
