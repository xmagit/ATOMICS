//! Settings, remote devices and the host's folder picker, shared by every client.
use crate::{Backend, Client, Command};
use atomics_core::ops::HostAction;
use atomics_core::projection::{ManagementInfo, PairingInfo};
use atomics_core::selfconfig::Scope;
use egui::RichText;

mod remote;
mod settings;
pub mod setup;

#[derive(Default)]
pub struct State {
    pub settings: bool,
    pub remote: bool,
    pub project: bool,
    /// The setup screen shows.
    pub setup: bool,
    /// A first run has opened it, here, once.
    setup_opened: bool,
    /// It has asked the host to look for the programs since it opened.
    setup_loaded: bool,
    /// The Settings section that opens as Settings next draws.
    open_section: Option<String>,
    /// What was pasted for a setup step and not sent yet. It goes as Settings closes.
    setup_values: std::collections::HashMap<atomics_core::projection::SetupAction, String>,
    /// The setup step waiting for a yes, because it cannot be undone.
    setup_confirm: Option<atomics_core::projection::SetupAction>,
    pub folder: Option<String>,
    pub folder_page: u32,
    pub filter: String,
    pub toml: String,
    pub remove: String,
    pub reason: String,
    pub theme: String,
    pub connection_name: String,
    pub connection_url: String,
    pub credential_kind: String,
    pub credential_host: String,
    pub credential_value: String,
    pub scope: Option<Scope>,
    pub revoke: Option<String>,
    pub proposal_seen: Option<(String, f64)>,
    pub proposal_note: String,
    /// "This machine only" as the user set it; unset, it follows whether this host's own file picks a theme, so that
    /// a pick shows.
    theme_host_only: Option<bool>,
    /// What Settings has asked the host to read since it opened.
    loaded: Vec<atomics_core::ops::SettingsTool>,
    /// The action Settings waits for a yes on, because it cannot be undone.
    confirm: Option<settings::Confirm>,
    /// The credential's value field takes the keyboard when it next draws.
    focus_credential: bool,
    /// A window here was open as this frame began, and so holds the keyboard.
    keyboard: bool,
}

impl State {
    /// What lives as long as Settings is open goes when it closes: above all a value typed and never saved.
    fn forget_settings(&mut self) {
        self.credential_value.clear();
        self.setup_values.clear();
        self.setup_confirm = None;
        self.confirm = None;
        self.loaded.clear();
        self.focus_credential = false;
        if !self.remote {
            self.revoke = None;
        }
    }
}

/// Whether a shared window (Settings, Remote, setup, project picker or pull requests) is open.
pub fn window_open(client: &Client) -> bool {
    client.pull_requests.open
        || client.management.settings
        || client.management.remote
        || client.management.project
        || client.management.setup
}

/// The windows' share of the keyboard. Every client calls this as its frame begins, before it draws anything that
/// reads keys: Escape closes the window on top, one for each press, and a menu inside it closes first, so the key
/// never also reaches a terminal behind it. A window takes the keyboard when it opens. When the last one closes the
/// active terminal gets it back, unless a click has moved it since; asked here, the closing window's commands have
/// run, so Open project doesn't hand it to the session it just left. Whether a window just opened: a client whose
/// terminal keeps its own keyboard lets go.
pub fn begin_frame(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) -> bool {
    if window_open(client)
        && !egui::Popup::is_any_open(ctx)
        && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
    {
        let state = &mut client.management;
        let top = if client.pull_requests.open {
            &mut client.pull_requests.open
        } else if state.project {
            &mut state.project
        } else if state.remote {
            &mut state.remote
        } else if state.settings {
            &mut state.settings
        } else {
            &mut state.setup
        };
        *top = false;
    }
    let open = window_open(client);
    if open == client.management.keyboard {
        return false;
    }
    client.management.keyboard = open;
    if open {
        if let Some(id) = ctx.memory(|m| m.focused()) {
            ctx.memory_mut(|m| m.surrender_focus(id));
        }
    } else if ctx.memory(|m| m.focused().is_none()) {
        client.focus_terminal = backend.view_hints().active;
    }
    open
}

fn allowed(backend: &dyn Backend, action: &HostAction) -> bool {
    backend.available(&Command::Manage(action.clone()))
}

pub fn open_project(backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    client.management.project = true;
    client.management.folder_page = 0;
    let path = client.management.folder.get_or_insert(info.folder_start).clone();
    client.send(Command::Manage(HostAction::BrowseFolder { path }));
}

pub fn buttons(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    let remote = ui.button(if info.remote.problems.is_empty() { "Remote" } else { "Remote ●" });
    crate::topbar::record_item(ui, "Remote", &remote);
    if remote.clicked() {
        client.management.remote = !client.management.remote;
        ui.close();
    }
    let settings = ui.button("Settings");
    crate::topbar::record_item(ui, "Settings", &settings);
    if settings.clicked() {
        client.management.settings = !client.management.settings;
        ui.close();
    }
}

pub fn dialogs(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    if !client.management.settings {
        client.management.forget_settings();
    }
    let Some(info) = backend.management() else { return };
    let screen = ctx.content_rect();
    let width = (screen.width() - 24.0).clamp(160.0, 780.0);
    setup::track(ctx, backend, client, &info);
    setup::window(ctx, backend, client, &info);
    if client.management.settings {
        // Its top stays put as sections open and close, so the row under the pointer does not move away.
        const TOP: f32 = 44.0;
        const WINDOW_CHROME: f32 = 52.0;
        let mut open = true;
        egui::Window::new("Settings")
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, TOP))
            .open(&mut open)
            .collapsible(false)
            .auto_sized()
            .min_width(width.min(680.0))
            .max_width(width.min(680.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height((screen.height() - TOP - WINDOW_CHROME).max(120.0))
                    .show(ui, |ui| settings::draw(ui, backend, client, &info));
            });
        // Something in it may have closed it, as Open the setup screen does.
        client.management.settings &= open;
    }
    if client.management.remote {
        let mut open = true;
        egui::Window::new("Remote devices")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .default_width(width.min(440.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(ctx.content_rect().height() * 0.75)
                    .show(ui, |ui| remote::draw(ui, backend, client, &info));
            });
        client.management.remote = open;
    }
    if client.management.project {
        let mut open = true;
        egui::Window::new("Open project on host")
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .default_width(width.min(600.0))
            .show(ctx, |ui| folder(ui, backend, client, &info));
        client.management.project &= open;
    }
    proposal(ctx, backend, client, &info);
    setup::start_choice(ctx, backend, client, &info);
    // What opened or closed as this frame drew moves the keyboard as the next begins.
    if window_open(client) != client.management.keyboard {
        ctx.request_repaint();
    }
}

fn folder(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    ui.label("Choose a folder on the host.");
    let path = client.management.folder.clone().unwrap_or_else(|| info.folder_start.clone());
    ui.label(RichText::new(&path).monospace());
    let enabled = allowed(backend, &HostAction::BrowseFolder { path: path.clone() });
    ui.add_enabled_ui(enabled, |ui| {
        let found = info.folders.iter().find(|f| {
            (f.page.worktree == path || f.requested == path) && f.page.offset == client.management.folder_page
        });
        let mut browse = None;
        let mut next_page = None;
        ui.horizontal(|ui| {
            if ui.button("Home").clicked() {
                browse = Some(info.folder_start.clone());
            }
            if ui.button("Refresh").clicked() {
                browse = Some(path.clone());
            }
        });
        if let Some(folder) = found {
            client.management.folder = Some(folder.page.worktree.clone());
            if let Some(parent) = &folder.parent
                && ui.button("↑ Parent folder").clicked()
            {
                browse = Some(parent.clone());
            }
            if ui.add_enabled(folder.project, egui::Button::new("Open this project or worktree")).clicked() {
                client.send(Command::OpenProject(folder.worktree.as_deref().unwrap_or(&path).into()));
                client.management.project = false;
            }
            let init =
                HostAction::SettingsTool { tool: atomics_core::ops::SettingsTool::InitProject { path: path.clone() } };
            if !folder.project
                && allowed(backend, &init)
                && ui.button("Make this folder a project (git init)").clicked()
            {
                client.send(Command::Manage(init));
                client.management.project = false;
            }
            ui.horizontal(|ui| {
                if let Some(next) = folder.page.next_offset
                    && ui.button("Next folders").clicked()
                {
                    next_page = Some(next);
                }
                if folder.page.offset > 0 && ui.button("First folders").clicked() {
                    next_page = Some(0);
                }
            });
            egui::ScrollArea::vertical().max_height(ui.ctx().content_rect().height() * 0.55).show(ui, |ui| {
                for entry in &folder.page.entries {
                    let child = format!("{}/{}", folder.page.worktree.trim_end_matches('/'), entry.path);
                    if ui.button(format!("▸ {}", entry.path)).clicked() {
                        browse = Some(child);
                    }
                }
            });
        } else {
            ui.label("Waiting for the host folder listing…");
        }
        if let Some(offset) = next_page {
            client.management.folder_page = offset;
            client.send(Command::Manage(HostAction::BrowseFolderPage { path: path.clone(), offset }));
        }
        if let Some(path) = browse {
            client.management.folder_page = 0;
            client.management.folder = Some(path.clone());
            client.send(Command::Manage(HostAction::BrowseFolder { path }));
        }
    });
}

fn proposal(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let Some(pending) = info.proposals.first() else {
        client.management.proposal_seen = None;
        return;
    };
    let now = ctx.input(|i| i.time);
    if client.management.proposal_seen.as_ref().is_none_or(|(id, _)| id != &pending.id) {
        client.management.proposal_seen = Some((pending.id.clone(), now));
        client.management.proposal_note.clear();
    }
    let ready = now - client.management.proposal_seen.as_ref().unwrap().1 >= 0.8;
    if !ready {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let enabled =
        backend.available(&Command::DecideProposal { id: pending.id.clone(), apply: true, note: String::new() });
    let shown = egui::Window::new("Proposed config change")
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .id(egui::Id::NULL.with("config-proposal"))
        .collapsible(false)
        .default_width((ctx.content_rect().width() - 24.0).min(820.0))
        .show(ctx, |ui| {
            ui.label(RichText::new(format!("{} · {} scope", pending.file, pending.scope)).strong());
            ui.label(match &pending.reason {
                Some(reason) => format!("From {}: {reason}", pending.from),
                None => format!("From {}", pending.from),
            });
            for risk in &pending.risks {
                ui.colored_label(client.palette.warning, risk);
            }
            for warning in &pending.warnings {
                ui.colored_label(client.palette.warning, warning);
            }
            if let Some(preview) = &pending.preview
                && let Ok(tokens) = serde_json::from_value::<atomics_core::theme::ThemeTokens>(preview.clone())
            {
                ui.label("Preview");
                crate::theme::Palette::new(&tokens, crate::theme::Fonts::default()).show_preview(ui);
            }
            egui::ScrollArea::both().max_height(ctx.content_rect().height() * 0.45).show(ui, |ui| {
                for line in pending.diff.lines() {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
            if !enabled {
                ui.label("View only: applying settings requires the host grant.");
            }
            ui.horizontal(|ui| {
                for (label, apply) in [("Apply", true), ("Reject", false)] {
                    if ui.add_enabled(enabled && (!apply || ready), egui::Button::new(label)).clicked() {
                        let note = std::mem::take(&mut client.management.proposal_note);
                        client.send(Command::DecideProposal { id: pending.id.clone(), apply, note });
                    }
                }
            });
            ui.add(
                crate::controls::single_line(&mut client.management.proposal_note)
                    .hint_text("Note for the chief (optional)"),
            );
        });
    // Above Settings, which a click may have raised: Apply stays in reach.
    if let Some(shown) = shown {
        ctx.move_to_top(shown.response.layer_id);
    }
}
