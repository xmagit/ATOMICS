//! The dialogs the shared views ask for and a platform draws itself. The desktop's are its own; these say what
//! the browser can do. Closing a session and closing a plan ask where the desktop asks ([`Client::confirm_close`],
//! [`Client::confirm_plan_close`]), plus where the mirror cannot tell a close that deletes something
//! ([`may_be_scratch`]). `+ New` opens a small picker of what the mirror names ([`Picker`]).

use std::path::PathBuf;

use atomics_core::plan::PlanStatus;
use atomics_ui::touch;
use atomics_ui::{Backend, Client, Command};
use egui::{Align2, Vec2};

use crate::remote::RemoteBackend;

/// Whether closing session `id` may delete a scratch folder without asking. The desktop asks then, even once the
/// agent exited; the mirror does not say which sessions those are, so it asks for any outside a worktree and
/// a plan. A running one asks anyway.
pub fn may_be_scratch(remote: &RemoteBackend, id: &str) -> bool {
    remote.session_list().iter().any(|s| s.id == id && s.ungrouped && s.worktree.is_none())
}

/// The plan at `index` to ask about closing: an open one, by id, with the status the shared board names.
pub fn plan_to_close(remote: &RemoteBackend, index: usize) -> Option<(String, PlanStatus)> {
    let row = remote.plan_row(index).filter(|row| row.status == PlanStatus::Open)?;
    Some((row.id, PlanStatus::Completed))
}

/// A dialog over the middle of the page, as the desktop's are, and never wider than a phone's screen.
fn dialog(ctx: &egui::Context, title: &str, touch: bool, add: impl FnOnce(&mut egui::Ui)) -> bool {
    let mut open = true;
    let width = ctx.content_rect().width() - 32.0;
    let window = egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .max_width(width)
        .anchor(Align2::CENTER_CENTER, Vec2::ZERO);
    // A finger has the dialog's own buttons; its title bar's × is a mouse's.
    let window = if touch { window } else { window.open(&mut open) };
    window.show(ctx, add);
    open
}

/// The confirmations and the picker waiting for the user, if any. What can no longer be done takes its dialog
/// with it: the session or plan gone, the channel down.
pub fn draw(ctx: &egui::Context, remote: &RemoteBackend, client: &mut Client, picker: &mut Option<Picker>) {
    close_plan(ctx, remote, client);
    close_session(ctx, remote, client);
    new_session(ctx, remote, client, picker);
}

fn close_session(ctx: &egui::Context, remote: &RemoteBackend, client: &mut Client) {
    let Some(id) = client.confirm_close.clone() else { return };
    let row = remote.session_list().into_iter().find(|s| s.id == id);
    let Some(row) = row.filter(|_| remote.available(&Command::CloseSession(id.clone()))) else {
        client.confirm_close = None;
        return;
    };
    let text = if may_be_scratch(remote, &id) {
        format!("Closing “{}” stops its process, and if it runs in a scratch folder, deletes that folder.", row.title)
    } else {
        format!("Closing “{}” stops its process. Claude sessions can be resumed later.", row.title)
    };
    let touched = client.touch;
    let mut answered = false;
    let open = dialog(ctx, "Close session?", touched.is_some(), |ui| {
        ui.label(text);
        ui.horizontal_wrapped(|ui| {
            if ui.add(touch::sized(egui::Button::new("Close session"), touched)).clicked() {
                // Past `carry_out`, which would ask again.
                remote.send(Command::CloseSession(id.clone()));
                answered = true;
            }
            answered |= ui.add(touch::sized(egui::Button::new("Cancel"), touched)).clicked();
        });
    });
    if answered || !open {
        client.confirm_close = None;
    }
}

fn close_plan(ctx: &egui::Context, remote: &RemoteBackend, client: &mut Client) {
    let Some((id, _)) = client.confirm_plan_close.clone() else { return };
    let plans = remote.plan_list();
    let index = plans.iter().position(|plan| plan.id == id && plan.status == PlanStatus::Open);
    // What atomics.v1 has of the desktop's two ways to close a plan.
    let ways: Vec<(&str, usize, PlanStatus)> = index
        .into_iter()
        .flat_map(|plan| {
            [("Complete plan", plan, PlanStatus::Completed), ("Archive plan", plan, PlanStatus::Archived)]
                .into_iter()
                .filter(|(_, plan, status)| remote.available(&Command::ClosePlan { plan: *plan, status: *status }))
        })
        .collect();
    let Some(goal) = index.filter(|_| !ways.is_empty()).map(|plan| plans[plan].goal.clone()) else {
        client.confirm_plan_close = None;
        return;
    };
    let touched = client.touch;
    let mut answered = false;
    let open = dialog(ctx, "Close plan?", touched.is_some(), |ui| {
        ui.label(format!("Close “{goal}”? You can view it from History, and reopen it on the computer."));
        ui.horizontal_wrapped(|ui| {
            for (label, plan, status) in ways {
                if ui.add(touch::sized(egui::Button::new(label), touched)).clicked() {
                    client.commands.push(Command::ClosePlan { plan, status });
                    answered = true;
                }
            }
            answered |= ui.add(touch::sized(egui::Button::new("Cancel"), touched)).clicked();
        });
    });
    if answered || !open {
        client.confirm_plan_close = None;
    }
}

/// What the new-session picker has picked: which agent, and where; `None` is a scratch folder of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picker {
    agent: Option<String>,
    folder: Option<PathBuf>,
}

/// The picker of `+ New`: the agents and folders the mirror names, which is what atomics.v1 lets a client know.
/// A new session is outside any plan, so the page goes to the Ungrouped tab once the host has started it.
fn new_session(ctx: &egui::Context, remote: &RemoteBackend, client: &mut Client, picker: &mut Option<Picker>) {
    if picker.is_none() {
        return;
    }
    if !remote.available(&Command::NewSession) {
        *picker = None;
        return;
    }
    let agents = remote.ui_config().agents;
    let folders = remote.folders();
    let Some(picked) = picker.as_mut() else { return };
    let agent = picked.agent.clone().filter(|agent| agents.contains(agent)).or_else(|| agents.first().cloned());
    let Some(mut agent) = agent else { return };
    let mut start = false;
    let mut cancel = false;
    let place = |folder: &Option<PathBuf>| match folder {
        Some(folder) => remote.display(folder),
        None => "Scratch folder".to_string(),
    };
    let open = dialog(ctx, "New session", false, |ui| {
        egui::Grid::new("new-session").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
            ui.label("Agent");
            egui::ComboBox::from_id_salt("agent").selected_text(&agent).show_ui(ui, |ui| {
                for name in &agents {
                    ui.selectable_value(&mut agent, name.clone(), name);
                }
            });
            ui.end_row();
            ui.label("Where");
            egui::ComboBox::from_id_salt("where").selected_text(place(&picked.folder)).width(320.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut picked.folder, None, place(&None));
                for folder in &folders {
                    let folder = Some(folder.clone());
                    ui.selectable_value(&mut picked.folder, folder.clone(), place(&folder));
                }
            });
            ui.end_row();
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            start = ui.button("Start").clicked();
            cancel = ui.button("Cancel").clicked();
        });
    });
    let folder = picked.folder.clone();
    picked.agent = Some(agent.clone());
    if start {
        client.send(Command::Start { agent, folder });
    }
    if start || cancel || !open {
        *picker = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_host;
    use crate::world::World;

    #[test]
    fn the_picker_starts_nothing_until_asked_and_keeps_what_was_picked() {
        let mut world = World::paired(fake_host::host());
        world.boot(None);
        let remote = &world.driver.remote;
        remote.set_chief_window(
            crate::desk::ChiefWindow { open: true, dock: atomics_core::config::ChiefDock::Docked, width: None },
            true,
        );
        let ctx = egui::Context::default();
        let mut client = Client::default();
        let mut picker = Some(Picker::default());
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 1000.0))),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| draw(ui.ctx(), remote, &mut client, &mut picker));
        output.drop_without_applying_deltas();
        assert!(client.commands.is_empty(), "{:?}", client.commands);
        assert_eq!(picker.and_then(|p| p.agent).as_deref(), Some("claude"), "the first the mirror names");
    }
}
