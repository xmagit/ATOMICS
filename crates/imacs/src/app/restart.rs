use super::*;
use imacs_core::store::RestartState;

impl State {
    pub(super) fn restart_blockers(&self) -> Vec<String> {
        let mut blockers = Vec::new();
        for session in &self.sessions {
            // The chief is executing this request; its own turn is the handoff.
            if !session.chief && matches!(session.status, AgentStatus::Working | AgentStatus::Starting) {
                blockers.push(format!("agent {} ({}) is {}", session.title, session.id, session.status.label()));
            }
            if session.status == AgentStatus::NeedsInput || session.approval_hold || !session.approvals.is_empty() {
                blockers.push(format!("approval or question pending for {} ({})", session.title, session.id));
            }
            if session.pending.is_some() || !session.held.is_empty() {
                blockers.push(format!("undelivered message for {} ({})", session.title, session.id));
            }
        }
        for session in &self.pending_questions {
            blockers.push(format!("question pending for {session}"));
        }
        let (running, waiting) = self.governor.snapshot();
        for ticket in &running {
            blockers.push(format!("queue slot held: {} ({})", ticket.label, ticket.id));
        }
        for ticket in &waiting {
            blockers.push(format!("queued work waiting: {} ({})", ticket.label, ticket.id));
        }
        if !self.creating.is_empty() || !self.pending_codex.is_empty() || !self.pending_sandbox.is_empty() {
            blockers.push(format!(
                "worktree or agent launch running: {}",
                self.creating.iter().map(|(_, branch, _)| branch.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.integrating.is_empty() || !self.integration_checks.is_empty() {
            blockers.push("integration running".into());
        }
        if self.cleanup.as_ref().is_some_and(|cleanup| cleanup.busy.is_some() || cleanup.rows.is_none())
            || self.plan_cleanup.as_ref().is_some_and(|cleanup| cleanup.busy)
        {
            blockers.push("worktree cleanup job running".into());
        }
        if self.plan_edit_open {
            blockers.push("unsaved plan edit is open; finish editing the plan first".into());
        }
        for pending in &self.proposals {
            blockers.push(format!("config change {} waits for the user to apply or reject it", pending.id));
        }
        blockers
    }

    pub(super) fn restart(&mut self, force: bool, reason: Option<String>) -> ControlResponse {
        self.restart_using(force, reason, crate::restart::Target::current)
    }

    fn restart_using(
        &mut self,
        force: bool,
        reason: Option<String>,
        target: impl FnOnce() -> anyhow::Result<crate::restart::Target>,
    ) -> ControlResponse {
        let result = (|| -> anyhow::Result<()> {
            if self.hook_error.is_some() {
                anyhow::bail!("this instance does not own its control socket");
            }
            let blockers = self.restart_blockers();
            if !force && !blockers.is_empty() {
                anyhow::bail!("restart refused:\n- {}", blockers.join("\n- "));
            }
            let target = target()?;
            target.validate()?;
            self.governor.begin_restart(force)?;
            let saved = self.save_restart(reason);
            if let Err(error) = saved.and_then(|()| {
                // Recheck after saving: rebuilding/installing may have replaced the file.
                target.validate()?;
                target.spawn(std::process::id(), &paths::imacs_home()).map(|_| ())
            }) {
                self.governor.cancel_restart();
                self.store.restart = None;
                self.store.save(&paths::store_file())?;
                return Err(error);
            }
            // No further runtime actions may start work after the snapshot. This close
            // is already authorized and must not pass through session-close dialogs.
            self.restarting = true;
            self.client_events.push(ClientEvent::Viewport(egui::ViewportCommand::Close));
            self.ctx.request_repaint();
            Ok(())
        })();
        match result {
            Ok(()) => ControlResponse::ok(json!({"restarting":true, "old_version":crate::restart::VERSION})),
            Err(error) => ControlResponse::error(format!("{error:#}")),
        }
    }

    fn save_restart(&mut self, reason: Option<String>) -> anyhow::Result<()> {
        for plan in self.plans.iter().filter(|plan| !plan.id.is_empty()) {
            plan.save_in(&paths::imacs_home())?;
        }
        let sessions: Vec<_> = self
            .sessions
            .iter()
            .filter(|s| s.status != AgentStatus::Exited && s.terminal.as_ref().is_some_and(|t| !t.exited()))
            .filter_map(|s| {
                Some(SessionRecord {
                    chief: s.chief,
                    pane: Some(s.id.clone()),
                    claude_session_id: s.claude_id.clone()?,
                    agent: s.agent.clone(),
                    cwd: s.cwd.clone(),
                    title: s.title.clone(),
                    last_prompt: s.last_prompt.clone(),
                    updated: imacs_core::store::now(),
                    sandbox: s.sandbox.clone(),
                })
            })
            .collect();
        for record in &sessions {
            self.store.upsert(record.clone());
        }
        self.store.projects = self.open_projects.clone();
        self.store.chief_dock = Some(self.chief_dock());
        self.store.restart = Some(RestartState {
            old_version: crate::restart::VERSION.into(),
            reason,
            sessions,
            chief_open: self.chief_open,
            active: self.active.clone(),
            selected_plan: self.plans.get(self.selected_plan).map(|plan| plan.id.clone()),
            plan_view: self.plan_view,
            diff_open: self.diff_open,
            diff_root: self.diff_root.clone(),
            view: self.view.clone(),
            split: self.desktop_split,
            diff_split: self.desktop_diff_split,
            panel: match self.panel {
                PanelMode::Plan => "plan",
                PanelMode::Editor => "editor",
                PanelMode::Diff => "diff",
            }
            .into(),
            window_size: self.ctx.input(|input| input.viewport().inner_rect.map(|rect| [rect.width(), rect.height()])),
            maximized: self.ctx.input(|input| input.viewport().maximized.unwrap_or(false)),
        });
        self.store.save(&paths::store_file())
    }

    pub(super) fn restore_restart(&mut self) {
        let Some(restart) = self.store.restart.take() else { return };
        if let Err(error) = self.store.save(&paths::store_file()) {
            self.store.restart = Some(restart);
            self.toast(format!("Could not consume restart handoff: {error:#}"));
            return;
        }
        self.event_seq += 1;
        let message = format!(
            "restarted: {} -> {}, reason {}",
            restart.old_version,
            crate::restart::VERSION,
            restart.reason.as_deref().unwrap_or("not specified")
        );
        self.restart_event = Some(json!({
            "seq":self.event_seq, "status":"restarted", "message":message,
            "old_version":restart.old_version, "new_version":crate::restart::VERSION,
            "reason":restart.reason, "at":imacs_core::store::now()
        }));
        self.resume = restart.sessions.clone();
        // --open belongs to the original launch; the handoff already contains those agents.
        self.pending_open.clear();
        self.chief_open = restart.chief_open;
        self.active = restart.active.clone();
        self.selected_plan = restart
            .selected_plan
            .as_ref()
            .and_then(|id| self.plans.iter().position(|plan| &plan.id == id))
            .unwrap_or(self.selected_plan);
        self.plan_view = restart.plan_view;
        self.diff_open = restart.diff_open;
        self.diff_root = restart.diff_root.clone();
        self.view = restart.view.clone();
        self.panel = if restart.panel == "plan" { PanelMode::Plan } else { PanelMode::Diff };
        self.desktop_split = restart.split;
        self.desktop_diff_split = restart.diff_split;
        self.restart_layout = Some(restart);
    }

    pub(super) fn resume_record(&mut self, ctx: &egui::Context, record: SessionRecord) {
        let handoff = self.restart_event.is_some();
        let selection = (self.active.clone(), self.diff_root.clone(), self.focus_terminal.clone());
        let args = if record.chief { self.config.chief.args.clone() } else { Vec::new() };
        // A prompt starts a new chief turn, so it can call imacs_wait immediately.
        let prompt = (record.chief && handoff)
            .then(|| "IMACS restarted. Call imacs_wait to receive the restart event, then carry on.".into());
        // A resumed task agent keeps its task's isolation.
        let role = if record.chief {
            Role::Chief
        } else {
            let task = record.pane.as_ref().and_then(|pane| {
                self.plans.iter().flat_map(|plan| &plan.tasks).find(|task| task.session.as_ref() == Some(pane))
            });
            Role::Agent(task.and_then(|task| task.isolation))
        };
        match self.start_as(ctx, &record.agent, record.cwd, Some(record.claude_session_id), args, prompt, role) {
            Ok(id) => {
                if let Some(session) = self.sessions.iter_mut().find(|s| s.id == id) {
                    session.last_prompt = record.last_prompt;
                }
                for plan in &mut self.plans {
                    let mut changed = false;
                    for task in &mut plan.tasks {
                        if record.pane.is_some() && task.session == record.pane {
                            task.session = Some(id.clone());
                            changed = true;
                        }
                    }
                    if changed && let Err(error) = plan.save_in(&paths::imacs_home()) {
                        eprintln!("IMACS: saving resumed task session failed: {error:#}");
                    }
                }
                if handoff {
                    (self.active, self.diff_root, self.focus_terminal) = selection;
                    if self.active == record.pane && record.pane.is_some() {
                        self.active = Some(id.clone());
                    }
                }
                if record.chief {
                    if let Some(event) = &mut self.restart_event {
                        event["session"] = json!(id);
                    }
                    self.focus_terminal = self.chief_open.then_some(id);
                }
            }
            Err(error) => self.toast(format!("Could not resume {}: {error}", record.title)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn refused(app: &mut Desktop<'_>, expected: &str) {
        let response = request(app, json!({"cmd":"restart"}));
        assert!(!response.ok);
        assert!(response.error.unwrap().contains(expected), "expected {expected}");
        assert!(!app.runtime.restarting);
        assert!(app.runtime.store.restart.is_none());
        let forced = app.runtime.restart_using(true, None, || {
            Ok(crate::restart::Target { executable: PathBuf::from("/missing/imacs"), command: vec![] })
        });
        assert!(
            forced.error.unwrap().contains("target is missing"),
            "force must bypass {expected}, but validate the binary"
        );
    }

    #[test]
    fn restart_refuses_each_busy_condition_and_allows_idle_accepted_and_chief_turns() {
        with_app(
            "app::restart::tests::restart_refuses_each_busy_condition_and_allows_idle_accepted_and_chief_turns",
            |app, _| {
                for status in [AgentStatus::Working, AgentStatus::Starting, AgentStatus::NeedsInput] {
                    app.runtime.sessions = vec![session("worker", AgentKind::Claude, status, &app.runtime.launch_dir)];
                    refused(
                        app,
                        if status == AgentStatus::NeedsInput { "approval or question" } else { status.label() },
                    );
                }
                app.runtime.sessions[0].status = AgentStatus::Idle;
                app.runtime.sessions[0].approval_hold = true;
                refused(app, "approval or question");
                app.runtime.sessions[0].approval_hold = false;
                app.runtime.sessions[0].approvals.push(PendingApproval {
                    id: "approval".into(),
                    reply: None,
                    request: ApprovalRequest {
                        tool: "Bash".into(),
                        command: None,
                        cwd: app.runtime.launch_dir.clone(),
                        reason: None,
                        input: json!({}),
                    },
                });
                refused(app, "approval or question");
                app.runtime.sessions[0].approvals.clear();
                app.runtime.pending_questions.insert("worker".into());
                refused(app, "question pending for worker");
                app.runtime.pending_questions.clear();
                app.runtime.creating.push((PathBuf::new(), "branch".into(), PathBuf::new()));
                refused(app, "worktree");
                app.runtime.creating.clear();
                app.runtime.integrating.insert(("plan".into(), "task".into()));
                refused(app, "integration");
                app.runtime.integrating.clear();
                app.runtime.integration_checks.insert(("plan".into(), "task".into()));
                refused(app, "integration");
                app.runtime.integration_checks.clear();
                app.runtime.plan_edit_open = true;
                refused(app, "unsaved plan edit");
                app.runtime.plan_edit_open = false;
                app.runtime.sessions[0].held.push("message awaiting permission".into());
                refused(app, "undelivered message");
                app.runtime.sessions[0].held.clear();
                let (tx, rx) = app.runtime.wake.channel();
                app.runtime.cleanup = Some(Cleanup {
                    repo: app.runtime.launch_dir.clone(),
                    rows: None,
                    sandboxes: Default::default(),
                    busy: None,
                    log: vec![],
                    tx,
                    rx,
                });
                refused(app, "cleanup job");
                app.runtime.cleanup = None;
                let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
                client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let governor = app.runtime.governor.clone();
                let queue =
                    std::thread::spawn(move || governor.serve(&server, &json!({"cmd":"queue", "label":"test suite"})));
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&client), &mut line).unwrap();
                assert!(line.contains("granted"));
                refused(app, "queue slot held: test suite");
                drop(client);
                queue.join().unwrap().unwrap();
                for status in [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Exited] {
                    app.runtime.sessions[0].status = status;
                    assert!(app.runtime.restart_blockers().is_empty());
                }
                app.runtime.sessions[0].chief = true;
                app.runtime.sessions[0].status = AgentStatus::Working;
                assert!(app.runtime.restart_blockers().is_empty());
                app.runtime.sessions[0].approval_hold = true;
                refused(app, "approval or question");
            },
        );
    }

    #[test]
    fn restart_snapshot_saves_all_plans_live_session_ids_and_layout() {
        with_app("app::restart::tests::restart_snapshot_saves_all_plans_live_session_ids_and_layout", |app, ctx| {
            app.runtime.config.chief.command = vec!["/bin/sh".into(), "-c".into(), "exec sleep 30".into()];
            app.runtime.config.chief.args.clear();
            let id = app.runtime.start_chief(ctx, None).unwrap();
            let claude_id = app.runtime.sessions[0].claude_id.clone().unwrap();
            app.runtime.plans =
                vec![Plan { id: "saved-plan".into(), goal: "save all plans".into(), ..Default::default() }];
            app.runtime.chief_open = true;
            app.runtime.store.chief_dock = Some(ChiefDock::Docked);
            app.runtime.desktop_split = Some(700.0);
            app.runtime.save_restart(Some("snapshot test".into())).unwrap();
            let saved = Store::load(&paths::store_file());
            assert_eq!(saved.sessions[0].claude_session_id, claude_id);
            assert!(saved.sessions[0].chief);
            let handoff = saved.restart.unwrap();
            assert_eq!(handoff.sessions.len(), 1);
            assert_eq!(handoff.sessions[0].pane.as_deref(), Some(id.as_str()));
            assert_eq!(handoff.sessions[0].claude_session_id, claude_id);
            assert!(handoff.chief_open);
            assert_eq!(handoff.split, Some(700.0));
            assert_eq!(plan::load_plans(&paths::imacs_home()).unwrap().0[0].goal, "save all plans");
            app.runtime.shutdown();
        });
    }

    #[test]
    fn restart_force_bypasses_busy_checks_but_never_validation_or_save_failures() {
        with_app(
            "app::restart::tests::restart_force_bypasses_busy_checks_but_never_validation_or_save_failures",
            |app, _| {
                app.runtime.plan_edit_open = true;
                let missing = || {
                    Ok(crate::restart::Target {
                        executable: PathBuf::from("/missing/imacs"),
                        command: vec!["/usr/bin/true".into()],
                    })
                };
                let refused = app.runtime.restart_using(false, None, missing);
                assert!(refused.error.unwrap().contains("unsaved plan edit"));
                let forced = app.runtime.restart_using(true, None, missing);
                assert!(forced.error.unwrap().contains("target is missing"));
                assert!(!app.runtime.restarting);
                let exe = paths::imacs_home().join("version-probe");
                std::fs::write(&exe, "#!/bin/sh\ntest \"$1\" = --version\n").unwrap();
                std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
                let target =
                    || Ok(crate::restart::Target { executable: exe.clone(), command: vec!["/usr/bin/true".into()] });
                let store_path = paths::store_file();
                std::fs::remove_file(&store_path).unwrap();
                std::fs::create_dir(&store_path).unwrap();
                assert!(!app.runtime.restart_using(true, None, target).ok);
                assert!(!app.runtime.restarting);
                assert!(!app.runtime.governor.restarting_for_test());
                std::fs::remove_dir(&store_path).unwrap();
                let forced = app.runtime.restart_using(true, Some("force test".into()), target);
                assert!(forced.ok, "{forced:?}");
                assert!(app.runtime.restarting);
                assert!(app.runtime.governor.restarting_for_test());
                assert!(
                    app.runtime
                        .client_events
                        .iter()
                        .any(|event| matches!(event, ClientEvent::Viewport(egui::ViewportCommand::Close)))
                );
                assert_eq!(Store::load(&store_path).restart.unwrap().reason.as_deref(), Some("force test"));
            },
        );
    }

    #[test]
    fn restart_startup_resumes_chief_role_ids_layout_and_delivers_event_once() {
        with_app(
            "app::restart::tests::restart_startup_resumes_chief_role_ids_layout_and_delivers_event_once",
            |app, ctx| {
                std::fs::write(
                    paths::config_file(),
                    "[chief]\ncommand = [\"/bin/sh\", \"-c\", \"exec sleep 30\"]\nargs = []\n",
                )
                .unwrap();
                let record = SessionRecord {
                    chief: true,
                    pane: Some("old-pane".into()),
                    claude_session_id: "saved-claude-id".into(),
                    agent: "chief".into(),
                    cwd: app.runtime.launch_dir.clone(),
                    title: "chief".into(),
                    last_prompt: Some("carry on".into()),
                    updated: 0,
                    sandbox: None,
                };
                app.runtime.store.chief_dock = Some(ChiefDock::Docked);
                app.runtime.store.chief_width = Some(500.0);
                app.runtime.store.restart = Some(RestartState {
                    old_version: "old-commit".into(),
                    reason: Some("new build".into()),
                    sessions: vec![record],
                    chief_open: true,
                    active: Some("old-pane".into()),
                    diff_open: true,
                    split: Some(600.0),
                    diff_split: Some(DiffSplit::Never),
                    ..Default::default()
                });
                app.runtime.store.save(&paths::store_file()).unwrap();
                app.runtime.shutdown();
                let cc = eframe::CreationContext::_new_kittest(ctx.clone());
                let mut reopened = State::new(&cc, Options::default(), &crate::load_icon());
                reopened.runtime_tick(ctx);
                assert_eq!(reopened.sessions.len(), 1);
                let chief = &reopened.sessions[0];
                assert!(chief.chief);
                assert_eq!(chief.claude_id.as_deref(), Some("saved-claude-id"));
                assert_eq!(chief.last_prompt.as_deref(), Some("carry on"));
                assert_eq!(reopened.active.as_deref(), Some(chief.id.as_str()));
                assert!(reopened.chief_open);
                assert_eq!(reopened.chief_dock(), ChiefDock::Docked);
                assert_eq!(reopened.store.chief_width, Some(500.0));
                assert_eq!(reopened.desktop_split, Some(600.0));
                assert_eq!(reopened.desktop_diff_split, Some(DiffSplit::Never));
                let (tx, rx) = channel();
                reopened.control(
                    ctx,
                    serde_json::from_value(
                        json!({"cmd":"wait", "after":999999, "statuses":["heartbeat"], "timeout_secs":0}),
                    )
                    .unwrap(),
                    tx,
                );
                let ControlOutcome::Done(response) = rx.recv().unwrap() else { panic!("wait response") };
                let event = &response.result["events"][0];
                assert_eq!(event["status"], "restarted");
                assert!(event["message"].as_str().unwrap().starts_with("restarted: old-commit -> "));
                assert!(event["message"].as_str().unwrap().ends_with(", reason new build"));
                assert!(reopened.restart_event.is_none());
                assert!(Store::load(&paths::store_file()).restart.is_none());
                reopened.shutdown();
            },
        );
    }
}
