//! Settings → Connections and each sandboxed tile's "Connect <name>" banner. sbx, the browser and
//! the OS keychain are used on worker threads; the state keeps only what they report, never a
//! value or sbx's output. Only the user's click registers, authorizes or loads a server: an agent
//! that asks for one (`imacs_mcp_connect`) only raises its tile's banner, since a connected server
//! reaches every sandbox.
use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::connections::{self as mcp, Registered, Status};
use imacs_core::mcp_import::{self, Definition};

#[derive(Default)]
pub(super) struct Connections {
    /// sbx's servers as last read, or why they could not be read; `None` before the first read.
    registered: Option<Result<Vec<Registered>, String>>,
    /// What Import found and servers added by URL, from `connections.json`.
    definitions: Vec<Definition>,
    /// Import's notes on files it could not read.
    notes: Vec<String>,
    /// Servers with a connect or disconnect under way.
    busy: BTreeSet<String>,
    /// Each server's last failure, until it connects.
    errors: BTreeMap<String, String>,
    refreshing: bool,
    importing: bool,
    jobs: Vec<Receiver<Done>>,
    /// How authorization pages open, and the config files Import reads; tests use stand-ins.
    browser: Option<mcp::Browser>,
    sources: Option<mcp::Sources>,
}

/// An MCP server a sandboxed session needs that is not connected, shown on its tile's banner.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Needed {
    pub name: String,
    /// The URL the agent gave for a server IMACS knew nothing of: shown in full, and used only
    /// when the user clicks Connect.
    pub url: Option<String>,
    /// The agent asked for it, rather than its sandbox starting without it.
    pub asked: bool,
}

impl Needed {
    pub fn missing(name: String) -> Self {
        Self { name, url: None, asked: false }
    }
}

enum Done {
    Refreshed(Result<Vec<Registered>, String>, Vec<Definition>),
    Imported(Result<Vec<Definition>, String>, Vec<String>),
    Connected(String, Result<Vec<String>, String>),
    Disconnected(String, Result<(), String>),
}

/// One server in the Connections list: registered with sbx, imported, or both.
struct Row {
    name: String,
    status: Option<Status>,
    detail: String,
    /// Environment values its definition has, for a server sbx does not have yet.
    env: usize,
    skipped: usize,
}

impl Connections {
    fn status(&self, name: &str) -> Option<&Status> {
        self.registered.as_ref()?.as_ref().ok()?.iter().find(|server| server.name == name).map(|server| &server.status)
    }

    fn definition(&self, name: &str) -> Option<&Definition> {
        self.definitions.iter().find(|definition| definition.name == name)
    }

    fn rows(&self) -> Vec<Row> {
        let registered = self.registered.as_ref().and_then(|listed| listed.as_ref().ok());
        let mut rows: Vec<Row> = registered
            .into_iter()
            .flatten()
            .map(|server| {
                let mut detail = match self.definition(&server.name) {
                    Some(definition) => format!("{} · {}", definition.display(), server.transport),
                    None => server.transport.clone(),
                };
                if let Some(other) = registered.into_iter().flatten().find(|other| {
                    other.name != server.name && other.status == Status::Connected && server.same_url(other)
                }) {
                    detail.push_str(&format!(" · same URL as {}; tools loaded once per sandbox", other.name));
                }
                Row { name: server.name.clone(), status: Some(server.status.clone()), detail, env: 0, skipped: 0 }
            })
            .collect();
        for definition in &self.definitions {
            if !rows.iter().any(|row| row.name == definition.name) {
                rows.push(Row {
                    name: definition.name.clone(),
                    status: None,
                    detail: format!("{} · from {}", definition.display(), definition.source.label()),
                    env: definition.env.len(),
                    skipped: definition.skipped,
                });
            }
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// The environment values connecting `name` from its definition would store.
    fn env_for(&self, name: &str) -> usize {
        if self.status(name).is_some() { 0 } else { self.definition(name).map_or(0, |definition| definition.env.len()) }
    }

    /// The servers being connected or disconnected; a connect may wait for sign-in in the browser.
    pub(super) fn busy_names(&self) -> Vec<String> {
        self.busy.iter().cloned().collect()
    }

    fn sources(&self) -> mcp::Sources {
        self.sources.clone().unwrap_or_else(mcp::Sources::current)
    }
}

impl State {
    fn connection_job(&mut self, job: impl FnOnce() -> Done + Send + 'static) {
        let (tx, rx) = self.wake.channel();
        self.connections.jobs.push(rx);
        std::thread::spawn(move || {
            let _ = tx.send(job());
        });
    }

    /// The sandboxes of sessions that run sandboxed, which get a server as soon as it connects.
    fn session_sandboxes(&self) -> Vec<String> {
        self.sessions.iter().filter_map(|session| session.sandbox.clone()).collect()
    }

    pub(super) fn refresh_connections(&mut self) {
        if self.connections.refreshing {
            return;
        }
        self.connections.refreshing = true;
        let program = resolve_program(&self.sbx);
        self.connection_job(move || {
            let registered = mcp::registered(&program).map_err(|_| {
                "sbx's MCP servers are unavailable; check that sbx is installed and signed in".to_string()
            });
            Done::Refreshed(registered, mcp::definitions(&paths::imacs_home()))
        });
    }

    pub(super) fn import_connections(&mut self) {
        if self.connections.importing {
            return;
        }
        self.connections.importing = true;
        let sources = self.connections.sources();
        self.connection_job(move || {
            let imported = mcp::import(&sources);
            let saved =
                mcp::remember(&paths::imacs_home(), &imported.definitions).map_err(|error| format!("{error:#}"));
            Done::Imported(saved, imported.notes)
        });
    }

    /// Connects `name` on the user's click: sbx's registration, else its definition, else the URL
    /// an agent gave for it (shown on the banner the user clicked). Its environment is stored first
    /// when `store_env` (the user chose to).
    pub(super) fn connect_server(&mut self, name: &str, store_env: bool, url: Option<String>) {
        if !mcp_import::valid_name(name) || !self.connections.busy.insert(name.to_string()) {
            return;
        }
        self.connections.errors.remove(name);
        let definition = self
            .connections
            .definition(name)
            .cloned()
            .or_else(|| url.and_then(|url| mcp::added(name, &url)).filter(|definition| definition.name == name));
        let browser = self.connections.browser.clone().unwrap_or_else(mcp::Browser::current);
        let sources = self.connections.sources();
        let (program, keychain, sandboxes) =
            (resolve_program(&self.sbx), self.relay.keychain().clone(), self.session_sandboxes());
        let name = name.to_string();
        self.connection_job(move || {
            let result = (|| {
                if let Some(definition) = definition.as_ref().filter(|d| d.source == mcp_import::Source::Added) {
                    mcp::remember(&paths::imacs_home(), std::slice::from_ref(definition))?;
                }
                let job = mcp::Connect {
                    home: paths::imacs_home(),
                    sbx: program,
                    name: name.clone(),
                    definition,
                    store_env,
                    sources,
                    exe: crate::setup::executable_path()?,
                    browser,
                    sandboxes,
                };
                mcp::connect(&job, &crate::credentials::Store::new(keychain))
            })();
            Done::Connected(name, result.map_err(|error| format!("{error:#}")))
        });
    }

    /// Connects a remote server the user added by name and URL.
    pub(super) fn add_connection(&mut self, name: &str, url: &str) -> Result<String, String> {
        let definition = mcp::added(name, url).ok_or("Give a name and an https:// URL without credentials")?;
        let name = definition.name.clone();
        self.connections.definitions.retain(|known| known.name != name);
        self.connections.definitions.push(definition);
        self.connect_server(&name, false, None);
        Ok(name)
    }

    pub(super) fn disconnect_server(&mut self, name: &str) {
        if !mcp_import::valid_name(name) || !self.connections.busy.insert(name.to_string()) {
            return;
        }
        self.connections.errors.remove(name);
        let (program, keychain, sandboxes) =
            (resolve_program(&self.sbx), self.relay.keychain().clone(), self.session_sandboxes());
        let name = name.to_string();
        self.connection_job(move || {
            let store = crate::credentials::Store::new(keychain);
            let result = mcp::disconnect(&paths::imacs_home(), &program, &name, &sandboxes, &store)
                .map_err(|error| format!("{error:#}"));
            Done::Disconnected(name, result)
        });
    }

    pub(super) fn poll_connections(&mut self) -> bool {
        let mut changed = false;
        for rx in std::mem::take(&mut self.connections.jobs) {
            let done = match rx.try_recv() {
                Ok(done) => done,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.connections.jobs.push(rx);
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => continue,
            };
            changed = true;
            match done {
                Done::Refreshed(registered, definitions) => {
                    self.connections.refreshing = false;
                    self.connections.registered = Some(registered);
                    self.connections.definitions = definitions;
                }
                Done::Imported(saved, notes) => {
                    self.connections.importing = false;
                    self.connections.notes = notes;
                    match saved {
                        Ok(definitions) => {
                            let count = definitions.len();
                            self.connections.definitions = definitions;
                            self.toast(format!(
                                "Imported {count} MCP server definitions; connect the ones agents need"
                            ));
                        }
                        Err(error) => self.toast(format!("Import failed: {error}")),
                    }
                }
                Done::Connected(name, result) => {
                    self.connections.busy.remove(&name);
                    match result {
                        Ok(failed) => {
                            for session in &mut self.sessions {
                                if session.sandbox.as_ref().is_some_and(|sandbox| failed.contains(sandbox)) {
                                    if !session.mcp_needed.iter().any(|needed| needed.name == name) {
                                        session.mcp_needed.push(Needed::missing(name.clone()));
                                    }
                                } else {
                                    session.mcp_needed.retain(|needed| needed.name != name);
                                }
                            }
                            self.toast(if failed.is_empty() {
                                format!("Connected {name}: every sandboxed session gets it")
                            } else {
                                format!(
                                    "Connected {name}; loading it into {} failed, retry Connect on those tiles",
                                    failed.join(", ")
                                )
                            });
                        }
                        Err(error) => {
                            self.toast(format!("Connecting {name} failed: {error}"));
                            self.connections.errors.insert(name, error);
                        }
                    }
                    self.refresh_connections();
                }
                Done::Disconnected(name, result) => {
                    self.connections.busy.remove(&name);
                    match result {
                        Ok(()) => self.toast(format!("Disconnected {name}")),
                        Err(error) => {
                            self.toast(format!("Disconnecting {name} failed: {error}"));
                            self.connections.errors.insert(name, error);
                        }
                    }
                    self.refresh_connections();
                }
            }
        }
        changed
    }

    /// An agent asks for MCP server `server`, optionally at `url`. Nothing is registered,
    /// authorized or loaded on its word: its tile shows a "Connect <name>" banner with the URL in
    /// full, and only the user's click connects the server.
    pub(super) fn request_connection(
        &mut self,
        pane: Option<String>,
        server: &str,
        url: Option<&str>,
    ) -> ControlResponse {
        let Some(index) = self.pane_session(pane).and_then(|id| self.find(&id)) else {
            return ControlResponse::error("imacs_mcp_connect works only from an agent IMACS started");
        };
        if self.sessions[index].sandbox.is_none() {
            return ControlResponse::error(
                "this agent runs on this machine (bare): it uses the MCP servers in its own Claude Code or Codex \
                 configuration",
            );
        }
        let Some(name) = mcp_import::sbx_name(server).filter(|name| name == server.trim()) else {
            return ControlResponse::error("give the server's name: lowercase letters, digits, - and _");
        };
        if url.is_some_and(|url| mcp::added(&name, url).is_none()) {
            return ControlResponse::error("url must be an https:// URL without credentials");
        }
        if self.connections.registered.is_none() {
            self.refresh_connections();
        }
        // A server IMACS knows keeps its own registration or definition, whatever URL the agent gives.
        let known = self.connections.status(&name).is_some() || self.connections.definition(&name).is_some();
        let needed = Needed { name: name.clone(), url: url.filter(|_| !known).map(str::to_string), asked: true };
        let session = &mut self.sessions[index];
        session.mcp_needed.retain(|other| other.name != name);
        session.mcp_needed.push(needed);
        ControlResponse::ok(json!({
            "asked": true,
            "message": if known || url.is_some() {
                format!("Asked the user to connect {name} on your tile. Once they do, its tools appear in this session without a restart.")
            } else {
                format!("Asked the user to connect {name}, which IMACS does not know yet: call again with its https MCP URL as url, or the user adds it in Settings → Connections.")
            },
        }))
    }
}

/// What the Connections panel asks for.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Action {
    Refresh,
    Import,
    Connect(String, bool),
    Disconnect(String),
    Add,
}

impl Desktop<'_> {
    pub(super) fn connections_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Connections", |ui| {
            if let Some(action) = self.connections_panel(ui) {
                self.connection_action(action);
            }
        });
    }

    pub(super) fn connection_action(&mut self, action: Action) {
        match action {
            Action::Refresh => self.post(|state| state.refresh_connections()),
            Action::Import => self.post(|state| state.import_connections()),
            Action::Connect(name, store) => self.post(move |state| state.connect_server(&name, store, None)),
            Action::Disconnect(name) => self.post(move |state| state.disconnect_server(&name)),
            Action::Add => {
                let (name, url) =
                    (std::mem::take(&mut self.client.connection_name), std::mem::take(&mut self.client.connection_url));
                self.post(move |state| {
                    if let Err(error) = state.add_connection(&name, &url) {
                        state.toast(error);
                    }
                });
            }
        }
    }

    pub(super) fn connections_panel(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let p = self.runtime.palette.clone();
        let mut action = None;
        ui.set_max_width(640.0);
        let connections = &self.runtime.connections;
        if connections.registered.is_none() && !connections.refreshing {
            action = Some(Action::Refresh);
        }
        ui.label(RichText::new("MCP servers for sandboxed agents").strong());
        ui.label(
            RichText::new(
                "Every connected server loads into every sandboxed session, Ungrouped ones too, and into running \
                 ones as it connects. They run with your permissions: remote servers through sbx's gateway, \
                 command servers on this machine.",
            )
            .size(12.0)
            .color(p.muted),
        );
        match &connections.registered {
            None => {
                ui.label("Reading sbx's MCP servers…");
            }
            Some(Err(error)) => {
                ui.label(RichText::new(error).color(p.warning));
            }
            Some(Ok(_)) => {}
        }
        let rows = connections.rows();
        if rows.is_empty() && connections.registered.as_ref().is_some_and(Result::is_ok) {
            ui.label("No servers yet: import them from Claude Code and Codex, or add one by URL.");
        }
        egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
            for row in &rows {
                let busy = connections.busy.contains(&row.name);
                let (dot, color, label) = match &row.status {
                    _ if busy => ("◌", p.info, "working… finish any sign-in in your browser".to_string()),
                    Some(Status::Connected) => ("●", p.success, "connected".into()),
                    Some(Status::NeedsAuth) => ("●", p.warning, "needs sign-in".into()),
                    Some(status) => ("●", p.error, status.label()),
                    None => ("○", p.muted, "not connected".into()),
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(dot).color(color));
                    ui.label(RichText::new(&row.name).strong());
                    ui.label(RichText::new(label).color(color));
                    if busy {
                        return;
                    }
                    let connect = match &row.status {
                        None if row.env > 0 => {
                            if ui.button(format!("Connect and store {} env values", row.env)).clicked() {
                                action = Some(Action::Connect(row.name.clone(), true));
                            }
                            "Connect without them"
                        }
                        None | Some(Status::NeedsAuth) => "Connect",
                        _ => "",
                    };
                    if !connect.is_empty() && ui.button(connect).clicked() {
                        action = Some(Action::Connect(row.name.clone(), false));
                    }
                    if row.status.is_some() && ui.button("Disconnect").clicked() {
                        action = Some(Action::Disconnect(row.name.clone()));
                    }
                });
                ui.add(egui::Label::new(RichText::new(&row.detail).size(11.5).color(p.muted)).wrap());
                if row.env > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} environment values, hidden: stored in the OS keychain only if you choose",
                            row.env
                        ))
                        .size(11.5)
                        .color(p.muted),
                    );
                }
                if row.skipped > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} header or token settings not imported: they hold secrets; set them up with sbx",
                            row.skipped
                        ))
                        .size(11.5)
                        .color(p.muted),
                    );
                }
                if let Some(error) = connections.errors.get(&row.name) {
                    ui.add(egui::Label::new(RichText::new(error).size(11.5).color(p.error)).wrap());
                }
                ui.add_space(4.0);
            }
        });
        for note in &connections.notes {
            ui.label(RichText::new(note).size(11.5).color(p.warning));
        }
        ui.horizontal(|ui| {
            let import = if connections.importing { "Importing…" } else { "Import from Claude Code / Codex" };
            if ui.add_enabled(!connections.importing, egui::Button::new(import)).clicked() {
                action = Some(Action::Import);
            }
            if ui.add_enabled(!connections.refreshing, egui::Button::new("Refresh")).clicked() {
                action = Some(Action::Refresh);
            }
        });
        ui.label(
            RichText::new(
                "Import reads server definitions only, never sign-in tokens; environment values stay hidden.",
            )
            .size(11.5)
            .color(p.muted),
        );
        ui.separator();
        ui.label("Add a remote server");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.client.connection_name).hint_text("name").desired_width(110.0));
            ui.add(
                egui::TextEdit::singleline(&mut self.client.connection_url)
                    .hint_text("https://mcp.example.com/mcp")
                    .desired_width(300.0),
            );
            let valid = mcp::added(&self.client.connection_name, &self.client.connection_url).is_some();
            if ui.add_enabled(valid, egui::Button::new("Connect")).clicked() {
                action = Some(Action::Add);
            }
        });
        ui.label(
            RichText::new(
                "claude.ai connectors stay in your claude.ai account and do not reach sandboxed Claude: its \
                 setup-token can only run models. Add the service's MCP URL here instead.",
            )
            .size(11.5)
            .color(p.muted),
        );
        action
    }

    /// The buttons a tile's banner offers for the servers it needs: each connects its server,
    /// storing its environment when it has one to store (the button says so).
    /// `asker` is the session's title: the banner says which agent asked.
    pub(super) fn connect_buttons(&self, needed: &[Needed], asker: &str) -> Vec<ConnectButton> {
        let connections = &self.runtime.connections;
        needed
            .iter()
            .map(|needed| {
                let name = &needed.name;
                let env = connections.env_for(name);
                let busy = connections.busy.contains(name);
                let label = match (busy, env) {
                    (true, _) => format!("Connecting {name}…"),
                    (false, 0) => format!("Connect {name}"),
                    (false, env) => format!("Connect {name} (stores {env} env values)"),
                };
                let checked = connections.registered.is_some();
                let what = match (connections.status(name), connections.definition(name), &needed.url) {
                    (Some(status), _, _) => format!("registered with sbx, {}", status.label()),
                    (None, Some(definition), _) => {
                        format!("{} from {}", definition.display(), definition.source.label())
                    }
                    (None, None, Some(url)) => url.clone(),
                    (None, None, None) if !checked => "checking sbx…".into(),
                    (None, None, None) => "unknown to IMACS: add its URL in Settings → Connections".into(),
                };
                let detail = if needed.asked { format!("asked by {asker}: {what}") } else { what };
                let enabled = !busy
                    && checked
                    && (connections.status(name).is_some()
                        || connections.definition(name).is_some()
                        || needed.url.is_some());
                ConnectButton {
                    name: name.clone(),
                    label,
                    detail,
                    url: needed.url.clone(),
                    store_env: env > 0,
                    enabled,
                }
            })
            .collect()
    }
}

pub(super) struct ConnectButton {
    pub name: String,
    pub label: String,
    /// What connecting it would use, and who asked.
    pub detail: String,
    pub url: Option<String>,
    pub store_env: bool,
    pub enabled: bool,
}

#[cfg(test)]
#[path = "connections_tests.rs"]
mod tests;
