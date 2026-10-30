use super::*;

impl State {
    /// Sessions in the selected project or plan view, excluding the chief.
    pub(super) fn visible_sessions(&self) -> Vec<usize> {
        (0..self.sessions.len())
            .filter(|&i| !self.sessions[i].chief)
            .filter(|&i| {
                if self.plan_view {
                    self.plans[self.selected_plan].contains_session(&self.sessions[i].id)
                } else {
                    match &self.view {
                        None => true,
                        Some(project) => self.project_of(&self.sessions[i].root()).as_ref() == Some(project),
                    }
                }
            })
            .collect()
    }

    pub(super) fn completed_tasks(&self) -> Vec<(usize, usize)> {
        self.plans
            .iter()
            .enumerate()
            .filter(|(index, plan)| {
                plan.status != plan::PlanStatus::Archived
                    && if self.plan_view {
                        *index == self.selected_plan
                    } else {
                        match &self.view {
                            None => plan.is_open(),
                            Some(project) => plan.project.as_ref() == Some(project),
                        }
                    }
            })
            .flat_map(|(index, plan)| {
                plan.tasks
                    .iter()
                    .enumerate()
                    .filter(|(_, task)| {
                        task.finished_at.is_some()
                            && task.worktree.is_some()
                            && !self.sessions.iter().any(|s| task.session.as_deref() == Some(&s.id))
                    })
                    .map(move |(task, _)| (index, task))
            })
            .collect()
    }

    /// Closes the project's idle and exited agents that no plan task is using. Shells
    /// are always idle to IMACS, so they are left alone.
    pub(super) fn close_idle(&mut self, project: &Path) -> usize {
        let in_plan: HashSet<String> =
            self.plans.iter().flat_map(|p| &p.tasks).filter_map(|t| t.session.clone()).collect();
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| !s.chief && s.is_agent() && matches!(s.status, AgentStatus::Idle | AgentStatus::Exited))
            .filter(|s| !in_plan.contains(&s.id))
            .filter(|s| self.project_of(&s.root()).as_deref() == Some(project))
            .map(|s| s.id.clone())
            .collect();
        for id in &idle {
            self.close(id);
        }
        idle.len()
    }
}

/// What the slot queue holds for the process tree under `pid`.
pub(super) fn queue_note(pid: u32, running: &[Ticket], waiting: &[Ticket]) -> Option<String> {
    if let Some(ticket) = running.iter().find(|t| t.ancestors.contains(&pid)) {
        return Some(format!("{} slot", ticket.class.name()));
    }
    let ticket = waiting.iter().find(|t| t.ancestors.contains(&pid))?;
    let place = waiting.iter().filter(|t| t.class == ticket.class).position(|t| t.id == ticket.id)? + 1;
    Some(format!("queued for a {} slot ({place} in line)", ticket.class.name()))
}

impl Desktop<'_> {
    pub(super) fn tab_strip(&mut self, ui: &mut egui::Ui, rect: Rect, tiled: &[usize], visible: &[usize]) {
        let p = self.runtime.palette.clone();
        ui.painter().rect_filled(rect, 0.0, p.panel);
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("tab-strip")
                .max_rect(rect.shrink2(vec2(6.0, 3.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut clicked = None;
        egui::ScrollArea::horizontal().id_salt("tab-strip-scroll").show(&mut child, |ui| {
            ui.horizontal(|ui| {
                for &i in visible {
                    let s = &self.runtime.sessions[i];
                    let active = self.runtime.active.as_deref() == Some(s.id.as_str());
                    let shown = tiled.contains(&i);
                    let color = if active {
                        p.foreground
                    } else if shown {
                        lerp(p.muted, p.foreground, 0.5)
                    } else {
                        p.muted
                    };
                    let text =
                        RichText::new(format!("{}  {}", status_glyph(s.status), s.title)).color(color).size(12.5);
                    let response = ui.add(
                        egui::Button::new(text)
                            .fill(if active { lerp(p.panel, p.accent, 0.25) } else { Color32::TRANSPARENT })
                            .stroke(Stroke::NONE),
                    );
                    if response.clicked() {
                        clicked = Some(s.id.clone());
                    }
                }
            });
        });
        if let Some(id) = clicked {
            self.activate(&id, false);
        }
    }

    pub(super) fn tiles(&mut self, ui: &mut egui::Ui, rect: Rect, layout: &Layout, tiled: &[usize]) {
        if layout.tiled + layout.completed == 0 {
            return;
        }
        let metrics = self.metrics(ui);
        let p = self.runtime.palette.clone();
        let (cols, rows) = (layout.columns.max(1), layout.rows.max(1));
        let size = vec2(rect.width() / cols as f32, rect.height() / rows as f32);
        let mut activate: Option<(String, bool)> = None;
        let mut close: Option<String> = None;
        let mut resume: Option<usize> = None;
        let mut approval = None;
        let focus_request = self.client.focus_terminal.take();
        let now = ui.input(|i| i.time);
        let (slots_running, slots_waiting) = self.runtime.governor.snapshot();

        for (slot, &index) in tiled.iter().enumerate() {
            let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
            let tile = Rect::from_min_size(min, size).shrink(3.0);
            let active = self.runtime.active.as_deref() == Some(self.runtime.sessions[index].id.as_str());
            let reported = if matches!(self.runtime.sessions[index].status, AgentStatus::Idle | AgentStatus::Done) {
                self.runtime
                    .plans
                    .iter()
                    .flat_map(|p| &p.tasks)
                    .find(|t| t.session.as_deref() == Some(&self.runtime.sessions[index].id))
                    .and_then(|t| t.reported_line(imacs_core::store::now()))
            } else {
                None
            };
            let session = &mut self.runtime.sessions[index];

            let header = tile.with_max_y(tile.top() + TILE_HEADER);
            let mut body = tile.with_min_y(header.bottom());
            ui.painter().rect_filled(header, 2.0, if active { lerp(p.raised, p.accent, 0.12) } else { p.raised });
            ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, p.border));
            if active {
                ui.painter().rect_filled(header.with_max_x(header.left() + 2.0), 0.0, p.accent);
            }
            ui.painter().circle_filled(
                pos2(header.left() + 12.0, header.center().y),
                3.5,
                status_dot(session.status, &p, now),
            );
            let mut badge_width = 0.0;
            if let Some(name) = &session.sandbox {
                let usage = self.runtime.sandbox_usage.usage.get(name);
                let text = usage.map_or_else(|| "sandbox · —".into(), |usage| format!("sandbox · {}", usage.label()));
                let galley = ui.fonts_mut(|f| f.layout_no_wrap(text, p.ui(10.0), p.info));
                badge_width = galley.size().x + 15.0;
                let badge = Rect::from_min_max(
                    pos2((header.right() - 28.0 - badge_width).max(header.left() + 22.0), header.center().y - 8.5),
                    pos2(header.right() - 31.0, header.center().y + 8.5),
                );
                ui.painter().rect_filled(badge, 4.0, lerp(p.raised, p.info, 0.15));
                ui.painter().with_clip_rect(badge).galley(
                    badge.left_center() + vec2(5.0, -galley.size().y / 2.0),
                    galley,
                    p.info,
                );
                ui.interact(badge, ui.scope_id().with(("sandbox-usage", name)), Sense::hover()).on_hover_text(
                    self.runtime.sandbox_usage.error.as_deref().unwrap_or(
                        "Host VM process tree: resident memory and ps CPU (100% = one core). — means no matching VM process yet."
                    )
                );
            }
            let title_rect = Rect::from_min_max(
                pos2(header.left() + 22.0, header.top()),
                pos2(header.right() - 28.0 - badge_width, header.bottom()),
            );
            let title_response =
                ui.interact(title_rect, ui.scope_id().with(("tile-title", &session.id)), Sense::click());
            let title_color = if title_response.hovered() || active { p.accent } else { p.foreground };
            let mut subtitle = match session.branch() {
                Some(branch) => format!("{} · {} · {}", session.agent, branch, session.status.label()),
                None => format!("{} · {}", session.agent, session.status.label()),
            };
            if let Some(note) =
                session.terminal.as_ref().and_then(|t| queue_note(t.process_id(), &slots_running, &slots_waiting))
            {
                subtitle.push_str(&format!(" · {note}"));
            }
            let galley = ui.fonts_mut(|f| f.layout_no_wrap(session.title.clone(), p.ui(13.0), title_color));
            let title_width = galley.size().x;
            ui.painter().with_clip_rect(title_rect).galley(
                pos2(title_rect.left(), header.center().y - galley.size().y / 2.0),
                galley,
                title_color,
            );
            ui.painter().with_clip_rect(title_rect).text(
                pos2(title_rect.left() + title_width + 10.0, header.center().y),
                egui::Align2::LEFT_CENTER,
                subtitle,
                p.ui(11.5),
                p.muted,
            );
            if title_response.on_hover_text("Show this worktree's diff against main").clicked() {
                activate = Some((session.id.clone(), true));
            }
            let close_rect = Rect::from_center_size(pos2(header.right() - 14.0, header.center().y), Vec2::splat(18.0));
            let close_response =
                ui.interact(close_rect, ui.scope_id().with(("tile-close", &session.id)), Sense::click());
            ui.painter().text(
                close_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                p.ui(15.0),
                if close_response.hovered() { p.error } else { p.muted },
            );
            if close_response.clicked() {
                close = Some(session.id.clone());
            }
            let edge = if active { p.accent.gamma_multiply(0.7) } else { p.border };
            ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Outside);

            if session.sandbox.is_none()
                && let Some(pending) = session.approvals.first()
            {
                let request = &pending.request;
                let deciding = self.client.approval_decisions.contains(&(session.id.clone(), pending.id.clone()));
                let height = (body.height() - 220.0).clamp(110.0, 210.0).min(body.height());
                let banner = body.with_max_y(body.top() + height);
                ui.painter().rect_filled(banner, 0.0, p.panel);
                let mut child = ui
                    .new_child(egui::UiBuilder::new().id_salt(("approval", &session.id)).max_rect(banner.shrink(6.0)));
                child.set_clip_rect(banner);
                child.label(RichText::new("Permission requested").color(p.warning).strong());
                child.horizontal_wrapped(|ui| {
                    for (label, decision) in
                        [("Allow", Decision::Allow), ("Allow always", Decision::AllowAlways), ("Deny", Decision::Deny)]
                    {
                        let response = ui.add_enabled(!deciding, egui::Button::new(label));
                        #[cfg(test)]
                        ui.ctx().data_mut(|data| {
                            data.insert_temp(egui::Id::unique(("approval-button", label)), response.enabled());
                        });
                        if response.clicked() {
                            approval = Some((index, pending.id.clone(), decision));
                        }
                    }
                });
                if deciding {
                    child.label("Decision pending…");
                }
                egui::ScrollArea::vertical().max_height(child.available_height()).show(&mut child, |ui| {
                    ui.label(RichText::new(request.command.as_deref().unwrap_or(&request.tool)).monospace());
                    ui.label(format!("cwd: {}", request.cwd.display()));
                    ui.label(request.reason.as_deref().unwrap_or("Permission requested"));
                });
                body = body.with_min_y(banner.bottom());
            }
            let term_id = ui.scope_id().with(("term", &session.id));
            if let Some(reported) = reported {
                let banner = body.with_max_y(body.top() + 24.0);
                ui.painter().rect_filled(banner, 0.0, p.panel);
                ui.painter().text(
                    banner.left_center() + vec2(8.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    reported,
                    p.ui(12.0),
                    p.muted,
                );
                body.min.y = banner.bottom();
            }
            match &mut session.terminal {
                Some(terminal) => {
                    let response = self
                        .client
                        .terminal_views
                        .entry(term_id)
                        .or_default()
                        .show(terminal, ui, body, term_id, &metrics, &p);
                    if focus_request.as_deref() == Some(session.id.as_str()) {
                        response.request_focus();
                    }
                    if (response.clicked() || response.gained_focus()) && !active {
                        activate = Some((session.id.clone(), false));
                    }
                    if terminal.exited() {
                        let overlay = Rect::from_center_size(body.center(), vec2(body.width().min(320.0), 80.0));
                        ui.painter().rect_filled(overlay, 8.0, p.panel);
                        let mut child = ui.new_child(
                            egui::UiBuilder::new().id_salt(("exited", &session.id)).max_rect(overlay.shrink(12.0)),
                        );
                        child.vertical_centered(|ui| {
                            ui.label(RichText::new("Process exited").color(p.foreground));
                            ui.horizontal(|ui| {
                                if session.kind == AgentKind::Claude
                                    && session.claude_id.is_some()
                                    && ui.button("Resume").clicked()
                                {
                                    resume = Some(index);
                                }
                                if ui.button("Close").clicked() {
                                    close = Some(session.id.clone());
                                }
                            });
                        });
                    }
                }
                None => {
                    ui.painter().rect_filled(body, 0.0, p.background);
                    let mut child = ui.new_child(
                        egui::UiBuilder::new().id_salt(("outside", &session.id)).max_rect(body.shrink(14.0)),
                    );
                    child.label(
                        RichText::new(format!("Running outside IMACS in {}", paths::display(&session.cwd)))
                            .color(p.muted)
                            .size(12.0),
                    );
                    child.label(
                        RichText::new("Messages reach it at its next tool call or turn end.").color(p.muted).size(12.0),
                    );
                    child.add_space(8.0);
                    if let Some(prompt) = &session.last_prompt {
                        child.label(RichText::new("You").color(p.accent).size(12.0));
                        child.label(RichText::new(truncate(prompt, 600)).color(p.foreground));
                        child.add_space(6.0);
                    }
                    if let Some(message) = &session.last_message {
                        child.label(RichText::new("Claude").color(p.success).size(12.0));
                        child.label(RichText::new(truncate(message, 1500)).color(p.foreground));
                    }
                    if ui.interact(body, term_id, Sense::click()).clicked() && !active {
                        activate = Some((session.id.clone(), false));
                    }
                }
            }
        }

        let mut task_action = None;
        for (offset, (plan, task)) in self.runtime.completed_tasks().into_iter().take(layout.completed).enumerate() {
            let slot = tiled.len() + offset;
            let min = rect.min + vec2((slot % cols) as f32 * size.x, (slot / cols) as f32 * size.y);
            let tile = Rect::from_min_size(min, size).shrink(3.0);
            ui.painter().rect_filled(tile, 2.0, p.raised);
            ui.painter().rect_stroke(tile, 2.0, Stroke::new(1.0, p.border), egui::StrokeKind::Inside);
            let mut child =
                ui.new_child(egui::UiBuilder::new().id_salt(("completed", plan, task)).max_rect(tile.shrink(14.0)));
            let task_data = &self.runtime.plans[plan].tasks[task];
            child.label(RichText::new(&task_data.title).color(p.foreground).strong().size(13.0));
            if let Some(action) = task_result(
                &mut child,
                task_data,
                None,
                false,
                !self.runtime.plans[plan].is_open(),
                &self.runtime.config,
                &p,
            ) {
                task_action = Some((plan, task, action));
            }
        }
        if let Some((plan, task, action)) = task_action {
            self.task_action(plan, task, action);
        }

        if let Some((index, approval, decision)) = approval {
            self.approve_request(index, approval, decision);
        }

        if let Some((id, open)) = activate {
            self.activate(&id, open);
        }
        if let Some(id) = close {
            let running = self
                .sessions
                .iter()
                .find(|s| s.id == id)
                .and_then(|s| s.terminal.as_ref())
                .is_some_and(|t| !t.exited());
            if running { self.client.confirm_close = Some(id) } else { self.close(&id) }
        }
        if let Some(index) = resume {
            let s = &self.runtime.sessions[index];
            let record = SessionRecord {
                chief: s.chief,
                pane: Some(s.id.clone()),
                claude_session_id: s.claude_id.clone().unwrap_or_default(),
                agent: s.agent.clone(),
                cwd: s.cwd.clone(),
                title: s.title.clone(),
                last_prompt: None,
                updated: 0,
                sandbox: s.sandbox.clone(),
            };
            let id = s.id.clone();
            self.close(&id);
            self.post(move |state| state.resume.push(record));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, session, with_app};
    use super::*;

    #[test]
    fn queue_notes_name_the_slot_or_the_place_in_line() {
        let ticket =
            |id, class, ancestors: &[u32]| Ticket { id, class, label: String::new(), ancestors: ancestors.to_vec() };
        let running = [ticket(1, SlotClass::Test, &[10, 1])];
        let waiting =
            [ticket(2, SlotClass::Qa, &[30]), ticket(3, SlotClass::Test, &[40]), ticket(4, SlotClass::Test, &[50])];
        assert_eq!(queue_note(10, &running, &waiting).as_deref(), Some("test slot"));
        assert_eq!(queue_note(50, &running, &waiting).as_deref(), Some("queued for a test slot (2 in line)"));
        assert_eq!(queue_note(30, &running, &waiting).as_deref(), Some("queued for a qa slot (1 in line)"));
        assert_eq!(queue_note(99, &running, &waiting), None);
    }

    #[test]
    fn all_view_only_shows_completed_tiles_from_open_plans() {
        with_app("app::fleet::tests::all_view_only_shows_completed_tiles_from_open_plans", |app, _| {
            let dir = tempfile::tempdir().unwrap();
            app.runtime.sessions.clear();
            app.runtime.plans = [PlanStatus::Open, PlanStatus::Completed, PlanStatus::Archived]
                .into_iter()
                .enumerate()
                .map(|(index, status)| Plan {
                    id: format!("plan-{index}"),
                    status,
                    project: Some(dir.path().into()),
                    tasks: vec![Task {
                        finished_at: Some(100),
                        worktree: Some(dir.path().join("removed")),
                        ..Task::default()
                    }],
                    ..Plan::default()
                })
                .collect();
            app.runtime.plan_view = false;
            app.runtime.view = None;
            assert_eq!(app.runtime.completed_tasks(), [(0, 0)]);
            app.runtime.view = Some(dir.path().into());
            assert_eq!(app.runtime.completed_tasks(), [(0, 0), (1, 0)]);
            app.runtime.plan_view = true;
            app.runtime.selected_plan = 1;
            assert_eq!(app.runtime.completed_tasks(), [(1, 0)], "completed plan history stays visible in its own view");
            app.runtime.selected_plan = 2;
            assert!(app.runtime.completed_tasks().is_empty());
        });
    }

    #[test]
    fn completed_task_resume_detects_codex_by_program_instead_of_agent_name() {
        with_app(
            "app::fleet::tests::completed_task_resume_detects_codex_by_program_instead_of_agent_name",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                for (name, program, expected) in
                    [("custom-codex", "/tools/codex", true), ("codex", "/tools/other", false)]
                {
                    app.runtime.config.agents = vec![imacs_core::config::AgentConfig {
                        name: name.into(),
                        command: vec![program.into()],
                        kind: AgentKind::Generic,
                        busy_text: None,
                        isolation: None,
                    }];
                    let task = Task {
                        agent: name.into(),
                        finished_at: Some(100),
                        worktree: Some(dir.path().into()),
                        ..Task::default()
                    };
                    let render = |events| {
                        let mut action = None;
                        let output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                action = task_result(
                                    ui,
                                    &task,
                                    None,
                                    false,
                                    false,
                                    &app.runtime.config,
                                    &app.runtime.palette,
                                );
                            });
                        });
                        let pos = output
                            .shapes
                            .iter()
                            .find_map(|shape| match &shape.shape {
                                egui::epaint::Shape::Text(t) if t.galley.text() == "resume" => {
                                    Some(t.pos + t.galley.rect.center().to_vec2())
                                }
                                _ => None,
                            })
                            .unwrap();
                        output.drop_without_applying_deltas();
                        (pos, action)
                    };
                    let (pos, _) = render(vec![]);
                    for pressed in [true, false] {
                        let (_, action) = render(vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ]);
                        if !pressed {
                            assert_eq!(action, expected.then_some("resume"), "{name}: {program}");
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn removed_worktrees_disable_completed_result_actions() {
        with_app("app::fleet::tests::removed_worktrees_disable_completed_result_actions", |app, ctx| {
            let (_dir, repo) = repository();
            let worktree = repo.with_file_name("task");
            git::add_worktree(&repo, &worktree, "task", "main").unwrap();
            app.runtime.plans = vec![Plan {
                id: "cleaned".into(),
                status: PlanStatus::Completed,
                project: Some(repo),
                tasks: vec![Task {
                    branch: Some("task".into()),
                    finished_at: Some(100),
                    worktree: Some(worktree.clone()),
                    resume_id: Some("resume-id".into()),
                    ..Task::default()
                }],
                ..Plan::default()
            }];
            app.runtime.selected_plan = 0;
            let task = app.runtime.plans[0].tasks[0].clone();
            let mut time = 0.0;
            let mut render = |events| {
                time += 0.05;
                let mut action = None;
                let output = ctx.run_ui(egui::RawInput { time: Some(time), events, ..Default::default() }, |ui| {
                    egui::CentralPanel::default().show(ui, |ui| {
                        action = task_result(ui, &task, None, false, false, &app.runtime.config, &app.runtime.palette);
                    });
                });
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|s| match &s.shape {
                        egui::epaint::Shape::Text(t) => {
                            Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                        }
                        _ => None,
                    })
                    .collect();
                output.drop_without_applying_deltas();
                (text, action)
            };
            for available in [true, false] {
                for label in ["diff", "resume"] {
                    let (text, _) = render(vec![]);
                    let pos = text.iter().find(|(t, _)| t == label).unwrap().1;
                    for pressed in [true, false] {
                        let (_, action) = render(vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ]);
                        if !pressed {
                            assert_eq!(action, available.then_some(label));
                        }
                    }
                }
                if available {
                    let rows = imacs_core::cleanup::preview(&app.runtime.plans[0], "main").unwrap();
                    imacs_core::cleanup::remove(&app.runtime.plans[0], "main", &rows[0]).unwrap();
                    assert!(!worktree.exists());
                }
            }
            app.runtime.active = None;
            app.runtime.diff_root = None;
            let sessions = app.runtime.sessions.len();
            for action in ["diff", "resume"] {
                app.runtime.task_action(0, 0, action);
                assert!(app.runtime.diff_target().is_none());
                assert_eq!(app.runtime.sessions.len(), sessions);
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
            }
        });
    }

    #[test]
    fn completed_tiles_and_board_show_results_actions_and_idle_report_age() {
        with_app(
            "app::fleet::tests::completed_tiles_and_board_show_results_actions_and_idle_report_age",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                app.runtime.sessions =
                    (0..3).map(|i| session(&format!("live-{i}"), AgentKind::Claude, AgentStatus::Idle, root)).collect();
                let index = app.runtime.selected_plan;
                let completed = Task {
                    id: "done".into(),
                    title: "Finished task".into(),
                    status: TaskStatus::Done,
                    dispatched_at: Some(100),
                    finished_at: Some(890),
                    worktree: Some(root.into()),
                    resume_id: Some("resume-id".into()),
                    report: Some(Report {
                        status: "done".into(),
                        summary: "Report first line\nReport second line\nReport third line\nHidden fourth line".into(),
                        at: 890,
                    }),
                    additions: 412,
                    deletions: 88,
                    ..Task::default()
                };
                app.runtime.plans[index] = Plan {
                    id: "results".into(),
                    goal: "Results".into(),
                    tasks: vec![
                        completed,
                        Task {
                            id: "waiting".into(),
                            status: TaskStatus::Review,
                            session: Some("live-0".into()),
                            report: Some(Report { at: imacs_core::store::now() - 1500, ..Report::default() }),
                            ..Task::default()
                        },
                    ],
                    ..Plan::default()
                };
                let mut time = 0.0;
                let mut render = |app: &mut Desktop<'_>, board: bool, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| {
                                if board {
                                    app.plan_panel(ui, ui.max_rect());
                                } else {
                                    let layout =
                                        layout::compute(1600.0, 1000.0, 3, 1, false, 0.0, &app.runtime.config.layout);
                                    assert_eq!(layout.completed, 1);
                                    app.tiles(ui, ui.max_rect(), &layout, &[0, 1, 2]);
                                }
                            });
                            app.dialogs(ctx);
                        },
                    );
                    app.runtime.drain_actions();
                    for shape in &output.shapes {
                        if let egui::epaint::Shape::Text(text) = &shape.shape
                            && text.galley.text().starts_with("Agent completed in")
                        {
                            assert!(!text.galley.job.justify, "completion text must keep normal word spacing");
                        }
                    }
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|s| match &s.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                for board in [false, true] {
                    let text = render(app, board, vec![]);
                    for expected in [
                        "Finished task",
                        "Agent completed in 13m 10s",
                        "+412 −88",
                        "diff",
                        "resume",
                        "reported 25 min ago",
                    ] {
                        assert!(text.iter().any(|(t, _)| t == expected), "missing {expected}: {text:?}");
                    }
                    assert!(
                        text.iter().any(|(t, _)| t.contains("Report first line") && t.contains("Report third line"))
                    );
                    assert!(!text.iter().any(|(t, _)| t.contains("Hidden fourth line")));
                }
                app.runtime.sessions[0].status = AgentStatus::Working;
                for board in [false, true] {
                    assert!(!render(app, board, vec![]).iter().any(|(t, _)| t == "reported 25 min ago"));
                }
                let text = render(app, false, vec![]);
                let pos = text.iter().find(|(t, _)| t == "diff").unwrap().1;
                for pressed in [true, false] {
                    render(
                        app,
                        false,
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
                assert_eq!(app.runtime.diff_target().as_deref(), Some(root));
                app.runtime.plans[index].status = plan::PlanStatus::Completed;
                let text = render(app, true, vec![]);
                let pos = text.iter().find(|(t, _)| t == "Archive").unwrap().1;
                for pressed in [true, false] {
                    render(
                        app,
                        true,
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
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Archived);
            },
        );
    }
}
