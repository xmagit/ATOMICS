//! The chief's configuration tools: the effective configuration with each value's source,
//! and proposed changes the user applies or rejects. Everything returned to an agent is
//! cleaned of secrets, and only the user's click on Apply writes a file: no control
//! request can apply a proposal.
use super::*;
use imacs_core::selfconfig::{self, Layer, Proposal, Risk, Scope, Source};
use imacs_core::tickets::Cleaner;

/// Proposals waiting at once; more are refused until the user decides.
const MAX_PROPOSALS: usize = 5;

/// How long Apply stays disabled after a proposal appears, so a click meant for whatever
/// was under the pointer never applies it.
pub(super) const APPLY_DELAY: Duration = Duration::from_millis(800);

/// A proposed change waiting for the user.
pub(super) struct Pending {
    pub(super) id: String,
    /// The file to write, as found when the change was proposed.
    file: PathBuf,
    proposal: Proposal,
    reason: Option<String>,
    /// Who proposed it, as far as IMACS can tell. The user judges the diff, not the name.
    from: String,
}

impl State {
    /// The files that make up the configuration, lowest precedence first: the main file,
    /// this host's, the active workspace's, and the project's own when given.
    pub(super) fn config_layers(&self, project: Option<&Path>) -> Vec<Layer> {
        let [main, host] = Config::source_paths(&self.config_path);
        let mut layers = vec![Layer::read(Source::Main, &main), Layer::read(Source::Host, &host)];
        if let Some(file) = self.workspace.as_deref().and_then(|name| Workspace::file(name).ok()) {
            layers.push(Layer::read(Source::Workspace, &file));
        }
        layers.extend(project.map(|root| imacs_core::repo_workflow::RepoWorkflow::read(root).config));
        layers
    }

    /// The configuration IMACS runs with, the workspace's tickets included.
    pub(super) fn effective_config(&self) -> Config {
        Config { tickets: self.effective_tickets(), workflow: self.workflow().clone(), ..self.config.clone() }
    }

    /// The cleaner for everything the configuration tools return: built from the secrets
    /// the configuration runs with, read from the environment IMACS already knows.
    pub(super) fn config_cleaner(&self, layers: &[Layer]) -> Cleaner {
        Cleaner::new(selfconfig::secrets(&self.effective_config(), layers, crate::setup::known_env_var))
    }

    /// `imacs_config_get`: every effective value with its source, the files involved, and
    /// one project's settings when given. Secret values never leave this function.
    pub(super) fn config_get(&self, project: Option<&str>, key: Option<&str>) -> Result<serde_json::Value, String> {
        let project = match project.map(str::trim).filter(|p| !p.is_empty()) {
            Some(path) => Some(workspace::project_root(&paths::expand(path)).map_err(|err| format!("{err:#}"))?),
            None => None,
        };
        let layers = self.config_layers(project.as_deref());
        let cleaner = self.config_cleaner(&layers);
        let mut effective = self.effective_config();
        if let Some(layer) = layers.iter().find(|layer| layer.source == Source::Repo) {
            let repo = imacs_core::repo_workflow::RepoWorkflow { config: layer.clone(), ..Default::default() };
            effective = repo.apply(&effective);
        }
        let files: Vec<_> = layers
            .iter()
            .map(|layer| {
                json!({
                    "source": layer.source,
                    "path": layer.path,
                    "exists": layer.text.is_some(),
                    "error": layer.error.as_deref().map(|error| cleaner.text(error)),
                })
            })
            .collect();
        let workspace = self.workspace.as_ref().map(|name| {
            json!({
                "name": name,
                "projects": self.open_projects.iter().map(|p| paths::display(p)).collect::<Vec<_>>(),
            })
        });
        let project = project.map(|root| {
            json!({
                "path": paths::display(&root),
                "open": self.open_projects.contains(&root) || self.repos.contains(&root),
                "sources": layers.iter().map(|layer| layer.source).collect::<Vec<_>>(),
                "workflow_files": imacs_core::repo_workflow::RepoWorkflow::read(&root).instructions.iter().map(|(file, _)| file).collect::<Vec<_>>(),
                "warnings": imacs_core::repo_workflow::RepoWorkflow::read(&root).warnings,
                "new_worktree_example": paths::display(&git::worktree_path(&effective.worktree_path, &root, "example")),
            })
        });
        let load_error = selfconfig::load_error(&layers).map(|error| cleaner.text(&error));
        let sections: Vec<_> = selfconfig::prompt_sections(&layers)
            .into_iter()
            .map(|(source, text)| json!({ "source": source, "text": cleaner.text(&text) }))
            .collect();
        Ok(json!({
            "host": paths::host_name(),
            "merge": "Built-in defaults, then config.toml, then config.d/<host>.toml: tables merge key by key, arrays and other values replace. A workspace's [tickets] and [workflow] replace each section as a whole. A project overlays allowlisted .imacs/config.toml settings; its [workflow] replaces the workspace workflow.",
            "files": files,
            "workspace": workspace,
            "project": project,
            "load_error": load_error,
            "running_on_last_good_config": self.config_error.is_some(),
            "values": selfconfig::values(&effective, &layers, &cleaner, key),
            "chief_prompt": {
                "built_in": effective.chief.system_prompt == imacs_core::config::CHIEF_PROMPT,
                "sections": sections,
                "composed": "The built-in prompt (or chief.system_prompt), then the ticket workflow, then each [chief] prompt section: main, host, workspace; then a bounded section per project from IMACS.md, .imacs/*.md and declarative config. Read when the chief starts.",
            },
            "secrets": "Values of secret-looking names, credential flags and URL credentials read as ***. Ticket and voice keys stay in the environment variables token_env and api_key_env name.",
        }))
    }
}

impl State {
    /// `imacs_config_propose`: checks a change and holds it for the user. Nothing is written
    /// here; the user applies or rejects it in the window, and the outcome becomes a `config` event.
    pub(super) fn propose_config(
        &mut self,
        scope: Scope,
        toml: &str,
        remove: &[String],
        reason: Option<String>,
        pane: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        if self.proposals.len() >= MAX_PROPOSALS {
            return Err(format!(
                "{} config changes already wait for the user; wait for their config events first",
                self.proposals.len()
            ));
        }
        let layers = self.config_layers(None);
        let cleaner = self.config_cleaner(&layers);
        let file = match scope {
            Scope::Main => self.config_path.clone(),
            Scope::Host => Config::source_paths(&self.config_path)[1].clone(),
            Scope::Workspace => match self.workspace.as_deref().map(Workspace::file) {
                Some(file) => file.map_err(|err| format!("{err:#}"))?,
                // `propose` explains that no workspace is open.
                None => PathBuf::new(),
            },
        };
        let mut proposal = selfconfig::propose(scope, &layers, toml, remove).map_err(|error| cleaner.text(&error))?;
        if scope == Scope::Workspace && file.parent() != Some(workspace::workspaces_dir().as_path()) {
            proposal.warnings.push(format!("{} is a flat workspace file that every host shares", proposal.file));
        }
        self.proposal_seq += 1;
        let id = format!("config-{}", self.proposal_seq);
        let from = match pane.and_then(|pane| self.sessions.iter().find(|s| s.id == pane)) {
            Some(session) if session.chief => "the chief".to_string(),
            Some(session) => session.title.clone(),
            None => "a client of the IMACS socket".to_string(),
        };
        let reply = cleaner.setting(json!({
            "proposal": id,
            "scope": scope,
            "file": proposal.file,
            "changes": proposal.changes,
            "warnings": proposal.warnings,
            "next": "Nothing is written until the user applies the change in IMACS. Its outcome (applied, rejected or failed) arrives as a config event in imacs_wait.",
        }));
        self.toast(format!("Config change proposed for {}: review it", proposal.file));
        self.proposals.push(Pending { id, file, proposal, reason: reason.filter(|r| !r.trim().is_empty()), from });
        Ok(reply)
    }

    /// Writes a proposal the user applied, after a backup, and reloads the config.
    pub(super) fn apply_proposal(&mut self, id: &str, note: String) {
        let Some(index) = self.proposals.iter().position(|pending| pending.id == id) else { return };
        let pending = self.proposals.remove(index);
        let proposal = &pending.proposal;
        let written =
            selfconfig::write(&pending.file, proposal.before.as_deref(), &proposal.after, imacs_core::store::now());
        let outcome = match written {
            Ok(backup) => {
                self.reload_config();
                self.toast(format!("Config change applied to {}", proposal.file));
                let chief = proposal.changes.iter().any(|change| {
                    ["chief.", "tickets.", "workflow.", "base_ref", "repos"]
                        .iter()
                        .any(|prefix| change.key.starts_with(prefix))
                });
                json!({
                    "outcome": "applied",
                    "backup": backup.map(|backup| paths::display(&backup)),
                    "load_error": self.config_error.is_some().then_some("the config no longer loads; IMACS keeps the last good one"),
                    "chief": chief.then_some("the chief's prompt and launch settings take effect when the chief starts again"),
                })
            }
            Err(err) => {
                self.toast(format!("Config change not applied: {err:#}"));
                json!({ "outcome": "failed", "error": format!("{err:#}") })
            }
        };
        self.config_event(&pending, outcome, note);
    }

    pub(super) fn reject_proposal(&mut self, id: &str, note: String) {
        let Some(index) = self.proposals.iter().position(|pending| pending.id == id) else { return };
        let pending = self.proposals.remove(index);
        self.config_event(&pending, json!({ "outcome": "rejected" }), note);
    }

    /// The user's decision on a proposal, for `imacs_wait`.
    fn config_event(&mut self, pending: &Pending, mut outcome: serde_json::Value, note: String) {
        outcome["proposal"] = json!(pending.id);
        outcome["scope"] = json!(pending.proposal.scope);
        outcome["file"] = json!(pending.proposal.file);
        if !note.trim().is_empty() {
            outcome["note"] = json!(note.trim());
        }
        self.event_seq += 1;
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: String::new(),
            title: format!("Config change {}", pending.id),
            status: AgentStatus::Idle,
            at: imacs_core::store::now(),
            plan: None,
            task: None,
            report: None,
            approval: None,
            findings: None,
            ticket: None,
            config: Some(outcome),
        });
        self.serve_waiters();
    }
}

impl Desktop<'_> {
    /// The oldest proposed change, as a diff with Apply and Reject. Settings that run
    /// commands, send credentials or instruct agents are labeled above the diff.
    pub(super) fn config_proposal_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.runtime.proposals.first() else {
            self.client.config_seen = None;
            return;
        };
        let p = self.runtime.palette.clone();
        let seen = match &self.client.config_seen {
            Some((id, at)) if *id == pending.id => *at,
            _ => {
                self.client.config_note.clear();
                let now = Instant::now();
                self.client.config_seen = Some((pending.id.clone(), now));
                now
            }
        };
        let ready = seen.elapsed() >= APPLY_DELAY;
        if !ready {
            ctx.request_repaint_after(APPLY_DELAY - seen.elapsed());
        }
        let count = self.runtime.proposals.len();
        let mut decision = None;
        let note = &mut self.client.config_note;
        let screen = ctx.content_rect();
        egui::Window::new("Proposed config change")
            .id(egui::Id::NULL.with("config-proposal"))
            .collapsible(true)
            .resizable(true)
            .default_size(vec2(screen.width().min(820.0), (screen.height() * 0.7).min(760.0)))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                let proposal = &pending.proposal;
                // Who, what and its risks above, the decision below; the diff scrolls between them.
                egui::Panel::top("config-proposal-head").frame(egui::Frame::new()).show(ui, |ui| {
                    let of = if count > 1 { format!(" · 1 of {count}") } else { String::new() };
                    ui.label(
                        RichText::new(format!("{} · {} scope{of}", proposal.file, proposal.scope.name())).strong(),
                    );
                    ui.label(match &pending.reason {
                        Some(reason) => format!("From {}: {reason}", pending.from),
                        None => format!("From {}", pending.from),
                    });
                    if proposal.before.is_none() {
                        ui.label(RichText::new("Creates this file.").color(p.muted));
                    }
                    for (risk, keys) in proposal.risks() {
                        let color = if risk == Risk::RunsCommands { p.error } else { p.warning };
                        let text = format!("⚠ {}: {}", risk.label(), keys.join(", "));
                        ui.label(RichText::new(text).color(color).strong());
                    }
                    for warning in &proposal.warnings {
                        ui.label(RichText::new(warning).color(p.warning).size(12.0));
                    }
                    ui.add_space(4.0);
                });
                egui::Panel::bottom("config-proposal-actions").frame(egui::Frame::new()).show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let apply = egui::Button::new(RichText::new("Apply").color(Color32::WHITE))
                            .fill(p.accent.gamma_multiply(0.6));
                        if ui
                            .add_enabled(ready, apply)
                            .on_hover_text("Back up the file, write the change and reload")
                            .clicked()
                        {
                            decision = Some(true);
                        }
                        if ui.button("Reject").clicked() {
                            decision = Some(false);
                        }
                        ui.add(
                            egui::TextEdit::singleline(note)
                                .desired_width(ui.available_width())
                                .hint_text("Note for the chief (optional)"),
                        );
                    });
                });
                egui::CentralPanel::default().frame(egui::Frame::new().fill(p.background)).show(ui, |ui| {
                    egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                        for line in proposal.diff.lines() {
                            let text = RichText::new(line).monospace().size(12.0);
                            let text = if line.starts_with("+++") || line.starts_with("---") {
                                text.color(p.muted)
                            } else if line.starts_with('+') {
                                text.color(p.foreground).background_color(p.added)
                            } else if line.starts_with('-') {
                                text.color(p.foreground).background_color(p.removed)
                            } else if line.starts_with('@') {
                                text.color(p.info)
                            } else {
                                text.color(p.muted)
                            };
                            ui.label(text);
                        }
                    });
                });
            });
        if let Some(apply) = decision {
            let id = pending.id.clone();
            let note = std::mem::take(&mut self.client.config_note);
            self.post(move |state| {
                if apply {
                    state.apply_proposal(&id, note);
                } else {
                    state.reject_proposal(&id, note);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, with_app};
    use super::*;

    fn render(app: &mut Desktop<'_>, ctx: &egui::Context, events: Vec<egui::Event>) -> Vec<(String, Pos2)> {
        let time = ctx.input(|i| i.time) + 0.1;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                time: Some(time),
                events,
                ..Default::default()
            },
            |ui| app.dialogs(ui.ctx()),
        );
        app.runtime.drain_actions();
        let texts = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => {
                    Some((text.galley.text().to_string(), text.pos + text.galley.rect.center().to_vec2()))
                }
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        texts
    }

    fn click(app: &mut Desktop<'_>, ctx: &egui::Context, label: &str) {
        render(app, ctx, vec![]);
        let texts = render(app, ctx, vec![]);
        let pos = texts.iter().find(|(text, _)| text == label).unwrap_or_else(|| panic!("no {label}: {texts:?}")).1;
        for pressed in [true, false] {
            let button = egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            };
            render(app, ctx, vec![egui::Event::PointerMoved(pos), button]);
        }
    }

    fn wait_for_config(app: &mut Desktop<'_>, ctx: &egui::Context) -> std::sync::mpsc::Receiver<ControlOutcome> {
        let (tx, rx) = std::sync::mpsc::channel();
        let after = app.runtime.event_seq;
        let wait =
            ControlRequest::Wait { sessions: vec![], statuses: vec![], after: Some(after), timeout_secs: Some(30) };
        app.runtime.control(ctx, wait, tx);
        assert!(rx.try_recv().is_err(), "nothing happened yet");
        rx
    }

    fn event(rx: &std::sync::mpsc::Receiver<ControlOutcome>) -> serde_json::Value {
        let Ok(ControlOutcome::Done(response)) = rx.recv_timeout(Duration::from_secs(2)) else { panic!("no event") };
        assert_eq!(response.result["events"][0]["status"], "config", "{}", response.result);
        response.result["events"][0]["config"].clone()
    }

    fn backups(dir: &Path) -> Vec<PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "bak"))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn config_get_exposes_project_settings_and_workflow_sources() {
        with_app("app::selfconfig::tests::config_get_exposes_project_settings_and_workflow_sources", |app, _| {
            let (_dir, repo) = super::super::tests::repository();
            let [main, host] = Config::source_paths(&app.runtime.config_path);
            std::fs::write(&main, "[workflow]\ndone = 'merged'\n").unwrap();
            app.runtime.config = Config::load(&main).unwrap();
            let get = |app: &mut Desktop<'_>| request(app, json!({"cmd":"config_get", "project": repo})).result;
            assert_eq!(get(app)["values"]["workflow.done"], json!({"value":"merged", "source":"main"}));
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            std::fs::write(&host, "[workflow]\ndone = 'ci_passed'\n").unwrap();
            app.runtime.config = Config::load(&main).unwrap();
            assert_eq!(get(app)["values"]["workflow.done"], json!({"value":"ci_passed", "source":"host"}));
            app.runtime.workspace = Some("repo-team".into());
            let file = Workspace::file("repo-team").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                &file,
                "projects = []\n[tickets]\nbranch = 'workspace/{slug}'\n[workflow]\ndone = 'pr_opened'\n",
            )
            .unwrap();
            app.runtime.apply_ticket_config();
            assert_eq!(get(app)["values"]["workflow.done"], json!({"value":"pr_opened", "source":"workspace"}));
            std::fs::create_dir(repo.join(".imacs")).unwrap();
            std::fs::write(repo.join(".imacs/config.toml"), "base_ref = 'main'\npr_title = '#{key} - {title}'\ntest_commands = ['cargo test']\n[workflow]\ndone = 'qa_passed'\nstages = ['accepted', 'qa_passed']\n[tickets]\nbranch = 'repo/{slug}'\nserver.command = ['evil']\n").unwrap();
            let got = get(app);
            for key in ["workflow.done", "workflow.stages", "tickets.branch", "base_ref", "pr_title", "test_commands"] {
                assert_eq!(got["values"][key]["source"], "repo", "{key}: {got}");
            }
            assert_eq!(got["values"]["workflow.done"]["value"], "qa_passed");
            assert_eq!(got["values"]["tickets.branch"]["value"], "repo/{slug}");
            assert!(
                got["project"]["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w.as_str().unwrap().contains("ignored tickets.server"))
            );
            assert!(!got.to_string().contains("evil"));
            assert_eq!(got["files"][3]["source"], "repo");
            assert_eq!(got["files"][3]["exists"], true);
            let without_project = request(app, json!({"cmd":"config_get", "key":"workflow"})).result;
            assert_eq!(without_project["values"]["workflow.done"]["source"], "workspace");
            let proposal = request(
                app,
                json!({"cmd":"config_propose", "scope":"workspace", "toml":"[workflow]\ndone = 'merged'"}),
            );
            assert!(proposal.ok, "{proposal:?}");
            assert!(std::fs::read_to_string(file).unwrap().contains("done = 'pr_opened'"));
        });
    }

    #[test]
    fn chief_prompt_extensions_follow_main_host_and_workspace_order() {
        with_app("app::selfconfig::tests::chief_prompt_extensions_follow_main_host_and_workspace_order", |app, _| {
            let [main, host] = Config::source_paths(&app.runtime.config_path);
            let base = app.runtime.chief_prompt();
            assert!(!base.contains("The user's own instructions"));
            std::fs::write(&main, "[chief]\nprompt = 'MAIN: keep briefs to five lines.'\n").unwrap();
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            std::fs::write(&host, "[chief]\nprompt = 'HOST: this laptop has 8 cores.'\n").unwrap();
            app.runtime.reload_config();
            app.runtime.workspace = Some("team".into());
            let file = Workspace::file("team").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                &file,
                "projects = []\n[tickets]\nsystem = 'relay'\nserver = { command = ['relay-mcp'] }\n\
                 workflow = 'TICKETS: one board per plan.'\n",
            )
            .unwrap();
            app.runtime.apply_ticket_config();

            // The workspace's section arrives through a proposal the user applies.
            let proposed = request(
                app,
                json!({"cmd":"config_propose", "scope":"workspace", "toml":"[chief]\nprompt = 'WORKSPACE: ask before touching the API.'"}),
            );
            assert!(proposed.ok, "{:?}", proposed.error);
            assert_eq!(
                proposed.result["changes"],
                json!([{"key":"chief.prompt", "change":"added", "risk":"instructs_agents"}])
            );
            assert_eq!(proposed.result["warnings"], json!([]), "this host's own workspace file");
            let id = app.runtime.proposals[0].id.clone();
            app.runtime.apply_proposal(&id, String::new());
            assert!(std::fs::read_to_string(&file).unwrap().contains("[tickets]\nsystem = 'relay'"));

            let prompt = app.runtime.chief_prompt();
            assert!(prompt.starts_with(&base[..200]), "the built-in prompt comes first");
            let at = |text: &str| prompt.find(text).unwrap_or_else(|| panic!("{text} missing from {prompt}"));
            assert!(at("TICKETS:") < at("The user's own instructions"));
            assert!(at("MAIN:") < at("HOST:") && at("HOST:") < at("WORKSPACE:"), "{prompt}");
            assert!(prompt.ends_with("WORKSPACE: ask before touching the API."));

            let got = request(app, json!({"cmd":"config_get", "key":"chief"}));
            let sections = &got.result["chief_prompt"]["sections"];
            assert_eq!(
                sections,
                &json!([
                    {"source":"main", "text":"MAIN: keep briefs to five lines."},
                    {"source":"host", "text":"HOST: this laptop has 8 cores."},
                    {"source":"workspace", "text":"WORKSPACE: ask before touching the API."}
                ])
            );
            assert_eq!(got.result["chief_prompt"]["built_in"], true);
            assert!(got.result["values"].get("chief.prompt").is_none());

            // A flat workspace file is shared by every host, and the proposal says so.
            std::fs::remove_file(&file).unwrap();
            let flat = file.parent().unwrap().parent().unwrap().join("team.toml");
            std::fs::write(&flat, "projects = []\n").unwrap();
            let proposed =
                request(app, json!({"cmd":"config_propose", "scope":"workspace", "toml":"[chief]\nprompt = 'x'"}));
            assert!(
                proposed.result["warnings"][0].as_str().unwrap().contains("every host shares"),
                "{}",
                proposed.result
            );
        });
    }

    #[test]
    fn proposals_wait_for_the_user_then_back_up_keep_comments_and_reload() {
        with_app(
            "app::selfconfig::tests::proposals_wait_for_the_user_then_back_up_keep_comments_and_reload",
            |app, ctx| {
                let main = app.runtime.config_path.clone();
                let folder = main.parent().unwrap().to_path_buf();
                std::fs::write(&main, imacs_core::config::DEFAULT_CONFIG).unwrap();
                app.runtime.reload_config();
                app.runtime.sessions =
                    vec![super::super::tests::session("boss", AgentKind::Claude, AgentStatus::Working, &folder)];
                app.runtime.sessions[0].chief = true;

                let invalid = request(
                    app,
                    json!({"cmd":"config_propose", "scope":"main", "toml":"[layout]\nmin_tile_width = 'wide'"}),
                );
                assert_eq!(
                    invalid.error.as_deref(),
                    Some(
                        "the config would not load: layout.min_tile_width: invalid type: string \"wide\", expected f32"
                    )
                );
                assert!(app.runtime.proposals.is_empty());

                let events = wait_for_config(app, ctx);
                let proposed = request(
                    app,
                    json!({"cmd":"config_propose", "scope":"main", "pane":"boss", "reason":"Narrower tiles and an aider agent",
                       "toml":"[layout]\nmin_tile_width = 400.0\n[[agents]]\nname = 'aider'\ncommand = ['aider', '--yes']"}),
                );
                assert!(proposed.ok, "{:?}", proposed.error);
                let id = proposed.result["proposal"].as_str().unwrap().to_string();
                let changes = proposed.result["changes"].as_array().unwrap();
                assert!(
                    changes.contains(&json!({"key":"agents", "change":"changed", "risk":"runs_commands"})),
                    "{changes:?}"
                );
                assert!(changes.contains(&json!({"key":"layout.min_tile_width", "change":"changed"})), "{changes:?}");
                assert!(proposed.result.get("diff").is_none(), "the diff, with its context lines, stays with the user");
                // Nothing is written before the user applies it.
                assert_eq!(std::fs::read_to_string(&main).unwrap(), imacs_core::config::DEFAULT_CONFIG);
                assert!(backups(&folder).is_empty());
                assert!(events.try_recv().is_err());
                assert!(
                    app.runtime.restart_blockers().iter().any(|blocker| blocker.contains(&id)),
                    "pending changes block a restart"
                );

                // A new window's first frame only measures it.
                render(app, ctx, vec![]);
                let shown = render(app, ctx, vec![]);
                let shown: Vec<&str> = shown.iter().map(|(text, _)| text.as_str()).collect();
                assert!(shown.contains(&"⚠ runs commands: agents"), "{shown:?}");
                assert!(shown.contains(&"From the chief: Narrower tiles and an aider agent"), "{shown:?}");
                assert!(shown.iter().any(|line| line.starts_with("+min_tile_width = 400.0")), "{shown:?}");
                assert!(shown.iter().any(|line| line.starts_with("-min_tile_width = 520.0")), "{shown:?}");

                // Apply waits a moment after the proposal appears, so a stray click does nothing.
                click(app, ctx, "Apply");
                assert_eq!(app.runtime.proposals.len(), 1);
                assert_eq!(std::fs::read_to_string(&main).unwrap(), imacs_core::config::DEFAULT_CONFIG);
                app.client.config_seen.as_mut().unwrap().1 -= APPLY_DELAY;
                app.client.config_note = "Looks right".into();
                click(app, ctx, "Apply");
                assert!(app.runtime.proposals.is_empty());
                let applied = event(&events);
                assert_eq!(applied["outcome"], "applied");
                assert_eq!(applied["proposal"], id);
                assert_eq!(applied["note"], "Looks right");
                let backup = backups(&folder);
                assert_eq!(backup.len(), 1);
                assert_eq!(applied["backup"], paths::display(&backup[0]));
                assert_eq!(std::fs::read_to_string(&backup[0]).unwrap(), imacs_core::config::DEFAULT_CONFIG);
                let written = std::fs::read_to_string(&main).unwrap();
                assert!(
                    written.contains("min_tile_width = 400.0      # below this a terminal tile collapses into a tab")
                );
                assert!(
                    written
                        .contains("# Every diff is taken against the merge-base of this ref and the worktree's HEAD,")
                );
                assert!(written.contains("# BEGIN RELAY PROFILE"), "commented profiles stay");
                assert_eq!(app.runtime.config.layout.min_tile_width, 400.0, "applied changes reload at once");
                assert_eq!(app.runtime.config.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["aider"]);

                // Rejecting writes nothing, and the note reaches the chief.
                let events = wait_for_config(app, ctx);
                let host = Config::source_paths(&main)[1].clone();
                assert!(request(app, json!({"cmd":"config_propose", "scope":"host", "toml":"[font]\nsize = 15.0"})).ok);
                // A note belongs to the proposal on screen: a new one starts without it.
                app.client.config_note = "Typed for the last one".into();
                render(app, ctx, vec![]);
                assert!(app.client.config_note.is_empty());
                app.client.config_note = "Not on this machine".into();
                click(app, ctx, "Reject");
                let rejected = event(&events);
                assert_eq!(
                    (rejected["outcome"].as_str(), rejected["scope"].as_str()),
                    (Some("rejected"), Some("host"))
                );
                assert_eq!(rejected["note"], "Not on this machine");
                assert!(!host.exists());

                // A file edited after the proposal is never overwritten.
                let events = wait_for_config(app, ctx);
                assert!(
                    request(app, json!({"cmd":"config_propose", "scope":"main", "remove":["layout.min_tile_width"]}))
                        .ok
                );
                std::fs::write(&main, format!("{written}\n# edited by hand\n")).unwrap();
                let id = app.runtime.proposals[0].id.clone();
                app.runtime.apply_proposal(&id, String::new());
                let failed = event(&events);
                assert_eq!(failed["outcome"], "failed");
                assert!(failed["error"].as_str().unwrap().contains("changed since"), "{failed}");
                assert!(std::fs::read_to_string(&main).unwrap().ends_with("# edited by hand\n"));
                assert_eq!(backups(&folder).len(), 1);

                // Proposals are bounded while the user is away.
                for size in 0..MAX_PROPOSALS {
                    let toml = format!("[font]\nsize = {}.0", 14 + size);
                    assert!(request(app, json!({"cmd":"config_propose", "scope":"host", "toml":toml})).ok);
                }
                let refused =
                    request(app, json!({"cmd":"config_propose", "scope":"host", "toml":"[font]\nsize = 30.0"}));
                assert!(refused.error.unwrap().contains("already wait for the user"));
                assert_eq!(app.runtime.proposals[0].from, "a client of the IMACS socket");
            },
        );
    }

    #[test]
    fn config_get_shows_sources_and_never_secret_values() {
        with_app("app::selfconfig::tests::config_get_shows_sources_and_never_secret_values", |app, _| {
            let [main, host] = Config::source_paths(&app.runtime.config_path);
            std::fs::write(
                &main,
                "base_ref = 'main'\n[voice]\napi_key = 'sk-main-voice-secret'\n[tickets]\nsystem = 'relay'\n\
                 server = { command = ['relay-mcp', '--token', 'cli-token-secret'], env = { RELAY_TOKEN = 'env-token-secret', PROFILE = 'work' } }\n",
            )
            .unwrap();
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            std::fs::write(&host, "[layout]\nsidebar_width = 333.0\n").unwrap();
            app.runtime.config = Config::load(&main).unwrap();
            app.runtime.workspace = Some("team".into());
            let file = Workspace::file("team").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                &file,
                "projects = []\n[tickets]\nsystem = 'linear'\nserver = { url = 'https://linear.example/mcp', token_env = 'LINEAR_TOKEN' }\n",
            )
            .unwrap();
            app.runtime.apply_ticket_config();

            let response = request(app, json!({"cmd":"config_get"}));
            assert!(response.ok, "{:?}", response.error);
            let result = response.result;
            let text = result.to_string();
            for secret in ["sk-main-voice-secret", "cli-token-secret", "env-token-secret"] {
                assert!(!text.contains(secret), "{secret} leaked: {text}");
            }
            let values = &result["values"];
            assert_eq!(values["base_ref"], json!({"value": "main", "source": "main"}));
            assert_eq!(values["layout.sidebar_width"], json!({"value": 333.0, "source": "host"}));
            assert_eq!(values["layout.diff_min_width"]["source"], "default");
            assert_eq!(values["voice.api_key"], json!({"value": "***", "source": "main"}));
            // The workspace's section replaces the main config's as a whole.
            assert_eq!(values["tickets.system"], json!({"value": "linear", "source": "workspace"}));
            assert_eq!(values["tickets.server.token_env"]["value"], "LINEAR_TOKEN");
            assert!(values.get("tickets.server.env.PROFILE").is_none());
            let sources: Vec<_> = result["files"].as_array().unwrap().iter().map(|f| f["source"].clone()).collect();
            assert_eq!(sources, [json!("main"), json!("host"), json!("workspace")]);
            assert_eq!(result["files"][1]["exists"], true);

            // Without the workspace, the main config's tickets apply, still masked.
            std::fs::write(&file, "projects = []\n").unwrap();
            app.runtime.apply_ticket_config();
            let response = request(app, json!({"cmd":"config_get", "key":"tickets.server"}));
            let values = &response.result["values"];
            assert_eq!(values["tickets.server.env.PROFILE"], json!({"value": "work", "source": "main"}));
            assert_eq!(values["tickets.server.env.RELAY_TOKEN"]["value"], "***");
            assert_eq!(values["tickets.server.command"]["value"], json!(["relay-mcp", "--token", "***"]));
            assert!(values.as_object().unwrap().keys().all(|key| key.starts_with("tickets.server.")));
            for secret in ["cli-token-secret", "env-token-secret"] {
                assert!(!response.result.to_string().contains(secret), "{}", response.result);
            }

            // A broken host file is reported by place, without quoting its line.
            std::fs::write(&host, "[layout]\nsidebar_width = sk-unquoted-secret\n").unwrap();
            let response = request(app, json!({"cmd":"config_get"}));
            let error = response.result["files"][1]["error"].as_str().unwrap();
            assert!(error.starts_with("line 2"), "{error}");
            assert!(!response.result.to_string().contains("sk-unquoted-secret"));

            let (_dir, repo) = super::super::tests::repository();
            let response = request(app, json!({"cmd":"config_get", "project": repo, "key":"base_ref"}));
            assert!(response.ok, "{:?}", response.error);
            assert_eq!(response.result["project"]["path"], paths::display(&repo.canonicalize().unwrap()));
            assert_eq!(response.result["project"]["sources"], json!(["main", "host", "workspace", "repo"]));
            assert!(!request(app, json!({"cmd":"config_get", "project": "/nonexistent/imacs"})).ok);
        });
    }
}
