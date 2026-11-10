//! The Settings window. What people change comes first, in plain sections that read what they show as they open;
//! what can be undone acts at once, and only what cannot asks first. The raw configuration is under Advanced.
use super::*;
use crate::keys::shortcut_label;
use crate::status::{Mark, paint_mark};
use crate::theme::{Palette, lerp, parse_hex};
use crate::touch;
use atomics_core::ops::SettingsTool;
use atomics_core::projection::{ThemeChoice, UpdateInfo, UpdatePhase};
use egui::text::LayoutJob;
use egui::{Align, Color32, FontSelection, Rect, Sense, Stroke, Vec2};

/// What Settings asks about first, because it cannot be undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Confirm {
    RemoveCredential {
        kind: String,
        host: Option<String>,
        label: String,
    },
    /// The new value waits in its field.
    ReplaceCredential {
        kind: String,
        host: Option<String>,
        label: String,
    },
    CleanCredentials,
}

/// The section that says where agents run and what sandboxes need.
pub(super) const AGENTS: &str = "Agents";

/// A mark in the flow of a row, as tall as a line of text.
pub(super) fn mark(ui: &mut egui::Ui, mark: Mark, color: Color32) {
    let height = ui.text_style_height(&egui::TextStyle::Body);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(12.0, height), Sense::hover());
    paint_mark(ui.painter(), rect.center(), mark, color);
}

/// A line of a check the host ran, with the mark it starts with painted.
fn check_line(ui: &mut egui::Ui, p: &Palette, line: &str) {
    let (status, text, color) = if let Some(text) = line.strip_prefix("✓ ") {
        (Some(Mark::Check), text, p.foreground)
    } else if let Some(text) = line.strip_prefix("✗ ") {
        (Some(Mark::Cross), text, p.warning)
    } else {
        (None, line, p.foreground)
    };
    ui.horizontal_top(|ui| {
        if let Some(status) = status {
            mark(ui, status, color);
        }
        ui.vertical(|ui| ui.add(egui::Label::new(RichText::new(text).color(color)).wrap()));
    });
}

/// The `[keys]` bindings in the words the shortcut list uses.
const SHORTCUTS: [(&str, &str); 16] = [
    ("keys.quick_open", "open a file"),
    ("keys.toggle_diff", "show or hide the diff"),
    ("keys.toggle_plan", "show or hide the plan board"),
    ("keys.toggle_tickets", "show or hide tickets"),
    ("keys.toggle_editor", "show or hide the editor"),
    ("keys.toggle_chief", "show or hide the chief"),
    ("keys.next_attention", "next agent that needs you"),
    ("keys.focus_steer", "steer bar"),
    ("keys.new_session", "new session"),
    ("keys.close_session", "close the session"),
    ("keys.refresh_diff", "read the diff again"),
    ("keys.plan_with_chief", "plan a ticket with the chief"),
    ("keys.voice", "voice: record, send (Esc cancels)"),
    ("keys.github", "GitHub controls"),
    ("keys.back", "back to where you were"),
    ("keys.forward", "forward again"),
];

pub(super) fn draw(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let forced = client.management.open_section.take();
    let forced = forced.as_deref();
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::CheckSandbox });
    if !enabled {
        ui.colored_label(p.warning, "View only: settings changes require the host grant.");
    }
    if let Some(problem) = info.config["load_error"].as_str() {
        ui.colored_label(p.warning, problem);
    }
    // A window on the host says nothing while nothing is new; a client always shows which version the host runs.
    // One id either way, a scope's or a skipped one, so that the section appearing leaves a field below it the keyboard.
    if let Some(update) = backend.update().filter(|update| update.has_news() || !backend.available(&Command::Update)) {
        let summary = update_summary(&update);
        ui.scope(|ui| {
            section(ui, &p, forced, "Updates", &summary, update.has_news(), |ui| updates(ui, backend, client, &update));
        });
    } else {
        ui.skip_ahead_auto_ids(1);
    }
    let selected = info.themes.choices.iter().find(|choice| choice.selected).map_or("", |choice| &choice.name);
    section(ui, &p, forced, "Theme", selected, true, |ui| theme(ui, backend, client, info));
    section(ui, &p, forced, "Shortcuts", "", false, |ui| shortcuts(ui, backend, client, info));
    section(ui, &p, forced, "Identity", "", false, |ui| identity(ui, backend, client, enabled, info));
    section(ui, &p, forced, AGENTS, &agents_summary(info), false, |ui| agents(ui, client, enabled, info));
    section(ui, &p, forced, "Getting started", "", false, |ui| getting_started(ui, client));
    section(ui, &p, forced, "Connections", &connections_summary(info), false, |ui| {
        connections(ui, backend, client, enabled, info)
    });
    section(ui, &p, forced, "Remote devices", &remote_summary(info), false, |ui| {
        remote::draw(ui, backend, client, info)
    });
    section(ui, &p, forced, "Credentials", &credentials_summary(info), false, |ui| {
        credentials(ui, client, enabled, info)
    });
    section(ui, &p, forced, "Advanced", "", false, |ui| advanced(ui, backend, client, enabled, info));
    // The VM panel can be inserted as another Settings section without changing the shell.
}

/// A section's row, which opens it whole, and what shows once it is open. The summary says what is in it meanwhile.
fn section(
    ui: &mut egui::Ui,
    p: &Palette,
    forced: Option<&str>,
    title: &str,
    summary: &str,
    open: bool,
    body: impl FnOnce(&mut egui::Ui),
) {
    let mut label = LayoutJob::default();
    let style = ui.style().clone();
    let add = |label: &mut LayoutJob, text: RichText| {
        text.append_to(label, &style, FontSelection::Default, Align::Center);
    };
    add(&mut label, RichText::new(title).strong().size(14.0).color(p.foreground));
    if !summary.is_empty() {
        add(&mut label, RichText::new(format!("  ·  {summary}")).size(12.0).color(p.muted));
    }
    let header = egui::CollapsingHeader::new(label).id_salt(title).default_open(open);
    let header = if forced == Some(title) { header.open(Some(true)) } else { header };
    header.show(ui, |ui| {
        body(ui);
        ui.add_space(6.0);
    });
}

/// What a section's row and text say beside its controls.
pub(super) fn note(ui: &mut egui::Ui, p: &Palette, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text).size(11.0).color(p.muted)).wrap());
}

fn subheading(ui: &mut egui::Ui, p: &Palette, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).strong().color(p.foreground));
}

fn identity(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    if let Some(error) = info.tools["identity_error"].as_str() {
        ui.colored_label(p.warning, error);
    }
    let rows = info.tools["identities"].as_array().map_or(&[][..], Vec::as_slice);
    for row in rows {
        ui.horizontal_wrapped(|ui| {
            ui.label(row["display"].as_str().unwrap_or_default());
            ui.label(RichText::new(row["label"].as_str().unwrap_or("Git identity")).strong());
            if ui.add_enabled(enabled, egui::Button::new("Change…")).clicked() {
                if let Ok(identity) = serde_json::from_value(row["identity"].clone()) {
                    client.management.show_identity(row["project"].as_str().unwrap_or_default().into(), identity);
                }
            }
        });
    }
    note(
        ui,
        &p,
        "Project overrides workspace, then main config, then Git. Switching workspace changes its identity and updates ATOMICS worktrees.",
    );
    let workspace = backend.project_menu().current;
    ui.add_enabled_ui(enabled, |ui| {
        let state = &mut client.management;
        if state.identity_target.is_empty() {
            state.identity_target = if workspace.is_some() { "workspace" } else { "main" }.into();
        }
        let selected = match state.identity_target.as_str() {
            "main" => "Main config".to_string(),
            "workspace" => format!("Workspace: {}", workspace.as_deref().unwrap_or("none")),
            project => format!("Project: {project}"),
        };
        egui::ComboBox::from_label("Identity for").selected_text(selected).show_ui(ui, |ui| {
            ui.selectable_value(&mut state.identity_target, "main".into(), "Main config");
            if let Some(workspace) = &workspace {
                ui.selectable_value(&mut state.identity_target, "workspace".into(), format!("Workspace: {workspace}"));
            }
            for row in rows {
                let path = row["project"].as_str().unwrap_or_default();
                ui.selectable_value(
                    &mut state.identity_target,
                    path.into(),
                    format!("Project: {}", row["display"].as_str().unwrap_or(path)),
                );
            }
        });
        ui.add(
            crate::controls::single_line(&mut state.identity_name)
                .hint_text("Commit name")
                .desired_width(ui.available_width()),
        );
        ui.add(
            crate::controls::single_line(&mut state.identity_email)
                .hint_text("Commit email (you@example.com)")
                .desired_width(ui.available_width()),
        );
        ui.add(
            crate::controls::single_line(&mut state.identity_words)
                .hint_text("Forbidden words, separated by commas")
                .desired_width(ui.available_width()),
        );
        let ready = !state.identity_name.trim().is_empty()
            && !state.identity_email.trim().is_empty()
            && (state.identity_target != "workspace" || workspace.is_some());
        if ui.add_enabled(ready, egui::Button::new("Review identity change")).clicked() {
            let target = &state.identity_target;
            let scope = if target == "workspace" { Scope::Workspace } else { Scope::Main };
            let header = if matches!(target.as_str(), "main" | "workspace") {
                "identity".to_string()
            } else {
                atomics_core::selfconfig::key(&["identity".into(), "projects".into(), target.clone()])
            };
            let words: Vec<_> =
                state.identity_words.split(',').map(str::trim).filter(|word| !word.is_empty()).collect();
            let quote = |value: &str| serde_json::to_string(value).unwrap_or_default();
            let toml = format!(
                "[{header}]\nname = {}\nemail = {}\nforbidden_words = {}\n",
                quote(state.identity_name.trim()),
                quote(state.identity_email.trim()),
                serde_json::to_string(&words).unwrap_or_default()
            );
            client.send(Command::Manage(HostAction::ConfigPropose {
                scope,
                toml,
                remove: Vec::new(),
                theme: None,
                reason: Some("Switch commit identity".into()),
            }));
        }
    });
    note(
        ui,
        &p,
        "Review and Apply saves the change in your config. Repositories cannot set an identity. The guard blocks integrate on forbidden words or a different author email and reports file and line only.",
    );
}

pub(super) fn button(ui: &mut egui::Ui, client: &Client, text: impl Into<String>) -> egui::Response {
    ui.add(touch::sized(egui::Button::new(text.into()), client.touch))
}

/// Asks about what cannot be undone: `Some` once answered.
pub(super) fn ask(ui: &mut egui::Ui, client: &Client, question: &str, yes: &str) -> Option<bool> {
    ui.add(egui::Label::new(RichText::new(question).color(client.palette.warning)).wrap());
    let mut answer = None;
    ui.horizontal_wrapped(|ui| {
        if button(ui, client, yes).clicked() {
            answer = Some(true);
        }
        if button(ui, client, "Keep").clicked() {
            answer = Some(false);
        }
    });
    answer
}

pub(super) fn tool(client: &mut Client, tool: SettingsTool) {
    client.send(Command::Manage(HostAction::SettingsTool { tool }));
}

/// Asks the host to read what a section shows, once each time Settings opens: a section shows its content as it
/// opens, and a Refresh only reads it again. A client that may not ask sees what the host read last.
fn load(client: &mut Client, enabled: bool, busy: bool, asked: SettingsTool) {
    if !enabled || client.management.loaded.contains(&asked) {
        return;
    }
    client.management.loaded.push(asked.clone());
    if !busy {
        tool(client, asked);
    }
}

// ── Updates ──────────────────────────────────────────────────────────────────────────────────────────

fn update_summary(update: &UpdateInfo) -> String {
    match (&update.phase, &update.latest) {
        (UpdatePhase::Working { step }, _) => step.clone(),
        (UpdatePhase::WaitingForAgents { .. }, _) => "restarts when agents finish".into(),
        (UpdatePhase::RestartsIn { seconds }, _) => format!("restarting in {seconds} s"),
        (UpdatePhase::Failed { .. }, _) => "update failed".into(),
        (UpdatePhase::Installed { version }, _) => format!("{version} installed"),
        (_, Some(latest)) => format!("{latest} available"),
        (_, None) if update.check_error.is_some() => "the last check failed".into(),
        (_, None) => format!("host runs {}", update.current),
    }
}

fn updates(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, update: &UpdateInfo) {
    let p = client.palette.clone();
    let on_host = backend.available(&Command::Update);
    let name = if on_host { "This version" } else { "The host runs" };
    ui.label(format!("{name}: {}", update.current));
    if let Some(latest) = &update.latest {
        ui.label(format!("New version: {latest}"));
    }
    if let Some(detail) = &update.detail {
        note(ui, &p, detail.clone());
    }
    if let Some(url) = &update.notes {
        ui.hyperlink_to("Release notes", url);
    }
    match &update.phase {
        UpdatePhase::Working { step } => note(ui, &p, step.clone()),
        UpdatePhase::WaitingForAgents { blockers } => {
            ui.label("Restarts when agents finish.");
            for blocker in blockers {
                note(ui, &p, blocker.clone());
            }
            if backend.available(&Command::CancelUpdate) && button(ui, client, "Cancel").clicked() {
                client.send(Command::CancelUpdate);
            }
        }
        UpdatePhase::RestartsIn { seconds } => {
            ui.label(format!("Restarting in {seconds} s."));
            if backend.available(&Command::CancelUpdate) && button(ui, client, "Cancel").clicked() {
                client.send(Command::CancelUpdate);
            }
        }
        UpdatePhase::Installed { version } => {
            note(ui, &p, format!("{version} is installed. The next start runs it."));
            if on_host && button(ui, client, "Restart to update").clicked() {
                client.send(Command::Update);
            }
        }
        UpdatePhase::Failed { .. } | UpdatePhase::Idle | UpdatePhase::Unknown => {
            if let UpdatePhase::Failed { error } = &update.phase {
                ui.add(egui::Label::new(RichText::new(error).color(p.warning)).wrap());
            }
            if update.latest.is_some() {
                if crate::update::can_update(backend, update) {
                    if button(ui, client, "Update").clicked() {
                        client.send(Command::Update);
                    }
                } else {
                    note(ui, &p, crate::update::why_not(backend, update));
                }
            }
        }
    }
    if let Some(error) = &update.check_error {
        note(ui, &p, format!("The last check failed: {error}"));
    }
}

// ── Theme ────────────────────────────────────────────────────────────────────────────────────────────

fn theme(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let themes = &info.themes;
    if themes.choices.is_empty() {
        note(ui, &p, "This host does not offer a theme picker.");
        return;
    }
    let enabled = allowed(backend, &HostAction::SelectTheme { name: String::new(), host: false });
    let mut host_only = client.management.theme_host_only.unwrap_or(themes.host_picks);
    let height = client.touch.map_or(24.0, |touch| touch.target);
    for choice in &themes.choices {
        if theme_row(ui, &p, choice, enabled, height).clicked() && !choice.selected {
            client.send(Command::Manage(HostAction::SelectTheme { name: choice.name.clone(), host: host_only }));
        }
    }
    // A window on the host says this machine; from anywhere else, that would be the wrong one.
    let on_host = backend.available(&Command::EditConfig);
    let only = if on_host { "This machine only" } else { "Only this host" };
    let label = format!("{only} ({})", themes.host_file);
    ui.scope(|ui| {
        // An outline that stays visible on the window, where the checkbox would vanish.
        let inactive = &mut ui.visuals_mut().widgets.inactive;
        inactive.bg_fill = p.background;
        inactive.bg_stroke = Stroke::new(1.0, lerp(p.border, p.foreground, 0.3));
        if ui.add_enabled(enabled, egui::Checkbox::new(&mut host_only, label)).changed() {
            client.management.theme_host_only = Some(host_only);
        }
    });
    if !host_only && themes.host_picks {
        ui.colored_label(
            p.warning,
            format!("{} picks this machine's theme and wins over config.toml", themes.host_file),
        );
    }
    if !themes.inline.is_empty() {
        note(ui, &p, format!("Set in [theme], so shown over any theme: {}", themes.inline.join(", ")));
    }
    if let Some(error) = &themes.error {
        ui.colored_label(p.warning, format!("theme: {error}"));
    }
}

/// One theme: the check mark on the one in use, the strip of its colors, its name, and whether it is the user's file.
fn theme_row(ui: &mut egui::Ui, p: &Palette, choice: &ThemeChoice, enabled: bool, height: f32) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(420.0), height), sense);
    response
        .widget_info(|| egui::WidgetInfo::selected(egui::Role::RadioButton, enabled, choice.selected, &choice.name));
    let painter = ui.painter_at(rect);
    if choice.selected {
        painter.rect_filled(rect, 2.0, p.selection);
    } else if response.hovered() && enabled {
        painter.rect_filled(rect, 2.0, p.hover());
    }
    // An outline that stays visible where a dark swatch would vanish into the window.
    let edge = lerp(p.border, p.foreground, 0.3);
    if response.has_focus() {
        painter.rect_stroke(rect, 2.0, Stroke::new(1.0, p.accent), egui::StrokeKind::Inside);
    }
    let y = rect.center().y;
    if choice.selected {
        paint_mark(&painter, egui::pos2(rect.left() + 10.0, y), Mark::Check, p.accent);
    }
    let strip = Rect::from_min_size(egui::pos2(rect.left() + 22.0, y - 6.0), Vec2::new(12.0 * 7.0, 12.0));
    for (index, color) in choice.swatches.iter().enumerate() {
        let cell = Rect::from_min_size(strip.min + Vec2::new(12.0 * index as f32, 0.0), Vec2::splat(12.0));
        painter.rect_filled(cell, 0.0, parse_hex(color).unwrap_or(Color32::TRANSPARENT));
    }
    painter.rect_stroke(strip, 1.0, Stroke::new(1.0, edge), egui::StrokeKind::Outside);
    let name = if choice.selected { p.accent } else { p.foreground };
    painter.text(egui::pos2(strip.right() + 10.0, y), egui::Align2::LEFT_CENTER, &choice.name, p.ui(12.0), name);
    if choice.file {
        painter.text(
            rect.right_center() - Vec2::new(6.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            "yours",
            p.ui(11.0),
            p.quiet,
        );
    }
    let hover = if choice.file { format!("themes/{}.toml", choice.name) } else { "Built in".to_string() };
    response.on_hover_text(hover)
}

// ── Shortcuts ────────────────────────────────────────────────────────────────────────────────────────

fn shortcuts(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let values = &info.config["values"];
    let everywhere = [
        (crate::window_keys::agents_label(ui.ctx()), "that agent, with its diff"),
        ("Shift+Up/Down".to_string(), "previous or next changed file"),
        ("PageUp/PageDown".to_string(), "previous or next hunk, while the diff has the keyboard"),
        ("Esc".to_string(), "close the window on top"),
    ];
    let configured: Vec<_> = SHORTCUTS
        .iter()
        .filter_map(|(key, what)| Some((shortcut_label(ui.ctx(), values[*key]["value"].as_str()?), *what)))
        .collect();
    let width = everywhere.iter().chain(&configured).map(|(chord, _)| chord.chars().count()).max().unwrap_or(0);
    let list = |ui: &mut egui::Ui, lines: &[(String, &str)]| {
        for (chord, what) in lines {
            ui.label(RichText::new(format!("{chord:<width$}  {what}")).monospace().size(12.0));
        }
    };
    list(ui, &everywhere);
    if !backend.has_shortcuts() {
        note(
            ui,
            &p,
            "The desktop app on the host also has these. This client leaves them to the terminal, or to the browser:",
        );
    }
    list(ui, &configured);
    note(ui, &p, "Change the configured ones under [keys] in the config: see Advanced.");
}

// ── Agents ───────────────────────────────────────────────────────────────────────────────────────────

/// The configured agents, as their masked `[[agents]]` entries.
fn configured_agents(info: &ManagementInfo) -> &[serde_json::Value] {
    info.config["values"]["agents"]["value"].as_array().map_or(&[], Vec::as_slice)
}

pub(super) fn isolation(info: &ManagementInfo) -> &str {
    info.config["values"]["isolation.mode"]["value"].as_str().unwrap_or("")
}

fn agents_summary(info: &ManagementInfo) -> String {
    let names: Vec<_> = configured_agents(info).iter().filter_map(|agent| agent["name"].as_str()).collect();
    [names.join(", "), isolation(info).to_string()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

fn agents(ui: &mut egui::Ui, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let tools = &info.tools;
    let busy = tools["sandbox_busy"] == true;
    load(client, enabled, busy, SettingsTool::CheckSandbox);
    load(client, enabled, info.setup.checking, SettingsTool::CheckPrograms);
    subheading(ui, &p, "Where agents run");
    ui.add_enabled_ui(enabled, |ui| {
        for (mode, text) in [
            (atomics_core::isolation::IsolationMode::Bare, "This machine, under your own login"),
            (atomics_core::isolation::IsolationMode::Sandbox, "Docker Sandboxes, each agent in a VM of its own"),
        ] {
            if ui.radio(isolation(info) == mode.name(), text).clicked() && isolation(info) != mode.name() {
                tool(client, SettingsTool::SetIsolation { mode });
            }
        }
    });
    subheading(ui, &p, "Agents");
    for agent in configured_agents(info) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(agent["name"].as_str().unwrap_or_default()).strong());
            let kind = agent["kind"].as_str().unwrap_or_default();
            let runs = agent["isolation"].as_str().map_or(String::new(), |mode| format!(" · runs {mode}"));
            ui.label(RichText::new(format!("{kind}{runs}")).size(11.0).color(p.muted));
        });
        if let Some(command) = agent["command"].as_array() {
            let words: Vec<_> = command.iter().filter_map(|word| word.as_str()).collect();
            if !words.is_empty() {
                ui.add(egui::Label::new(RichText::new(words.join(" ")).monospace().size(11.0).color(p.muted)).wrap());
            }
        }
    }
    note(ui, &p, "Agents are the [[agents]] entries in the config: see Advanced.");
    subheading(ui, &p, "What they need");
    super::setup::programs(ui, client, enabled, &info.setup);
    subheading(ui, &p, "Docker Sandboxes");
    if isolation(info) == atomics_core::isolation::IsolationMode::Bare.name() {
        note(ui, &p, "Agents run on this machine, so none of this is needed yet. Get it ready, then switch above.");
    }
    let lines = tools["sandbox"].as_array().map_or(&[][..], Vec::as_slice);
    if busy || (lines.is_empty() && info.setup.sandbox_steps.is_empty()) {
        note(ui, &p, if enabled { "Checking Docker Sandboxes…" } else { "The host has not checked them yet." });
    }
    super::setup::sandbox_steps(ui, client, enabled, &info.setup);
    ui.add_enabled_ui(enabled && !busy, |ui| {
        if button(ui, client, "Check again").clicked() {
            tool(client, SettingsTool::CheckSandbox);
        }
    });
    // A host that does not list steps has only these; on one that does, they are the detail behind them.
    let details = |ui: &mut egui::Ui| {
        for line in lines.iter().filter_map(|line| line.as_str()) {
            check_line(ui, &p, line);
        }
    };
    if info.setup.sandbox_steps.is_empty() {
        details(ui);
    } else if !lines.is_empty() {
        egui::CollapsingHeader::new("Every check").id_salt("sandbox-checks").show(ui, details);
    }
    subheading(ui, &p, "Claude sessions outside ATOMICS");
    if tools["hooks"] == true {
        ui.label("Hooks for all Claude sessions are installed");
    } else {
        ui.add_enabled_ui(enabled, |ui| {
            let install = button(ui, client, "Install hooks for all Claude sessions").on_hover_text(
                "Adds `atomics hook` to ~/.claude/settings.json, so agents in any terminal show up here and can be steered",
            );
            if install.clicked() {
                tool(client, SettingsTool::InstallHooks);
            }
        });
    }
}

fn getting_started(ui: &mut egui::Ui, client: &mut Client) {
    let p = client.palette.clone();
    note(ui, &p, "Agents, where they run, a first project and connections, on one screen.");
    if button(ui, client, "Open the setup screen").clicked() {
        client.management.settings = false;
        client.management.setup = true;
    }
}

// ── Connections ──────────────────────────────────────────────────────────────────────────────────────

fn connections_summary(info: &ManagementInfo) -> String {
    let Some(rows) = info.tools["connections"]["rows"].as_array() else { return String::new() };
    let connected = rows.iter().filter(|row| row["status"] == "connected").count();
    match rows.len() {
        0 => String::new(),
        total => format!("{connected} of {total} connected"),
    }
}

fn connections(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let connections = &info.tools["connections"];
    let refreshing = connections["refreshing"] == true;
    load(client, enabled, refreshing, SettingsTool::RefreshConnections);
    note(
        ui,
        &p,
        "MCP servers for sandboxed agents. Every connected server loads into every sandboxed session. They run with \
         your permissions: remote servers through sbx's gateway, command servers on the host.",
    );
    if let Some(error) = connections["error"].as_str() {
        ui.colored_label(p.warning, error);
    }
    let rows = connections["rows"].as_array().map_or(&[][..], Vec::as_slice);
    if refreshing {
        note(ui, &p, "Reading sbx's MCP servers…");
    } else if rows.is_empty() && connections["error"].is_null() {
        note(
            ui,
            &p,
            if enabled {
                "No servers yet: import them from Claude Code and Codex, or add one by URL."
            } else {
                "The host has not read its servers yet."
            },
        );
    }
    for row in rows {
        connection(ui, backend, client, enabled, row);
    }
    for line in connections["notes"].as_array().into_iter().flatten().filter_map(|line| line.as_str()) {
        ui.colored_label(p.warning, line);
    }
    ui.add_enabled_ui(enabled, |ui| {
        ui.horizontal_wrapped(|ui| {
            let importing = connections["importing"] == true;
            let label = if importing { "Importing…" } else { "Import from Claude Code / Codex" };
            if ui.add_enabled(!importing, touch::sized(egui::Button::new(label), client.touch)).clicked() {
                tool(client, SettingsTool::ImportConnections);
            }
            if ui.add_enabled(!refreshing, touch::sized(egui::Button::new("Refresh"), client.touch)).clicked() {
                tool(client, SettingsTool::RefreshConnections);
            }
        });
        note(ui, &p, "Import reads server definitions only, never sign-in tokens; environment values stay hidden.");
        subheading(ui, &p, "Add a remote server");
        let state = &mut client.management;
        ui.add(
            crate::controls::single_line(&mut state.connection_name)
                .hint_text("Server name")
                .desired_width(ui.available_width()),
        );
        ui.add(
            crate::controls::single_line(&mut state.connection_url)
                .hint_text("https://mcp.example.com/mcp")
                .desired_width(ui.available_width()),
        );
        let ready = !state.connection_name.trim().is_empty() && !state.connection_url.trim().is_empty();
        if ui.add_enabled(ready, touch::sized(egui::Button::new("Add and connect"), client.touch)).clicked() {
            let name = std::mem::take(&mut client.management.connection_name);
            let url = std::mem::take(&mut client.management.connection_url);
            tool(client, SettingsTool::AddConnection { name, url });
        }
    });
}

/// One MCP server: its state, what connecting or leaving it takes, and why it last failed.
fn connection(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, row: &serde_json::Value) {
    let p = client.palette.clone();
    let name = row["name"].as_str().unwrap_or_default();
    let status = row["status"].as_str();
    let busy = row["busy"] == true;
    let working = if backend.available(&Command::EditConfig) {
        "working… finish any sign-in in your browser"
    } else {
        "working… finish any sign-in in a browser on the host"
    };
    let (dot, color, label) = match status {
        _ if busy => (Mark::Ring, p.info, working),
        Some("connected") => (Mark::Dot, p.success, "connected"),
        Some("needs sign-in") => (Mark::Dot, p.warning, "needs sign-in"),
        Some(other) => (Mark::Dot, p.error, other),
        None => (Mark::Ring, p.muted, "not connected"),
    };
    ui.horizontal_wrapped(|ui| {
        mark(ui, dot, color);
        ui.label(RichText::new(name).strong());
        ui.label(RichText::new(label).color(color));
    });
    if let Some(detail) = row["detail"].as_str() {
        note(ui, &p, detail);
    }
    let env = row["env"].as_u64().unwrap_or(0);
    if env > 0 {
        note(ui, &p, format!("{env} environment values, hidden: stored in the OS keychain only if you choose"));
    }
    let skipped = row["skipped"].as_u64().unwrap_or(0);
    if skipped > 0 {
        note(
            ui,
            &p,
            format!("{skipped} header or token settings not imported: they hold secrets; set them up with sbx"),
        );
    }
    if let Some(error) = row["error"].as_str() {
        ui.add(egui::Label::new(RichText::new(error).size(11.0).color(p.error)).wrap());
    }
    if !busy {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                let name = name.to_string();
                let connect = match status {
                    None if env > 0 => {
                        if button(ui, client, format!("Connect and store {env} env values")).clicked() {
                            tool(client, SettingsTool::Connect { name: name.clone(), store_env: true });
                        }
                        "Connect without them"
                    }
                    None | Some("needs sign-in") => "Connect",
                    _ => "",
                };
                if !connect.is_empty() && button(ui, client, connect).clicked() {
                    tool(client, SettingsTool::Connect { name: name.clone(), store_env: false });
                }
                if status.is_some() && button(ui, client, "Disconnect").clicked() {
                    tool(client, SettingsTool::Disconnect { name });
                }
            });
        });
    }
    ui.add_space(4.0);
}

// ── Remote devices ───────────────────────────────────────────────────────────────────────────────────

fn remote_summary(info: &ManagementInfo) -> String {
    let remote = &info.remote;
    match remote.devices.len() {
        0 => remote.status.clone(),
        1 => format!("1 device · {}", remote.status),
        count => format!("{count} devices · {}", remote.status),
    }
}

// ── Credentials ──────────────────────────────────────────────────────────────────────────────────────

fn credentials_summary(info: &ManagementInfo) -> String {
    let Some(entries) = info.tools["credentials"]["entries"].as_array() else { return String::new() };
    let set = entries.iter().filter(|entry| entry["present"] == true).count();
    format!("{set} of {} set", entries.len())
}

fn credentials(ui: &mut egui::Ui, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let status = &info.tools["credentials"];
    let busy = info.tools["credential_busy"] == true;
    load(client, enabled, busy, SettingsTool::RefreshCredentials);
    note(
        ui,
        &p,
        "Stored values are never displayed. Replacing or removing one takes it from the sessions using it: restart them.",
    );
    if let Some(error) = status["error"].as_str() {
        ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap());
    }
    if busy {
        note(ui, &p, "Waiting for the OS credential store…");
    } else if status.is_null() && !enabled {
        note(ui, &p, "The host has not read its credential store yet.");
    }
    let entries = status["entries"].as_array().map_or(&[][..], Vec::as_slice);
    for entry in entries {
        credential(ui, client, enabled && !busy, entry);
    }
    let invalid = status["invalid"].as_u64().unwrap_or(0);
    if invalid > 0 {
        ui.colored_label(p.warning, format!("{invalid} unknown or misfiled entries (names and values hidden)"));
        if client.management.confirm == Some(Confirm::CleanCredentials) {
            match ask(ui, client, "Remove them all? This cannot be undone.", "Remove them") {
                Some(true) => {
                    client.management.confirm = None;
                    tool(client, SettingsTool::CleanCredentials);
                }
                Some(false) => client.management.confirm = None,
                None => {}
            }
        } else {
            ui.add_enabled_ui(enabled && !busy, |ui| {
                if button(ui, client, "Remove unknown entries…").clicked() {
                    client.management.confirm = Some(Confirm::CleanCredentials);
                }
            });
        }
    }
    ui.add_enabled_ui(enabled && !busy, |ui| {
        credential_form(ui, client, info);
        if ui.add(touch::sized(egui::Button::new("Refresh"), client.touch)).clicked() {
            tool(client, SettingsTool::RefreshCredentials);
        }
    });
    subheading(ui, &p, "Azure CLI");
    let values = &info.config["values"];
    let mut azure = values["isolation.sandbox.azure.enabled"]["value"].as_bool().unwrap_or(false);
    if ui.add_enabled(enabled, egui::Checkbox::new(&mut azure, "Enable Azure CLI access")).changed() {
        tool(client, SettingsTool::SetAzure { enabled: azure });
    }
    note(
        ui,
        &p,
        if azure { "On: agents reach Azure with the host's login." } else { "Off: no Azure placeholders are granted." },
    );
    if let Some(line) = status["azure"].as_str() {
        note(ui, &p, line);
    }
    note(ui, &p, "Bare Claude can opt in under [isolation] in the config. The chief follows this setting too.");
}

/// One entry of the credential store: whether it is set, where its value goes, who used it, and what can change it.
fn credential(ui: &mut egui::Ui, client: &mut Client, enabled: bool, entry: &serde_json::Value) {
    let p = client.palette.clone();
    let label = entry["label"].as_str().unwrap_or_default();
    let present = entry["present"] == true;
    let kind = entry["kind"].as_str();
    let host = entry["host"].as_str().map(str::to_string);
    ui.horizontal_wrapped(|ui| {
        match present {
            true => mark(ui, Mark::Check, p.success),
            false => mark(ui, Mark::Ring, p.muted),
        }
        ui.label(RichText::new(label).strong());
        if !present {
            ui.label(RichText::new("not set").size(11.0).color(p.muted));
        }
    });
    if let Some(route) = entry["route"].as_str() {
        note(ui, &p, route.replace('→', "›"));
    }
    if present {
        let used = entry["last_used"].as_str().unwrap_or("never");
        let sessions = entry["sessions"]
            .as_array()
            .map_or(Vec::new(), |list| list.iter().filter_map(|session| session.as_str()).collect());
        let sessions = if sessions.is_empty() { "none".to_string() } else { sessions.join(", ") };
        note(ui, &p, format!("Last used: {used}. Sessions: {sessions}."));
    }
    let Some(kind) = kind else {
        ui.add_space(4.0);
        return;
    };
    let removing = Confirm::RemoveCredential { kind: kind.into(), host: host.clone(), label: label.into() };
    if client.management.confirm.as_ref() == Some(&removing) {
        let question = format!("Remove {label}? The stored value is gone for good, and sessions using it lose it.");
        match ask(ui, client, &question, "Remove") {
            Some(true) => {
                client.management.confirm = None;
                client.management.credential_value.clear();
                tool(client, SettingsTool::RemoveCredential { kind: kind.into(), host });
            }
            Some(false) => client.management.confirm = None,
            None => {}
        }
    } else {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal_wrapped(|ui| {
                if button(ui, client, if present { "Replace" } else { "Add" }).clicked() {
                    let state = &mut client.management;
                    state.credential_kind = kind.into();
                    state.credential_host = host.clone().unwrap_or_default();
                    state.credential_value.clear();
                    state.focus_credential = true;
                }
                if present && button(ui, client, "Remove…").clicked() {
                    client.management.confirm = Some(removing);
                }
            });
        });
    }
    ui.add_space(4.0);
}

/// Adds or replaces a credential. The value is masked, travels once to the host, and goes when Settings closes.
fn credential_form(ui: &mut egui::Ui, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    subheading(ui, &p, "Add or replace");
    let kinds = info.tools["credential_kinds"].as_array().map_or(&[][..], Vec::as_slice);
    let state = &mut client.management;
    if state.credential_kind.is_empty() {
        state.credential_kind = "claude".into();
    }
    let label_of = |id: &str| {
        kinds.iter().find(|kind| kind["id"] == id).and_then(|kind| kind["label"].as_str()).unwrap_or(id).to_string()
    };
    egui::ComboBox::from_label("Credential type").selected_text(label_of(&state.credential_kind)).show_ui(ui, |ui| {
        for kind in kinds {
            let id = kind["id"].as_str().unwrap_or_default();
            let label = kind["label"].as_str().unwrap_or(id);
            if ui.selectable_value(&mut state.credential_kind, id.into(), label).changed() {
                state.credential_value.clear();
                state.credential_host.clear();
                state.confirm = None;
            }
        }
    });
    let hint = match state.credential_kind.as_str() {
        "registry" => Some("registry.example.com (host only)"),
        "github-app" => Some("GitHub Enterprise host; empty for github.com (Connect GitHub is simpler)"),
        _ => None,
    };
    if let Some(hint) = hint {
        ui.add(
            crate::controls::single_line(&mut state.credential_host)
                .hint_text(hint)
                .desired_width(ui.available_width()),
        );
    }
    let value = ui.add(
        crate::controls::single_line(&mut state.credential_value)
            .id(egui::Id::NULL.with("credential-value"))
            .password(true)
            .hint_text("New value (hidden)")
            .desired_width(ui.available_width()),
    );
    if std::mem::take(&mut state.focus_credential) && !ui.is_sizing_pass() {
        value.request_focus();
    }
    let kind = state.credential_kind.clone();
    let host = hint.map(|_| state.credential_host.trim().to_string());
    let same = |entry: &&serde_json::Value| {
        entry["kind"] == kind.as_str() && entry["host"].as_str().unwrap_or("") == host.as_deref().unwrap_or("")
    };
    let stored = info.tools["credentials"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(same)
        .filter(|entry| entry["present"] == true);
    let replacing = Confirm::ReplaceCredential {
        kind: kind.clone(),
        host: host.clone(),
        label: stored.and_then(|entry| entry["label"].as_str()).unwrap_or_default().into(),
    };
    if state.confirm.as_ref() == Some(&replacing) {
        let Confirm::ReplaceCredential { label, .. } = &replacing else { unreachable!("just built") };
        let question = format!("Replace {label}? The old value is gone for good, and sessions using it lose it.");
        match ask(ui, client, &question, "Replace") {
            Some(true) => {
                client.management.confirm = None;
                let value = std::mem::take(&mut client.management.credential_value);
                tool(client, SettingsTool::PutCredential { kind, value, host });
            }
            Some(false) => client.management.confirm = None,
            None => {}
        }
        return;
    }
    let ready = !state.credential_value.is_empty() && (kind != "registry" || host.as_deref() != Some(""));
    let label = if stored.is_some() { "Save, replacing the old value…" } else { "Save credential" };
    if ui.add_enabled(ready, touch::sized(egui::Button::new(label), client.touch)).clicked() {
        if stored.is_some() {
            client.management.confirm = Some(replacing);
        } else {
            let value = std::mem::take(&mut client.management.credential_value);
            tool(client, SettingsTool::PutCredential { kind, value, host });
        }
    }
}

// ── Advanced ─────────────────────────────────────────────────────────────────────────────────────────

fn advanced(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    subheading(ui, &p, "Config files");
    for file in info.config["files"].as_array().into_iter().flatten() {
        let source = file["source"].as_str().unwrap_or_default();
        let path = file["path"].as_str().unwrap_or_default();
        let missing = if file["exists"] == true { "" } else { " (not created yet)" };
        note(ui, &p, format!("{source}: {path}{missing}"));
    }
    if backend.available(&Command::EditConfig) {
        if button(ui, client, "Edit config").clicked() {
            client.send(Command::EditConfig);
        }
    } else {
        note(
            ui,
            &p,
            "Edit config opens the file in an editor on the host, so it is on the host's desktop only. From here, \
             propose a change below: you review its diff, and Apply saves it on the host.",
        );
    }
    ui.add_space(4.0);
    ui.separator();
    subheading(ui, &p, "Effective configuration");
    ui.add(
        crate::controls::single_line(&mut client.management.filter)
            .hint_text("Find a setting")
            .desired_width(ui.available_width()),
    );
    if let Some(values) = info.config["values"].as_object() {
        egui::ScrollArea::vertical().id_salt("effective-settings").max_height(230.0).show(ui, |ui| {
            for (key, entry) in values {
                if !key.to_lowercase().contains(&client.management.filter.to_lowercase()) {
                    continue;
                }
                let source = entry["source"].as_str().unwrap_or("default");
                ui.add(
                    egui::Label::new(RichText::new(format!("{key} = {}  ({source})", entry["value"])).monospace())
                        .wrap(),
                );
            }
        });
    }
    ui.separator();
    subheading(ui, &p, "Propose a change");
    note(ui, &p, "Changes made here are proposed first. Review the diff, then Apply saves them on the host.");
    ui.add_enabled_ui(enabled, |ui| {
        let state = &mut client.management;
        let mut scope = state.scope.unwrap_or(Scope::Main);
        egui::ComboBox::from_label("Save to").selected_text(scope.name()).show_ui(ui, |ui| {
            for target in [Scope::Main, Scope::Host, Scope::Workspace, Scope::Theme] {
                ui.selectable_value(&mut scope, target, target.name());
            }
        });
        state.scope = Some(scope);
        if scope == Scope::Theme {
            ui.add(
                crate::controls::single_line(&mut state.theme)
                    .hint_text("Theme name")
                    .desired_width(ui.available_width()),
            );
        }
        ui.add(
            egui::TextEdit::multiline(&mut state.toml)
                .code_editor()
                .desired_rows(5)
                .desired_width(ui.available_width())
                .hint_text("[layout]\nmin_tile_width = 400.0"),
        );
        ui.add(
            crate::controls::single_line(&mut state.remove)
                .hint_text("Keys to remove, separated by commas")
                .desired_width(ui.available_width()),
        );
        ui.add(
            crate::controls::single_line(&mut state.reason)
                .hint_text("Reason (optional)")
                .desired_width(ui.available_width()),
        );
        let ready = !state.toml.trim().is_empty() || !state.remove.trim().is_empty();
        if ui.add_enabled(ready, touch::sized(egui::Button::new("Propose change"), client.touch)).clicked() {
            let state = &client.management;
            let action = HostAction::ConfigPropose {
                scope,
                toml: state.toml.clone(),
                remove: state.remove.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
                theme: (scope == Scope::Theme).then(|| state.theme.clone()),
                reason: Some(state.reason.clone()),
            };
            client.send(Command::Manage(action));
        }
    });
}
