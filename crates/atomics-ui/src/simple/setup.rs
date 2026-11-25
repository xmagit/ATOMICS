//! Simple mode's setup page: what ATOMICS has and still needs, at a glance, in a modal over the window. The agents
//! it found and whether their accounts answer, the ticket system, the open projects, where the agents run and what
//! would make that safer, light or dark, and simple mode itself. Everything can wait; what takes more than a click
//! is in the full setup ("Getting started" in Settings), one link away.

use atomics_core::ops::{HostAction, SettingsTool};
use atomics_core::projection::{SetupClick, SetupInfo, SetupProgram};

use super::*;

/// Opens the setup page.
pub fn open(client: &mut Client) {
    client.simple.systems_open = false;
    client.simple.setup_open = true;
}

/// The page, while it is open: Escape, a click outside or Close closes it.
pub(super) fn modal(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if !client.simple.setup_open {
        return;
    }
    let p = client.palette.clone();
    let screen = ui.ctx().content_rect();
    let width = (screen.width() - 74.0).clamp(1.0, 600.0);
    let height = (screen.height() - 140.0).max(200.0);
    let frame = egui::Frame::new()
        .fill(p.overlay.to_opaque())
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(14)
        .inner_margin(egui::Margin::same(20));
    let modal = egui::Modal::new(egui::Id::NULL.with("simple-setup-modal")).frame(frame).show(ui.ctx(), |ui| {
        super::interface_type(ui);
        ui.set_width(width);
        let mut close = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Setup").font(font(20.0)).color(p.foreground));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let text = RichText::new("Close").font(font(12.5)).color(p.muted);
                let response = ui.add(egui::Button::new(text).frame(false));
                flow::record(ui, "close-setup", response.rect);
                close = response.clicked();
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.selectable_label(!client.simple.systems_open, "Workspace").clicked() {
                client.simple.systems_open = false;
            }
            if ui.selectable_label(client.simple.systems_open, "Connected systems").clicked() {
                open_systems(backend, client);
            }
        });
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .id_salt(("simple-setup-scroll", client.simple.systems_open))
            .max_height(height)
            .min_scrolled_height(height)
            .show(ui, |ui| {
                close |=
                    if client.simple.systems_open { systems(ui, backend, client) } else { body(ui, backend, client) };
            });
        close
    });
    if modal.inner || modal.should_close() {
        client.simple.setup_open = false;
    }
}

/// The page's sections; true when a link left it for another page.
fn body(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) -> bool {
    let p = client.palette.clone();
    let info = backend.management();
    let mut left = false;
    // The agents, and git.
    heading(ui, "Agents", &p);
    match info.as_ref().map(|info| &info.setup) {
        None => note(ui, "This window cannot read the host's setup.", &p),
        Some(setup) if setup.programs.is_empty() => {
            note(ui, if setup.checking { "Looking for them…" } else { "Not looked for yet." }, &p);
            check_again(ui, setup, backend, client);
        }
        Some(setup) => {
            for program in &setup.programs {
                program_row(ui, program, setup, backend, client);
            }
            check_again(ui, setup, backend, client);
        }
    }
    // The projects.
    heading(ui, "Projects", &p);
    let repos = backend.repos();
    if repos.is_empty() {
        note(ui, "None open: the chief works in a project of yours.", &p);
    }
    let can_close = backend.available(&Command::CloseProject(std::path::PathBuf::new()));
    for repo in &repos {
        let name = repo.file_name().map_or_else(|| backend.display(repo), |n| n.to_string_lossy().into_owned());
        ui.horizontal(|ui| {
            mark(ui, Mark::Done, &p);
            ui.label(RichText::new(&name).font(font(13.5)).color(p.foreground));
            ui.label(RichText::new(backend.display(repo)).font(font(11.5)).color(p.quiet));
            if can_close {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let text = RichText::new("Remove").font(font(12.5)).color(p.muted);
                    let response = ui.add(egui::Button::new(text).frame(false));
                    flow::record(ui, &format!("remove:{name}"), response.rect);
                    if response.on_hover_text("Close it in ATOMICS; nothing on disk changes").clicked() {
                        client.send(Command::CloseProject(repo.clone()));
                    }
                });
            }
        });
    }
    if backend.available(&Command::PickProject) && pill(ui, "Add a project…", false, client).clicked() {
        client.send(Command::PickProject);
    }
    // Where the agents run, and what would make it safer.
    heading(ui, "Where agents run", &p);
    let agents = backend.agents();
    let (short, long) = place_words(agents.as_ref(), client.link, client.machine_place.as_deref());
    let in_vm = agents.as_ref().is_some_and(|a| a.machine == atomics_core::projection::AgentMachine::Vm);
    let safe = agents.as_ref().is_none_or(|a| a.hardened != Some(false) && (in_vm || a.isolation != "bare"));
    row(ui, if safe { Mark::Done } else { Mark::Warn }, &short, long.trim_end_matches(" Machine has the details."), &p);
    if let Some(fix) = agents.as_ref().and_then(|agents| fix(agents, info.as_ref().map(|i| &i.setup))) {
        ui.add(egui::Label::new(RichText::new(fix).font(font(12.5)).color(p.warning)).wrap());
        left |= full_setup(ui, "Isolation and sandboxes…", client);
    }
    if in_vm {
        heading(ui, "NixOS VM", &p);
        note(ui, "Check Claude, Codex and GitHub sign-ins in a terminal inside the guest.", &p);
        ui.horizontal_wrapped(|ui| {
            for (label, cmd) in
                [("Copy sign-in check", "atomics-onboard --check"), ("Copy sign-in command", "atomics-onboard")]
            {
                if pill(ui, label, false, client).on_hover_text(cmd).clicked() {
                    ui.ctx().copy_text(cmd.into());
                    client.send(Command::Toast("Command copied".into()));
                }
            }
        });
        if (backend.machine_available() || client.machine_controls.is_some())
            && ui.link("VM and machine details…").clicked()
        {
            client.machine_open = true;
            left = true;
        }
    } else if agents.as_ref().is_some_and(|a| a.machine == atomics_core::projection::AgentMachine::Mac) {
        egui::CollapsingHeader::new(RichText::new("Set up a NixOS VM").font(font(13.0))).show(ui, |ui| {
            note(ui, "Run these commands in a Mac terminal to set up the guest and open its host.", &p);
            for cmd in ["atomics vm setup", "atomics --connect vm"] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(cmd).font(font(12.0)));
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(cmd.into());
                    }
                });
            }
        });
    }
    // The look, and this mode.
    heading(ui, "Appearance", &p);
    ui.horizontal(|ui| {
        let dark = client.palette.mode == atomics_core::theme::ThemeMode::Dark;
        let can = backend.available(&Command::Manage(HostAction::SelectTheme { name: String::new(), host: false }));
        for (label, name, on) in [("Dark", atomics_core::theme::DEFAULT, dark), ("Light", "paper", !dark)] {
            let response = ui.add_enabled(can, egui::Button::new(RichText::new(label).font(font(13.0))).selected(on));
            flow::record(ui, &format!("look:{label}"), response.rect);
            if response.clicked() && !on {
                client.send(Command::Manage(HostAction::SelectTheme { name: name.into(), host: false }));
            }
        }
        ui.add_space(16.0);
        let mut simple = true;
        let response = ui.checkbox(&mut simple, RichText::new("Simple mode").font(font(13.0)));
        flow::record(ui, "setup-simple", response.rect);
        if response.on_hover_text(format!("This window's layout; {} switches too", mode_shortcut(ui))).changed() {
            switch(backend, client, UiMode::Full);
            left = true;
        }
    });
    ui.add_space(14.0);
    ui.separator();
    left |= full_setup(ui, "The full setup, and every setting →", client);
    left
}

/// Use the same maintained profile as the configuration guide. Only the creation team's identifier varies.
fn linear_profile(team: &str) -> Option<String> {
    let (_, block) = atomics_core::config::DEFAULT_CONFIG.split_once("# BEGIN LINEAR PROFILE")?;
    let (block, _) = block.split_once("# END LINEAR PROFILE")?;
    let (_, tables) = block.split_once("# [tickets]")?;
    let mut profile = String::from("[tickets]\n");
    for line in tables.lines().filter_map(|line| line.strip_prefix("# ")) {
        profile.push_str(&line.replace("team = \"ENG\"", &format!("team = \"{team}\"")));
        profile.push('\n');
    }
    Some(profile)
}

pub(super) fn open_systems(backend: &dyn Backend, client: &mut Client) {
    client.simple.setup_open = true;
    client.simple.systems_open = true;
    let cmd = Command::Manage(HostAction::SettingsTool { tool: SettingsTool::RefreshConnections });
    if backend.available(&cmd) {
        client.send(cmd);
    }
}

fn systems(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) -> bool {
    let p = client.palette.clone();
    let info = backend.management();
    let mut left = false;
    heading(ui, "Ticket system", &p);
    if let Some(tickets) = backend.tickets() {
        row(ui, Mark::Done, &tickets.system, tickets.identity.as_deref().unwrap_or("Profile configured"), &p);
        if !crate::signin::banner(ui, backend, client) && pill(ui, "Open tickets", false, client).clicked() {
            client.simple.work = Work::Tickets;
            client.simple.pane = Pane::Work;
            client.simple.chat_focus = false;
            left = true;
        }
    } else if !crate::signin::banner(ui, backend, client) {
        let scope = if backend.project_menu().current.is_some() {
            atomics_core::selfconfig::Scope::Workspace
        } else {
            atomics_core::selfconfig::Scope::Main
        };
        egui::CollapsingHeader::new(RichText::new("Connect Linear").font(font(13.5))).show(ui, |ui| {
            note(ui, "Enter the team that will own new issues.", &p);
            let field = ui.add(egui::TextEdit::singleline(&mut client.simple.linear_team).hint_text("Team identifier"));
            touch::opens_keyboard(ui, &field);
            let team = client.simple.linear_team.trim();
            let valid = !team.is_empty()
                && team.len() <= 64
                && team.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
            if valid && let Some(profile) = linear_profile(team) {
                let action = Command::Manage(HostAction::ConfigPropose {
                    scope,
                    toml: profile,
                    remove: Vec::new(),
                    theme: None,
                    reason: Some(format!("Connect the {team} Linear team")),
                });
                if backend.available(&action) && pill(ui, "Review Linear setup", true, client).clicked() {
                    client.send(action);
                    left = true;
                }
            }
        });
        left |= full_setup(ui, "Other ticket systems…", client);
    }
    if let Some(info) = &info {
        let in_vm = backend.agents().is_some_and(|a| a.machine == atomics_core::projection::AgentMachine::Vm);
        heading(ui, "GitHub", &p);
        let credentials = &info.tools["credentials"];
        let accounts: Vec<_> = credentials["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["github_account"].as_str())
            .collect();
        let connected = info
            .setup
            .connections
            .iter()
            .find(|c| c.id == "github")
            .and_then(|c| c.connected.clone())
            .or_else(|| (!accounts.is_empty()).then(|| accounts.join(", ")));
        if let Some(account) = connected {
            row(ui, Mark::Done, &account, "", &p);
        } else if let Some(code) = credentials["github_device"]["code"].as_str() {
            ui.label(format!("Enter {code} at GitHub"));
            if let Some(url) = credentials["github_device"]["url"].as_str().and_then(crate::markdown::web_url) {
                ui.hyperlink_to("Open GitHub", url);
            }
        } else if backend.available(&Command::ConnectGithub) {
            if pill(ui, "Connect GitHub account", false, client).clicked() {
                client.send(Command::ConnectGithub);
            }
        } else {
            note(
                ui,
                if in_vm {
                    "Run atomics-onboard in a guest terminal to sign in to GitHub."
                } else {
                    "Connect GitHub in the host window."
                },
                &p,
            );
        }
        if let Some(error) = credentials["error"].as_str() {
            note(ui, error, &p);
        }
        heading(ui, "MCP servers", &p);
        let connections = &info.tools["connections"];
        note(
            ui,
            if in_vm {
                "Import reads Claude Code, Codex, Copilot CLI and Cursor CLI definitions inside this VM."
            } else {
                "Import reads Claude Code, Codex, Copilot CLI and Cursor CLI definitions on the host."
            },
            &p,
        );
        let rows = connections["rows"].as_array().map(Vec::as_slice).unwrap_or_default();
        if connections["refreshing"] == true {
            note(ui, "Checking servers…", &p);
        } else if rows.is_empty() && connections["error"].is_null() {
            note(ui, "No servers listed yet.", &p);
        }
        for server in rows {
            let Some(name) = server["name"].as_str() else { continue };
            let status = server["status"].as_str();
            let busy = server["busy"] == true;
            row(
                ui,
                if status == Some("connected") { Mark::Done } else { Mark::Open },
                name,
                if busy { "Connecting…" } else { status.unwrap_or("Available to connect") },
                &p,
            );
            if let Some(source) = server["source"].as_str() {
                let label = if source == "added" { "Added in ATOMICS".to_string() } else { format!("From {source}") };
                note(ui, &label, &p);
            }
            if !busy && (status.is_none() || status == Some("needs sign-in")) {
                let env = server["env"].as_u64().unwrap_or(0);
                ui.horizontal_wrapped(|ui| {
                    let choices = if env > 0 {
                        vec![
                            ("Connect without environment values".into(), false),
                            (format!("Store {env} values and connect"), true),
                        ]
                    } else {
                        vec![("Connect".into(), false)]
                    };
                    for (label, store_env) in choices {
                        let action = Command::Manage(HostAction::SettingsTool {
                            tool: SettingsTool::Connect { name: name.to_owned(), store_env },
                        });
                        if ui
                            .add_enabled(
                                backend.available(&action),
                                egui::Button::new(RichText::new(label).font(font(13.0))),
                            )
                            .clicked()
                        {
                            client.send(action);
                        }
                    }
                });
            }
            if let Some(error) = server["error"].as_str() {
                note(ui, error, &p);
            }
            ui.add_space(6.0);
        }
        let import = Command::Manage(HostAction::SettingsTool { tool: SettingsTool::ImportConnections });
        if connections["importing"] == true {
            note(ui, "Importing servers…", &p);
        } else if ui
            .add_enabled(
                backend.available(&import),
                egui::Button::new(RichText::new("Import MCP servers").font(font(13.0))),
            )
            .clicked()
        {
            client.send(import);
        }
        if let Some(error) = connections["error"].as_str() {
            note(ui, error, &p);
        }
        if ui.link("Manage servers…").clicked() {
            client.management.show_section("Connections");
            left = true;
        }
    }
    left
}

/// The shortcut that switches the layout, as this platform writes it.
fn mode_shortcut(ui: &egui::Ui) -> String {
    ui.ctx().format_shortcut(&super::chord(ui.ctx().os()))
}

/// What would make where the agents run safer, in a sentence: the VM's hardening, or the sandboxes that still need
/// something.
fn fix(agents: &atomics_core::projection::AgentsInfo, setup: Option<&SetupInfo>) -> Option<String> {
    if agents.hardened == Some(false) {
        return Some(
            "The VM is not hardened, so its user, and every agent, may become root. To harden it, set \
             atomics.vm.hardened = true in a NixOS module of your own and run atomics vm update on the Mac."
                .into(),
        );
    }
    if agents.machine == atomics_core::projection::AgentMachine::Vm && agents.isolation == "bare" {
        return None;
    }
    if agents.isolation == "bare" {
        return Some("Agents run bare, with your account's access. Sandboxes keep each to its own worktree.".into());
    }
    let missing: Vec<&str> = setup
        .map(|setup| setup.sandbox_steps.iter().filter(|step| !step.done).map(|step| step.name.as_str()).collect())
        .unwrap_or_default();
    (!missing.is_empty()).then(|| format!("Sandboxes still need: {}.", missing.join(", ")))
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mark {
    Done,
    Open,
    Warn,
}

fn heading(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add_space(12.0);
    ui.label(RichText::new(text).font(font(11.0)).color(p.quiet));
    ui.add_space(2.0);
}

fn note(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.add(egui::Label::new(RichText::new(text).font(font(12.5)).color(p.muted)).wrap());
}

fn mark(ui: &mut egui::Ui, mark: Mark, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
    let c = rect.center();
    match mark {
        Mark::Done => flow::tick(ui.painter(), c, 8.0, Stroke::new(1.6, p.success)),
        Mark::Open => {
            ui.painter().circle_stroke(c, 4.0, Stroke::new(1.3, p.quiet));
        }
        Mark::Warn => {
            ui.painter().circle_filled(c, 5.0, p.warning);
            ui.painter().text(c, Align2::CENTER_CENTER, "!", font(9.5), p.on_accent);
        }
    }
}

/// A line: its mark, its name and what it says.
fn row(ui: &mut egui::Ui, state: Mark, name: &str, detail: &str, p: &Palette) {
    ui.horizontal_wrapped(|ui| {
        mark(ui, state, p);
        ui.label(RichText::new(name).font(font(13.5)).color(p.foreground));
        if !detail.is_empty() {
            ui.label(RichText::new(detail).font(font(12.5)).color(p.muted));
        }
    });
}

/// A program: found where, whether its account answers, or what gets it.
fn program_row(
    ui: &mut egui::Ui,
    program: &SetupProgram,
    setup: &SetupInfo,
    backend: &dyn Backend,
    client: &mut Client,
) {
    let p = client.palette.clone();
    let Some(found) = &program.found else {
        let detail = format!("not found · {}", program.install);
        row(ui, Mark::Open, &program.name, &detail, &p);
        if let Some(click) = program.click.as_ref().filter(|click| simple_click(click)) {
            run(ui, click, setup, backend, client);
        }
        return;
    };
    let providers = backend.available_chiefs();
    let auth = providers
        .as_ref()
        .and_then(|options| options.providers.iter().find(|row| Some(row.agent.name()) == program.agent.as_deref()));
    let detail = if let Some(row) = auth {
        format!("{found} · {}", model::auth(row.auth))
    } else if program.agent.is_some() {
        let resources = backend.resources();
        let usage = resources.as_ref().and_then(|r| match program.agent.as_deref() {
            Some("claude") => Some(&r.claude),
            Some("codex") => Some(&r.codex),
            _ => None,
        });
        let status = if usage.is_some_and(|u| u.observed_at > 0) { "Usage available" } else { "Sign-in unknown" };
        format!("{found} · {status}")
    } else {
        found.clone()
    };
    row(ui, Mark::Done, &program.name, &detail, &p);
}

/// A click that needs no pasted value and no question first: those are in the full setup.
fn simple_click(click: &SetupClick) -> bool {
    click.paste.is_none() && click.confirm.is_none()
}

fn run(ui: &mut egui::Ui, click: &SetupClick, setup: &SetupInfo, backend: &dyn Backend, client: &mut Client) {
    let tool = SettingsTool::RunSetup { action: click.action, value: None };
    let command = Command::Manage(HostAction::SettingsTool { tool });
    if let Some(job) = setup.job.as_ref().filter(|job| job.action == click.action) {
        ui.horizontal(|ui| {
            ui.spinner();
            note(ui, &job.progress, &client.palette.clone());
        });
        return;
    }
    if backend.available(&command) && setup.job.is_none() && pill(ui, &click.label, false, client).clicked() {
        client.send(command);
    }
}

fn check_again(ui: &mut egui::Ui, setup: &SetupInfo, backend: &dyn Backend, client: &mut Client) {
    let command = Command::Manage(HostAction::SettingsTool { tool: SettingsTool::CheckPrograms });
    if !setup.checking && backend.available(&command) {
        let text = RichText::new("Look again").font(font(12.5)).color(client.palette.accent);
        if ui.add(egui::Button::new(text).frame(false)).clicked() {
            client.send(command);
        }
    }
}

/// A link to the full setup; true when clicked.
fn full_setup(ui: &mut egui::Ui, label: &str, client: &mut Client) -> bool {
    let text = RichText::new(label).font(font(12.5)).color(client.palette.accent);
    let response = ui.add(egui::Button::new(text).frame(false));
    flow::record(ui, &format!("setup-link:{label}"), response.rect);
    if response.clicked() {
        client.management.show_section("Getting started");
        client.simple.setup_open = false;
        return true;
    }
    false
}
