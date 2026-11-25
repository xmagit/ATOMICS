//! The top bar's navigation: the All, Ungrouped and Repos tabs and the Boards dropdown ([`crate::boards`]) beside them.
//! The tabs that do not fit fold into that dropdown's menu, the selected one last. A tab whose command the backend
//! cannot carry out is left out.

use std::path::{Path, PathBuf};

use egui::RichText;

use crate::boards;
use crate::topbar::{measure, record_item};
use crate::{Backend, Client, Command, SessionRow, ViewHints, controls};

/// The size of a tab's text, as the bar's other buttons have it.
const TEXT: f32 = 13.0;

#[derive(Clone)]
enum ViewTab {
    All,
    /// Every session outside a plan.
    Ungrouped,
    Repos,
    Project(PathBuf),
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

/// The navigation, laid out in the space `ui` has left.
pub fn view_tabs(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let gap = ui.spacing().item_spacing.x;
    let (natural, least) = boards::widths(ui, backend, client);
    let divider = measure_divider(ui) + gap;
    let mut nav = Nav::new(backend, client);
    let tabs = nav.tabs();
    let widths: Vec<f32> = tabs.iter().map(|tab| measure(ui, |ui| nav.view_tab(ui, tab, false))).collect();
    let selected = tabs.iter().position(|tab| nav.view_tab_selected(tab));
    let shown = tabs_in_row(&widths, selected, gap, ui.available_width() - least - divider);
    for &index in &shown {
        nav.view_tab(ui, &tabs[index], false);
    }
    if !shown.is_empty() {
        ui.separator();
    }
    let folded: Vec<ViewTab> =
        tabs.iter().enumerate().filter(|(index, _)| !shown.contains(index)).map(|(_, tab)| tab.clone()).collect();
    let width = natural.min(ui.available_width()).max(least);
    let folded = (!folded.is_empty()).then_some(move |ui: &mut egui::Ui, client: &mut Client| {
        let mut nav = Nav::new(backend, client);
        for tab in &folded {
            nav.view_tab(ui, tab, true);
        }
    });
    boards::dropdown(ui, width, backend, client, folded);
}

/// Which tabs of these `widths` stay in the row in `room`: as many as fit, from the first, with the selected one kept
/// over the others.
fn tabs_in_row(widths: &[f32], selected: Option<usize>, gap: f32, room: f32) -> Vec<usize> {
    let mut shown: Vec<usize> = (0..widths.len()).collect();
    let width = |shown: &[usize]| shown.iter().map(|&index| widths[index] + gap).sum::<f32>();
    while width(&shown) > room {
        match shown.iter().rposition(|&index| Some(index) != selected) {
            Some(at) => shown.remove(at),
            None => return Vec::new(),
        };
    }
    shown
}

/// The session filters where there is no top-bar tab strip, such as a phone's Agents pane.
pub fn filters(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let mut nav = Nav::new(backend, client);
    ui.horizontal_wrapped(|ui| {
        if backend.available(&Command::SetView(None)) {
            nav.view_tab(ui, &ViewTab::All, true);
            nav.view_tab(ui, &ViewTab::Repos, true);
        }
        if backend.available(&Command::ShowUngrouped) {
            nav.view_tab(ui, &ViewTab::Ungrouped, true);
        }
    });
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
    fn tabs(&self) -> Vec<ViewTab> {
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
        tabs
    }

    fn view_tab_selected(&self, tab: &ViewTab) -> bool {
        match tab {
            ViewTab::All => !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.is_none(),
            ViewTab::Ungrouped => !self.hints.plan_view && self.hints.ungrouped,
            ViewTab::Repos => !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.is_some(),
            ViewTab::Project(root) => {
                !self.hints.plan_view && !self.hints.ungrouped && self.hints.project.as_ref() == Some(root)
            }
        }
    }

    fn repos_tab(&mut self, ui: &mut egui::Ui, in_menu: bool) {
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
            let sessions = crate::frame::sessions(ui.ctx(), self.backend);
            let changes: usize = self.backend.repos().iter().map(|root| project_summary(&sessions, root).1).sum();
            format!("Repos · {changes}")
        };
        let text = RichText::new(&label).font(self.client.palette.ui(TEXT)).color(self.client.palette.foreground);
        let projects = |nav: &mut Self, ui: &mut egui::Ui| {
            for root in nav.backend.repos() {
                nav.view_tab(ui, &ViewTab::Project(root), true);
            }
        };
        if in_menu {
            ui.menu_button(text, |ui| projects(self, ui));
            return;
        }
        let palette = self.client.palette.clone();
        let arrow = controls::chevron_atom(palette.muted);
        let button = controls::flat(egui::Button::new((text, arrow)), selected, &palette);
        let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| projects(self, ui));
        record_item(ui, &label, &response);
    }

    /// A tab, in the row or (`in_menu`) in a menu, where a click also closes the menu.
    fn view_tab(&mut self, ui: &mut egui::Ui, tab: &ViewTab, in_menu: bool) {
        if matches!(tab, ViewTab::Repos) {
            self.repos_tab(ui, in_menu);
            return;
        }
        let p = self.client.palette.clone();
        let selected = self.view_tab_selected(tab);
        let (label, hover) = match tab {
            ViewTab::All => {
                let agents = crate::frame::sessions(ui.ctx(), self.backend).iter().filter(|s| !s.chief).count();
                (format!("All · {agents}"), "All projects".to_string())
            }
            ViewTab::Ungrouped => {
                let count = crate::frame::sessions(ui.ctx(), self.backend).iter().filter(|s| s.ungrouped).count();
                let hover = "Sessions outside any plan. New sessions start here, in scratch folders of their own";
                (format!("Ungrouped · {count}"), hover.to_string())
            }
            ViewTab::Project(root) => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (count, changes) = project_summary(&crate::frame::sessions(ui.ctx(), self.backend), root);
                (format!("{name} · {count} sessions · {changes} changes"), self.backend.display(root))
            }
            ViewTab::Repos => unreachable!(),
        };
        let text = RichText::new(&label).font(p.ui(TEXT)).color(p.foreground);
        let response = ui.add(controls::flat(egui::Button::new(text), selected, &p)).on_hover_text(hover);
        if !in_menu {
            record_item(ui, &label, &response);
        }
        if response.clicked() {
            match tab {
                ViewTab::All => self.client.send(Command::SetView(None)),
                ViewTab::Ungrouped => self.client.send(Command::ShowUngrouped),
                ViewTab::Project(root) => self.client.send(Command::SetView(Some(root.clone()))),
                ViewTab::Repos => unreachable!(),
            }
            if in_menu {
                ui.close();
            }
        }
        if let ViewTab::Project(root) = tab
            && self.backend.available(&Command::CloseIdle(root.clone()))
        {
            response.context_menu(|ui| {
                if ui.button("Close idle sessions").on_hover_text("Idle or exited sessions no plan task uses").clicked()
                {
                    self.client.send(Command::CloseIdle(root.clone()));
                    ui.close();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tabs_fold_from_the_last_and_the_selected_one_stays_longest() {
        let widths = [50.0, 90.0, 60.0];
        let gap = 10.0;
        assert_eq!(tabs_in_row(&widths, None, gap, 230.0), [0, 1, 2], "all of them fit");
        assert_eq!(tabs_in_row(&widths, None, gap, 229.0), [0, 1]);
        assert_eq!(tabs_in_row(&widths, Some(2), gap, 229.0), [0, 2], "the selected one stays");
        assert_eq!(tabs_in_row(&widths, Some(2), gap, 70.0), [2], "over every other");
        assert_eq!(tabs_in_row(&widths, Some(2), gap, 69.0), [] as [usize; 0], "until it does not fit itself");
        assert_eq!(tabs_in_row(&widths, None, gap, -5.0), [] as [usize; 0]);
    }
}
