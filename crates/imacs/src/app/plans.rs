use super::*;

impl App {
    pub(super) fn show_plan_fleet(&mut self) {
        self.changed_plans.remove(&self.plans[self.selected_plan].id);
        self.ctx.request_repaint();
        self.view = self.plans[self.selected_plan].project.clone();
        self.plan_view = true;
        self.panel = PanelMode::Diff;
        if self.chief_dock() == ChiefDock::Floating || self.last_layout.is_some_and(|layout| layout.dock_width == 0.0) {
            self.chief_open = false;
        }
        let visible = self.visible_sessions();
        if !self.active_index().is_some_and(|i| visible.contains(&i)) {
            if let Some(&i) = visible.first() {
                self.activate(&self.sessions[i].id.clone(), false);
            } else {
                self.active = None;
                self.diff_root = None;
            }
        }
    }

    pub(super) fn finish_plan_agents(&mut self) {
        let now = imacs_core::store::now();
        for index in 0..self.plans.len() {
            if self.plans[index].status == plan::PlanStatus::Archived {
                continue;
            }
            let mut changed = self.plans[index].finish_tasks(now);
            let mut close = Vec::new();
            for task in &mut self.plans[index].tasks {
                if task.finished_at.is_none() {
                    continue;
                }
                let Some(session) = self.sessions.iter().find(|s| task.session.as_deref() == Some(&s.id) && !s.chief)
                else {
                    continue;
                };
                if task.worktree.is_none() || self.config.plan.close_done_agents {
                    let root = session.root();
                    task.worktree = Some(root.clone());
                    task.resume_id = session.claude_id.clone();
                    if let Some(prepared) = self.diffs.get(&root).and_then(|s| s.prepared.as_ref()) {
                        task.additions = prepared.additions;
                        task.deletions = prepared.deletions;
                    }
                    self.diffs.entry(root).or_default().mark_dirty();
                    changed = true;
                }
                if self.config.plan.close_done_agents {
                    close.push(session.id.clone());
                }
            }
            for id in close {
                self.remember(&id);
                if let Some(cid) = self.sessions.iter().find(|s| s.id == id).and_then(|s| s.claude_id.clone()) {
                    self.dismissed.insert(cid);
                }
                self.close(&id);
            }
            if changed {
                self.save_plan(index);
            }
        }
    }

    pub(super) fn task_action(&mut self, index: usize, task_index: usize, action: &str) {
        let task = self.plans[index].tasks[task_index].clone();
        let Some(root) = task.worktree else {
            return;
        };
        if !root.is_dir() {
            return;
        }
        match action {
            "diff" => {
                self.active = None;
                self.diff_root = Some(root);
                self.diff_open = true;
                self.panel = PanelMode::Diff;
            }
            "resume" if !self.sessions.iter().any(|s| task.session.as_deref() == Some(&s.id)) => {
                let ctx = self.ctx.clone();
                let agent = if task.agent.is_empty() { "claude" } else { &task.agent };
                match self.start(&ctx, agent, root, Some(task.resume_id.unwrap_or_default()), task.args, None) {
                    Ok(session) => {
                        let task = &mut self.plans[index].tasks[task_index];
                        task.session = Some(session);
                        task.set_status(TaskStatus::Running);
                        task.finished_at = None;
                        task.dispatched_at = Some(imacs_core::store::now());
                        self.plans[index].status = plan::PlanStatus::Open;
                        self.save_plan(index);
                    }
                    Err(err) => self.toast(err),
                }
            }
            _ => {}
        }
    }

    pub(super) fn close_plan(&mut self, index: usize, status: PlanStatus) -> Result<(), String> {
        if status == PlanStatus::Open {
            return Err("Choose completed or archived.".into());
        }
        let mut closed = self.plans[index].clone();
        closed.status = status;
        closed.save_in(&paths::imacs_home()).map_err(|err| format!("Closing plan failed: {err:#}"))?;
        self.plans[index] = closed;
        self.changed_plans.remove(&self.plans[index].id);
        if self.selected_plan == index {
            self.selected_plan = self.plans.iter().position(Plan::is_open).unwrap_or_else(|| {
                self.plans.push(Plan::default());
                self.plans.len() - 1
            });
            if self.plan_view {
                if self.plans[self.selected_plan].is_open() {
                    self.show_plan_fleet();
                } else {
                    self.set_view(None);
                }
                self.panel = PanelMode::Diff;
            }
        }
        self.ctx.request_repaint();
        Ok(())
    }

    pub(super) fn reopen_plan(&mut self, index: usize) -> Result<(), String> {
        let mut plan = self.plans[index].clone();
        plan.status = PlanStatus::Open;
        for task in &mut plan.tasks {
            task.integration_verified = false;
        }
        plan.save_in(&paths::imacs_home()).map_err(|err| format!("Reopening plan failed: {err:#}"))?;
        self.plans[index] = plan;
        self.integration_revision += 1;
        self.selected_plan = index;
        self.show_plan_fleet();
        self.panel = PanelMode::Plan;
        self.diff_open = true;
        Ok(())
    }

    pub(super) fn save_plan(&self, index: usize) {
        if self.plans[index].id.is_empty() {
            return;
        }
        if let Err(err) = self.plans[index].save_in(&paths::imacs_home()) {
            eprintln!("IMACS: saving the plan failed: {err:#}");
        }
    }

    pub(super) fn plan_json(&self, index: usize) -> serde_json::Value {
        let mut value = serde_json::to_value(&self.plans[index]).unwrap_or_default();
        value["ready"] = json!(self.plans[index].ready().iter().map(|t| &t.id).collect::<Vec<_>>());
        value["integration"] = json!(self.plans[index].integration_branch());
        value
    }

    /// Mutation replies omit task briefs, reports and note history.
    pub(super) fn plan_mutation_json(&self, index: usize, task: Option<&str>) -> serde_json::Value {
        let plan = &self.plans[index];
        json!({
            "plan": plan.id,
            "status": plan.status,
            "task": task.and_then(|id| plan.tasks.iter().find(|t| t.id == id)).map(|t| json!({
                "id": t.id,
                "status": t.status,
                "note": t.notes.last(),
                "integrated": t.integrated,
            })),
            "ready": plan.ready().iter().map(|t| &t.id).collect::<Vec<_>>(),
        })
    }

    /// Starts `only`, or every ready task, each in a new worktree with its brief as
    /// the agent's first message.
    pub(super) fn dispatch(&mut self, index: usize, only: Option<&str>) -> Result<Vec<String>, String> {
        if !self.plans[index].approved {
            return Err("The plan is not approved yet: the user approves it on the plan board.".into());
        }
        self.refresh_integrations();
        let ready: Vec<Task> =
            self.plans[index].ready().into_iter().filter(|t| only.is_none_or(|id| t.id == id)).cloned().collect();
        if ready.is_empty() {
            if let Some(task) = self.plans[index].tasks.iter().find(|task| {
                only.is_none_or(|id| task.id == id)
                    && task.status == TaskStatus::Planned
                    && !self.plans[index].unintegrated_dependencies(task).is_empty()
            }) {
                return Err(format!(
                    "Task {} needs integrated dependencies: {}. Use imacs_plan_integrate on them first.",
                    task.id,
                    self.plans[index].unintegrated_dependencies(task).join(", ")
                ));
            }
            return Err(match only {
                Some(id) => format!("Task {id} is not ready (it must be planned with all dependencies done)."),
                None => "No task is ready.".into(),
            });
        }
        let mut started = Vec::new();
        for task in ready {
            let Some(repo) = self.task_repo(index, &task) else {
                return Err("No project is open to branch from; give the task a repo.".into());
            };
            let branch = task
                .branch
                .clone()
                .unwrap_or_else(|| format!("imacs/{}-{}-{}", self.plans[index].id, task.id, short_slug(&task.title)));
            let agent = if task.agent.trim().is_empty() { "claude".to_string() } else { task.agent.clone() };
            let mut brief = task.compose_brief(&self.plans[index].goal);
            if task.id == imacs_core::plan::REVIEW_TASK {
                // The reviewer reads every task's branch from the shared repository.
                let base = &self.config.base_ref;
                brief.push_str("\n\nBranches to review (all in this repository):\n");
                let integration = self.plans[index].integration_branch();
                brief.push_str(&format!(
                    "- integration branch {integration} (git diff {base}...{integration}): all accepted work, review this first\n"
                ));
                for other in self.plans[index].tasks.iter().filter(|t| task.depends_on.contains(&t.id)) {
                    let branch = other.branch.clone().unwrap_or_else(|| "(not dispatched)".into());
                    brief.push_str(&format!("- {}: {} (git diff {base}...{branch})\n", other.id, other.title));
                }
            }
            let launch = Launch {
                agent,
                prompt: Some(brief),
                args: task.args.clone(),
                reply: None,
                task: Some((self.plans[index].id.clone(), task.id.clone())),
            };
            let start = self.plans[index].start_branch(&task);
            self.create_worktree(repo.clone(), branch.clone(), start, launch);
            if let Some(t) = self.plans[index].task_mut(&task.id) {
                t.set_status(TaskStatus::Running);
                t.branch = Some(branch);
                t.finished_at = None;
                t.dispatched_at = Some(imacs_core::store::now());
                t.session = None;
                t.report = None;
                t.repo.get_or_insert_with(|| repo.to_string_lossy().into_owned());
            }
            started.push(task.id);
        }
        if self.selected_plan == index && (self.plan_view || (self.panel == PanelMode::Plan && self.diff_open)) {
            self.show_plan_fleet();
        } else {
            self.changed_plans.insert(self.plans[index].id.clone());
        }
        self.save_plan(index);
        Ok(started)
    }

    /// A sub-agent's `imacs_report`: attach it to its task, move the task along, and
    /// wake anyone waiting.
    pub(super) fn report(&mut self, pane: Option<&str>, status: &str, summary: &str) -> Option<String> {
        let found = pane.and_then(|p| self.sessions.iter().position(|s| s.id == p));
        if let Some(found) = found {
            self.sessions[found].activity.last_event = imacs_core::store::now();
        }
        let session = found.map(|found| &self.sessions[found]);
        let session_id = pane.unwrap_or_default().to_string();
        let title = session.map(|s| s.title.clone()).unwrap_or_else(|| "closed agent".into());
        let owner = self.plans.iter().enumerate().find_map(|(index, plan)| {
            plan.tasks.iter().find(|t| t.session.as_deref() == Some(&session_id)).map(|task| (index, task.id.clone()))
        });
        let task_id = owner.as_ref().map(|(_, task)| task.clone());
        let plan_id = owner.as_ref().map(|(index, _)| self.plans[*index].id.clone());
        if let Some((index, id)) = &owner {
            let task = self.plans[*index].task_mut(id).expect("task owner");
            task.report =
                Some(Report { status: status.to_string(), summary: summary.to_string(), at: imacs_core::store::now() });
            if task.status != TaskStatus::Done {
                match status {
                    "done" => task.set_status(TaskStatus::Review),
                    "blocked" | "question" => task.set_status(TaskStatus::Blocked),
                    _ => {}
                }
            }
            self.save_plan(*index);
        }
        self.event_seq += 1;
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: session_id,
            plan: plan_id,
            task: task_id.clone(),
            title: title.clone(),
            status: session.map(|s| s.status).unwrap_or(AgentStatus::Idle),
            at: imacs_core::store::now(),
            report: Some((status.to_string(), summary.to_string(), task_id.clone())),
            approval: None,
            findings: None,
        });
        self.serve_waiters();
        let what = task_id.as_deref().map(|t| format!("task {t}")).unwrap_or(title);
        self.toast(format!("{what} reported {status}"));
        task_id
    }

    /// Running tasks whose agent exited without reporting are blocked.
    pub(super) fn sync_plan(&mut self) {
        self.finish_plan_agents();
        for index in 0..self.plans.len() {
            if !self.plans[index].is_open() {
                continue;
            }
            let mut changed = false;
            let plan_id = self.plans[index].id.clone();
            for task in &mut self.plans[index].tasks {
                if task.status != TaskStatus::Running {
                    continue;
                }
                let gone = match &task.session {
                    Some(id) => {
                        self.sessions.iter().find(|s| &s.id == id).is_none_or(|s| s.status == AgentStatus::Exited)
                    }
                    None => false,
                };
                if gone {
                    self.heartbeat.gone.push(imacs_core::heartbeat::Finding {
                        plan: plan_id.clone(),
                        task: task.id.clone(),
                        session: task.session.clone(),
                        kind: imacs_core::heartbeat::Kind::AgentGone,
                    });
                    task.set_status(TaskStatus::Blocked);
                    task.notes.push("the agent exited before reporting".into());
                    changed = true;
                }
            }
            if changed {
                self.save_plan(index);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, session, with_app};
    use super::*;

    #[test]
    fn dispatched_review_records_branch_closes_on_acceptance_and_can_be_cleaned_up() {
        with_app(
            "app::plans::tests::dispatched_review_records_branch_closes_on_acceptance_and_can_be_cleaned_up",
            |app, ctx| {
                let (_dir, repo) = repository();
                app.plans = vec![Plan {
                    id: "review-cleanup".into(),
                    project: Some(repo.clone()),
                    approved: true,
                    tasks: vec![Task { id: plan::REVIEW_TASK.into(), ..Task::default() }],
                    ..Plan::default()
                }];
                app.selected_plan = 0;
                app.dispatch(0, Some(plan::REVIEW_TASK)).unwrap();
                let (branch, worktree, _) = app.worktree_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let worktree = worktree.unwrap();
                assert_eq!(app.plans[0].tasks[0].branch.as_deref(), Some(branch.as_str()));
                assert_eq!(git::worktree_of(&worktree).unwrap().branch.as_deref(), Some(branch.as_str()));
                assert_eq!(plan::load_plans(&paths::imacs_home()).unwrap().0[0].tasks[0].branch, Some(branch.clone()));
                app.sessions = vec![session("reviewer", AgentKind::Claude, AgentStatus::Done, &worktree)];
                app.plans[0].tasks[0].session = Some("reviewer".into());
                app.plans[0].tasks[0].status = TaskStatus::Review;
                for (id, can_integrate) in [("code", true), (plan::REVIEW_TASK, false)] {
                    app.plans[0].tasks[0].id = id.into();
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
                        },
                    );
                    assert_eq!(
                        output.shapes.iter().any(|shape| {
                            matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.text() == "integrate")
                        }),
                        can_integrate
                    );
                    output.drop_without_applying_deltas();
                }
                assert!(
                    request(
                        app,
                        json!({
                            "cmd":"plan_update", "plan":"review-cleanup", "id":"review", "status":"done"
                        })
                    )
                    .ok
                );
                assert!(app.find("reviewer").is_none());
                assert_eq!(app.plans[0].status, PlanStatus::Completed);
                assert!(app.plans[0].tasks[0].integrated.is_none());
                assert_eq!(app.plans[0].tasks[0].worktree.as_ref(), Some(&worktree));
                let rows = imacs_core::cleanup::preview(&app.plans[0], "main").unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].branch, branch);
                imacs_core::cleanup::remove(&app.plans[0], "main", &rows[0]).unwrap();
                assert!(!worktree.exists());
                assert!(git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_err());
            },
        );
    }

    #[test]
    fn accepted_agents_exit_keep_results_and_complete_without_closing_chief() {
        with_app(
            "app::plans::tests::accepted_agents_exit_keep_results_and_complete_without_closing_chief",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                let terminal = Terminal::spawn(
                    ctx,
                    app.palette.clone(),
                    "/bin/sh".into(),
                    vec!["-c".into(), "exec sleep 600".into()],
                    root,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let pid = terminal.process_id();
                app.sessions = vec![
                    session("worker", AgentKind::Claude, AgentStatus::Done, root),
                    session("reviewer", AgentKind::Claude, AgentStatus::Idle, root),
                    session("chief", AgentKind::Claude, AgentStatus::Working, root),
                    session("other-plan", AgentKind::Generic, AgentStatus::Working, root),
                ];
                app.sessions[0].terminal = Some(terminal);
                app.sessions[2].chief = true;
                let index = app.selected_plan;
                app.plans[index] = Plan {
                    id: "lifecycle".into(),
                    goal: "Ship".into(),
                    tasks: vec![
                        Task {
                            id: "code".into(),
                            title: "Completed code".into(),
                            branch: Some("code".into()),
                            status: TaskStatus::Review,
                            session: Some("worker".into()),
                            dispatched_at: Some(100),
                            report: Some(Report { summary: "Last report\nSecond line".into(), ..Report::default() }),
                            ..Task::default()
                        },
                        Task {
                            id: plan::REVIEW_TASK.into(),
                            branch: Some("review".into()),
                            status: TaskStatus::Review,
                            session: Some("reviewer".into()),
                            ..Task::default()
                        },
                        Task {
                            id: "chief-reference".into(),
                            status: TaskStatus::Done,
                            session: Some("chief".into()),
                            ..Task::default()
                        },
                    ],
                    ..Plan::default()
                };
                let into = app.plans[index].integration_branch();
                app.plans[index].tasks[0].status = TaskStatus::Running;
                app.plans[index].tasks[0].integrated = Some(into.clone());
                assert_eq!(app.report(Some("worker"), "done", "Last report\nSecond line"), Some("code".into()));
                app.sync_plan();
                assert_eq!(app.plans[index].tasks[0].status, TaskStatus::Review);
                assert!(app.plans[index].tasks[0].finished_at.is_none());
                assert!(app.completed_tasks().is_empty());
                assert!(app.sessions[app.find("worker").unwrap()].terminal.as_ref().is_some_and(|t| !t.exited()));
                app.plans[index].tasks[0].integrated = None;
                assert!(
                    request(app, json!({"cmd":"plan_update", "plan":"lifecycle", "id":"code", "status":"done"})).ok
                );
                assert!(app.find("worker").is_some(), "acceptance alone must not close unintegrated work");
                app.diffs.entry(root.into()).or_default().prepared = Some(Arc::new(diff_view::Prepared {
                    diff: imacs_core::diff::WorktreeDiff::default(),
                    highlights: vec![],
                    longest_line: 0,
                    additions: 412,
                    deletions: 88,
                    all_files: Arc::default(),
                }));
                app.finish_integration(Integrated {
                    plan: "lifecycle".into(),
                    task: "code".into(),
                    branch: "code".into(),
                    into,
                    result: Ok(Integration { committed: false, merge: Merge::Merged, worktree: root.into() }),
                    reply: None,
                });
                assert!(app.find("worker").is_none());
                assert_eq!(app.report(Some("worker"), "done", "Final late report"), Some("code".into()));
                assert_eq!(app.plans[index].tasks[0].status, TaskStatus::Done);
                let event = imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"Stop", "session_id":"worker", "cwd":root,
                }))
                .unwrap();
                app.on_hook(HookMessage {
                    pane: Some("worker".into()),
                    ancestors: vec![],
                    event,
                    delivered: 0,
                    approval_reply: None,
                });
                assert!(app.find("worker").is_none(), "late shutdown hooks must not recreate a closed agent");
                let deadline = Instant::now() + Duration::from_secs(5);
                while std::process::Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "pid="])
                    .output()
                    .unwrap()
                    .status
                    .success()
                {
                    assert!(Instant::now() < deadline, "closed agent process is still running");
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(root.exists(), "closing never removes the worktree");
                assert_eq!(app.completed_tasks(), [(index, 0)]);
                let task = &app.plans[index].tasks[0];
                assert_eq!(task.worktree.as_deref(), Some(root));
                assert_eq!(task.resume_id.as_deref(), Some("worker"));
                assert_eq!((task.additions, task.deletions), (412, 88));
                assert_eq!(task.report.as_ref().unwrap().summary, "Final late report");
                assert_eq!(app.plans[index].status, plan::PlanStatus::Open);
                assert_eq!(app.report(Some("reviewer"), "done", "Review passed"), Some("review".into()));
                app.sync_plan();
                assert!(app.find("reviewer").is_some(), "the reviewer report must also wait for acceptance");
                assert!(app.plans[index].tasks[1].finished_at.is_none());
                assert!(
                    request(app, json!({"cmd":"plan_update", "plan":"lifecycle", "id":"review", "status":"done"})).ok
                );
                assert_eq!(app.plans[index].status, plan::PlanStatus::Completed);
                assert!(app.find("reviewer").is_none());
                assert!(app.find("chief").is_some());
                assert!(app.find("other-plan").is_some());
                app.task_action(index, 0, "diff");
                assert_eq!(app.diff_target().as_deref(), Some(root));
                assert!(app.panel == PanelMode::Diff);
                app.reopen_plan(index).unwrap();
                app.sync_plan();
                assert_eq!(app.plans[index].status, PlanStatus::Open, "reopening must survive a lifecycle sweep");
                app.close_plan(index, PlanStatus::Archived).unwrap();
                assert_eq!(app.plans[index].status, plan::PlanStatus::Archived);
                assert!(app.completed_tasks().is_empty());
                assert_eq!(
                    plan::load_plans(&paths::imacs_home())
                        .unwrap()
                        .0
                        .iter()
                        .find(|p| p.id == "lifecycle")
                        .unwrap()
                        .status,
                    plan::PlanStatus::Archived
                );
                assert!(root.exists());
            },
        );
    }

    #[test]
    fn disabling_automatic_closing_keeps_agents_and_completed_plans() {
        with_app("app::plans::tests::disabling_automatic_closing_keeps_agents_and_completed_plans", |app, _| {
            app.config.plan.close_done_agents = false;
            app.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Idle, Path::new("/work"))];
            app.plans[app.selected_plan] = Plan {
                id: "disabled".into(),
                tasks: vec![Task { status: TaskStatus::Done, session: Some("worker".into()), ..Task::default() }],
                ..Plan::default()
            };
            app.finish_plan_agents();
            assert!(app.find("worker").is_some());
            assert_eq!(app.plans[app.selected_plan].status, plan::PlanStatus::Completed);
            assert!(app.completed_tasks().is_empty());
            app.config.plan.close_done_agents = true;
            app.finish_plan_agents();
            assert!(app.find("worker").is_none());
            assert!(app.completed_tasks().is_empty());
            app.plan_view = true;
            assert_eq!(app.completed_tasks(), [(app.selected_plan, 0)]);
        });
    }

    #[test]
    fn completed_task_resume_uses_claude_or_codex_resume_and_reopens_plan() {
        with_app("app::plans::tests::completed_task_resume_uses_claude_or_codex_resume_and_reopens_plan", |app, _| {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("agent.sh");
            let output = dir.path().join("args");
            std::fs::write(&script, "output=$1\nshift\nprintf '%s\\n' \"$@\" > \"$output\"\nexec sleep 600\n").unwrap();
            for (agent, kind, resume_id, expected) in [
                ("claude", AgentKind::Claude, Some("claude-id"), "--resume\nclaude-id\n"),
                ("codex", AgentKind::Generic, Some("codex-id"), "resume\ncodex-id\n"),
                ("codex", AgentKind::Generic, None, "resume\n--last\n"),
            ] {
                if output.exists() {
                    std::fs::remove_file(&output).unwrap();
                }
                app.config.agents = vec![imacs_core::config::AgentConfig {
                    name: agent.into(),
                    command: vec![
                        "/bin/sh".into(),
                        script.to_string_lossy().into_owned(),
                        output.to_string_lossy().into_owned(),
                    ],
                    kind,
                    busy_text: None,
                }];
                let index = app.selected_plan;
                app.plans[index] = Plan {
                    id: "resume".into(),
                    status: plan::PlanStatus::Completed,
                    tasks: vec![Task {
                        agent: agent.into(),
                        status: TaskStatus::Done,
                        finished_at: Some(890),
                        worktree: Some(dir.path().into()),
                        resume_id: resume_id.map(str::to_string),
                        args: vec!["--model".into(), "test-model".into()],
                        ..Task::default()
                    }],
                    ..Plan::default()
                };
                app.task_action(index, 0, "resume");
                let deadline = Instant::now() + Duration::from_secs(5);
                let args = loop {
                    let args = std::fs::read_to_string(&output).unwrap_or_default();
                    if args.contains("test-model") {
                        break args;
                    }
                    assert!(Instant::now() < deadline, "resume did not start: {:?}", app.toast);
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(args.contains(expected), "{args}");
                assert_eq!(app.plans[index].status, plan::PlanStatus::Open);
                let task = &app.plans[index].tasks[0];
                assert_eq!(task.status, TaskStatus::Running);
                assert!(task.finished_at.is_none() && task.dispatched_at.is_some() && task.integrated.is_none());
                let session = task.session.clone().unwrap();
                assert_eq!(app.sessions[app.find(&session).unwrap()].root(), dir.path());
                assert!(app.completed_tasks().is_empty());
                let count = app.sessions.len();
                app.task_action(index, 0, "resume");
                assert_eq!(app.sessions.len(), count, "a live session must never be resumed into a duplicate");
                assert_eq!(app.plans[index].tasks[0].session.as_deref(), Some(session.as_str()));
                app.close(&session);
            }
        });
    }

    #[test]
    fn redispatch_and_late_exit_events_preserve_the_current_task_session() {
        with_app("app::plans::tests::redispatch_and_late_exit_events_preserve_the_current_task_session", |app, ctx| {
            let (_dir, repo) = repository();
            app.plans[0] = Plan {
                id: "delivery".into(),
                approved: true,
                tasks: vec![Task {
                    id: "task".into(),
                    repo: Some(repo.to_string_lossy().into_owned()),
                    session: Some("old".into()),
                    report: Some(Report { status: "blocked".into(), summary: "old".into(), at: 0 }),
                    status: TaskStatus::Planned,
                    ..Task::default()
                }],
                ..Plan::default()
            };
            app.sessions = vec![session("old", AgentKind::Generic, AgentStatus::Exited, &repo)];
            app.dispatch(0, Some("task")).unwrap();
            assert!(app.plans[0].tasks[0].session.is_none());
            assert!(app.plans[0].tasks[0].report.is_none());
            app.sync_plan();
            assert_eq!(app.plans[0].tasks[0].status, TaskStatus::Running);
            app.sessions.push(session("new", AgentKind::Generic, AgentStatus::Working, &repo));
            app.sessions[1].terminal = Some(
                Terminal::spawn(
                    ctx,
                    app.palette.clone(),
                    "/bin/sleep".into(),
                    vec!["10".into()],
                    &repo,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
            app.plans[0].tasks[0].session = Some("new".into());
            let exit = |pane: &str, id: &str| HookMessage {
                pane: Some(pane.into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: None,
                event: imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"SessionEnd", "session_id":id,
                }))
                .unwrap(),
            };
            app.on_hook(exit("new", "old"));
            app.on_hook(exit("old", "old"));
            let mut stale = exit("new", "retired");
            stale.pane = None;
            stale.ancestors = vec![app.sessions[1].terminal.as_ref().unwrap().process_id()];
            app.on_hook(stale);
            app.sync_plan();
            assert_eq!(app.sessions[1].status, AgentStatus::Working);
            assert_eq!(app.plans[0].tasks[0].status, TaskStatus::Running);
            app.on_hook(exit("new", "new"));
            app.sync_plan();
            assert_eq!(app.plans[0].tasks[0].status, TaskStatus::Blocked);
            assert_eq!(app.plans[0].tasks[0].notes, ["the agent exited before reporting"]);
            // Finish the worktree job before its temporary repository is removed.
            let (_, result, _) = app.worktree_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            result.unwrap();
        });
    }

    #[test]
    fn plan_mutations_return_compact_replies_and_get_keeps_the_full_history() {
        with_app(
            "app::plans::tests::plan_mutations_return_compact_replies_and_get_keeps_the_full_history",
            |app, _| {
                let brief = "long brief ".repeat(8000);
                let plan = request(
                    app,
                    json!({
                        "cmd":"plan_set", "goal":"Compact replies", "tasks":[
                            {"id":"a", "brief":brief},
                            {"id":"b", "depends_on":["a"]}
                        ]
                    }),
                )
                .result["plan"]
                    .clone();
                let index = app.selected_plan;
                app.plans[index].tasks[0].notes = vec!["old note".into(); 1000];
                app.plans[index].tasks[0].report =
                    Some(Report { status: "done".into(), summary: brief.clone(), at: 1 });
                let update = request(
                    app,
                    json!({
                        "cmd":"plan_update", "plan":plan, "id":"a", "status":"done", "note":"verified"
                    }),
                );
                assert!(update.ok);
                assert_eq!(
                    update.result,
                    json!({
                        "plan":plan, "status":"open",
                        "task":{"id":"a", "status":"done", "note":"verified", "integrated":null},
                        "ready":[]
                    })
                );
                assert!(update.result.to_string().len() < 1024);
                let into = app.plans[index].integration_branch();
                for (merge, committed, merged, note) in [
                    (Merge::Merged, true, true, format!("committed and merged into {into}")),
                    (Merge::UpToDate, false, false, format!("already in {into}")),
                ] {
                    let (reply, received) = channel();
                    app.finish_integration(Integrated {
                        plan: plan.as_str().unwrap().into(),
                        task: "a".into(),
                        branch: "task-a".into(),
                        into: into.clone(),
                        result: Ok(Integration { committed, merge, worktree: "/integration".into() }),
                        reply: Some(reply),
                    });
                    let ControlOutcome::Done(integrated) = received.try_recv().unwrap() else {
                        panic!("integration reply");
                    };
                    assert!(integrated.ok);
                    assert_eq!(
                        integrated.result,
                        json!({
                            "plan":plan, "status":"open",
                            "task":{"id":"a", "status":"done", "note":note, "integrated":into},
                            "ready":["b"],
                            "integration":into, "worktree":"/integration", "committed":committed, "merged":merged
                        })
                    );
                    assert!(integrated.result.to_string().len() < 1024);
                }
                let full = request(app, json!({"cmd":"plan_get", "plan":plan}));
                assert!(full.ok);
                assert_eq!(full.result["tasks"][0]["brief"], brief);
                assert_eq!(full.result["tasks"][0]["report"]["summary"], brief);
                assert_eq!(full.result["tasks"][0]["notes"].as_array().unwrap().len(), 1003);
                assert_eq!(full.result["ready"], json!(["b"]));
                let closed = request(app, json!({"cmd":"plan_close", "plan":plan, "status":"completed"}));
                assert!(closed.ok);
                assert_eq!(
                    closed.result,
                    json!({
                        "plan":plan, "status":"completed", "task":null, "ready":["b"]
                    })
                );
                assert!(closed.result.to_string().len() < 1024);
                let full = request(app, json!({"cmd":"plan_get", "plan":plan}));
                assert_eq!(full.result["status"], "completed");
                assert_eq!(full.result["tasks"][0]["brief"], brief);
            },
        );
    }

    #[test]
    fn plan_close_persists_status_and_restores_implicit_single_plan_selection() {
        with_app(
            "app::plans::tests::plan_close_persists_status_and_restores_implicit_single_plan_selection",
            |app, _| {
                let one =
                    request(app, json!({"cmd":"plan_set", "goal":"One", "tasks":[{"id":"a"}]})).result["plan"].clone();
                let two =
                    request(app, json!({"cmd":"plan_set", "goal":"Two", "tasks":[{"id":"a"}]})).result["plan"].clone();
                assert!(!request(app, json!({"cmd":"plan_close", "plan":two, "status":"open"})).ok);
                let closed = request(app, json!({"cmd":"plan_close", "plan":two, "status":"completed"}));
                assert!(closed.ok);
                assert_eq!(closed.result["status"], "completed");
                app.sync_plan();
                assert_eq!(request(app, json!({"cmd":"plan_get", "plan":two})).result["status"], "completed");
                assert!(request(app, json!({"cmd":"plan_update", "id":"a", "status":"done"})).ok);
                assert_eq!(app.plans[app.selected_plan].id, one.as_str().unwrap());
                assert_eq!(request(app, json!({"cmd":"plan_get"})).result["plans"].as_array().unwrap().len(), 1);
                assert!(request(app, json!({"cmd":"plan_close", "plan":one, "status":"archived"})).ok);
                assert!(!app.plan_view);
                assert!(app.plans.iter().all(|p| !p.is_open()));
                let (loaded, errors) = plan::load_plans(&paths::imacs_home()).unwrap();
                assert!(errors.is_empty());
                assert_eq!(loaded.iter().find(|p| p.id == one.as_str().unwrap()).unwrap().status, PlanStatus::Archived);
                assert_eq!(
                    loaded.iter().find(|p| p.id == two.as_str().unwrap()).unwrap().status,
                    PlanStatus::Completed
                );
                assert!(CHIEF_ALLOWED_TOOLS.contains(&"mcp__imacs__imacs_plan_close"));
            },
        );
    }

    #[test]
    fn dispatch_from_visible_board_returns_to_fleet_and_preserves_docked_chief() {
        with_app(
            "app::plans::tests::dispatch_from_visible_board_returns_to_fleet_and_preserves_docked_chief",
            |app, ctx| {
                let (dir, repo) = repository();
                app.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "fixture".into(),
                    command: vec!["/bin/cat".into()],
                    kind: AgentKind::Shell,
                    busy_text: None,
                });
                for dock in [ChiefDock::Floating, ChiefDock::Docked] {
                    request(
                        app,
                        json!({"cmd":"plan_set", "goal":format!("Board {dock:?}"), "tasks":[{"id":"a", "repo":repo, "agent":"fixture"}]}),
                    );
                    let index = app.selected_plan;
                    app.plans[index].approved = true;
                    app.plan_view = false;
                    app.panel = PanelMode::Plan;
                    app.diff_open = true;
                    app.chief_open = true;
                    app.store.chief_dock = Some(dock);
                    assert_eq!(app.dispatch(index, Some("a")).unwrap(), ["a"]);
                    assert!(app.plan_view && app.panel == PanelMode::Diff);
                    assert_eq!(app.chief_open, dock == ChiefDock::Docked);
                    assert!(!app.changed_plans.contains(&app.plans[index].id));
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !app.creating.is_empty() {
                        assert!(Instant::now() < deadline);
                        app.pump(ctx);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let session = app.plans[index].tasks[0].session.as_ref().unwrap();
                    assert_eq!(
                        app.visible_sessions().iter().map(|&i| &app.sessions[i].id).collect::<Vec<_>>(),
                        [session]
                    );
                }
            },
        );
    }

    #[test]
    fn background_plan_updates_and_successful_dispatch_preserve_the_foreground_and_chief() {
        with_app(
            "app::plans::tests::background_plan_updates_and_successful_dispatch_preserve_the_foreground_and_chief",
            |app, ctx| {
                let (dir, repo) = repository();
                app.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "fixture".into(),
                    command: vec!["/bin/cat".into()],
                    kind: AgentKind::Shell,
                    busy_text: None,
                });
                let first = request(app, json!({"cmd":"plan_set", "goal":"Background", "tasks":[{"id":"a", "repo":repo, "agent":"fixture"}]})).result["plan"].clone();
                let one = app.selected_plan;
                request(app, json!({"cmd":"plan_set", "goal":"Foreground", "tasks":[{"id":"a"}]}));
                let two = app.selected_plan;
                app.sessions.push(session("foreground", AgentKind::Generic, AgentStatus::Idle, &repo));
                app.active = Some("foreground".into());
                app.diff_root = Some(repo.clone());
                app.focus_terminal = Some("foreground".into());
                app.chief_open = true;
                app.panel = PanelMode::Editor;
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Background revised", "tasks":[
                {"id":"a", "repo":repo, "agent":"fixture"}]})
                    )
                    .ok
                );
                assert_eq!(app.selected_plan, two);
                assert!(app.chief_open && app.panel == PanelMode::Editor);
                assert_eq!(app.active.as_deref(), Some("foreground"));
                assert_eq!(app.diff_root.as_ref(), Some(&repo));
                assert!(app.changed_plans.contains(first.as_str().unwrap()));
                assert!(app.toast.as_ref().unwrap().0.starts_with("Plan updated:"));
                app.plans[one].approved = true;
                assert!(request(app, json!({"cmd":"plan_dispatch", "plan":first, "id":"a"})).ok);
                let deadline = Instant::now() + Duration::from_secs(10);
                while !app.creating.is_empty() {
                    assert!(Instant::now() < deadline);
                    app.pump(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(app.plans[one].tasks[0].session.is_some());
                assert_eq!(app.selected_plan, two);
                assert!(app.chief_open && app.panel == PanelMode::Editor);
                assert_eq!(app.active.as_deref(), Some("foreground"));
                assert_eq!(app.diff_root.as_ref(), Some(&repo));
                assert_eq!(app.focus_terminal.as_deref(), Some("foreground"));
                assert!(app.changed_plans.contains(first.as_str().unwrap()));
                app.selected_plan = one;
                app.show_plan_fleet();
                assert!(!app.changed_plans.contains(first.as_str().unwrap()));
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Selected update", "tasks":[{"id":"a"}]})
                    )
                    .ok
                );
                assert!(app.panel == PanelMode::Plan);
                assert!(!app.chief_open);
            },
        );
    }

    #[test]
    fn multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate() {
        with_app(
            "app::plans::tests::multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate",
            |app, _| {
                app.config.plan.close_done_agents = false;
                let first = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"First task"}]}),
                );
                assert!(first.ok);
                let first_id = first.result["plan"].as_str().unwrap().to_string();
                let one = app.selected_plan;
                app.plans[one].approved = true;
                app.plans[one].tasks[0].status = TaskStatus::Blocked;
                app.plans[one].tasks[0].session = Some("one-agent".into());
                let second = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"Second task"}]}),
                );
                assert!(second.ok);
                let second_id = second.result["plan"].as_str().unwrap().to_string();
                assert_ne!(first_id, second_id);
                let two = app.selected_plan;
                assert!(app.plans[one].approved);
                assert!(!app.plans[two].approved);
                assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
                assert_ne!(app.plans[one].integration_branch(), app.plans[two].integration_branch());
                assert_eq!(app.plans[one].tasks.last().unwrap().id, "review");
                assert_eq!(app.plans[two].tasks.last().unwrap().id, "review");
                app.plans[two].tasks[0].session = Some("two-agent".into());
                app.sessions = vec![
                    session("one-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                    session("two-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                    session("outside", AgentKind::Generic, AgentStatus::Idle, Path::new("/project")),
                    session("chief", AgentKind::Claude, AgentStatus::Idle, Path::new("/project")),
                ];
                app.sessions[3].chief = true;
                app.repos = vec!["/project".into()];
                for (index, visible) in [(one, vec![0]), (two, vec![1])] {
                    app.selected_plan = index;
                    app.show_plan_fleet();
                    assert_eq!(app.visible_sessions(), visible);
                    app.toggle_plan();
                    assert!(app.panel == PanelMode::Plan);
                    app.toggle_plan();
                    assert!(app.panel == PanelMode::Diff);
                }
                app.set_view(None);
                assert_eq!(app.visible_sessions(), [0, 1, 2]);
                app.set_view(Some("/project".into()));
                assert_eq!(app.visible_sessions(), [0, 1, 2]);
                app.selected_plan = two;
                app.plans[one].project = Some("/one".into());
                app.plans[two].project = Some("/two".into());
                assert_eq!(app.task_repo(one, &Task::default()), Some("/one".into()));
                assert_eq!(app.task_repo(two, &Task::default()), Some("/two".into()));
                for cmd in ["plan_update", "plan_dispatch", "plan_integrate"] {
                    let response = request(app, json!({"cmd":cmd, "id":"a", "status":"done"}));
                    let error = response.error.unwrap();
                    assert!(error.contains(&first_id) && error.contains(&second_id), "{error}");
                }
                let summary = request(app, json!({"cmd":"plan_get"}));
                assert_eq!(summary.result["plans"].as_array().unwrap().len(), 2);
                assert_eq!(summary.result["plans"][0]["waiting"], json!(["a"]));
                let full = request(app, json!({"cmd":"plan_get", "plan":first_id}));
                assert_eq!(full.result["tasks"][0]["title"], "First task");
                assert_eq!(app.selected_plan, two);
                let update = request(
                    app,
                    json!({"cmd":"plan_update", "plan":first_id, "id":"a", "status":"done", "note":"verified"}),
                );
                assert!(update.ok);
                assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Done);
                assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
                assert_eq!(app.selected_plan, two);
                let into = app.plans[one].integration_branch();
                app.integrating.insert((first_id.clone(), "a".into()));
                app.integrating.insert((second_id.clone(), "a".into()));
                app.finish_integration(Integrated {
                    plan: first_id.clone(),
                    task: "a".into(),
                    branch: "first".into(),
                    into: into.clone(),
                    result: Ok(Integration { committed: true, merge: Merge::Merged, worktree: "/integration".into() }),
                    reply: None,
                });
                assert_eq!(app.plans[one].tasks[0].integrated.as_deref(), Some(into.as_str()));
                assert!(app.plans[two].tasks[0].integrated.is_none());
                assert!(app.integrating.contains(&(second_id.clone(), "a".into())));
                assert_eq!(app.report(Some("one-agent"), "done", "first report"), Some("a".into()));
                assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Done);
                assert!(app.plans[two].tasks[0].report.is_none());
                let waited = request(app, json!({"cmd":"wait", "after":0, "statuses":["reported"]}));
                assert_eq!(waited.result["events"][0]["plan"], first_id);
                assert_eq!(waited.result["events"][0]["task"], "a");
                app.record_events();
                for (session, plan) in [("one-agent", &first_id), ("two-agent", &second_id)] {
                    let event =
                        app.events.iter().find(|e| e.session == session && e.report.is_none()).unwrap().to_json();
                    assert_eq!(event["plan"], *plan);
                    assert_eq!(event["task"], "a");
                }
                app.sessions[0].status = AgentStatus::Exited;
                app.plans[one].tasks[0].status = TaskStatus::Running;
                app.sync_plan();
                assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Blocked);
                assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
                let old = app.plans[two].clone();
                assert!(request(app, json!({"cmd":"plan_set", "plan":first_id, "goal":"First goal", "tasks":[{"id":"a", "title":"Revised"}]})).ok);
                assert_eq!(app.plans[two], old);
                assert_eq!(app.plans[one].tasks[0].session.as_deref(), Some("one-agent"));
                let loaded = plan::load_plans(&paths::imacs_home()).unwrap().0;
                assert_eq!(loaded.iter().find(|p| p.id == first_id).unwrap(), &app.plans[one]);
                assert_eq!(loaded.iter().find(|p| p.id == second_id).unwrap().tasks[0].title, "Second task");
            },
        );
    }

    #[test]
    fn plan_set_rewords_migrated_plan_without_losing_progress_and_creates_fresh_plans() {
        with_app(
            "app::plans::tests::plan_set_rewords_migrated_plan_without_losing_progress_and_creates_fresh_plans",
            |app, _| {
                let mut legacy = Plan::default();
                legacy.replace(
                    "Ship the API".into(),
                    vec![Task { id: "a".into(), title: "Build API".into(), ..Task::default() }],
                );
                legacy.approved = true;
                legacy.integration = Some("imacs/already-integrated".into());
                for task in &mut legacy.tasks {
                    task.status = if task.id == "a" { TaskStatus::Running } else { TaskStatus::Blocked };
                    task.session = Some(format!("agent-{}", task.id));
                    task.branch = Some(format!("imacs/existing-{}", task.id));
                    task.repo = Some("/project".into());
                    task.notes = vec!["Existing note".into()];
                    task.report = Some(Report { status: "progress".into(), summary: "Existing report".into(), at: 42 });
                    task.integrated = legacy.integration.clone();
                }
                legacy.save(&paths::plan_file()).unwrap();
                app.plans = plan::load_plans(&paths::imacs_home()).unwrap().0;
                app.selected_plan = 0;
                let id = app.plans[0].id.clone();
                let progress = app.plans[0].tasks.clone();
                let tasks =
                    json!([{"id":"a", "title":"Revised API task"}, {"id":"review", "title":"Review revised plan"}]);
                let updated =
                    request(app, json!({"cmd":"plan_set", "plan":id, "goal":"Deliver the API", "tasks":tasks}));
                assert!(updated.ok);
                assert_eq!(app.plans[0].goal, "Deliver the API");
                assert!(!app.plans[0].approved);
                assert_eq!(app.plans[0].integration, legacy.integration);
                assert_eq!(updated.result["integration"], "imacs/already-integrated");
                for (task, old) in app.plans[0].tasks.iter().zip(&progress) {
                    assert_eq!(task.status, old.status);
                    assert_eq!(task.session, old.session);
                    assert_eq!(task.branch, old.branch);
                    assert_eq!(task.repo, old.repo);
                    assert_eq!(task.notes, old.notes);
                    assert_eq!(task.report, old.report);
                    assert_eq!(task.integrated, old.integrated);
                }
                let original = app.plans[0].clone();
                for goal in ["Deliver the API", "Another goal"] {
                    let created = request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":tasks}));
                    assert!(created.ok);
                    let fresh = &app.plans[app.selected_plan];
                    assert_ne!(fresh.id, id);
                    assert!(!fresh.approved);
                    assert!(fresh.integration.is_none());
                    assert_ne!(fresh.integration_branch(), original.integration_branch());
                    for task in &fresh.tasks {
                        assert_eq!(task.status, TaskStatus::Planned);
                        assert!(task.session.is_none());
                        assert!(task.branch.is_none());
                        assert!(task.notes.is_empty());
                        assert!(task.report.is_none());
                        assert!(task.integrated.is_none());
                    }
                    assert_eq!(app.plans[0], original);
                }
                let loaded = plan::load_plans(&paths::imacs_home()).unwrap().0;
                assert_eq!(loaded.len(), 3);
                assert_eq!(loaded.iter().find(|p| p.id == id).unwrap(), &original);
            },
        );
    }

    #[test]
    fn background_dispatch_preserves_selection_and_routes_delayed_start_failure() {
        with_app(
            "app::plans::tests::background_dispatch_preserves_selection_and_routes_delayed_start_failure",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                let repo = dir.path().join("repo");
                std::fs::create_dir(&repo).unwrap();
                git::git(&repo, &["init", "-q", "-b", "main"]).unwrap();
                git::git(
                    &repo,
                    &[
                        "-c",
                        "user.name=Test",
                        "-c",
                        "user.email=test@example.org",
                        "commit",
                        "--allow-empty",
                        "-qm",
                        "initial",
                    ],
                )
                .unwrap();
                app.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                let mut indices = Vec::new();
                for goal in ["One", "Two"] {
                    assert!(
                        request(
                            app,
                            json!({"cmd":"plan_set", "goal":goal, "tasks":[
                    {"id":"a", "title":"Same task", "repo":repo, "agent":"missing-test-agent"}]})
                        )
                        .ok
                    );
                    indices.push(app.selected_plan);
                }
                let (one, two) = (indices[0], indices[1]);
                assert!(app.dispatch(one, Some("a")).unwrap_err().contains("not approved"));
                app.plans[one].approved = true;
                assert_eq!(app.dispatch(one, Some("a")).unwrap(), ["a"]);
                assert!(app.plans[one].tasks[0].dispatched_at.is_some());
                assert_eq!(app.selected_plan, two);
                assert!(app.plan_view);
                assert!(app.changed_plans.contains(&app.plans[one].id));
                app.selected_plan = two;
                app.show_plan_fleet();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !app.creating.is_empty() && Instant::now() < deadline {
                    app.pump(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(app.creating.is_empty());
                assert_eq!(app.selected_plan, two);
                assert_eq!(app.plans[one].tasks[0].status, TaskStatus::Blocked);
                assert!(app.plans[one].tasks[0].notes.last().unwrap().contains("could not start"));
                assert_eq!(app.plans[two].tasks[0].status, TaskStatus::Planned);
                assert!(app.plans[two].tasks[0].notes.is_empty());
            },
        );
    }
}
