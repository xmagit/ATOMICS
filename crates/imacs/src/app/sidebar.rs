use super::*;

#[derive(Clone)]
enum ViewTab {
    All,
    Project(PathBuf),
    Plan(usize),
}

impl App {
    /// Tabs share a bounded navigation slot. First shorten labels, then move
    /// excess tabs into a menu, keeping the selected tab in the visible row.
    pub(super) fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let mut tabs = vec![ViewTab::All];
        tabs.extend(self.repos.iter().cloned().map(ViewTab::Project));
        tabs.extend(
            self.plans
                .iter()
                .enumerate()
                .filter(|(_, p)| !p.id.is_empty() && p.status != PlanStatus::Archived)
                .map(|(index, _)| ViewTab::Plan(index)),
        );
        let gap = ui.spacing().item_spacing.x;
        let widths: Vec<f32> = tabs
            .iter()
            .map(|tab| {
                super::topbar::measure(ui, |ui| {
                    self.view_tab(ui, tab, None);
                })
            })
            .collect();
        let closed = self.plans.iter().any(|p| !p.id.is_empty() && !p.is_open());
        let closed_width = if closed {
            super::topbar::measure(ui, |ui| {
                ui.menu_button("Closed plans", |_| {});
            }) + gap
        } else {
            0.0
        };
        let menu_width = super::topbar::measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let available = ui.available_width();
        let pin_closed = closed && available >= closed_width + menu_width + gap + 90.0;
        let budget = available - if pin_closed { closed_width } else { 0.0 };
        let minimum: Vec<f32> = widths.iter().map(|w| w.min(90.0)).collect();
        let total = |widths: &[f32]| widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
        let overflow = total(&minimum) > budget || (closed && !pin_closed);
        let mut visible: Vec<usize> = (0..tabs.len()).collect();
        if overflow {
            let mut used = menu_width;
            visible.clear();
            for (index, width) in minimum.iter().enumerate() {
                if used + gap + width > budget {
                    break;
                }
                used += gap + width;
                visible.push(index);
            }
            if let Some(selected) = tabs.iter().position(|tab| self.view_tab_selected(tab))
                && !visible.contains(&selected)
                && let Some(last) = visible.last_mut()
            {
                // Preserve the selected tab when it fits the last visible slot.
                if minimum[selected] <= minimum[*last] {
                    *last = selected;
                }
            }
        }
        let visible_budget = budget - if overflow { menu_width + gap } else { 0.0 };
        let natural: Vec<f32> = visible.iter().map(|&i| widths[i]).collect();
        let shrink = (total(&natural) - visible_budget).max(0.0);
        let capacity: f32 = visible.iter().map(|&i| widths[i] - minimum[i]).sum();
        for &index in &visible {
            let width = widths[index]
                - if capacity > 0.0 { shrink.min(capacity) * (widths[index] - minimum[index]) / capacity } else { 0.0 };
            self.view_tab(ui, &tabs[index], Some(width));
        }
        if overflow {
            let response = ui
                .menu_button("Tabs", |ui| {
                    ui.set_max_width(500.0_f32.min(ui.ctx().content_rect().width() - 32.0));
                    for (index, tab) in tabs.iter().enumerate() {
                        if !visible.contains(&index) {
                            self.view_tab(ui, tab, None);
                        }
                    }
                    if closed && !pin_closed {
                        self.closed_plans_menu(ui);
                    }
                })
                .response;
            super::topbar::record_item(ui, "Tabs", &response);
        }
        if pin_closed {
            self.closed_plans_menu(ui);
        }
    }

    fn view_tab_selected(&self, tab: &ViewTab) -> bool {
        match tab {
            ViewTab::All => !self.plan_view && self.view.is_none(),
            ViewTab::Project(root) => !self.plan_view && self.view.as_ref() == Some(root),
            ViewTab::Plan(index) => self.plan_view && self.selected_plan == *index,
        }
    }

    fn view_tab(&mut self, ui: &mut egui::Ui, tab: &ViewTab, width: Option<f32>) {
        let p = self.palette.clone();
        let selected = self.view_tab_selected(tab);
        let (label, hover, color) = match tab {
            ViewTab::All => {
                let agents = self.sessions.iter().filter(|s| !s.chief).count();
                (format!("All · {agents}"), "All projects".into(), p.foreground)
            }
            ViewTab::Project(root) => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let count = self
                    .sessions
                    .iter()
                    .filter(|s| !s.chief && self.project_of(&s.root()).as_ref() == Some(root))
                    .count();
                (format!("{name} · {count}"), paths::display(root), if selected { p.accent } else { p.foreground })
            }
            ViewTab::Plan(index) => {
                let plan = &self.plans[*index];
                let name: String = plan.goal.chars().take(28).collect();
                let marker = if self.changed_plans.contains(&plan.id) { " *" } else { "" };
                let glyph = if plan.status == PlanStatus::Completed { "✓" } else { "●" };
                let attention =
                    if self.sessions.iter().any(|s| plan.contains_session(&s.id) && s.status.needs_attention()) {
                        "needs you"
                    } else {
                        plan.attention()
                    };
                let color = match attention {
                    "needs you" => p.warning,
                    "all done" => p.success,
                    _ => p.accent,
                };
                (
                    format!("{glyph} {name} · {}/{}{marker}", plan.count(TaskStatus::Done), plan.tasks.len()),
                    format!("{}: {}", plan.goal, attention),
                    color,
                )
            }
        };
        let button = egui::Button::selectable(selected, RichText::new(&label).size(12.5).color(color)).truncate();
        let response = match width {
            Some(width) => ui.add_sized(vec2(width, ui.spacing().interact_size.y), button),
            None => ui.add(button),
        }
        .on_hover_text(hover);
        if width.is_some() {
            super::topbar::record_item(ui, &label, &response);
        }
        if response.clicked() {
            match tab {
                ViewTab::All => self.set_view(None),
                ViewTab::Project(root) => self.set_view(Some(root.clone())),
                ViewTab::Plan(index) => {
                    self.selected_plan = *index;
                    self.show_plan_fleet();
                }
            }
            if width.is_none() {
                ui.close();
            }
        }
        match tab {
            ViewTab::Project(root) => {
                response.context_menu(|ui| {
                    if ui
                        .button("Close idle sessions")
                        .on_hover_text("Idle or exited sessions no plan task uses")
                        .clicked()
                    {
                        let closed = self.close_idle(root);
                        self.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
                        ui.close();
                    }
                });
            }
            ViewTab::Plan(index) if self.plans[*index].status == PlanStatus::Completed => {
                response.context_menu(|ui| {
                    if ui.button("Archive plan").clicked() {
                        self.confirm_plan_close = Some((self.plans[*index].id.clone(), PlanStatus::Archived));
                        ui.close();
                    }
                });
            }
            _ => {}
        }
    }

    fn closed_plans_menu(&mut self, ui: &mut egui::Ui) {
        let mut reopen = None;
        let response = ui
            .menu_button("Closed plans", |ui| {
                for (index, plan) in self.plans.iter().enumerate().filter(|(_, p)| !p.id.is_empty() && !p.is_open()) {
                    let status = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
                    if ui.button(format!("Reopen {} ({status})", plan.goal)).clicked() {
                        reopen = Some(index);
                        ui.close();
                    }
                }
            })
            .response;
        super::topbar::record_item(ui, "Closed plans", &response);
        if let Some(index) = reopen
            && let Err(err) = self.reopen_plan(index)
        {
            self.toast(err);
        }
    }

    pub(super) fn set_view(&mut self, view: Option<PathBuf>) {
        self.plan_view = false;
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

    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette.clone();
        let files_root = self.diff_target();
        section(ui, &p, "USAGE");
        self.usage.show(ui, &p);
        ui.separator();
        let total = ui.available_height();
        let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(ui, |ui| {
            section(ui, &p, "AGENTS");
            let mut clicked: Option<String> = None;
            let mut close: Option<String> = None;
            let visible = self.visible_sessions();
            for (index, session) in self.sessions.iter_mut().enumerate() {
                if !visible.contains(&index) {
                    continue;
                }
                let active = self.active.as_deref() == Some(session.id.as_str());
                let stats = self.diffs.get(&session.root()).and_then(|d| d.prepared.as_ref()).map(|p| (p.additions, p.deletions));
                let row = session_row(ui, &p, session, active, stats);
                if row.clicked() {
                    clicked = Some(session.id.clone());
                }
                row.context_menu(|ui| {
                    ui.checkbox(&mut session.selected, "Include in “selected”");
                    if ui.button("Close").clicked() {
                        close = Some(session.id.clone());
                        ui.close();
                    }
                });
            }
            for branch in &self.creating {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("checking out {branch}…")).color(p.muted).size(12.0));
                });
            }
            if self.sessions.is_empty() && self.creating.is_empty() {
                ui.label(RichText::new("No agents yet. Start one with + New,\nor install hooks to see agents\nrunning in other terminals.").color(p.muted).size(12.0));
            }
            if let Some(id) = clicked {
                self.activate(&id, true);
            }
            if let Some(id) = close {
                self.confirm_close = Some(id);
            }

            let live_ids: HashSet<&str> = self.sessions.iter().filter_map(|s| s.claude_id.as_deref()).collect();
            let recent: Vec<SessionRecord> = self
                .store
                .sessions
                .iter()
                .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir() && r.agent != "chief")
                .filter(|r| self.project_of(&r.cwd).is_some())
                .filter({
                    let mut seen = HashSet::new();
                    move |r| seen.insert((r.title.clone(), r.cwd.clone()))
                })
                .take(8)
                .cloned()
                .collect();
            if !recent.is_empty() {
                ui.add_space(10.0);
                section(ui, &p, "RECENT · click to resume");
                for record in recent {
                    let hover = record.last_prompt.clone().unwrap_or_else(|| paths::display(&record.cwd));
                    let response = ui.selectable_label(false, RichText::new(&record.title).color(p.muted).size(12.5)).on_hover_text(hover);
                    if response.clicked() {
                        self.pending_open_resume(record);
                    }
                }
            }

        });

        ui.add_space(4.0);
        ui.separator();
        self.explorer(ui, &p);
    }

    /// The solution explorer: a switcher over the open projects, the focused
    /// project's worktrees, and its main checkout's files with the changes of the
    /// worktree under review laid over them.
    fn explorer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let target = self.diff_target();
        let focus = target.as_deref().and_then(|t| self.project_of(t)).or_else(|| self.repos.first().cloned());
        ui.horizontal(|ui| {
            section(ui, p, "EXPLORER");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("+").on_hover_text("Open project…").clicked() {
                    self.pick_project();
                }
            });
        });
        let mut switch_to: Option<PathBuf> = None;
        let mut close: Option<PathBuf> = None;
        ui.horizontal_wrapped(|ui| {
            for root in self.repos.clone() {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let agents =
                    self.sessions.iter().filter(|s| self.project_of(&s.root()).as_ref() == Some(&root)).count();
                let label = if agents > 0 { format!("{name} ·{agents}") } else { name };
                let selected = focus.as_ref() == Some(&root);
                let color = if selected { p.accent } else { p.foreground };
                let response = ui.selectable_label(selected, RichText::new(label).color(color).size(12.5));
                if response.clicked() && !selected {
                    switch_to = Some(root.clone());
                }
                response.on_hover_text(paths::display(&root)).context_menu(|ui| {
                    for agent in self.config.agents.clone() {
                        if ui.button(format!("Start {} in the main checkout", agent.name)).clicked() {
                            self.pending_open.push((agent.name.clone(), root.clone()));
                            ui.close();
                        }
                    }
                    if ui.button("New worktree…").clicked() {
                        self.open_new_dialog(Some(root.clone()));
                        if let Some(dialog) = &mut self.new_dialog {
                            dialog.new_worktree = true;
                            dialog.repo = paths::display(&root);
                        }
                        ui.close();
                    }
                    if ui.button("Close project").clicked() {
                        close = Some(root.clone());
                        ui.close();
                    }
                });
            }
        });
        if let Some(root) = switch_to {
            self.active = None;
            self.diff_root = Some(root);
            self.diff_open = true;
        }
        if let Some(root) = close {
            self.close_project(&root);
            return;
        }
        let Some(project) = focus else {
            ui.add_space(8.0);
            ui.label(RichText::new("Open a project with + or Project › Open project…").color(p.muted).size(12.0));
            return;
        };

        // Worktrees of the focused project, folded by default (big repos have dozens).
        let worktrees: Vec<WorktreeEntry> = self
            .worktrees
            .iter()
            .filter(|w| w.path != project && self.worktree_project.get(&w.path) == Some(&project))
            .cloned()
            .collect();
        let explorer = self.explorers.entry(project.clone()).or_default();
        let arrow = if explorer.show_worktrees { "▾" } else { "▸" };
        let mut open_cleanup = false;
        ui.horizontal(|ui| {
            let header = ui.add(
                egui::Label::new(
                    RichText::new(format!("{arrow} worktrees ({})", worktrees.len())).color(p.muted).size(12.0),
                )
                .sense(Sense::click()),
            );
            if header.clicked() {
                explorer.show_worktrees = !explorer.show_worktrees;
            }
            if ui
                .small_button("clean up…")
                .on_hover_text("Find merged or abandoned worktrees and remove them")
                .clicked()
            {
                open_cleanup = true;
            }
        });
        if open_cleanup {
            self.open_cleanup(project.clone(), None);
            return;
        }
        let explorer = self.explorers.entry(project.clone()).or_default();
        if explorer.show_worktrees {
            let mut pick: Option<PathBuf> = None;
            let mut remove_one: Option<PathBuf> = None;
            egui::ScrollArea::vertical().id_salt(("worktrees", &project)).max_height(190.0).show(ui, |ui| {
                let main_selected = target.as_ref() == Some(&project);
                if ui.selectable_label(main_selected, RichText::new("main checkout").size(12.0)).clicked() {
                    pick = Some(project.clone());
                }
                for wt in &worktrees {
                    let name = wt.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let agent = self.sessions.iter().find(|s| s.root() == wt.path);
                    let selected = target.as_ref() == Some(&wt.path);
                    let response = ui
                        .horizontal(|ui| {
                            if let Some(agent) = agent {
                                ui.label(RichText::new("●").color(status_color(agent.status, p)).size(10.0));
                            }
                            let response = ui.selectable_label(selected, RichText::new(&name).size(12.0));
                            if let Some(branch) = &wt.branch {
                                ui.label(RichText::new(branch).color(p.muted).size(10.5));
                            }
                            response
                        })
                        .inner;
                    if response.clicked() {
                        pick = Some(wt.path.clone());
                    }
                    response.context_menu(|ui| {
                        for agent in self.config.agents.clone() {
                            if ui.button(format!("Start {} here", agent.name)).clicked() {
                                self.pending_open.push((agent.name.clone(), wt.path.clone()));
                                ui.close();
                            }
                        }
                        if ui.button("Remove worktree…").clicked() {
                            remove_one = Some(wt.path.clone());
                            ui.close();
                        }
                    });
                }
            });
            if let Some(path) = remove_one {
                self.open_cleanup(project.clone(), Some(path));
                return;
            }
            if let Some(path) = pick {
                match self.sessions.iter().find(|s| s.root() == path).map(|s| s.id.clone()) {
                    Some(id) => self.activate(&id, true),
                    None => {
                        self.active = None;
                        self.diff_root = Some(path);
                        self.diff_open = true;
                    }
                }
            }
        }

        let review =
            target.filter(|t| self.project_of(t).as_ref() == Some(&project)).unwrap_or_else(|| project.clone());
        if review != project {
            let name = review.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ui.label(RichText::new(format!("changes from {name}")).color(p.accent).size(11.0));
        }
        let main = self.diffs.get(&project).and_then(|d| d.prepared.clone());
        let changes = self.diffs.get(&review).and_then(|d| d.prepared.clone());
        let viewing = self.diffs.get(&review).and_then(|d| d.viewing.clone());
        let (main, changes) = match (main, changes) {
            (Some(main), Some(changes)) => (main, changes),
            (Some(main), None) => (main.clone(), main),
            (None, Some(changes)) => (changes.clone(), changes),
            (None, None) => {
                ui.label(RichText::new("Reading files…").color(p.muted).size(12.0));
                return;
            }
        };
        let explorer = self.explorers.entry(project).or_default();
        if let Some(path) = diff_view::files_panel(ui, explorer, &main, &changes, viewing.as_deref(), p) {
            self.open_file(&review, &path);
        }
    }

    pub(super) fn project_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open project…").clicked() {
            ui.close();
            self.pick_project();
        }
        let recent: Vec<PathBuf> = self
            .store
            .recent_projects
            .iter()
            .filter(|p| !self.open_projects.contains(p) && p.is_dir())
            .take(8)
            .cloned()
            .collect();
        if !recent.is_empty() {
            ui.separator();
            ui.label(RichText::new("Recent projects").size(11.0).color(self.palette.muted));
            for root in recent {
                if ui.button(paths::display(&root)).clicked() {
                    self.open_project(&root);
                    ui.close();
                }
            }
        }
        ui.separator();
        ui.label(RichText::new("Workspaces").size(11.0).color(self.palette.muted));
        for name in Workspace::list() {
            let current = self.workspace.as_deref() == Some(name.as_str());
            if ui.selectable_label(current, &name).clicked() {
                self.open_workspace(&name);
                ui.close();
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.workspace_input).hint_text("name").desired_width(120.0));
            let name = self.workspace_input.trim().to_string();
            if ui
                .add_enabled(!name.is_empty() && !self.open_projects.is_empty(), egui::Button::new("Save workspace"))
                .clicked()
            {
                self.save_workspace(&name);
                self.workspace_input.clear();
                ui.close();
            }
        });
    }

    pub(super) fn settings_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Edit config").clicked() {
            let _ = open_path(&self.config_path);
            ui.close();
        }
        if global_hooks_installed() {
            ui.label(RichText::new("Hooks for all Claude sessions are installed").size(12.0));
        } else if ui
            .button("Install hooks for all Claude sessions")
            .on_hover_text(
                "Adds `imacs hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
            )
            .clicked()
        {
            match install_global_hooks() {
                Ok(path) => self.toast(format!("Hooks added to {}", paths::display(&path))),
                Err(err) => self.toast(format!("Installing hooks failed: {err:#}")),
            }
            ui.close();
        }
        ui.separator();
        let keys = &self.config.keys;
        for (key, what) in [
            (&keys.quick_open, "open file"),
            (&keys.toggle_diff, "show/hide diff"),
            (&keys.next_attention, "next agent that needs you"),
            (&keys.focus_steer, "steer bar"),
            (&keys.new_session, "new session"),
            (&keys.voice, "voice: record, send (Esc cancels)"),
        ] {
            ui.label(RichText::new(format!("{key}  {what}")).size(12.0).monospace());
        }
    }

    fn pending_open_resume(&mut self, record: SessionRecord) {
        self.resume.push(record);
    }
}

fn section(ui: &mut egui::Ui, p: &Palette, title: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("▍").color(p.accent).size(11.0));
        ui.label(RichText::new(title).size(10.5).color(p.muted).strong());
    });
    ui.add_space(1.0);
}

fn session_row(
    ui: &mut egui::Ui,
    p: &Palette,
    s: &Session,
    active: bool,
    stats: Option<(usize, usize)>,
) -> egui::Response {
    let height = 38.0;
    let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 2.0, lerp(p.panel, p.accent, 0.14));
        painter.rect_filled(rect.with_max_x(rect.left() + 2.0), 0.0, p.accent);
    } else if response.hovered() {
        painter.rect_filled(rect, 2.0, p.raised);
    }
    let now = ui.input(|i| i.time);
    painter.circle_filled(pos2(rect.left() + 10.0, rect.top() + 13.0), 3.5, status_dot(s.status, p, now));
    let clip = painter.with_clip_rect(rect.shrink2(vec2(2.0, 0.0)));
    clip.text(
        pos2(rect.left() + 20.0, rect.top() + 5.0),
        egui::Align2::LEFT_TOP,
        &s.title,
        p.ui(13.0),
        if active { p.accent } else { p.foreground },
    );
    let mut detail = format!("{} · {}", s.agent, s.status.label());
    if s.terminal.is_none() {
        detail.push_str(" · outside");
    }
    if s.selected {
        detail.push_str(" · ✔");
    }
    clip.text(pos2(rect.left() + 20.0, rect.top() + 21.0), egui::Align2::LEFT_TOP, detail, p.ui(11.0), p.muted);
    if let Some((add, del)) = stats {
        let text = format!("+{add} −{del}");
        clip.text(
            pos2(rect.right() - 6.0, rect.top() + 5.0),
            egui::Align2::RIGHT_TOP,
            text,
            FontId::monospace(10.5),
            p.muted,
        );
    }
    response
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
                let one = app.plans.iter().position(|p| p.goal == "Atlas").unwrap();
                let two = app.selected_plan;
                app.plans.push(Plan {
                    id: "archived".into(),
                    goal: "Archived".into(),
                    status: plan::PlanStatus::Archived,
                    ..Plan::default()
                });
                app.plans.push(Plan {
                    id: "completed".into(),
                    goal: "Completed".into(),
                    status: plan::PlanStatus::Completed,
                    ..Plan::default()
                });
                for (index, id) in [(one, "atlas-agent"), (two, "beacon-agent")] {
                    app.plans[index].tasks[0].session = Some(id.into());
                    app.sessions.push(session(id, AgentKind::Generic, AgentStatus::Working, Path::new("/project")));
                }
                let mut time = 0.0;
                let mut frame = |app: &mut App, events| {
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
                                if app.panel == PanelMode::Plan {
                                    app.plan_panel(ui, ui.max_rect());
                                }
                            });
                            app.dialogs(ctx);
                        },
                    );
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
                    assert!(!text.iter().any(|(t, _)| t.contains("Archived")));
                    assert!(text.iter().any(|(t, _)| t.starts_with("✓ Completed ·")));
                    let pos = text.iter().find(|(t, _)| t.starts_with(&format!("● {goal} ·"))).unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                    frame(app, vec![button(pos, false)]);
                    assert_eq!(app.selected_plan, index);
                    assert!(app.plan_view);
                    assert_eq!(app.visible_sessions(), [visible]);
                    app.toggle_plan();
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
                    assert!(app.plans[index].approved);
                    if index == one {
                        assert!(!app.plans[two].approved);
                    }
                }
                let completed = app.plans.iter().position(|p| p.goal == "Completed").unwrap();
                let text = frame(app, vec![]);
                let pos = text.iter().find(|(t, _)| t.starts_with("✓ Completed ·")).unwrap().1;
                for pressed in [true, false] {
                    frame(
                        app,
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Secondary,
                                pressed,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                    );
                }
                let text = frame(app, vec![]);
                let pos = text.iter().rev().find(|(t, _)| t == "Archive plan").unwrap().1;
                for pressed in [true, false] {
                    frame(app, vec![egui::Event::PointerMoved(pos), button(pos, pressed)]);
                }
                assert_eq!(app.plans[completed].status, PlanStatus::Completed);
                assert_eq!(app.confirm_plan_close, Some((app.plans[completed].id.clone(), PlanStatus::Archived)));
                let mut click = |app: &mut App, label: &str| {
                    frame(app, vec![]);
                    let text = frame(app, vec![]);
                    let pos =
                        text.iter().find(|(t, _)| t == label).unwrap_or_else(|| panic!("missing {label}: {text:?}")).1;
                    frame(app, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                    frame(app, vec![button(pos, false)]);
                };
                click(app, "Cancel");
                assert!(app.confirm_plan_close.is_none());
                click(app, "Closed plans");
                click(app, "Reopen Completed (completed)");
                app.sync_plan();
                assert!(app.plans[completed].is_open());
                assert_eq!(app.selected_plan, completed);
                for (index, action, confirm, status, reopen) in [
                    (two, "Archive plan", "Confirm archive", PlanStatus::Archived, "Reopen Beacon (archived)"),
                    (one, "Complete plan", "Confirm complete", PlanStatus::Completed, "Reopen Atlas (completed)"),
                ] {
                    app.selected_plan = index;
                    app.show_plan_fleet();
                    app.toggle_plan();
                    click(app, action);
                    assert_eq!(app.plans[index].status, PlanStatus::Open, "closing requires confirmation");
                    click(app, "Cancel");
                    assert!(app.confirm_plan_close.is_none());
                    assert_eq!(app.plans[index].status, PlanStatus::Open);
                    click(app, action);
                    click(app, confirm);
                    assert_eq!(app.plans[index].status, status);
                    click(app, "Closed plans");
                    click(app, reopen);
                    assert!(app.plans[index].is_open());
                    assert_eq!(app.selected_plan, index);
                    assert!(app.plan_view && app.panel == PanelMode::Plan);
                    let saved =
                        Plan::load(&paths::imacs_home().join("plans").join(format!("{}.json", app.plans[index].id)))
                            .unwrap();
                    assert_eq!(saved.status, PlanStatus::Open);
                    assert_eq!(saved.tasks, app.plans[index].tasks);
                    assert!(saved.approved);
                }
                assert_eq!(app.sessions.len(), 2, "closing plans does not terminate their sessions");
            },
        );
    }
}
