use std::path::{Path, PathBuf};

use atomics_core::ops::HostAction;
use atomics_core::projection::ChiefSelection;
use egui::{RichText, Stroke};

use crate::{Backend, Client, Command};

#[derive(Clone, Debug)]
pub(super) struct Dialog {
    pub request: String,
    pub submitted: bool,
    /// A session of its own, instead of the one in front.
    fresh: bool,
    /// The chief a session of its own starts with: the current one's provider and model, until another is picked.
    pub chief: Option<ChiefSelection>,
    prompt: String,
    project: Option<PathBuf>,
    projects: Vec<PathBuf>,
    workspace: Option<String>,
    browse: bool,
    focus: bool,
}

pub(super) fn open(backend: &dyn Backend, client: &mut Client) {
    match atomics_core::ticket_ui::request_id() {
        Ok(request) => {
            let project = super::message_project(backend, client);
            client.simple.plan_dialog = Some(Dialog {
                request,
                submitted: false,
                fresh: false,
                chief: backend.available_chiefs().map(|options| options.selected),
                prompt: String::new(),
                project,
                workspace: None,
                projects: Vec::new(),
                browse: false,
                focus: true,
            });
        }
        Err(_) => client.send(Command::Toast("Could not open a new plan. Try again.".into())),
    }
}

fn name(path: &Path) -> String {
    path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned()
}

pub(super) fn draw(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(mut draft) = client.simple.plan_dialog.take() else { return };
    let palette = client.palette.clone();
    let ctx = ui.ctx();
    let frame = super::look::dialog(ui, &palette).inner_margin(22);
    let mut cancel = false;
    let modal = egui::Modal::new(egui::Id::NULL.with("simple-new-plan")).frame(frame).show(ctx, |ui| {
        super::interface_type(ui);
        ui.set_width((ctx.content_rect().width() - 70.0).clamp(1.0, 420.0));
        ui.heading("New plan");
        ui.add_space(12.0);
        if draft.browse {
            if ui.button("Back").clicked() {
                draft.browse = false;
                draft.focus = true;
            }
            if let Some(info) = backend.management()
                && let Some(root) = crate::management::folder(ui, backend, client, &info, "Use this folder")
            {
                draft.project = Some(root);
                draft.workspace = None;
                draft.browse = false;
                draft.focus = true;
            }
            return;
        }
        ui.label("Project or workspace");
        let menu = backend.project_menu();
        let label = draft
            .workspace
            .clone()
            .or_else(|| draft.project.as_deref().map(name))
            .unwrap_or_else(|| "Choose a project…".into());
        crate::controls::combo("new-plan-destination").width(ui.available_width()).selected_text(label).show_ui(
            ui,
            |ui| {
                let mut seen = std::collections::HashSet::new();
                for path in menu.recent.into_iter().map(|(p, _)| p).chain(backend.repos()) {
                    if !seen.insert(path.clone()) {
                        continue;
                    }
                    if ui
                        .selectable_label(
                            draft.workspace.is_none() && draft.project.as_ref() == Some(&path),
                            name(&path),
                        )
                        .on_hover_text(backend.display(&path))
                        .clicked()
                    {
                        draft.project = Some(path);
                        draft.workspace = None;
                        draft.focus = true;
                    }
                }
                if !menu.workspaces.is_empty() {
                    ui.separator();
                    ui.small("Workspaces");
                    let info = backend.management();
                    for workspace in menu.workspaces {
                        if ui.selectable_label(draft.workspace.as_ref() == Some(&workspace), &workspace).clicked() {
                            draft.projects = info
                                .as_ref()
                                .and_then(|m| m.tools["workspace_projects"][&workspace].as_array())
                                .into_iter()
                                .flatten()
                                .filter_map(|p| p.as_str().map(PathBuf::from))
                                .collect();
                            draft.project = draft.projects.first().cloned();
                            draft.workspace = Some(workspace);
                            draft.focus = true;
                        }
                    }
                }
                if let Some(info) = backend.management() {
                    ui.separator();
                    if ui.button("Choose a folder…").clicked() {
                        let path = draft
                            .project
                            .as_deref()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_else(|| info.folder_start.clone());
                        client.management.folder = Some(path.clone());
                        client.management.folder_page = 0;
                        client.send(Command::Manage(HostAction::BrowseFolder { path }));
                        draft.browse = true;
                    }
                }
            },
        );
        if draft.workspace.is_some() {
            if draft.projects.is_empty() {
                ui.small("This workspace has no projects.");
            } else {
                ui.label("Project");
                crate::controls::combo("new-plan-workspace-project")
                    .width(ui.available_width())
                    .selected_text(draft.project.as_deref().map(name).unwrap_or_default())
                    .show_ui(ui, |ui| {
                        for path in &draft.projects {
                            ui.selectable_value(&mut draft.project, Some(path.clone()), name(path))
                                .on_hover_text(backend.display(path));
                        }
                    });
            }
        }
        ui.add_space(12.0);
        ui.label("What would you like to do?");
        let id = ui.make_persistent_id("initial-prompt");
        if draft.focus && !ui.is_sizing_pass() && ctx.memory(|m| m.top_modal_layer() == Some(ui.layer_id())) {
            draft.focus = false;
            ctx.memory_mut(|m| m.request_focus(id));
        }
        let enter = !crate::chief::composing(ui, id)
            && ctx.memory(|m| m.has_focus(id))
            && !egui::Popup::is_any_open(ctx)
            && ui.input_mut(|i| {
                let before = i.events.len();
                i.events.retain(|event| {
                    !matches!(event, egui::Event::Key { key: egui::Key::Enter, pressed: true, modifiers, .. }
                if !modifiers.shift && !modifiers.alt && (modifiers.is_none() || modifiers.command || modifiers.ctrl))
                });
                before != i.events.len()
            });
        let edit = ui.add(
            egui::TextEdit::multiline(&mut draft.prompt)
                .id(id)
                .return_key(egui::KeyboardShortcut::new(egui::Modifiers::SHIFT, egui::Key::Enter))
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        );
        let edit = edit.accessible_name("Plan instructions");
        crate::touch::opens_keyboard(ui, &edit);
        if backend.chief_threads() {
            ui.add_space(10.0);
            let going_on = super::chief_session(backend).map(|(_, session)| session);
            let options = backend.available_chiefs().filter(|_| backend.chief_choice());
            // The host may say which chiefs it offers only after the dialog opened.
            if draft.chief.is_none() {
                draft.chief = options.as_ref().map(|options| options.selected.clone());
            }
            let picker = options.as_ref().zip(draft.chief.as_mut()).map(|(options, choice)| {
                |ui: &mut egui::Ui, fresh| {
                    super::model::new_thread_chief(ui, "new-plan-chief", options, choice, fresh, &palette);
                }
            });
            super::fresh_context(ui, &mut draft.fresh, going_on.as_ref(), &palette, picker);
        }
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let action = Command::Manage(HostAction::StartPlan {
                fresh: draft.fresh,
                chief: draft.chief.clone().filter(|_| draft.fresh && backend.chief_choice()),
                request: draft.request.clone(),
                text: draft.prompt.trim().into(),
                project: draft.project.as_deref().map(|p| p.to_string_lossy().into_owned()),
            });
            let ready = draft.project.is_some() && !draft.prompt.trim().is_empty() && backend.available(&action);
            let start = egui::Button::new(RichText::new("Start plan").color(palette.on_accent))
                .fill(palette.accent)
                .stroke(Stroke::NONE)
                .corner_radius(8);
            if (ui.add_enabled(ready, start).clicked() || enter) && ready {
                client.send(action);
                draft.submitted = true;
            }
            cancel = ui.button("Cancel").clicked();
        });
    });
    if !cancel && !modal.should_close() {
        client.simple.plan_dialog = Some(draft);
    }
}
