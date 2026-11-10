//! The top bar's pieces: laying out a row of widgets (measuring it before drawing it, drawing into a
//! bounded slot), the brand, the notice line, the new-session button and the Project menu. The bar's layout
//! is [`crate::shell::top_bar`], the navigation is in [`crate::tabs`] and [`crate::boards`], the view switcher in
//! [`crate::switcher`] and the chief's pill in [`crate::chief`]. A button whose command the backend cannot carry out is not drawn.

use std::collections::HashSet;
use std::path::PathBuf;

use atomics_core::config::{AgentKind, KeyConfig};
use atomics_core::session::AgentStatus;
use egui::{Rect, RichText, Sense, vec2};

use crate::keys::shortcut_label;
use crate::shell::BrandSize;
use crate::{Backend, Client, Command};

/// Measure with the same fonts, padding and widgets that will actually be drawn.
/// The disabled, invisible child cannot activate controls or take space from the bar.
pub fn measure(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui)) -> f32 {
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("topbar-measure")
            .max_rect(Rect::from_min_size(ui.max_rect().min, vec2(100_000.0, ui.max_rect().height())))
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .sizing_pass()
            .invisible(),
    );
    contents(&mut child);
    child.min_rect().width().ceil()
}

/// A slot bounds both painting and layout. Popups retain their own clip rects.
pub fn slot(ui: &mut egui::Ui, id: &str, rect: Rect, contents: impl FnOnce(&mut egui::Ui)) {
    let mut child = ui.new_child(
        egui::UiBuilder::new().id_salt(id).max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child.set_clip_rect(ui.clip_rect().intersect(rect));
    child.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
    contents(&mut child);
}

/// Records where a top-bar item was drawn, for the desktop's layout tests (feature `test-support`).
pub fn record_item(_ui: &egui::Ui, _name: &str, _response: &egui::Response) {
    #[cfg(feature = "test-support")]
    if !_ui.is_sizing_pass() {
        _ui.ctx().data_mut(|data| {
            data.get_temp_mut_or_default::<Vec<(String, Rect)>>(egui::Id::NULL.with("topbar-items"))
                .push((_name.into(), _response.rect));
        });
    }
}

/// The icon, the name unless `size` is [`BrandSize::Mark`] and, at [`BrandSize::Full`], how many agents run
/// and what the sandbox VMs use.
pub fn brand(ui: &mut egui::Ui, size: BrandSize, backend: &dyn Backend, client: &mut Client) {
    if let Some(icon) = &client.icon {
        let response = ui.image((icon.id(), vec2(20.0, 20.0))).on_hover_text("ATOMICS");
        record_item(ui, "icon", &response);
    }
    if size != BrandSize::Mark || client.icon.is_none() {
        let name = RichText::new("ATOMICS").strong().size(14.0).color(client.palette.accent).monospace();
        let response = ui.label(name);
        record_item(ui, "brand", &response);
    }
    if size == BrandSize::Full {
        let sessions = backend.session_list();
        let statuses: Vec<_> =
            sessions.iter().filter(|s| !s.chief && s.kind != AgentKind::Shell).map(|s| s.status).collect();
        let response = ui.label(RichText::new(agent_count(&statuses)).color(client.palette.muted));
        record_item(ui, "agents", &response);
        let breakdown = agent_breakdown(&statuses);
        if !breakdown.is_empty() {
            response.on_hover_text(breakdown);
        }
        let names: HashSet<_> = sessions.iter().filter_map(|s| s.sandbox.as_ref()).collect();
        if !names.is_empty() {
            let load = backend.sandbox_load();
            let label = match &load.total {
                None => "VMs —".into(),
                Some(total) => format!("VMs {total}"),
            };
            let response =
                ui.label(RichText::new(label).size(11.0).color(client.palette.muted)).on_hover_text(format!(
                    "Host overhead of {}/{} sandbox VM process trees. 100% CPU = one core.{}",
                    load.sampled,
                    names.len(),
                    load.error.as_ref().map_or(String::new(), |e| format!("\n{e}"))
                ));
            record_item(ui, "sandbox-total", &response);
        }
    }
    ui.separator();
}

/// How the agents stand: "idle" only when every one of them is. It stays this narrow however they stand, because the
/// navigation beside it needs the room just as agents finish, and the notice line after it says who needs you.
fn agent_count(statuses: &[AgentStatus]) -> String {
    let open = statuses.len();
    let working = statuses.iter().filter(|status| **status == AgentStatus::Working).count();
    if working > 0 {
        format!("{working} working · {open} open")
    } else if statuses.iter().all(|status| *status == AgentStatus::Idle) {
        format!("{open} idle")
    } else {
        format!("{open} open")
    }
}

/// What the count leaves out, on hover: the agents that are neither working nor idle.
fn agent_breakdown(statuses: &[AgentStatus]) -> String {
    let count = |status| statuses.iter().filter(|s| **s == status).count();
    let need = count(AgentStatus::NeedsInput);
    let parts = [
        (need, if need == 1 { "needs you" } else { "need you" }),
        (count(AgentStatus::Done), "finished"),
        (count(AgentStatus::Exited), "exited"),
        (count(AgentStatus::Starting), "starting"),
    ];
    let parts: Vec<_> = parts.iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}")).collect();
    parts.join(" · ")
}

/// `response` with a tooltip naming the chord `key` reads from the `[keys]`, on a platform that takes them.
pub fn shortcut_tip(
    response: egui::Response,
    backend: &dyn Backend,
    key: impl FnOnce(&KeyConfig) -> &String,
) -> egui::Response {
    if !backend.has_shortcuts() {
        return response;
    }
    let keys = backend.ui_config().keys;
    let label = shortcut_label(&response.ctx, key(&keys));
    response.on_hover_text(label)
}

/// Opens the new-session dialog.
pub fn new_button(ui: &mut egui::Ui, in_menu: bool, backend: &dyn Backend, client: &mut Client) {
    if !backend.available(&Command::NewSession) {
        return;
    }
    let response = ui.button("+ New");
    record_item(ui, "+ New", &response);
    if shortcut_tip(response, backend, |keys| &keys.new_session).clicked() {
        client.send(Command::NewSession);
        if in_menu {
            ui.close();
        }
    }
}

/// Keeps the explorer reachable when the layout gives the sidebar's room to the diff or chief.
pub fn explorer_menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.repos().is_empty() {
        return;
    }
    ui.menu_button("Explorer", |ui| {
        ui.set_min_width(260.0);
        ui.set_max_width(360.0);
        ui.set_max_height(600.0);
        crate::sidebar::explorer(ui, backend, client);
    })
    .response
    .on_hover_text("The open projects, their worktrees and files");
}

/// The Project menu: open a project, a recent project or a workspace, or save the open projects as one.
pub fn project_menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.management().is_some() {
        for project in backend.repos() {
            ui.label(backend.display(&project));
        }
        if !backend.available(&Command::PickProject) {
            ui.label("View only: opening projects requires the host grant.");
        }
    }
    if ui.add_enabled(backend.available(&Command::PickProject), egui::Button::new("Open project…")).clicked() {
        ui.close();
        client.send(Command::PickProject);
    }
    let menu = backend.project_menu();
    if !menu.recent.is_empty() && backend.available(&Command::OpenProject(PathBuf::new())) {
        ui.separator();
        ui.label(RichText::new("Recent projects").size(11.0).color(client.palette.muted));
        for (root, display) in menu.recent {
            if ui.button(display).clicked() {
                client.send(Command::OpenProject(root));
                ui.close();
            }
        }
    }
    if !backend.available(&Command::OpenWorkspace(String::new())) {
        return;
    }
    ui.separator();
    ui.label(RichText::new("Workspaces").size(11.0).color(client.palette.muted));
    for name in menu.workspaces {
        let current = menu.current.as_deref() == Some(name.as_str());
        if ui.selectable_label(current, &name).clicked() {
            client.send(Command::OpenWorkspace(name));
            ui.close();
        }
    }
    if !backend.available(&Command::SaveWorkspace(String::new())) {
        return;
    }
    ui.horizontal(|ui| {
        ui.add(crate::controls::single_line(&mut client.workspace_input).hint_text("name").desired_width(120.0));
        let name = client.workspace_input.trim().to_string();
        if ui.add_enabled(!name.is_empty() && menu.can_save, egui::Button::new("Save workspace")).clicked() {
            client.send(Command::SaveWorkspace(name));
            client.workspace_input.clear();
            ui.close();
        }
    });
}

/// The notice line: how many sessions need the user, the last toast and what is wrong with the config, the
/// theme or the hooks. A click activates the first session that needs the user.
pub fn notice(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if ui.available_width() < 24.0 {
        return;
    }
    let sessions = backend.session_list();
    let waiting = sessions.iter().filter(|s| s.needs_user()).count();
    let state = backend.notices();
    let mut notices = Vec::new();
    if waiting > 0 {
        notices.push(format!("● {waiting} need{} you", if waiting == 1 { "s" } else { "" }));
    }
    if let Some(text) = &state.toast {
        notices.push(text.clone());
    }
    // Each problem says what to do about it: the hover shows the whole line.
    if let Some(err) = &state.config_error {
        notices.push(format!("config error: {err} (the last good config stays in effect; fix the file to reload)"));
    }
    if let Some(problems) = &state.config_problems {
        notices.push(format!("config: {problems} (atomics doctor says more)"));
    }
    if let Some(err) = &state.theme_error {
        notices.push(format!("theme: {err} (the rest of the theme applies)"));
    }
    if let Some(err) = &state.hook_error {
        notices.push(format!("hooks offline: {err} (agents cannot report; run atomics doctor)"));
    }
    if notices.is_empty() {
        return;
    }
    let text = notices.join(" · ");
    let response = ui.add(
        egui::Label::new(RichText::new(&text).color(if waiting > 0 {
            client.palette.warning
        } else {
            client.palette.foreground
        }))
        .truncate()
        .sense(Sense::click()),
    );
    record_item(ui, "notice", &response);
    if crate::touch::reach(ui, response).on_hover_text(text).clicked()
        && backend.available(&Command::Activate { session: String::new(), open_diff: true })
        && let Some(session) = sessions.iter().find(|s| s.needs_user())
    {
        client.send(Command::Activate { session: session.id.clone(), open_diff: true });
    }
}

/// The workspace open on the host, as a top-bar item a platform adds through its `top_bar_items` hook:
/// "Workspace: <name>", or "No workspace" in the warning colour, opening a picker of the saved workspaces
/// with the open one marked. In the Actions menu it is a submenu. Nothing is drawn where the backend
/// cannot open a workspace, or while none is open and none is saved, so there is nothing to pick.
pub fn workspace_menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if !backend.available(&Command::OpenWorkspace(String::new())) {
        return;
    }
    let menu = backend.project_menu();
    if menu.current.is_none() && menu.workspaces.is_empty() {
        return;
    }
    let p = client.palette.clone();
    let label = match &menu.current {
        Some(name) => RichText::new(format!("Workspace: {name}")),
        None => RichText::new("No workspace").color(p.warning),
    };
    let response = ui
        .menu_button(label, |ui| {
            let open = match &menu.current {
                Some(name) => format!("{name} is open"),
                None => "No workspace is open".into(),
            };
            ui.label(RichText::new(open).strong());
            ui.label(RichText::new("Open workspace").size(11.0).color(p.muted));
            if menu.workspaces.is_empty() {
                ui.label(RichText::new("none saved yet").size(11.0).color(p.muted));
            }
            for name in &menu.workspaces {
                let current = menu.current.as_deref() == Some(name.as_str());
                if ui.selectable_label(current, name).clicked() {
                    client.send(Command::OpenWorkspace(name.clone()));
                    ui.close();
                }
            }
        })
        .response;
    record_item(ui, "Workspace", &response);
    response.on_hover_text(
        "The workspace open on the host: its projects and its [tickets], workflow and chief settings. A start \
         that names none, and a restart, open it again.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentStatus::{Done, Exited, Idle, NeedsInput, Starting, Working};

    #[test]
    fn the_count_stays_narrow_and_the_breakdown_says_the_rest() {
        assert_eq!(agent_count(&[]), "0 idle");
        assert_eq!(agent_breakdown(&[]), "");
        assert_eq!(agent_count(&[Idle, Idle]), "2 idle");
        assert_eq!(agent_breakdown(&[Idle, Idle]), "");

        // However many states are on the fleet, the count takes the room of one number and one word.
        let mixed = [NeedsInput, NeedsInput, Done, Exited, Starting, Idle];
        assert_eq!(agent_count(&mixed), "6 open");
        assert_eq!(agent_breakdown(&mixed), "2 need you · 1 finished · 1 exited · 1 starting");
        assert_eq!(agent_breakdown(&[NeedsInput]), "1 needs you");

        // Work still counts first, and the hover still says who waits.
        assert_eq!(agent_count(&[Working, NeedsInput]), "1 working · 2 open");
        assert_eq!(agent_breakdown(&[Working, NeedsInput]), "1 needs you");
    }
}
