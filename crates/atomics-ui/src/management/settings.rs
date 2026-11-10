//! The Settings window. What people change comes first, in plain sections that read what they show as they open;
//! what can be undone acts at once, and only what cannot asks first. The raw configuration is under Advanced.
use super::*;
use crate::status::{Mark, paint_mark};
use crate::theme::{Palette, lerp, parse_hex};
use crate::touch;
use atomics_core::ops::SettingsTool;
use atomics_core::projection::{ThemeChoice, UpdateInfo, UpdatePhase};
use egui::{Color32, Rect, Sense, Stroke, Vec2};
use std::collections::BTreeMap;

/// What Settings asks about first, because it cannot be undone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Confirm {
    RemoveSecret(String),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Section {
    #[default]
    Agents,
    Chief,
    Identity,
    Connections,
    Credentials,
    Remote,
    Shortcuts,
    Theme,
    Updates,
    GettingStarted,
    Advanced,
}

impl Section {
    const ALL: [Self; 11] = [
        Self::Agents,
        Self::Chief,
        Self::Identity,
        Self::Connections,
        Self::Credentials,
        Self::Remote,
        Self::Shortcuts,
        Self::Theme,
        Self::Updates,
        Self::GettingStarted,
        Self::Advanced,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Agents => AGENTS,
            Self::Chief => "Chief",
            Self::Identity => "Identity",
            Self::Connections => "Connections",
            Self::Credentials => "Credentials",
            Self::Remote => "Remote devices",
            Self::Shortcuts => "Shortcuts",
            Self::Theme => "Theme",
            Self::Updates => "Updates",
            Self::GettingStarted => "Getting started",
            Self::Advanced => "Advanced",
        }
    }
}

pub(super) fn draw(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo, height: f32) {
    let p = client.palette.clone();
    if let Some(forced) = client.management.open_section.take()
        && let Some(section) = Section::ALL.into_iter().find(|section| section.name() == forced)
    {
        client.management.section = section;
        client.management.section_list = false;
    }
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::CheckSandbox });
    if !enabled {
        ui.colored_label(p.warning, "View only: settings changes require the host grant.");
    }
    if let Some(problem) = info.config["load_error"].as_str() {
        ui.colored_label(p.warning, problem);
    }
    let height = height - ui.min_size().y;
    if ui.available_width() < 600.0 {
        if client.management.section_list {
            section_list(ui, client, height);
        } else {
            if button(ui, client, "‹ Back to sections").clicked() {
                client.management.section_list = true;
            }
            section_content(ui, backend, client, enabled, info, height - 40.0);
        }
    } else {
        let height = height.min(680.0);
        ui.horizontal_top(|ui| {
            ui.set_max_height(height);
            ui.vertical(|ui| {
                ui.set_width(154.0);
                section_list(ui, client, height);
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                section_content(ui, backend, client, enabled, info, height);
            });
        });
    }
}

fn section_list(ui: &mut egui::Ui, client: &mut Client, height: f32) {
    egui::ScrollArea::vertical().id_salt("settings-sections").max_height(height).show(ui, |ui| {
        for section in Section::ALL {
            let response = ui.add(touch::sized(
                egui::Button::new(section.name()).selected(client.management.section == section).frame(false),
                client.touch,
            ));
            if response.clicked() {
                client.management.section = section;
                client.management.section_list = false;
            }
        }
    });
}

fn section_content(
    ui: &mut egui::Ui,
    backend: &dyn Backend,
    client: &mut Client,
    enabled: bool,
    info: &ManagementInfo,
    height: f32,
) {
    let section = client.management.section;
    ui.heading(section.name());
    egui::ScrollArea::vertical()
        .id_salt(("settings-content", section.name()))
        .max_height((height - 32.0).max(60.0))
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            match section {
                Section::Agents => {
                    note(ui, &client.palette, agents_summary(info));
                    agents(ui, client, enabled, info);
                }
                Section::Chief => chief(ui, client, enabled, info),
                Section::Identity => identity(ui, backend, client, enabled, info),
                Section::Connections => {
                    note(ui, &client.palette, connections_summary(info));
                    connections(ui, backend, client, enabled, info);
                }
                Section::Credentials => {
                    note(ui, &client.palette, credentials_summary(info));
                    credentials(ui, backend, client, enabled, info);
                }
                Section::Remote => {
                    note(ui, &client.palette, remote_summary(info));
                    remote::draw(ui, backend, client, info);
                }
                Section::Shortcuts => shortcuts(ui, backend, client, info),
                Section::Theme => theme(ui, backend, client, info),
                Section::Updates => {
                    if let Some(update) = backend.update() {
                        note(ui, &client.palette, update_summary(&update));
                        updates(ui, backend, client, &update);
                    } else {
                        note(ui, &client.palette, "No update information from the host yet.");
                    }
                }
                Section::GettingStarted => getting_started(ui, client),
                Section::Advanced => advanced(ui, backend, client, enabled, info),
            }
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

#[derive(Default)]
pub(super) struct IdentityForm {
    target: String,
    original: atomics_core::identity::Identity,
    name: String,
    email: String,
    words: String,
    clear_words: bool,
}

impl IdentityForm {
    pub(super) fn select(&mut self, target: String, identity: atomics_core::identity::Identity) {
        self.target = target;
        self.name = identity.name.clone().unwrap_or_default();
        self.email = identity.email.clone().unwrap_or_default();
        self.words = identity.forbidden_words.as_ref().map(|words| words.join("\n")).unwrap_or_default();
        self.clear_words = identity.forbidden_words.as_ref().is_some_and(Vec::is_empty);
        self.original = identity;
    }

    /// Only edited keys are proposed. Empty fields inherit; clearing the word guard is explicit.
    fn changes(&self) -> (atomics_core::identity::Identity, Vec<&'static str>) {
        let mut patch = atomics_core::identity::Identity::default();
        let mut remove = Vec::new();
        for (key, text, old, value) in [
            ("name", &self.name, &self.original.name, &mut patch.name),
            ("email", &self.email, &self.original.email, &mut patch.email),
        ] {
            if text != old.as_deref().unwrap_or("") {
                let text = text.trim();
                if text.is_empty() {
                    remove.push(key);
                } else {
                    *value = Some(text.into());
                }
            }
        }
        let original_words = self.original.forbidden_words.as_ref().map(|words| words.join("\n")).unwrap_or_default();
        let original_clear = self.original.forbidden_words.as_ref().is_some_and(Vec::is_empty);
        if self.words == original_words && self.clear_words == original_clear {
            return (patch, remove);
        }
        let words = if self.clear_words {
            Some(Vec::new())
        } else {
            let words: Vec<String> =
                self.words.lines().map(str::trim).filter(|word| !word.is_empty()).map(str::to_string).collect();
            (!words.is_empty()).then_some(words)
        };
        if words != self.original.forbidden_words {
            match words {
                Some(words) => patch.forbidden_words = Some(words),
                None if self.original.forbidden_words.is_some() => remove.push("forbidden_words"),
                None => {}
            }
        }
        (patch, remove)
    }

    fn proposal(&self, path: &[String]) -> Option<HostAction> {
        let (patch, remove) = self.changes();
        if patch == Default::default() && remove.is_empty() {
            return None;
        }
        let fields = serde_json::to_value(patch).ok()?;
        let fields = fields.as_object()?.iter().map(|(key, value)| format!("{key} = {value}\n")).collect::<String>();
        let toml = if fields.is_empty() {
            String::new()
        } else {
            format!("[{}]\n{fields}", atomics_core::selfconfig::key(path))
        };
        let remove = remove
            .into_iter()
            .map(|field| {
                let mut field_path = path.to_vec();
                field_path.push(field.into());
                atomics_core::selfconfig::key(&field_path)
            })
            .collect();
        Some(HostAction::ConfigPropose {
            scope: if self.target == "workspace" { Scope::Workspace } else { Scope::Main },
            toml,
            remove,
            theme: None,
            reason: Some("Update identity override".into()),
        })
    }
}

fn identity(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    if let Some(error) = info.tools["identity_error"].as_str() {
        ui.colored_label(p.warning, error);
    }
    let rows = info.tools["identities"].as_array().map_or(&[][..], Vec::as_slice);
    let scopes = info.config["identity_scopes"].as_array().map_or(&[][..], Vec::as_slice);
    for row in rows {
        ui.horizontal_wrapped(|ui| {
            ui.label(row["display"].as_str().unwrap_or_default());
            ui.label(RichText::new(row["label"].as_str().unwrap_or("Git identity")).strong());
            if ui.add_enabled(enabled, egui::Button::new("Change…")).clicked()
                && let Ok(identity) = serde_json::from_value(row["override"].clone())
            {
                client.management.show_identity(row["project"].as_str().unwrap_or_default().into(), identity);
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
        let form = &mut client.management.identity;
        let previous = form.target.clone();
        if form.target.is_empty() || form.target == "workspace" && workspace.is_none() {
            form.target = if workspace.is_some() { "workspace" } else { "main" }.into();
        }
        let selected = match form.target.as_str() {
            "main" => "Main config".to_string(),
            "workspace" => format!("Workspace: {}", workspace.as_deref().unwrap_or("none")),
            project => format!("Project: {project}"),
        };
        egui::ComboBox::from_label("Identity for").selected_text(selected).show_ui(ui, |ui| {
            ui.selectable_value(&mut form.target, "main".into(), "Main config");
            if let Some(workspace) = &workspace {
                ui.selectable_value(&mut form.target, "workspace".into(), format!("Workspace: {workspace}"));
            }
            for row in rows {
                let path = row["project"].as_str().unwrap_or_default();
                ui.selectable_value(
                    &mut form.target,
                    path.into(),
                    format!("Project: {}", row["display"].as_str().unwrap_or(path)),
                );
            }
        });
        let scope = scopes.iter().find(|scope| scope["target"] == form.target);
        if previous != form.target {
            let raw =
                scope.and_then(|scope| serde_json::from_value(scope["identity"].clone()).ok()).unwrap_or_default();
            form.select(form.target.clone(), raw);
        }
        ui.add(
            crate::controls::single_line(&mut form.name)
                .hint_text("Commit name (blank inherits)")
                .desired_width(ui.available_width()),
        );
        ui.add(
            crate::controls::single_line(&mut form.email)
                .hint_text("Commit email (blank inherits)")
                .desired_width(ui.available_width()),
        );
        ui.add_enabled(
            !form.clear_words,
            egui::TextEdit::multiline(&mut form.words)
                .hint_text("Forbidden words, one per line (blank inherits)")
                .desired_rows(3)
                .desired_width(ui.available_width()),
        );
        ui.checkbox(&mut form.clear_words, "Clear inherited words");
        let path = scope.and_then(|scope| serde_json::from_value::<Vec<String>>(scope["path"].clone()).ok());
        let proposal = path.as_deref().and_then(|path| form.proposal(path));
        if ui.add_enabled(proposal.is_some(), egui::Button::new("Review identity change")).clicked() {
            client.send(Command::Manage(proposal.unwrap()));
        }
    });
    note(
        ui,
        &p,
        "Only changed keys are saved in the selected scope. Blank fields inherit; clearing inherited words requires the checkbox. Review and Apply saves the change in your config. The guard reports file and line only.",
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
    if let Some(url) = update.notes.as_deref().and_then(crate::markdown::web_url) {
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

pub(super) struct ShortcutForm {
    original: BTreeMap<String, serde_json::Value>,
    bindings: BTreeMap<String, String>,
    scope: Scope,
}

impl ShortcutForm {
    fn values(info: &ManagementInfo) -> BTreeMap<String, serde_json::Value> {
        let defaults = serde_json::to_value(atomics_core::config::KeyConfig::default()).unwrap();
        SHORTCUTS
            .iter()
            .map(|(key, _)| {
                let value = info.config["values"][*key]["value"]
                    .as_str()
                    .or_else(|| defaults[key.strip_prefix("keys.").unwrap()].as_str())
                    .unwrap_or_default();
                ((*key).into(), value.into())
            })
            .collect()
    }

    fn new(original: BTreeMap<String, serde_json::Value>) -> Self {
        let bindings =
            original.iter().map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().into())).collect();
        Self { original, bindings, scope: Scope::Main }
    }

    fn problems(&self, os: egui::os::OperatingSystem) -> Vec<String> {
        let mut problems = Vec::new();
        for (key, what) in SHORTCUTS {
            let text = &self.bindings[key];
            if text.trim().is_empty() {
                continue;
            }
            let Some(chord) = crate::keys::shortcut_for_os(text, os) else {
                problems.push(format!("Invalid shortcut for {what}: {text}"));
                continue;
            };
            let taken = crate::keys::taken_shortcuts(
                self.bindings.iter().map(|(key, text)| (key.trim_start_matches("keys."), text.as_str())),
                key.trim_start_matches("keys."),
                os,
            );
            if let Some(owner) = crate::keys::shortcut_owner(&taken, &chord) {
                problems.push(if owner.starts_with("[keys]") {
                    format!("Conflict: {what} and {owner} use the same shortcut")
                } else {
                    format!("Conflict: {what} uses a fixed navigation shortcut ({owner})")
                });
            }
        }
        problems
    }

    fn proposal(&self, os: egui::os::OperatingSystem) -> Option<HostAction> {
        if !self.problems(os).is_empty() {
            return None;
        }
        let values = self.bindings.iter().map(|(key, text)| (key.clone(), text.clone().into())).collect();
        form_proposal(self.scope, &self.original, &values, "Update keyboard shortcuts")
    }
}

/// The forms send only edited fields, leaving inherited settings and custom arguments intact.
fn form_proposal(
    scope: Scope,
    original: &BTreeMap<String, serde_json::Value>,
    values: &BTreeMap<String, serde_json::Value>,
    reason: &str,
) -> Option<HostAction> {
    let mut toml = String::new();
    let mut remove = Vec::new();
    for (key, value) in values {
        if original.get(key) == Some(value) {
            continue;
        }
        if value.is_null() {
            remove.push(key.clone());
        } else {
            toml.push_str(&format!("{key} = {value}\n"));
        }
    }
    (!toml.is_empty() || !remove.is_empty()).then(|| HostAction::ConfigPropose {
        scope,
        toml,
        remove,
        theme: None,
        reason: Some(reason.into()),
    })
}

fn save_scope(ui: &mut egui::Ui, scope: &mut Scope) {
    egui::ComboBox::from_label("Save to").selected_text(scope.name()).show_ui(ui, |ui| {
        for target in [Scope::Main, Scope::Host] {
            ui.selectable_value(scope, target, target.name());
        }
    });
}

fn shortcuts(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, info: &ManagementInfo) {
    let p = client.palette.clone();
    let everywhere = [
        (crate::window_keys::agents_label(ui.ctx()), "that agent, with its diff"),
        ("Shift+Up/Down".to_string(), "previous or next changed file"),
        ("PageUp/PageDown".to_string(), "previous or next hunk, while the diff has the keyboard"),
        ("Esc".to_string(), "close the window on top"),
    ];
    for (chord, what) in everywhere {
        ui.add(egui::Label::new(format!("{chord}  {what}")).wrap());
    }
    if !backend.has_shortcuts() {
        note(
            ui,
            &p,
            "The desktop app on the host also has these. This client leaves them to the terminal, or to the browser:",
        );
    }
    let values = ShortcutForm::values(info);
    if client.management.shortcuts.as_ref().is_none_or(|form| form.original != values) {
        client.management.shortcuts = Some(ShortcutForm::new(values));
    }
    let enabled = allowed(backend, &HostAction::SettingsTool { tool: SettingsTool::CheckSandbox });
    let mut proposal = None;
    ui.add_enabled_ui(enabled, |ui| {
        let form = client.management.shortcuts.as_mut().unwrap();
        save_scope(ui, &mut form.scope);
        for (key, what) in SHORTCUTS {
            ui.label(what);
            ui.add(
                crate::controls::single_line(form.bindings.get_mut(key).unwrap())
                    .id_salt(key)
                    .desired_width(ui.available_width()),
            );
        }
        for problem in form.problems(ui.ctx().os()) {
            ui.add(egui::Label::new(RichText::new(problem).color(p.warning)).wrap());
        }
        let change = form.proposal(ui.ctx().os());
        if ui.add_enabled(change.is_some(), egui::Button::new("Review shortcut changes")).clicked() {
            proposal = change;
        }
    });
    if let Some(proposal) = proposal {
        client.send(Command::Manage(proposal));
    }
    note(
        ui,
        &p,
        "Use cmd, primary, ctrl, alt and shift, joined with + and a key. Blank disables a binding. Conflicts use this window's operating system. Review and Apply saves the shortcuts on the host.",
    );
}

// ── Chief ────────────────────────────────────────────────────────────────────────────────────────────

pub(super) struct ChiefForm {
    snapshot: serde_json::Value,
    original: BTreeMap<String, serde_json::Value>,
    model: Option<String>,
    legacy: bool,
    command: Vec<String>,
    dock: String,
    autostart: bool,
    heartbeat: u64,
    wake: u64,
    prefer: String,
    minimums: [f64; 2],
    scope: Scope,
}

const MAX_INTERVAL: u64 = i64::MAX as u64;

impl ChiefForm {
    fn snapshot(info: &ManagementInfo) -> serde_json::Value {
        info.config["values"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(key, _)| {
                matches!(
                    key.as_str(),
                    "chief.model"
                        | "chief.args"
                        | "chief.command"
                        | "chief.dock"
                        | "chief.autostart"
                        | "chief.heartbeat_secs"
                        | "chief.wake_secs"
                        | "chief.prefer"
                ) || key.starts_with("chief.min_left_percent.")
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn new(snapshot: serde_json::Value) -> Self {
        let value = |key: &str| &snapshot[key]["value"];
        let strings = |key| {
            value(key)
                .as_array()
                .map(|words| words.iter().filter_map(|word| word.as_str().map(str::to_string)).collect::<Vec<_>>())
        };
        let command = strings("chief.command").unwrap_or_else(|| vec!["claude".into()]);
        let args = strings("chief.args").unwrap_or_default();
        let legacy = atomics_core::config::ChiefConfig::legacy_model(&command, &args);
        let model = value("chief.model").as_str().map(str::to_string);
        let mut form = Self {
            original: BTreeMap::new(),
            model,
            legacy,
            command,
            dock: value("chief.dock").as_str().unwrap_or("floating").into(),
            autostart: value("chief.autostart").as_bool().unwrap_or(false),
            heartbeat: value("chief.heartbeat_secs").as_u64().unwrap_or(300),
            wake: value("chief.wake_secs").as_u64().unwrap_or(180),
            prefer: strings("chief.prefer").unwrap_or_else(|| vec!["codex".into(), "claude".into()]).join(", "),
            minimums: [
                value("chief.min_left_percent.codex").as_f64().unwrap_or(0.0),
                value("chief.min_left_percent.claude").as_f64().unwrap_or(0.0),
            ],
            snapshot,
            scope: Scope::Main,
        };
        form.original = form.values();
        form
    }

    fn values(&self) -> BTreeMap<String, serde_json::Value> {
        use serde_json::json;
        BTreeMap::from([
            ("chief.model".into(), json!(self.model)),
            ("chief.dock".into(), json!(self.dock)),
            ("chief.autostart".into(), json!(self.autostart)),
            ("chief.heartbeat_secs".into(), json!(self.heartbeat)),
            ("chief.wake_secs".into(), json!(self.wake)),
            (
                "chief.prefer".into(),
                json!(self.prefer.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>()),
            ),
            ("chief.min_left_percent.codex".into(), json!(self.minimums[0])),
            ("chief.min_left_percent.claude".into(), json!(self.minimums[1])),
        ])
    }

    fn proposal(&self) -> Option<HostAction> {
        if self.wake != 0 && self.wake < 30
            || self.model.as_deref().and_then(atomics_core::config::ChiefConfig::model_problem).is_some()
            || self.heartbeat > MAX_INTERVAL
            || self.wake > MAX_INTERVAL
        {
            return None;
        }
        form_proposal(self.scope, &self.original, &self.values(), "Update chief settings")
    }
}

fn chief(ui: &mut egui::Ui, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let snapshot = ChiefForm::snapshot(info);
    if client.management.chief.as_ref().is_none_or(|form| form.snapshot != snapshot) {
        client.management.chief = Some(ChiefForm::new(snapshot));
    }
    let mut proposal = None;
    ui.add_enabled_ui(enabled, |ui| {
        let form = client.management.chief.as_mut().unwrap();
        save_scope(ui, &mut form.scope);
        let choices = atomics_core::config::ChiefConfig::model_choices(&form.command);
        let selected = form
            .model
            .clone()
            .unwrap_or_else(|| if form.legacy { "Custom (args)".into() } else { "Command default".into() });
        ui.label("Model");
        egui::ComboBox::from_id_salt("chief-model")
            .selected_text(selected)
            .height(320.0)
            .width(ui.available_width().min(240.0))
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut form.model, None, if form.legacy { "Custom (args)" } else { "Command default" });
                for model in choices {
                    ui.selectable_value(&mut form.model, Some((*model).into()), *model);
                }
                if ui
                    .selectable_label(
                        form.model.as_ref().is_some_and(|model| !choices.contains(&model.as_str())),
                        "Custom model…",
                    )
                    .clicked()
                {
                    form.model = Some(String::new());
                }
            });
        if let Some(model) = &mut form.model
            && !choices.contains(&model.as_str())
        {
            ui.add(
                crate::controls::single_line(model)
                    .hint_text("Full model name accepted by the command")
                    .desired_width(ui.available_width()),
            );
        }
        if let Some(problem) = form.model.as_deref().and_then(atomics_core::config::ChiefConfig::model_problem) {
            ui.add(egui::Label::new(RichText::new(problem).color(p.warning)).wrap());
        }
        if form.legacy && form.model.is_none() {
            note(ui, &p, "Custom model from chief.args. Choosing a model overrides only its model argument at launch.");
        }
        egui::ComboBox::from_label("Dock").selected_text(&form.dock).show_ui(ui, |ui| {
            for dock in ["floating", "docked"] {
                ui.selectable_value(&mut form.dock, dock.into(), dock);
            }
        });
        ui.checkbox(&mut form.autostart, "Start chief when ATOMICS starts");
        ui.label("Heartbeat interval (seconds)");
        ui.add(egui::DragValue::new(&mut form.heartbeat).range(1..=MAX_INTERVAL).clamp_existing_to_range(false));
        // DragValue uses f64: the rounded representation of i64::MAX is one past TOML's limit.
        form.heartbeat = form.heartbeat.min(MAX_INTERVAL);
        ui.label("Wake interval (seconds; 0 turns wakes off)");
        ui.add(egui::DragValue::new(&mut form.wake).range(0..=MAX_INTERVAL));
        form.wake = form.wake.min(MAX_INTERVAL);
        if form.wake != 0 && form.wake < 30 {
            note(ui, &p, "Wake interval must be 0 or at least 30 seconds.");
        }
        subheading(ui, &p, "Chief's task agent preference");
        ui.add(
            crate::controls::single_line(&mut form.prefer)
                .hint_text("Agent names, in order, separated by commas")
                .desired_width(ui.available_width()),
        );
        note(ui, &p, "An empty preference leaves the choice to task fit.");
        for (index, provider) in ["codex", "claude"].into_iter().enumerate() {
            ui.label(format!("{provider} quota minimum"));
            ui.add(egui::DragValue::new(&mut form.minimums[index]).range(0.0..=100.0).suffix("% left"));
        }
        note(ui, &p, "Minimums apply to session and weekly quota left; 0 means no minimum. Plans below them warn and can still be saved.");
        let change = form.proposal();
        if ui.add_enabled(change.is_some(), egui::Button::new("Review chief changes")).clicked() {
            proposal = change;
        }
    });
    if let Some(proposal) = proposal {
        client.send(Command::Manage(proposal));
    }
    note(
        ui,
        &p,
        "Review the diff, then Apply saves these settings. Model and autostart take effect the next time the chief starts; intervals apply while it runs.",
    );
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
    let secrets = info.tools["credentials"]["secrets"].as_array().map_or(0, Vec::len);
    format!("{secrets} secrets · {set} of {} integrations set", entries.len())
}

fn credentials(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    let p = client.palette.clone();
    let local_enabled = enabled
        || backend.available(&Command::Manage(HostAction::SettingsTool { tool: SettingsTool::RefreshCredentials }));
    let status = &info.tools["credentials"];
    let busy = info.tools["credential_busy"] == true;
    load(client, local_enabled, busy, SettingsTool::RefreshCredentials);
    secrets(ui, backend, client, local_enabled && !busy, info);
    note(
        ui,
        &p,
        "Stored values are never displayed. Replacing or removing one takes it from the sessions using it: restart them.",
    );
    if let Some(error) = status["error"].as_str() {
        ui.add(egui::Label::new(RichText::new(error).color(p.error)).wrap());
    }
    if busy {
        note(
            ui,
            &p,
            if info.tools["credentials_local"] == true {
                "Waiting for this Mac's keychain or VM secret sync…"
            } else {
                "Waiting for the OS credential store…"
            },
        );
    } else if status.is_null() && !enabled {
        note(ui, &p, "The host has not read its credential store yet.");
    }
    let entries = status["entries"].as_array().map_or(&[][..], Vec::as_slice);
    for entry in entries {
        credential(ui, client, local_enabled && !busy, entry);
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
            ui.add_enabled_ui(local_enabled && !busy, |ui| {
                if button(ui, client, "Remove unknown entries…").clicked() {
                    client.management.confirm = Some(Confirm::CleanCredentials);
                }
            });
        }
    }
    ui.add_enabled_ui(local_enabled && !busy, |ui| {
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
fn secrets(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, enabled: bool, info: &ManagementInfo) {
    use atomics_core::secrets::{Auth, Scope as SecretScope};
    let p = client.palette.clone();
    subheading(ui, &p, "Secrets");
    if let Some(warning) = info.tools["credentials"]["vm_warning"].as_str() {
        note(ui, &p, warning);
    }
    if let Some(removals) = info.tools["credentials"]["vm_removals"].as_object() {
        for (name, state) in removals {
            note(
                ui,
                &p,
                format!("{name} · VM removal: {} · Refresh credentials to retry", state.as_str().unwrap_or("Pending")),
            );
        }
    }
    if let Some(note_text) = info.tools["credentials"]["copy_note"].as_str() {
        note(ui, &p, note_text);
    }
    note(
        ui,
        &p,
        if info.tools["credentials_local"] == true {
            "Secrets live in this Mac's keychain and sync to the VM broker. New VM sessions receive them in scope. Exposed values can be read and leaked by agents."
        } else {
            "New sessions receive secrets in their scope. Protected values stay in the host store; exposed values can be read and leaked by agents."
        },
    );
    for entry in info.tools["credentials"]["secrets"].as_array().into_iter().flatten() {
        let name = entry["spec"]["name"].as_str().unwrap_or_default();
        ui.label(RichText::new(name).strong());
        if let Some(state) = entry["vm_sync"].as_str() {
            note(ui, &p, format!("VM: {state}"));
        }
        note(
            ui,
            &p,
            format!(
                "{} · {} · Host: {} · Last use: {}",
                entry["kind"].as_str().unwrap_or_default(),
                entry["scope"].as_str().unwrap_or_default(),
                entry["spec"]["host"].as_str().unwrap_or("none"),
                entry["last_used"].as_str().unwrap_or("never")
            ),
        );
        let copy = Command::CopySecret(name.into());
        let local = backend.available(&copy);
        let unavailable = if !local {
            Some(
                "Copy is available only in a native window on the machine holding the credential store. Open ATOMICS there; remote and protocol clients cannot copy.",
            )
        } else {
            info.tools["secret_copy_unavailable"].as_str()
        };
        ui.horizontal(|ui| {
            if ui
                .add_enabled(enabled && local && unavailable.is_none(), egui::Button::new("Copy (OS authentication)"))
                .clicked()
            {
                client.send(copy);
            }
            if ui.add_enabled(enabled, egui::Button::new("Remove…")).clicked() {
                client.management.confirm = Some(Confirm::RemoveSecret(name.into()));
            }
        });
        if let Some(reason) = unavailable {
            note(ui, &p, reason);
        }
        if client.management.confirm == Some(Confirm::RemoveSecret(name.into())) {
            match ask(
                ui,
                client,
                &format!("Remove {name}? This cannot be undone. Restart existing sessions to clear their environment."),
                "Remove secret",
            ) {
                Some(true) => {
                    client.management.confirm = None;
                    tool(client, SettingsTool::RemoveSecret { name: name.into() });
                }
                Some(false) => client.management.confirm = None,
                None => {}
            }
        }
        ui.add_space(4.0);
    }
    if ui.add_enabled(enabled, egui::Button::new("Add secret")).clicked() {
        client.management.adding_secret = true;
    }
    if !client.management.adding_secret {
        ui.separator();
        return;
    }
    ui.add_enabled_ui(enabled, |ui| {
        let state = &mut client.management;
        ui.label("Environment variable name");
        if ui.add(crate::controls::single_line(&mut state.secret.name).hint_text("LOGO_DEV_API_KEY")).changed() {
            let known = atomics_core::secrets::service(&state.secret.name);
            state.secret.host = known.as_ref().map(|(host, _)| host.to_string());
            state.secret.auth = known.map(|(_, auth)| auth).unwrap_or_default();
        }
        ui.label("Value");
        ui.add(crate::controls::single_line(&mut state.secret_value).password(true).hint_text("New value (hidden)"));
        ui.label("Used with host (optional)");
        let mut host = state.secret.host.clone().unwrap_or_default();
        if ui
            .add(crate::controls::single_line(&mut host).hint_text("api.example.com or a service such as openai"))
            .changed()
        {
            let known = atomics_core::secrets::service(host.trim());
            state.secret.host = if host.trim().is_empty() {
                None
            } else {
                Some(known.as_ref().map_or_else(|| host.trim().to_ascii_lowercase(), |(h, _)| h.to_string()))
            };
            if let Some((_, auth)) = known {
                state.secret.auth = auth;
            }
            if state.secret.host.is_none() {
                state.secret.auth = Auth::Bearer;
                state.secret_methods.clear();
                state.secret_path.clear();
            }
        }
        ui.label(state.secret.kind());
        egui::ComboBox::from_label("Secret scope").selected_text(state.secret.scope.label()).show_ui(ui, |ui| {
            ui.selectable_value(&mut state.secret.scope, SecretScope::Everywhere, "Everywhere");
            for workspace in info.tools["secret_workspaces"].as_array().into_iter().flatten().filter_map(|s| s.as_str())
            {
                ui.selectable_value(
                    &mut state.secret.scope,
                    SecretScope::Workspace(workspace.into()),
                    format!("Workspace: {workspace}"),
                );
            }
            for project in
                info.tools["identities"].as_array().into_iter().flatten().filter_map(|s| s["project"].as_str())
            {
                ui.selectable_value(
                    &mut state.secret.scope,
                    SecretScope::Project(project.into()),
                    format!("Project: {project}"),
                );
            }
        });
        if state.secret.host.is_some() {
            let selected = match &state.secret.auth {
                Auth::Bearer => "Authorization bearer",
                Auth::Header(_) => "Named header",
                Auth::Basic(_) => "Basic auth",
                Auth::Query(_) => "Query parameter",
            };
            egui::ComboBox::from_label("Authentication").selected_text(selected).show_ui(ui, |ui| {
                for (auth, label) in [
                    (Auth::Bearer, "Authorization bearer"),
                    (Auth::Header(String::new()), "Named header"),
                    (Auth::Basic(String::new()), "Basic auth"),
                    (Auth::Query(String::new()), "Query parameter"),
                ] {
                    if ui
                        .selectable_label(
                            std::mem::discriminant(&state.secret.auth) == std::mem::discriminant(&auth),
                            label,
                        )
                        .clicked()
                    {
                        state.secret.auth = auth;
                    }
                }
            });
            match &mut state.secret.auth {
                Auth::Header(name) => {
                    ui.add(crate::controls::single_line(name).hint_text("Header name, e.g. x-api-key"));
                }
                Auth::Basic(user) => {
                    ui.add(
                        crate::controls::single_line(user).hint_text("Public username (value above is the password)"),
                    );
                }
                Auth::Query(name) => {
                    ui.add(crate::controls::single_line(name).hint_text("Query parameter name, e.g. api_key"));
                }
                _ => {}
            }
            ui.add(
                crate::controls::single_line(&mut state.secret_methods)
                    .hint_text("Allowed methods, e.g. GET,POST (empty: all)"),
            );
            ui.add(
                crate::controls::single_line(&mut state.secret_path).hint_text("Path prefix, e.g. /v1 (empty: all)"),
            );
        }
        state.secret.methods = state
            .secret_methods
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_uppercase)
            .collect();
        state.secret.path_prefix = (!state.secret_path.trim().is_empty()).then(|| state.secret_path.trim().into());
        let validation = state.secret.validate();
        if let Err(reason) = validation {
            note(ui, &p, reason);
        }
        let save = ui
            .add_enabled(validation.is_ok() && !state.secret_value.is_empty(), egui::Button::new("Save secret"))
            .clicked();
        let cancel = ui.button("Cancel").clicked();
        if save {
            let spec = state.secret.clone();
            let value = std::mem::take(&mut state.secret_value);
            state.adding_secret = false;
            tool(client, SettingsTool::AddSecret { spec, value });
        } else if cancel {
            state.secret_value.clear();
            state.adding_secret = false;
        }
    });
    ui.separator();
}

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

#[cfg(test)]
mod settings_tests {
    use super::*;
    use egui::os::OperatingSystem;

    #[test]
    fn section_order_and_selection_live_with_each_window() {
        assert_eq!(
            Section::ALL.map(Section::name),
            [
                "Agents",
                "Chief",
                "Identity",
                "Connections",
                "Credentials",
                "Remote devices",
                "Shortcuts",
                "Theme",
                "Updates",
                "Getting started",
                "Advanced"
            ]
        );
        let mut first = State { section: Section::Chief, section_list: true, ..Default::default() };
        first.forget_settings();
        assert_eq!(first.section, Section::Chief);
        assert!(!first.section_list);
        assert_eq!(State::default().section, Section::Agents);
    }

    #[test]
    fn closing_settings_discards_chief_and_shortcut_edits_but_keeps_the_section() {
        let mut chief = ChiefForm::new(serde_json::json!({}));
        chief.model = Some("unsaved-model".into());
        let mut shortcuts = ShortcutForm::new(ShortcutForm::values(&ManagementInfo::default()));
        shortcuts.bindings.insert("keys.quick_open".into(), "alt+p".into());
        let mut state =
            State { section: Section::Chief, chief: Some(chief), shortcuts: Some(shortcuts), ..Default::default() };
        state.forget_settings();
        assert!(state.chief.is_none() && state.shortcuts.is_none());
        assert_eq!(state.section, Section::Chief);
    }

    #[test]
    fn chief_intervals_stay_within_toml_integer_limits_and_invalid_models_block_review() {
        let mut form = ChiefForm::new(serde_json::json!({}));
        for model in ["", "-flag", "two words"] {
            form.model = Some(model.into());
            assert!(form.proposal().is_none());
        }
        form.model = Some("sonnet".into());
        form.heartbeat = MAX_INTERVAL;
        form.wake = MAX_INTERVAL;
        let HostAction::ConfigPropose { toml, remove, .. } = form.proposal().unwrap() else { panic!("proposal") };
        let after = atomics_core::selfconfig::edit("", &toml, &remove).unwrap();
        let config = atomics_core::config::Config::from_tables([after.parse().unwrap()]).unwrap();
        assert_eq!(config.chief.heartbeat_secs, MAX_INTERVAL);
        assert_eq!(config.chief.wake_secs, MAX_INTERVAL);
        form.wake = MAX_INTERVAL + 1;
        assert!(form.proposal().is_none());
    }

    #[test]
    fn settings_rejects_every_reserved_ticket_filter_shortcut() {
        let mut form = ShortcutForm::new(ShortcutForm::values(&ManagementInfo::default()));
        for n in 1..=9 {
            form.bindings.insert("keys.github".into(), format!("alt+{n}"));
            let problems = form.problems(egui::os::OperatingSystem::Nix);
            assert!(problems.iter().any(|problem| problem.contains("ticket filter")), "{problems:?}");
            assert!(form.proposal(egui::os::OperatingSystem::Nix).is_none());
        }
    }

    #[test]
    fn chief_form_shows_host_model_args_as_custom_and_can_replace_them_in_host_scope() {
        use atomics_core::config::Config;
        use atomics_core::selfconfig::{Layer, Source};
        let layers = [
            Layer::new(Source::Main, "main.toml", Some(atomics_core::config::DEFAULT_CONFIG.into())),
            Layer::new(Source::Host, "host.toml", Some("[chief]\nargs = ['--model', 'sonnet']\n".into())),
        ];
        let config = Config::from_tables(layers.iter().map(|layer| layer.table.clone())).unwrap();
        let info = ManagementInfo {
            config: serde_json::json!({ "values": atomics_core::selfconfig::values(
            &config, &layers, &atomics_core::tickets::Cleaner::new(Vec::new()), None,
        ) }),
            ..Default::default()
        };
        let mut form = ChiefForm::new(ChiefForm::snapshot(&info));
        assert!(form.legacy && form.model.is_none(), "the picker must show Custom (args)");
        form.scope = Scope::Host;
        form.model = Some("haiku".into());
        let HostAction::ConfigPropose { scope, toml, remove, .. } = form.proposal().unwrap() else {
            panic!("proposal")
        };
        let proposal = atomics_core::selfconfig::propose(scope, &layers, &toml, &remove).unwrap();
        let config = Config::from_tables([layers[0].table.clone(), proposal.after.parse().unwrap()]).unwrap();
        assert_eq!(config.chief.launch_args(), ["--model", "haiku"]);
    }

    #[test]
    fn chief_form_proposes_only_edits_and_keeps_custom_args() {
        let snapshot = serde_json::json!({
            "chief.args": {"value":["--model", "old-model", "--verbose"]},
            "chief.min_left_percent.codex": {"value":10.0},
        });
        let mut form = ChiefForm::new(snapshot);
        assert!(form.legacy && form.model.is_none());
        assert!(form.proposal().is_none());
        form.model = Some("sonnet".into());
        form.dock = "docked".into();
        form.autostart = true;
        form.heartbeat = 120;
        form.wake = 30;
        form.prefer = "claude, codex".into();
        form.minimums[1] = 25.0;
        let HostAction::ConfigPropose { scope, toml, remove, .. } = form.proposal().unwrap() else {
            panic!("proposal")
        };
        assert_eq!(scope, Scope::Main);
        assert!(remove.is_empty());
        assert!(!toml.contains("args") && !toml.contains("min_left_percent.codex"));
        let layer = atomics_core::selfconfig::Layer::new(
            atomics_core::selfconfig::Source::Main,
            "config.toml",
            Some("[chief]\nargs = ['--model', 'old-model', '--verbose']\nmin_left_percent = { codex = 10 }\n".into()),
        );
        let proposal = atomics_core::selfconfig::propose(scope, &[layer], &toml, &remove).unwrap();
        assert!(proposal.warnings.is_empty(), "{:?}", proposal.warnings);
        let config = atomics_core::config::Config::from_tables([proposal.after.parse().unwrap()]).unwrap();
        assert_eq!(config.chief.launch_args(), ["--verbose", "--model", "sonnet"]);
        assert_eq!(config.chief.wake_secs, 30);
        assert_eq!(config.chief.heartbeat_secs, 120);
        assert!(config.chief.autostart);
        assert_eq!(config.chief.dock, atomics_core::config::ChiefDock::Docked);
        assert_eq!(config.chief.prefer, ["claude", "codex"]);
        assert_eq!(config.chief.min_left_percent["codex"], 10.0);
        assert_eq!(config.chief.min_left_percent["claude"], 25.0);
        form.wake = 1;
        assert!(form.proposal().is_none());
        form.wake = 0;
        assert!(form.proposal().is_some());
    }

    #[test]
    fn shortcut_form_detects_client_aliases_fixed_keys_and_invalid_chords() {
        let mut values = ShortcutForm::values(&ManagementInfo::default());
        values.insert("keys.quick_open".into(), " cmd+p ".into());
        let mut form = ShortcutForm::new(values);
        assert!(form.proposal(OperatingSystem::Nix).is_none(), "opening the form preserves existing whitespace");
        assert!(form.problems(OperatingSystem::Mac).is_empty());
        assert!(form.problems(OperatingSystem::Nix).is_empty());
        form.bindings.insert("keys.quick_open".into(), "ctrl+shift+d".into());
        assert!(form.problems(OperatingSystem::Nix).iter().any(|p| p.contains("Conflict")));
        assert!(form.problems(OperatingSystem::Mac).is_empty(), "cmd differs on a Mac");
        assert!(form.proposal(OperatingSystem::Nix).is_none());
        form.bindings.insert("keys.quick_open".into(), "primary+1".into());
        assert!(form.problems(OperatingSystem::Mac).iter().any(|p| p.contains("fixed navigation")));
        form.bindings.insert("keys.quick_open".into(), "ctrl+a+b".into());
        assert!(form.problems(OperatingSystem::Mac).iter().any(|p| p.contains("Invalid")));
        form.bindings.insert("keys.quick_open".into(), String::new());
        let HostAction::ConfigPropose { toml, remove, .. } = form.proposal(OperatingSystem::Nix).unwrap() else {
            panic!("proposal")
        };
        assert_eq!(toml, "keys.quick_open = \"\"\n");
        assert!(remove.is_empty());
        assert!(atomics_core::selfconfig::edit("", &toml, &remove).is_ok());
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use atomics_core::identity::Identity;

    #[test]
    fn identity_form_preserves_inheritance_and_only_proposes_edited_keys() {
        let mut form = IdentityForm::default();
        form.select("/example/app".into(), Identity { name: Some("Project Example".into()), ..Default::default() });
        assert_eq!(form.name, "Project Example");
        assert!(form.email.is_empty() && form.words.is_empty());
        let path = vec!["identity".into(), "projects".into(), "~/app".into()];
        assert!(form.proposal(&path).is_none());
        form.email = "project@example.com".into();
        let HostAction::ConfigPropose { scope, toml, remove, .. } = form.proposal(&path).unwrap() else {
            panic!("proposal")
        };
        assert_eq!(scope, Scope::Main);
        assert!(remove.is_empty());
        assert!(toml.contains("email = \"project@example.com\""));
        assert!(!toml.contains("name =") && !toml.contains("forbidden_words"));
        let proposal = atomics_core::selfconfig::edit(
            "[identity]\nforbidden_words = ['contoso']\n[identity.projects.'~/app']\nname = 'Project Example'\n",
            &toml,
            &remove,
        )
        .unwrap();
        let config = atomics_core::config::Config::from_tables([proposal.parse().unwrap()]).unwrap();
        let resolved = config.identity.resolve("~/app", None, str::to_string);
        assert_eq!(resolved.forbidden_words, Some(vec!["contoso".into()]));
        assert_eq!(config.identity.projects["~/app"].name.as_deref(), Some("Project Example"));

        // Changing scope discards the other scope's prefill and draft.
        form.select("workspace".into(), Identity { email: Some("workspace@example.com".into()), ..Default::default() });
        assert!(form.name.is_empty() && form.words.is_empty());
        assert_eq!(form.email, "workspace@example.com");
        assert!(form.proposal(&["identity".into()]).is_none());
    }

    #[test]
    fn identity_form_clears_the_guard_only_with_the_explicit_toggle() {
        let path = vec!["identity".into()];
        let mut form = IdentityForm::default();
        form.select("workspace".into(), Identity::default());
        form.name = "Workspace Example".into();
        let HostAction::ConfigPropose { toml, .. } = form.proposal(&path).unwrap() else { panic!("proposal") };
        assert!(!toml.contains("forbidden_words"));
        form.clear_words = true;
        let HostAction::ConfigPropose { toml, .. } = form.proposal(&path).unwrap() else { panic!("proposal") };
        assert!(toml.contains("forbidden_words = []"));

        form.select(
            "workspace".into(),
            Identity {
                forbidden_words: Some(vec![" contoso ".into(), "example, internal".into()]),
                ..Default::default()
            },
        );
        assert!(form.proposal(&path).is_none(), "unchanged words are never normalized into an edit");
        form.words.clear();
        let HostAction::ConfigPropose { toml, remove, .. } = form.proposal(&path).unwrap() else { panic!("proposal") };
        assert!(toml.is_empty());
        assert_eq!(
            remove,
            ["identity.forbidden_words"],
            "blank restores inheritance, never clears the inherited guard"
        );
    }
}
