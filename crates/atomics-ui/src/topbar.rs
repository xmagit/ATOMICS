//! The top bar's pieces: laying out a row of widgets (measuring it before drawing it, drawing into a
//! bounded slot), the brand, the notice line, the diff, new-session and plan buttons and the Project menu.
//! The bar's layout is [`crate::shell::top_bar`], the navigation tabs are in [`crate::tabs`], and the
//! chief's pill in [`crate::chief`]. A button whose command the backend cannot carry out is not drawn.

use std::collections::HashSet;
use std::path::PathBuf;

use atomics_core::config::AgentKind;
use egui::{Rect, RichText, Sense, vec2};

use crate::keys::shortcut_label;
use crate::{Backend, Client, Command, PanelMode};

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

/// The icon, the name and, if `show_agents`, how many agents run and what the sandbox VMs use.
pub fn brand(ui: &mut egui::Ui, show_agents: bool, backend: &dyn Backend, client: &mut Client) {
    if let Some(icon) = &client.icon {
        let response = ui.image((icon.id(), vec2(20.0, 20.0)));
        record_item(ui, "icon", &response);
    }
    let response = ui.label(RichText::new("ATOMICS").strong().size(14.0).color(client.palette.accent).monospace());
    record_item(ui, "brand", &response);
    if show_agents {
        let sessions = backend.session_list();
        let agents = sessions.iter().filter(|s| s.kind != AgentKind::Shell).count();
        let response = ui.label(
            RichText::new(format!("{agents} agent{}", if agents == 1 { "" } else { "s" })).color(client.palette.muted),
        );
        record_item(ui, "agents", &response);
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

/// Shows or hides the diff panel.
pub fn diff_button(ui: &mut egui::Ui, in_menu: bool, backend: &dyn Backend, client: &mut Client) {
    if !backend.available(&Command::ToggleDiff) {
        return;
    }
    let response = ui.button(if backend.view_hints().diff_open { "Hide diff" } else { "Show diff" });
    record_item(ui, "diff", &response);
    if response.on_hover_text(shortcut_label(ui.ctx(), &backend.ui_config().keys.toggle_diff)).clicked() {
        client.send(Command::ToggleDiff);
        if in_menu {
            ui.close();
        }
    }
}

/// Opens the new-session dialog.
pub fn new_button(ui: &mut egui::Ui, in_menu: bool, backend: &dyn Backend, client: &mut Client) {
    if !backend.available(&Command::NewSession) {
        return;
    }
    let response = ui.button("+ New");
    record_item(ui, "+ New", &response);
    if response.on_hover_text(shortcut_label(ui.ctx(), &backend.ui_config().keys.new_session)).clicked() {
        client.send(Command::NewSession);
        if in_menu {
            ui.close();
        }
    }
}

/// The selected plan's progress; it shows or hides the plan board.
pub fn plan_button(ui: &mut egui::Ui, in_menu: bool, backend: &dyn Backend, client: &mut Client) {
    if !backend.available(&Command::TogglePlan) {
        return;
    }
    let hints = backend.view_hints();
    let plan = backend.plan_row(hints.selected_plan).expect("a plan is selected");
    let (done, total, attention) = (plan.done, plan.total, plan.attention);
    let label = if total == 0 { "Plan".to_string() } else { format!("Plan {done}/{total}") };
    let text =
        RichText::new(label).color(if attention > 0 { client.palette.warning } else { client.palette.foreground });
    let response = ui.selectable_label(hints.panel == PanelMode::Plan && hints.diff_open, text);
    record_item(ui, "Plan", &response);
    if response.on_hover_text(shortcut_label(ui.ctx(), &backend.ui_config().keys.toggle_plan)).clicked() {
        client.send(Command::TogglePlan);
        if in_menu {
            ui.close();
        }
    }
}

/// The Project menu: open a project, a recent project or a workspace, or save the open projects as one.
pub fn project_menu(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.available(&Command::PickProject) && ui.button("Open project…").clicked() {
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
        ui.add(egui::TextEdit::singleline(&mut client.workspace_input).hint_text("name").desired_width(120.0));
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
    let waiting = sessions.iter().filter(|s| s.status.needs_attention()).count();
    let state = backend.notices();
    let mut notices = Vec::new();
    if waiting > 0 {
        notices.push(format!("● {waiting} need{} you", if waiting == 1 { "s" } else { "" }));
    }
    if let Some(text) = &state.toast {
        notices.push(text.clone());
    }
    if let Some(err) = &state.config_error {
        notices.push(format!("config error: {err}"));
    }
    if let Some(problems) = &state.config_problems {
        notices.push(format!("config: {problems}"));
    }
    if let Some(err) = &state.theme_error {
        notices.push(format!("theme: {err}"));
    }
    if let Some(err) = &state.hook_error {
        notices.push(format!("hooks offline: {err}"));
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
    if response.on_hover_text(text).clicked()
        && backend.available(&Command::Activate { session: String::new(), open_diff: true })
        && let Some(session) = sessions.iter().find(|s| s.status.needs_attention())
    {
        client.send(Command::Activate { session: session.id.clone(), open_diff: true });
    }
}
