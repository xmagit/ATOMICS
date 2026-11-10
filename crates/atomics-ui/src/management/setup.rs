//! The setup screen: what a first run opens instead of a terminal, and what Settings brings back. It is one page, and
//! nothing on it holds up anything else: every step can be left for later, and the Start buttons work from the moment
//! an agent is found. With nothing running it takes the whole window; over running agents it is a window like Settings,
//! so that the keys that move between agents keep working. Also the question a person's start asks when no sandbox
//! can be had for it.
use super::settings::{self, button, isolation, mark, note, tool};
use super::*;
use crate::status::Mark;
use crate::theme::Palette;
use crate::touch;
use atomics_core::isolation::IsolationMode;
use atomics_core::ops::SettingsTool;
use atomics_core::projection::{SetupAction, SetupClick, SetupConnection, SetupInfo};
use egui::{Align, Color32, Layout, vec2};

const WIDTH: f32 = 640.0;
/// The guide to a `[tickets]` profile, which a click cannot write: what it needs differs by ticket system.
const TICKETS_GUIDE: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/main/docs/tickets.md");

/// The screen has the whole window: nothing is on screen for it to cover, and the window is more than a phone's.
pub fn fills_window(backend: &dyn Backend, client: &Client) -> bool {
    client.management.setup && client.touch.is_none() && backend.visible_sessions().is_empty()
}

/// The screen with the whole window to itself.
pub fn page(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(info) = backend.management() else { return };
    let column = |ui: &mut egui::Ui, draw: &mut dyn FnMut(&mut egui::Ui)| {
        let width = ui.available_width().min(WIDTH);
        ui.vertical_centered(|ui| {
            ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| draw(ui));
        });
    };
    let background = client.palette.background;
    egui::Panel::bottom("setup-actions")
        .frame(egui::Frame::new().fill(background).inner_margin(egui::Margin::symmetric(0, 10)))
        .show(ui, |ui| column(ui, &mut |ui| actions(ui, backend, client, &info)));
    egui::CentralPanel::default().frame(egui::Frame::new().fill(background)).show(ui, |ui| {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            column(ui, &mut |ui| {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("ATOMICS").size(28.0).strong().color(client.palette.accent));
                });
                if let Some(text) = backend.notices().toast {
                    ui.add(egui::Label::new(RichText::new(text).color(client.palette.warning)).wrap());
                }
                if let Some(code) = info.tools["credentials"]["github_device"]["code"].as_str() {
                    ui.label(format!("GitHub authorization code: {code}"));
                    if let Some(url) = info.tools["credentials"]["github_device"]["url"].as_str() {
                        ui.hyperlink(url);
                    }
                }
                if let Some(error) = info.tools["credentials"]["error"].as_str() {
                    ui.label(error);
                }
                steps(ui, backend, client, &info);
                ui.add_space(12.0);
            });
        });
    });
}

/// The screen as a window, over what is running.
pub(super) fn window(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    if !client.management.setup || fills_window(backend, client) {
        return;
    }
    const TOP: f32 = 44.0;
    const WINDOW_CHROME: f32 = 52.0;
    /// What stays in view under the steps: Start, and how to leave.
    const ACTIONS: f32 = 84.0;
    let screen = ctx.content_rect();
    let width = (screen.width() - 24.0).clamp(160.0, WIDTH);
    let mut open = true;
    egui::Window::new("Set up ATOMICS")
        .anchor(egui::Align2::CENTER_TOP, vec2(0.0, TOP))
        .open(&mut open)
        .collapsible(false)
        .auto_sized()
        .min_width(width)
        .max_width(width)
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height((screen.height() - TOP - WINDOW_CHROME - ACTIONS).max(120.0))
                .show(ui, |ui| steps(ui, backend, client, info));
            ui.separator();
            actions(ui, backend, client, info);
        });
    client.management.setup &= open;
}

/// Opens the screen on a first run, asks the host to look for the programs as it opens, and tells the host once it has
/// closed, whichever way it did. A client that may not change the host has nothing to do here.
pub(super) fn track(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::FinishSetup });
    let state = &mut client.management;
    if info.setup.first_run && enabled && !state.setup_opened {
        state.setup_opened = true;
        state.setup = true;
        ctx.request_repaint();
    }
    if state.setup {
        if !std::mem::replace(&mut state.setup_loaded, true) && enabled && !info.setup.checking {
            tool(client, SettingsTool::CheckPrograms);
        }
    } else if std::mem::take(&mut state.setup_loaded) && info.setup.first_run && enabled {
        tool(client, SettingsTool::FinishSetup);
    }
}

/// A numbered step's heading, and what it holds under it.
fn step(ui: &mut egui::Ui, p: &Palette, done: bool, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        mark(ui, if done { Mark::Check } else { Mark::Ring }, if done { p.accent } else { p.muted });
        ui.label(RichText::new(title).strong().size(15.0).color(p.foreground));
    });
    ui.add_space(2.0);
    ui.indent(title, body);
}

/// A row's button that puts a command on the clipboard, with its text in plain sight. Where the host knows no command,
/// as on a Linux with a package manager it does not know, its words say what to get, and there is nothing to copy.
fn copy_button(ui: &mut egui::Ui, client: &Client, command: &str) {
    if command.starts_with("install ") {
        note(ui, &client.palette, command);
    } else if button(ui, client, format!("Copy: {command}")).clicked() {
        ui.ctx().copy_text(command.into());
    }
}

fn found_color(client: &Client, found: bool) -> Color32 {
    if found { client.palette.foreground } else { client.palette.warning }
}

/// Every step.
fn steps(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::FinishSetup });
    let setup = &info.setup;
    ui.vertical_centered(|ui| {
        note(ui, &p, "Everything here can wait, and Settings brings this screen back.");
    });
    if let Some(install) = &setup.install {
        step(ui, &p, false, "Install ATOMICS", |ui| {
            note(
                ui,
                &p,
                format!(
                    "This runs from {}. Install it at {} so that it opens from your applications menu and can update \
                     itself. With nothing running, it reopens from there.",
                    install.from, install.to
                ),
            );
            let click = SetupClick::new(SetupAction::InstallSelf, "Install");
            ui.horizontal_wrapped(|ui| run_button(ui, client, enabled, setup, &click));
            failure(ui, client, setup, SetupAction::InstallSelf);
        });
    }
    let usable = setup.programs.iter().any(|program| program.agent.is_some() && program.found.is_some());
    step(ui, &p, usable, "Your agents", |ui| programs(ui, client, enabled, setup));
    step(ui, &p, true, "Where agents run", |ui| where_agents_run(ui, client, enabled, info));
    step(ui, &p, setup.project.is_some(), "A first project", |ui| project(ui, backend, client, setup));
    let connected = setup.connections.iter().any(|row| row.connected.is_some());
    step(ui, &p, connected, "Connections", |ui| {
        for row in &setup.connections {
            connection(ui, backend, client, enabled, setup, row);
        }
    });
}

/// Start, and how to leave, which stay in view however long the steps are.
fn actions(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let setup = &info.setup;
    let usable: Vec<_> =
        setup.programs.iter().filter(|program| program.agent.is_some() && program.found.is_some()).collect();
    let folder = setup.project.as_ref().map(std::path::PathBuf::from);
    let start = |agent: String| Command::Start { agent, folder: folder.clone() };
    let can_start = backend.available(&start(String::new()));
    ui.horizontal_wrapped(|ui| {
        for (index, program) in usable.iter().filter(|_| can_start).enumerate() {
            let label = format!("Start {}", program.name);
            // The one thing this screen is for; another agent found is a plain choice beside it.
            let started = if index == 0 {
                let primary = egui::Button::new(RichText::new(label).color(p.on_accent)).fill(p.accent);
                ui.add(touch::sized(primary, client.touch)).clicked()
            } else {
                button(ui, client, label).clicked()
            };
            if started && let Some(agent) = &program.agent {
                client.send(start(agent.clone()));
                client.management.setup = false;
            }
        }
        if button(ui, client, if setup.first_run { "Skip for now" } else { "Close" }).clicked() {
            client.management.setup = false;
        }
    });
    if can_start && !usable.is_empty() {
        let place = match &setup.project {
            Some(project) => format!("In {project}."),
            None => "In a scratch folder of its own, outside any project.".to_string(),
        };
        note(ui, &p, place);
    }
}

/// git and the agents, with what gets the ones that are missing.
pub(super) fn programs(ui: &mut egui::Ui, client: &mut Client, enabled: bool, setup: &SetupInfo) {
    let p = client.palette.clone();
    if setup.programs.is_empty() {
        note(ui, &p, if enabled { "Looking for git and your agents…" } else { "The host has not looked yet." });
    }
    for program in &setup.programs {
        ui.horizontal_wrapped(|ui| {
            let found = program.found.is_some();
            mark(ui, if found { Mark::Check } else { Mark::Cross }, found_color(client, found));
            ui.label(RichText::new(&program.name).strong());
            match (&program.found, &program.click) {
                (Some(path), _) => note(ui, &p, path.clone()),
                (None, Some(click)) => {
                    note(ui, &p, "not found");
                    run_button(ui, client, enabled, setup, click);
                }
                (None, None) => {
                    note(ui, &p, "not found");
                    copy_button(ui, client, &program.install);
                }
            }
        });
        // What it runs stays in reach once a click has not been enough.
        if let Some(click) = &program.click
            && program.found.is_none()
            && setup.failure.as_ref().is_some_and(|failure| failure.action == click.action)
        {
            failure(ui, client, setup, click.action);
            copy_button(ui, client, &program.install);
        }
    }
    ui.horizontal(|ui| {
        ui.add_enabled_ui(enabled && !setup.checking, |ui| {
            if button(ui, client, "Check again").clicked() {
                tool(client, SettingsTool::CheckPrograms);
            }
        });
        if setup.checking {
            ui.spinner();
        }
    });
}

/// Why `action` did not finish, if it was the last to try.
fn failure(ui: &mut egui::Ui, client: &Client, setup: &SetupInfo, action: SetupAction) {
    if let Some(failure) = setup.failure.as_ref().filter(|failure| failure.action == action) {
        ui.add(egui::Label::new(RichText::new(&failure.why).size(11.5).color(client.palette.warning)).wrap());
    }
}

/// A step's click: its button, what it is doing while it runs with a way to stop it, the field a pasted value goes in,
/// and the question it asks first if it cannot be undone. One runs at a time.
fn run_button(ui: &mut egui::Ui, client: &mut Client, enabled: bool, setup: &SetupInfo, click: &SetupClick) {
    let run = |client: &mut Client, value| tool(client, SettingsTool::RunSetup { action: click.action, value });
    if let Some(job) = setup.job.as_ref().filter(|job| job.action == click.action) {
        ui.spinner();
        note(ui, &client.palette.clone(), job.progress.clone());
        if button(ui, client, "Cancel").clicked() {
            tool(client, SettingsTool::CancelSetup);
        }
        return;
    }
    if let Some(question) = click.confirm.as_ref().filter(|_| client.management.setup_confirm == Some(click.action)) {
        match settings::ask(ui, client, question, &click.label) {
            Some(true) => {
                client.management.setup_confirm = None;
                run(client, None);
            }
            Some(false) => client.management.setup_confirm = None,
            None => {}
        }
        return;
    }
    ui.add_enabled_ui(enabled && setup.job.is_none(), |ui| {
        if let Some(name) = &click.paste {
            let field = client.management.setup_values.entry(click.action).or_default();
            ui.add(egui::TextEdit::singleline(field).password(true).hint_text(name.as_str()).desired_width(220.0));
            let ready = !field.is_empty();
            if ui.add_enabled(ready, touch::sized(egui::Button::new(&click.label), client.touch)).clicked() {
                let value = client.management.setup_values.remove(&click.action);
                run(client, value);
            }
        } else if button(ui, client, &click.label).clicked() {
            if click.confirm.is_some() {
                client.management.setup_confirm = Some(click.action);
            } else {
                run(client, None);
            }
        }
    });
}

/// What sandboxes need on this machine.
pub(super) fn sandbox_steps(ui: &mut egui::Ui, client: &mut Client, enabled: bool, setup: &SetupInfo) {
    let p = client.palette.clone();
    for step in &setup.sandbox_steps {
        ui.horizontal_wrapped(|ui| {
            mark(ui, if step.done { Mark::Check } else { Mark::Ring }, if step.done { p.accent } else { p.muted });
            ui.label(RichText::new(&step.name).strong());
            for click in &step.clicks {
                run_button(ui, client, enabled, setup, click);
            }
            if let Some(command) = &step.command {
                copy_button(ui, client, command);
            }
        });
        if !step.note.is_empty() {
            ui.indent(("step-note", &step.name), |ui| note(ui, &p, step.note.clone()));
        }
        for click in &step.clicks {
            failure(ui, client, setup, click.action);
        }
    }
}

fn where_agents_run(ui: &mut egui::Ui, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let mode = isolation(info);
    ui.add_enabled_ui(enabled, |ui| {
        let here = "This machine, under your own login (recommended)";
        if ui.radio(mode == IsolationMode::Bare.name(), here).clicked() && mode != IsolationMode::Bare.name() {
            tool(client, SettingsTool::SetIsolation { mode: IsolationMode::Bare });
        }
        note(ui, &p, "Agents can do what you can. You review what they did in each one's diff.");
        let sandboxes = "Docker Sandboxes, each agent in a VM of its own";
        if ui.radio(mode == IsolationMode::Sandbox.name(), sandboxes).clicked() && mode != IsolationMode::Sandbox.name()
        {
            tool(client, SettingsTool::SetIsolation { mode: IsolationMode::Sandbox });
        }
        note(ui, &p, "Needs Docker's tools and a Docker account. Settings can set them up, now or later.");
        if mode == IsolationMode::Sandbox.name() && button(ui, client, "Set up sandboxes").clicked() {
            open_sandbox_setup(client);
        }
    });
}

fn project(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, setup: &SetupInfo) {
    let p = client.palette.clone();
    match &setup.project {
        Some(project) => note(ui, &p, format!("Open: {project}")),
        None => note(ui, &p, "Until you pick one, each agent works in a scratch folder of its own."),
    }
    if backend.available(&Command::PickProject)
        && button(ui, client, if setup.project.is_some() { "Choose another folder" } else { "Choose a folder" })
            .clicked()
    {
        client.send(Command::PickProject);
    }
}

fn connection(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    enabled: bool,
    setup: &SetupInfo,
    row: &SetupConnection,
) {
    let p = client.palette.clone();
    let local_accounts = (row.id == "github").then(|| backend.management()).flatten().and_then(|info| {
        let accounts: Vec<_> = info.tools["credentials"]["secrets"]
            .as_array()?
            .iter()
            .filter_map(|row| row["github_account"].as_str())
            .map(str::to_string)
            .collect();
        (!accounts.is_empty()).then(|| accounts.join(", "))
    });
    let connected_as = row.connected.as_ref().or(local_accounts.as_ref());
    ui.horizontal_wrapped(|ui| {
        let connected = connected_as.is_some();
        mark(ui, if connected { Mark::Check } else { Mark::Ring }, if connected { p.accent } else { p.muted });
        ui.label(RichText::new(&row.name).strong());
        if let Some(who) = connected_as {
            note(ui, &p, who.clone());
        } else if setup.later.contains(&row.id) {
            note(ui, &p, "Later");
            if ui.add_enabled(enabled, touch::sized(egui::Button::new("Connect now"), client.touch)).clicked() {
                tool(client, SettingsTool::ConnectLater { id: row.id.clone(), later: false });
            }
        } else {
            connect_button(ui, backend, client, enabled, setup, row);
            if ui.add_enabled(enabled, touch::sized(egui::Button::new("Connect later"), client.touch)).clicked() {
                tool(client, SettingsTool::ConnectLater { id: row.id.clone(), later: true });
            }
        }
    });
    if connected_as.is_none() && !setup.later.contains(&row.id) {
        note(ui, &p, row.note.clone());
        if let Some(click) = &row.click {
            failure(ui, client, setup, click.action);
        }
    }
}

fn connect_button(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    enabled: bool,
    setup: &SetupInfo,
    row: &SetupConnection,
) {
    let p = client.palette.clone();
    match row.id.as_str() {
        "github" if backend.available(&Command::ConnectGithub) => {
            if button(ui, client, "Connect").clicked() {
                client.send(Command::ConnectGithub);
            }
        }
        "github" => note(ui, &p, "Connect it in the host's own window."),
        "tickets" if backend.available(&Command::SignInTickets) && backend.tickets_panel().is_some() => {
            if button(ui, client, "Sign in").clicked() {
                client.send(Command::SignInTickets);
            }
        }
        "tickets" => {
            if button(ui, client, "Read how").clicked()
                && let Some(url) = crate::markdown::web_url(TICKETS_GUIDE)
            {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
        "remote" => {
            if button(ui, client, "Connect").clicked() {
                client.management.remote = true;
            }
        }
        _ => {
            if let Some(click) = &row.click {
                run_button(ui, client, enabled, setup, click);
                return;
            }
            if let Some(command) = &row.command {
                copy_button(ui, client, command);
            }
            if ui.add_enabled(enabled, egui::Button::new("Check again")).clicked() {
                tool(client, SettingsTool::CheckPrograms);
            }
        }
    }
}

/// Settings, with the section that holds what sandboxes need open.
fn open_sandbox_setup(client: &mut Client) {
    client.management.setup = false;
    client.management.settings = true;
    client.management.open_section = Some(settings::AGENTS.to_string());
}

/// The question a person's start asks when no sandbox can be had for it.
pub(super) fn start_choice(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let Some(choice) = &info.setup.start_choice else { return };
    let p = client.palette.clone();
    let answer = |run_here| HostAction::SettingsTool { tool: SettingsTool::AnswerStart { run_here } };
    let enabled = allowed(backend, &answer(true));
    let width = (ctx.content_rect().width() - 24.0).clamp(160.0, 520.0);
    let shown = egui::Window::new("Sandboxes are not ready")
        .id(egui::Id::NULL.with("start-choice"))
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .collapsible(false)
        .resizable(false)
        .default_width(width)
        .show(ctx, |ui| {
            ui.add(
                egui::Label::new(if choice.sandbox_required {
                    format!(
                        "{} works in a repository that requires sandboxed agents, and sandboxes cannot run here yet.",
                        choice.agent
                    )
                } else {
                    format!("{} is set to run in a sandbox, and sandboxes cannot run here yet.", choice.agent)
                })
                .wrap(),
            );
            note(ui, &p, choice.why.clone());
            ui.add_space(6.0);
            ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if !choice.sandbox_required && button(ui, client, "Run on this machine").clicked() {
                        client.send(Command::Manage(answer(true)));
                    }
                    if button(ui, client, "Set up sandboxes").clicked() {
                        client.send(Command::Manage(answer(false)));
                        open_sandbox_setup(client);
                    }
                    if button(ui, client, "Cancel").clicked() {
                        client.send(Command::Manage(answer(false)));
                    }
                });
            });
            if !choice.sandbox_required {
                note(ui, &p, "This machine then runs agents under your own login. Agents, in Settings, changes that.");
            }
        });
    if let Some(shown) = shown {
        ctx.move_to_top(shown.response.layer_id);
    }
}
