//! The top bar's navigation: the All, Ungrouped and Repos tabs, a tab per plan, the menu the tabs that do
//! not fit move into, and the plan history. Tabs share a bounded slot: labels shorten first, then tabs move
//! into the menu, while the selected tab and the plans that need the user stay in the row. A tab, a close
//! button or a menu entry whose command the backend cannot carry out is left out.

use std::path::{Path, PathBuf};

use atomics_core::plan::PlanStatus;
use egui::{Color32, Rect, RichText, Sense, Stroke, pos2, vec2};

use crate::status::PULSE_STEP;
use crate::topbar::{measure, record_item};
use crate::{Backend, Client, Command, PlanRow, PlanState, SessionRow, ViewHints};

#[derive(Clone)]
enum ViewTab {
    All,
    /// Every session outside a plan.
    Ungrouped,
    Repos,
    Project(PathBuf),
    Plan(usize),
}

/// A project's agents, and the lines they changed.
fn project_summary(sessions: &[SessionRow], root: &Path) -> (usize, usize) {
    let agents = sessions.iter().filter(|s| !s.chief && s.project.as_deref() == Some(root));
    agents.fold((0, 0), |(count, changes), session| {
        (count + 1, changes + session.changes.map_or(0, |(additions, deletions)| additions + deletions))
    })
}

fn measure_divider(ui: &mut egui::Ui) -> f32 {
    measure(ui, |ui| {
        ui.separator();
    })
}

/// The navigation tabs, laid out in the space `ui` has left.
pub fn view_tabs(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    Nav::new(backend, client).view_tabs(ui);
}

/// The overflow menu's label when every plan tab is in it, and its color: how many plans need the user, or
/// how many agents run.
pub fn urgent_tabs_label(backend: &dyn Backend, client: &mut Client) -> (String, Color32) {
    Nav::new(backend, client).urgent_tabs_label()
}

/// The narrowest the navigation gets: the plans that need the user or run, the overflow menu and the
/// history.
pub fn urgent_navigation_width(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) -> f32 {
    Nav::new(backend, client).urgent_navigation_width(ui)
}

/// One draw of the navigation, with the view hints read once.
struct Nav<'a> {
    backend: &'a dyn Backend,
    client: &'a mut Client,
    hints: ViewHints,
}

impl<'a> Nav<'a> {
    fn new(backend: &'a dyn Backend, client: &'a mut Client) -> Self {
        Self { hints: backend.view_hints(), backend, client }
    }
}

impl Nav<'_> {
    /// Tabs share a bounded navigation slot. First shorten labels, then move
    /// excess tabs into a menu, keeping the selected tab in the visible row.
    fn urgent_tabs_label(&self) -> (String, Color32) {
        let rows = self.backend.plan_list();
        self.tabs_summary(&rows, 0..rows.len())
    }

    fn tabs_summary(&self, rows: &[PlanRow], indices: impl Iterator<Item = usize>) -> (String, Color32) {
        let mut needs = 0;
        let mut running = 0;
        for index in indices {
            match rows[index].state {
                PlanState::NeedsYou => needs += 1,
                PlanState::Running(count) => running += count,
                _ => {}
            }
        }
        if needs > 0 {
            (format!("Tabs · {needs} needs you"), self.client.palette.warning)
        } else if running > 0 {
            (format!("Tabs · {running} running"), self.client.palette.accent)
        } else {
            ("Tabs".into(), self.client.palette.foreground)
        }
    }

    fn plan_label_width(&self, ui: &mut egui::Ui, label: &str) -> f32 {
        measure(ui, |ui| {
            ui.add(egui::Button::new(RichText::new(label).font(self.client.palette.ui(12.5))).right_text("   "));
        })
    }

    fn urgent_navigation_width(&self, ui: &mut egui::Ui) -> f32 {
        let gap = ui.spacing().item_spacing.x;
        let menu = measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let rows = self.backend.plan_list();
        let protected = rows
            .iter()
            .filter(|row| row.state.urgency() > 0)
            .map(|row| {
                let marker = if row.changed { " *" } else { "" };
                self.plan_label_width(ui, &format!("{}{marker}", row.state.label("…"))) + gap
            })
            .sum::<f32>();
        let history = if protected > 0.0 && rows.iter().any(|row| !row.id.is_empty() && row.status != PlanStatus::Open)
        {
            measure(ui, |ui| {
                ui.menu_button("History", |_| {});
            }) + gap
        } else {
            0.0
        };
        protected + menu + history
    }

    fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let rows = self.backend.plan_list();
        let views = self.backend.available(&Command::SetView(None));
        let mut tabs = Vec::new();
        if views {
            tabs.push(ViewTab::All);
        }
        if self.backend.available(&Command::ShowUngrouped) {
            tabs.push(ViewTab::Ungrouped);
        }
        if views {
            tabs.push(ViewTab::Repos);
        }
        tabs.extend(
            rows.iter()
                .enumerate()
                .filter(|(index, row)| {
                    !row.id.is_empty()
                        && (row.status != PlanStatus::Archived
                            || self.hints.plan_view && self.hints.selected_plan == *index)
                        && self.backend.available(&plan_command(row, *index))
                })
                .map(|(index, _)| ViewTab::Plan(index)),
        );
        let gap = ui.spacing().item_spacing.x;
        let widths: Vec<f32> = tabs.iter().map(|tab| measure(ui, |ui| self.view_tab(ui, tab, None, None))).collect();
        let minimum: Vec<f32> = tabs
            .iter()
            .zip(&widths)
            .map(|(tab, width)| {
                if let ViewTab::Plan(index) = tab {
                    let row = &rows[*index];
                    if row.state.urgency() > 0 {
                        let marker = if row.changed { " *" } else { "" };
                        return self.plan_label_width(ui, &format!("{}{marker}", row.state.label("…")));
                    }
                }
                width.min(90.0)
            })
            .collect();
        let mut priority: Vec<usize> = (0..tabs.len()).collect();
        priority.sort_by_key(|&index| match &tabs[index] {
            ViewTab::Plan(plan) if rows[*plan].state == PlanState::NeedsYou => 0,
            ViewTab::Plan(plan) if matches!(rows[*plan].state, PlanState::Running(_)) => 1,
            ViewTab::All | ViewTab::Ungrouped | ViewTab::Repos if self.view_tab_selected(&tabs[index]) => 2,
            ViewTab::All | ViewTab::Ungrouped | ViewTab::Repos => 3,
            ViewTab::Plan(_) if self.view_tab_selected(&tabs[index]) => 4,
            ViewTab::Plan(plan) if rows[*plan].status == PlanStatus::Open => 5,
            _ => 6,
        });
        let has_plans = tabs.iter().any(|tab| matches!(tab, ViewTab::Plan(_)));
        let divider = if has_plans { measure_divider(ui) + gap } else { 0.0 };
        let closed = rows.iter().any(|row| !row.id.is_empty() && row.status != PlanStatus::Open);
        let history_width = if closed {
            measure(ui, |ui| {
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
                    && rows[*index].state.urgency() > 0
                {
                    Some(width + gap)
                } else {
                    None
                }
            })
            .sum();
        let mut menu_label = "Tabs".to_string();
        let mut menu_color = self.client.palette.foreground;
        let mut menu_width = measure(ui, |ui| {
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
                    .filter(|&index| matches!(tabs[index],ViewTab::Plan(plan) if rows[plan].state.urgency()>0))
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
                (menu_label, menu_color) = self.tabs_summary(
                    &rows,
                    tabs.iter().enumerate().filter_map(|(index, tab)| {
                        if !visible.contains(&index)
                            && let ViewTab::Plan(plan) = tab
                        {
                            Some(*plan)
                        } else {
                            None
                        }
                    }),
                );
                let measured = measure(ui, |ui| {
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
            record_item(ui, "Tabs", &response);
        }
        if pin_history {
            self.history_menu(ui);
        }
    }

    fn view_tab_selected(&self, tab: &ViewTab) -> bool {
        match tab {
            ViewTab::All => !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.is_none(),
            ViewTab::Ungrouped => !self.hints.plan_view && self.hints.ungrouped,
            ViewTab::Repos => !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.is_some(),
            ViewTab::Project(root) => {
                !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.as_ref() == Some(root)
            }
            ViewTab::Plan(index) => self.hints.plan_view && self.hints.selected_plan == *index,
        }
    }

    fn repos_tab(&mut self, ui: &mut egui::Ui, width: Option<f32>) {
        let selected = self.view_tab_selected(&ViewTab::Repos);
        let label = if selected {
            self.hints
                .project
                .as_ref()
                .and_then(|root| root.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            let sessions = self.backend.session_list();
            let changes: usize = self.backend.repos().iter().map(|root| project_summary(&sessions, root).1).sum();
            format!("Repos · {changes}")
        };
        let color = if selected { self.client.palette.accent } else { self.client.palette.foreground };
        let mut menu = |ui: &mut egui::Ui| {
            ui.menu_button(RichText::new(&label).font(self.client.palette.ui(12.5)).color(color), |ui| {
                for root in self.backend.repos() {
                    self.view_tab(ui, &ViewTab::Project(root), None, None);
                }
            })
            .response
        };
        let response = match width {
            Some(width) => {
                ui.allocate_ui_with_layout(vec2(width, ui.spacing().interact_size.y), *ui.layout(), |ui| {
                    ui.set_min_width(width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    menu(ui)
                })
                .inner
            }
            None => menu(ui),
        };
        if width.is_some() {
            record_item(ui, &label, &response);
        }
    }

    fn view_tab(&mut self, ui: &mut egui::Ui, tab: &ViewTab, width: Option<f32>, natural_width: Option<f32>) {
        if matches!(tab, ViewTab::Repos) {
            self.repos_tab(ui, width);
            return;
        }
        let p = self.client.palette.clone();
        let selected = self.view_tab_selected(tab);
        let plan_row = match tab {
            ViewTab::Plan(index) => self.backend.plan_row(*index),
            _ => None,
        };
        let (label, hover, color) = match tab {
            ViewTab::All => {
                let agents = self.backend.session_list().iter().filter(|s| !s.chief).count();
                (format!("All · {agents}"), "All projects".into(), p.foreground)
            }
            ViewTab::Ungrouped => {
                let count = self.backend.session_list().iter().filter(|s| s.ungrouped).count();
                let hover = "Sessions outside any plan. New sessions start here, in scratch folders of their own";
                (format!("Ungrouped · {count}"), hover.into(), if selected { p.accent } else { p.foreground })
            }
            ViewTab::Project(root) => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (count, changes) = project_summary(&self.backend.session_list(), root);
                (
                    format!("{name} · {count} sessions · {changes} changes"),
                    self.backend.display(root),
                    if selected { p.accent } else { p.foreground },
                )
            }
            ViewTab::Repos => unreachable!(),
            ViewTab::Plan(_) => {
                let row = plan_row.as_ref().expect("a plan tab has a plan");
                let state = row.state;
                let marker = if row.changed { " *" } else { "" };
                let mut name: String = row.tab_name.chars().take(28).collect();
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
                (format!("{}{marker}", state.label(&name)), row.goal.clone(), state.color(&p))
            }
        };
        let state = plan_row.as_ref().map(|row| row.state);
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
            record_item(ui, &label, &response);
        }
        let mut close_clicked = false;
        let mut over_close = false;
        if let ViewTab::Plan(index) = tab
            && self.backend.available(&Command::ClosePlanTab(*index))
        {
            let close =
                Rect::from_center_size(pos2(response.rect.right() - 12.0, response.rect.center().y), vec2(18.0, 18.0));
            let _id = egui::Id::NULL.with(("plan-close", plan_row.as_ref().map_or("", |row| row.id.as_str())));
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
                #[cfg(feature = "test-support")]
                if !ui.is_sizing_pass() {
                    ui.ctx().data_mut(|data| data.insert_temp(_id, close));
                }
            }
            close_clicked = hit.clicked() || response.clicked() && over_close;
            if close_clicked {
                self.client.send(Command::ClosePlanTab(*index));
                if width.is_none() {
                    ui.close();
                }
            }
        }
        if response.clicked() && !close_clicked && !over_close {
            match tab {
                ViewTab::All => self.client.send(Command::SetView(None)),
                ViewTab::Ungrouped => self.client.send(Command::ShowUngrouped),
                ViewTab::Project(root) => self.client.send(Command::SetView(Some(root.clone()))),
                ViewTab::Repos => unreachable!(),
                ViewTab::Plan(index) => {
                    if let Some(row) = &plan_row {
                        self.client.send(plan_command(row, *index));
                    } else {
                        self.client.send(Command::ViewPlanHistory(*index));
                    }
                }
            }
            if width.is_none() {
                ui.close();
            }
        }
        match tab {
            ViewTab::Project(root) if self.backend.available(&Command::CloseIdle(root.clone())) => {
                response.context_menu(|ui| {
                    if ui
                        .button("Close idle sessions")
                        .on_hover_text("Idle or exited sessions no plan task uses")
                        .clicked()
                    {
                        self.client.send(Command::CloseIdle(root.clone()));
                        ui.close();
                    }
                });
            }
            ViewTab::Plan(index)
                if plan_row.as_ref().is_some_and(|row| row.status == PlanStatus::Completed)
                    && self.backend.available(&Command::ClosePlanTab(*index)) =>
            {
                response.context_menu(|ui| {
                    if ui.button("Archive plan").clicked() {
                        self.client.send(Command::ClosePlanTab(*index));
                        ui.close();
                    }
                });
            }
            _ => {}
        }
    }

    fn history_menu(&mut self, ui: &mut egui::Ui) {
        let mut view = None;
        let mut reopen = None;
        let can_view = self.backend.available(&Command::ViewPlanHistory(0));
        let can_reopen = self.backend.available(&Command::ReopenPlan(0));
        let response = ui
            .menu_button("History", |ui| {
                for (index, plan) in self
                    .backend
                    .plan_list()
                    .iter()
                    .enumerate()
                    .filter(|(_, plan)| !plan.id.is_empty() && plan.status != PlanStatus::Open)
                {
                    ui.push_id(&plan.id, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&plan.goal).color(plan.state.color(&self.client.palette)));
                            let status = if plan.status == PlanStatus::Completed { "completed" } else { "archived" };
                            ui.label(
                                RichText::new(format!("{status} · {}", self.backend.plan_date(index)))
                                    .color(self.client.palette.muted),
                            )
                            .on_hover_text(
                                "Date the plan was last saved, or its last task completion when no saved file exists",
                            );
                            if can_view {
                                let response = ui.button("View");
                                record_history_action(ui, &plan.id, "view", &response);
                                if response.clicked() {
                                    view = Some(index);
                                    ui.close();
                                }
                            }
                            if can_reopen {
                                let response = ui.button("Reopen");
                                record_history_action(ui, &plan.id, "reopen", &response);
                                if response.clicked() {
                                    reopen = Some(index);
                                    ui.close();
                                }
                            }
                        });
                    });
                }
            })
            .response;
        record_item(ui, "History", &response);
        if let Some(index) = view {
            self.client.send(Command::ViewPlanHistory(index));
        }
        if let Some(index) = reopen {
            self.client.send(Command::ReopenPlan(index));
        }
    }
}

/// What a plan's tab asks for: an open plan's sessions and board, else the finished plan's board, read-only.
fn plan_command(row: &PlanRow, index: usize) -> Command {
    if row.status == PlanStatus::Open { Command::ShowPlan(index) } else { Command::ViewPlanHistory(index) }
}

fn record_history_action(_ui: &egui::Ui, _plan: &str, _action: &str, _response: &egui::Response) {
    #[cfg(feature = "test-support")]
    if !_ui.is_sizing_pass() {
        _ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::NULL.with(("plan-history", _plan, _action)), _response.rect));
    }
}
