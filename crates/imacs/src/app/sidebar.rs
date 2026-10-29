use super::*;

#[derive(Clone)]
enum ViewTab {
    All,
    Project(PathBuf),
    Plan(usize),
}

impl State {
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
}

fn measure_divider(ui: &mut egui::Ui) -> f32 {
    super::topbar::measure(ui, |ui| {
        ui.separator();
    })
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
    selected: bool,
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
    if selected {
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

impl Desktop<'_> {
    /// Tabs share a bounded navigation slot. First shorten labels, then move
    /// excess tabs into a menu, keeping the selected tab in the visible row.
    pub(super) fn urgent_tabs_label(&self) -> (String, Color32) {
        self.tabs_summary(self.runtime.plans.iter().enumerate().map(|(index, _)| index))
    }

    fn tabs_summary(&self, indices: impl Iterator<Item = usize>) -> (String, Color32) {
        let mut needs = 0;
        let mut running = 0;
        for index in indices {
            match self.runtime.plan_state(index) {
                PlanState::NeedsYou => needs += 1,
                PlanState::Running(count) => running += count,
                _ => {}
            }
        }
        if needs > 0 {
            (format!("Tabs · {needs} needs you"), self.runtime.palette.warning)
        } else if running > 0 {
            (format!("Tabs · {running} running"), self.runtime.palette.accent)
        } else {
            ("Tabs".into(), self.runtime.palette.foreground)
        }
    }

    fn plan_label_width(&self, ui: &mut egui::Ui, label: &str) -> f32 {
        super::topbar::measure(ui, |ui| {
            ui.add(egui::Button::new(RichText::new(label).font(self.runtime.palette.ui(12.5))).right_text("   "));
        })
    }

    pub(super) fn urgent_navigation_width(&self, ui: &mut egui::Ui) -> f32 {
        let gap = ui.spacing().item_spacing.x;
        let menu = super::topbar::measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let protected = self
            .runtime
            .plans
            .iter()
            .enumerate()
            .map(|(index, _)| (index, self.runtime.plan_state(index)))
            .filter(|(_, state)| state.urgency() > 0)
            .map(|(index, state)| {
                let marker = if self.runtime.changed_plans.contains(&self.runtime.plans[index].id) { " *" } else { "" };
                self.plan_label_width(ui, &format!("{}{marker}", state.label("…"))) + gap
            })
            .sum::<f32>();
        let history = if protected > 0.0
            && self.runtime.plans.iter().any(|plan| !plan.id.is_empty() && plan.status != PlanStatus::Open)
        {
            super::topbar::measure(ui, |ui| {
                ui.menu_button("History", |_| {});
            }) + gap
        } else {
            0.0
        };
        protected + menu + history
    }

    pub(super) fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let mut tabs = vec![ViewTab::All];
        tabs.extend(self.runtime.repos.iter().cloned().map(ViewTab::Project));
        tabs.extend(
            self.runtime
                .plans
                .iter()
                .enumerate()
                .filter(|(index, plan)| {
                    !plan.id.is_empty()
                        && (plan.status != PlanStatus::Archived
                            || self.runtime.plan_view && self.runtime.selected_plan == *index)
                })
                .map(|(index, _)| ViewTab::Plan(index)),
        );
        let gap = ui.spacing().item_spacing.x;
        let widths: Vec<f32> =
            tabs.iter().map(|tab| super::topbar::measure(ui, |ui| self.view_tab(ui, tab, None, None))).collect();
        let minimum: Vec<f32> = tabs
            .iter()
            .zip(&widths)
            .map(|(tab, width)| {
                if let ViewTab::Plan(index) = tab {
                    let state = self.runtime.plan_state(*index);
                    if state.urgency() > 0 {
                        let marker =
                            if self.runtime.changed_plans.contains(&self.runtime.plans[*index].id) { " *" } else { "" };
                        return self.plan_label_width(ui, &format!("{}{marker}", state.label("…")));
                    }
                }
                width.min(90.0)
            })
            .collect();
        let mut priority: Vec<usize> = (0..tabs.len()).collect();
        priority.sort_by_key(|&index| match &tabs[index] {
            ViewTab::Plan(plan) if self.runtime.plan_state(*plan) == PlanState::NeedsYou => 0,
            ViewTab::Plan(plan) if matches!(self.runtime.plan_state(*plan), PlanState::Running(_)) => 1,
            ViewTab::All | ViewTab::Project(_) if self.view_tab_selected(&tabs[index]) => 2,
            ViewTab::All | ViewTab::Project(_) => 3,
            ViewTab::Plan(_) if self.view_tab_selected(&tabs[index]) => 4,
            ViewTab::Plan(plan) if self.runtime.plans[*plan].status == PlanStatus::Open => 5,
            _ => 6,
        });
        let has_plans = tabs.iter().any(|tab| matches!(tab, ViewTab::Plan(_)));
        let divider = if has_plans { measure_divider(ui) + gap } else { 0.0 };
        let closed = self.runtime.plans.iter().any(|plan| !plan.id.is_empty() && plan.status != PlanStatus::Open);
        let history_width = if closed {
            super::topbar::measure(ui, |ui| {
                ui.menu_button("History", |_| {});
            }) + gap
        } else {
            0.0
        };
        let available = ui.available_width();
        let total = |widths: &[f32]| widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
        let all_fit = total(&widths) + divider + history_width <= available;
        let protected: f32 = tabs
            .iter()
            .zip(&minimum)
            .filter_map(|(tab, width)| {
                if let ViewTab::Plan(index) = tab
                    && self.runtime.plan_state(*index).urgency() > 0
                {
                    Some(width + gap)
                } else {
                    None
                }
            })
            .sum();
        let mut menu_label = "Tabs".to_string();
        let mut menu_color = self.runtime.palette.foreground;
        let mut menu_width = super::topbar::measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let mut allocated = minimum.clone();
        let mut visible = Vec::new();
        let mut pin_history = closed;
        let mut content_budget = available - divider - history_width;
        if all_fit {
            visible.extend(0..tabs.len());
        } else {
            for _ in 0..=tabs.len() {
                pin_history = closed && available - divider - history_width >= protected.max(90.0) + menu_width + gap;
                content_budget =
                    (available - divider - if pin_history { history_width } else { 0.0 } - menu_width - gap).max(0.0);
                visible.clear();
                allocated.clone_from(&minimum);
                let urgent: Vec<usize> = priority
                    .iter()
                    .copied()
                    .filter(
                        |&index| matches!(tabs[index],ViewTab::Plan(plan) if self.runtime.plan_state(plan).urgency()>0),
                    )
                    .collect();
                let full_urgent_width = urgent.iter().map(|&index| widths[index]).sum::<f32>()
                    + gap * urgent.len().saturating_sub(1) as f32;
                let full_urgent = full_urgent_width <= content_budget;
                let mut used = 0.0;
                for &index in &priority {
                    let is_urgent = urgent.contains(&index);
                    // Idle/completed tabs only use space left after the important
                    // names and their state suffixes have been protected.
                    if !is_urgent && !full_urgent && !urgent.is_empty() {
                        continue;
                    }
                    let width = if is_urgent && full_urgent { widths[index] } else { minimum[index] };
                    let needed = width + if visible.is_empty() { 0.0 } else { gap };
                    if used + needed <= content_budget {
                        used += needed;
                        visible.push(index);
                        allocated[index] = width;
                    } else if !is_urgent {
                        break;
                    }
                }
                (menu_label, menu_color) = self.tabs_summary(tabs.iter().enumerate().filter_map(|(index, tab)| {
                    if !visible.contains(&index)
                        && let ViewTab::Plan(plan) = tab
                    {
                        Some(*plan)
                    } else {
                        None
                    }
                }));
                let measured = super::topbar::measure(ui, |ui| {
                    ui.menu_button(RichText::new(&menu_label).color(menu_color), |_| {});
                });
                if measured <= menu_width + 0.5 {
                    break;
                }
                menu_width = measured;
            }
        }
        let overflow = visible.len() < tabs.len() || closed && !pin_history;
        if all_fit {
            allocated.clone_from(&widths);
        } else {
            let used = visible.iter().map(|&index| allocated[index]).sum::<f32>()
                + gap * visible.len().saturating_sub(1) as f32;
            let mut extra = (content_budget - used).max(0.0);
            for &index in &priority {
                if visible.contains(&index) {
                    let grow = extra.min((widths[index] - allocated[index]).max(0.0));
                    allocated[index] += grow;
                    extra -= grow;
                }
            }
        }
        visible.sort_unstable();
        let mut project_drawn = false;
        let mut divided = false;
        for &index in &visible {
            if matches!(tabs[index], ViewTab::Plan(_)) && project_drawn && !divided {
                ui.separator();
                divided = true;
            }
            project_drawn |= !matches!(tabs[index], ViewTab::Plan(_));
            self.view_tab(ui, &tabs[index], Some(allocated[index]), Some(widths[index]));
        }
        if overflow {
            if has_plans && project_drawn && !divided {
                ui.separator();
            }
            let response = ui
                .menu_button(RichText::new(&menu_label).color(menu_color), |ui| {
                    ui.set_max_width(500.0_f32.min(ui.ctx().content_rect().width() - 32.0));
                    for (index, tab) in tabs.iter().enumerate().filter(|(_, tab)| !matches!(tab, ViewTab::Plan(_))) {
                        if !visible.contains(&index) {
                            self.view_tab(ui, tab, None, None);
                        }
                    }
                    if tabs
                        .iter()
                        .enumerate()
                        .any(|(index, tab)| !visible.contains(&index) && !matches!(tab, ViewTab::Plan(_)))
                    {
                        ui.separator();
                    }
                    for &index in &priority {
                        if !visible.contains(&index) && matches!(tabs[index], ViewTab::Plan(_)) {
                            self.view_tab(ui, &tabs[index], None, None);
                        }
                    }
                    if closed && !pin_history {
                        ui.separator();
                        self.history_menu(ui);
                    }
                })
                .response;
            super::topbar::record_item(ui, "Tabs", &response);
        }
        if pin_history {
            self.history_menu(ui);
        }
    }

    fn view_tab_selected(&self, tab: &ViewTab) -> bool {
        match tab {
            ViewTab::All => !self.runtime.plan_view && self.runtime.view.is_none(),
            ViewTab::Project(root) => !self.runtime.plan_view && self.runtime.view.as_ref() == Some(root),
            ViewTab::Plan(index) => self.runtime.plan_view && self.runtime.selected_plan == *index,
        }
    }

    fn view_tab(&mut self, ui: &mut egui::Ui, tab: &ViewTab, width: Option<f32>, natural_width: Option<f32>) {
        let p = self.runtime.palette.clone();
        let selected = self.view_tab_selected(tab);
        let (label, hover, color) = match tab {
            ViewTab::All => {
                let agents = self.runtime.sessions.iter().filter(|s| !s.chief).count();
                (format!("All · {agents}"), "All projects".into(), p.foreground)
            }
            ViewTab::Project(root) => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let count = self
                    .sessions
                    .iter()
                    .filter(|s| !s.chief && self.runtime.project_of(&s.root()).as_ref() == Some(root))
                    .count();
                (format!("{name} · {count}"), paths::display(root), if selected { p.accent } else { p.foreground })
            }
            ViewTab::Plan(index) => {
                let plan = &self.runtime.plans[*index];
                let state = self.runtime.plan_state(*index);
                let marker = if self.runtime.changed_plans.contains(&plan.id) { " *" } else { "" };
                let mut name: String = plan.goal.chars().take(28).collect();
                if state.urgency() > 0
                    && let Some(width) = width
                {
                    let natural = natural_width.unwrap_or_else(|| self.plan_label_width(ui, &state.label(&name)));
                    let font = p.ui(12.5);
                    let budget = ui
                        .fonts_mut(|fonts| fonts.layout_no_wrap(name.clone(), font.clone(), state.color(&p)).size().x)
                        - (natural - width).max(0.0);
                    if natural > width {
                        let mut chars: Vec<char> = name.chars().collect();
                        loop {
                            name = format!("{}…", chars.iter().collect::<String>());
                            if chars.is_empty()
                                || ui.fonts_mut(|fonts| {
                                    fonts.layout_no_wrap(name.clone(), font.clone(), state.color(&p)).size().x
                                }) <= budget
                            {
                                break;
                            }
                            chars.pop();
                        }
                    }
                }
                (format!("{}{marker}", state.label(&name)), plan.goal.clone(), state.color(&p))
            }
        };
        let state = if let ViewTab::Plan(index) = tab { Some(self.runtime.plan_state(*index)) } else { None };
        let mut text = egui::text::LayoutJob::default();
        let mut rest = label.as_str();
        if matches!(state, Some(PlanState::Running(_))) {
            let pulse = 0.55 + 0.45 * (ui.input(|input| input.time) * 3.2).sin() as f32;
            text.append(
                "● ",
                0.0,
                egui::TextFormat { font_id: p.ui(12.5), color: color.gamma_multiply(pulse), ..Default::default() },
            );
            rest = rest.strip_prefix("● ").unwrap_or(rest);
            ui.ctx().request_repaint_after(PULSE_STEP);
        }
        text.append(rest, 0.0, egui::TextFormat { font_id: p.ui(12.5), color, ..Default::default() });
        let fill = match state {
            Some(PlanState::Running(_)) => p.accent.gamma_multiply(0.35),
            Some(PlanState::NeedsYou) => p.warning.gamma_multiply(0.12),
            _ => Color32::TRANSPARENT,
        };
        let mut button = egui::Button::new(text)
            .fill(fill)
            .stroke(if selected { Stroke::new(1.0, p.accent) } else { Stroke::NONE })
            .truncate();
        if state.is_some() {
            button = button.right_text("   ");
        }
        let response = match width {
            Some(width) => ui.add_sized(vec2(width, ui.spacing().interact_size.y), button),
            None => ui.add(button),
        }
        .on_hover_text(hover);
        if width.is_some() {
            super::topbar::record_item(ui, &label, &response);
        }
        let mut close_clicked = false;
        let mut over_close = false;
        if let ViewTab::Plan(index) = tab {
            let close =
                Rect::from_center_size(pos2(response.rect.right() - 12.0, response.rect.center().y), vec2(18.0, 18.0));
            let _id = egui::Id::NULL.with(("plan-close", self.runtime.plans[*index].id.as_str()));
            let hit = ui.interact(close, response.id.with("close"), Sense::click());
            over_close = ui.input(|input| input.pointer.hover_pos().is_some_and(|pos| close.contains(pos)));
            if selected || response.hovered() || hit.hovered() {
                if hit.hovered() {
                    ui.painter().rect_filled(close, 2.0, p.raised);
                }
                let center = close.center();
                for sign in [-1.0, 1.0] {
                    ui.painter().line_segment(
                        [center + vec2(-3.0, sign * -3.0), center + vec2(3.0, sign * 3.0)],
                        Stroke::new(1.0, p.muted),
                    );
                }
                #[cfg(test)]
                if !ui.is_sizing_pass() {
                    ui.ctx().data_mut(|data| data.insert_temp(_id, close));
                }
            }
            close_clicked = hit.clicked() || response.clicked() && over_close;
            if close_clicked {
                self.close_plan_tab(*index);
                if width.is_none() {
                    ui.close();
                }
            }
        }
        if response.clicked() && !close_clicked && !over_close {
            match tab {
                ViewTab::All => self.runtime.set_view(None),
                ViewTab::Project(root) => self.runtime.set_view(Some(root.clone())),
                ViewTab::Plan(index) => {
                    if self.runtime.plans[*index].status == PlanStatus::Open {
                        self.runtime.selected_plan = *index;
                        self.runtime.show_plan_fleet();
                    } else {
                        self.view_plan_history(*index);
                    }
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
                        let root = root.clone();
                        self.post(move |state| {
                            let closed = state.close_idle(&root);
                            state.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
                        });
                        ui.close();
                    }
                });
            }
            ViewTab::Plan(index) if self.runtime.plans[*index].status == PlanStatus::Completed => {
                response.context_menu(|ui| {
                    if ui.button("Archive plan").clicked() {
                        self.close_plan_tab(*index);
                        ui.close();
                    }
                });
            }
            _ => {}
        }
    }

    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = self.runtime.palette.clone();
        let files_root = self.runtime.diff_target();
        section(ui, &p, "USAGE");
        self.runtime.usage.show(ui, &p);
        ui.separator();
        let total = ui.available_height();
        let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(ui, |ui| {
            section(ui, &p, "AGENTS");
            let mut clicked: Option<String> = None;
            let mut close: Option<String> = None;
            let visible = self.runtime.visible_sessions();
            for (index, session) in self.runtime.sessions.iter_mut().enumerate() {
                if !visible.contains(&index) {
                    continue;
                }
                let active = self.runtime.active.as_deref() == Some(session.id.as_str());
                let stats = self.runtime.diffs.get(&session.root()).and_then(|d| d.prepared.as_ref()).map(|p| (p.additions, p.deletions));
                let row = session_row(ui, &p, session, active, self.client.selected.contains(&session.id), stats);
                if row.clicked() {
                    clicked = Some(session.id.clone());
                }
                row.context_menu(|ui| {
                    let mut selected = self.client.selected.contains(&session.id);
                    if ui.checkbox(&mut selected, "Include in “selected”").changed() {
                        if selected { self.client.selected.insert(session.id.clone()); }
                        else { self.client.selected.remove(&session.id); }
                    }
                    if ui.button("Close").clicked() {
                        close = Some(session.id.clone());
                        ui.close();
                    }
                });
            }
            for branch in &self.runtime.creating {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(format!("checking out {branch}…")).color(p.muted).size(12.0));
                });
            }
            if self.runtime.sessions.is_empty() && self.runtime.creating.is_empty() {
                ui.label(RichText::new("No agents yet. Start one with + New,\nor install hooks to see agents\nrunning in other terminals.").color(p.muted).size(12.0));
            }
            if let Some(id) = clicked {
                self.activate(&id, true);
            }
            if let Some(id) = close {
                self.client.confirm_close = Some(id);
            }

            let live_ids: HashSet<&str> = self.runtime.sessions.iter().filter_map(|s| s.claude_id.as_deref()).collect();
            let recent: Vec<SessionRecord> = self
                .store
                .sessions
                .iter()
                .filter(|r| !live_ids.contains(r.claude_session_id.as_str()) && r.cwd.is_dir() && r.agent != "chief")
                .filter(|r| self.runtime.project_of(&r.cwd).is_some())
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

    fn explorer(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let target = self.runtime.diff_target();
        let focus =
            target.as_deref().and_then(|t| self.runtime.project_of(t)).or_else(|| self.runtime.repos.first().cloned());
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
            for root in self.runtime.repos.clone() {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let agents = self
                    .runtime
                    .sessions
                    .iter()
                    .filter(|s| self.runtime.project_of(&s.root()).as_ref() == Some(&root))
                    .count();
                let label = if agents > 0 { format!("{name} ·{agents}") } else { name };
                let selected = focus.as_ref() == Some(&root);
                let color = if selected { p.accent } else { p.foreground };
                let response = ui.selectable_label(selected, RichText::new(label).color(color).size(12.5));
                if response.clicked() && !selected {
                    switch_to = Some(root.clone());
                }
                response.on_hover_text(paths::display(&root)).context_menu(|ui| {
                    for agent in self.runtime.config.agents.clone() {
                        if ui.button(format!("Start {} in the main checkout", agent.name)).clicked() {
                            self.runtime.pending_open.push((agent.name.clone(), root.clone()));
                            ui.close();
                        }
                    }
                    if ui.button("New worktree…").clicked() {
                        self.open_new_dialog(Some(root.clone()));
                        if let Some(dialog) = &mut self.client.new_dialog {
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
            self.runtime.active = None;
            self.runtime.diff_root = Some(root);
            self.runtime.diff_open = true;
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
            .filter(|w| w.path != project && self.runtime.worktree_project.get(&w.path) == Some(&project))
            .cloned()
            .collect();
        let explorer = self.client.explorers.entry(project.clone()).or_default();
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
        let explorer = self.client.explorers.entry(project.clone()).or_default();
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
                    let agent = self.runtime.sessions.iter().find(|s| s.root() == wt.path);
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
                        for agent in self.runtime.config.agents.clone() {
                            if ui.button(format!("Start {} here", agent.name)).clicked() {
                                self.runtime.pending_open.push((agent.name.clone(), wt.path.clone()));
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
                match self.runtime.sessions.iter().find(|s| s.root() == path).map(|s| s.id.clone()) {
                    Some(id) => self.activate(&id, true),
                    None => {
                        self.runtime.active = None;
                        self.runtime.diff_root = Some(path);
                        self.runtime.diff_open = true;
                    }
                }
            }
        }

        let review =
            target.filter(|t| self.runtime.project_of(t).as_ref() == Some(&project)).unwrap_or_else(|| project.clone());
        if review != project {
            let name = review.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ui.label(RichText::new(format!("changes from {name}")).color(p.accent).size(11.0));
        }
        let main = self.runtime.diffs.get(&project).and_then(|d| d.prepared.clone());
        let changes = self.runtime.diffs.get(&review).and_then(|d| d.prepared.clone());
        let viewing = self.client.diff_views.get(&review).and_then(|d| d.viewing.clone());
        let (main, changes) = match (main, changes) {
            (Some(main), Some(changes)) => (main, changes),
            (Some(main), None) => (main.clone(), main),
            (None, Some(changes)) => (changes.clone(), changes),
            (None, None) => {
                ui.label(RichText::new("Reading files…").color(p.muted).size(12.0));
                return;
            }
        };
        let explorer = self.client.explorers.entry(project).or_default();
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
            .filter(|p| !self.runtime.open_projects.contains(p) && p.is_dir())
            .take(8)
            .cloned()
            .collect();
        if !recent.is_empty() {
            ui.separator();
            ui.label(RichText::new("Recent projects").size(11.0).color(self.runtime.palette.muted));
            for root in recent {
                if ui.button(paths::display(&root)).clicked() {
                    self.open_project(&root);
                    ui.close();
                }
            }
        }
        ui.separator();
        ui.label(RichText::new("Workspaces").size(11.0).color(self.runtime.palette.muted));
        for name in Workspace::list() {
            let current = self.runtime.workspace.as_deref() == Some(name.as_str());
            if ui.selectable_label(current, &name).clicked() {
                self.open_workspace(&name);
                ui.close();
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.client.workspace_input).hint_text("name").desired_width(120.0));
            let name = self.client.workspace_input.trim().to_string();
            if ui
                .add_enabled(
                    !name.is_empty() && !self.runtime.open_projects.is_empty(),
                    egui::Button::new("Save workspace"),
                )
                .clicked()
            {
                self.save_workspace(&name);
                self.client.workspace_input.clear();
                ui.close();
            }
        });
    }

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
                "Adds `imacs hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
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
        let keys = &self.runtime.config.keys;
        for (key, what) in [
            (&keys.quick_open, "open file"),
            (&keys.toggle_diff, "show/hide diff"),
            (&keys.next_attention, "next agent that needs you"),
            (&keys.focus_steer, "steer bar"),
            (&keys.new_session, "new session"),
            (&keys.voice, "voice: record, send (Esc cancels)"),
        ] {
            ui.label(RichText::new(format!("{}  {what}", shortcut_label(ui.ctx(), key))).size(12.0).monospace());
        }
    }

    fn pending_open_resume(&mut self, record: SessionRecord) {
        self.post(move |state| state.resume.push(record));
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
