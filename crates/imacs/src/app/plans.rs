use super::*;

const MAX_BRANCH_ALLOCATION_ATTEMPTS: usize = 100;

impl State {
    pub(super) fn show_plan_fleet(&mut self) {
        self.changed_plans.remove(&self.plans[self.selected_plan].id);
        self.view = self.plans[self.selected_plan].project.clone();
        self.plan_view = true;
        self.ungrouped = false;
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
            let workflow = self.plan_workflow(&self.plans[index]);
            let was_open = self.plans[index].is_open();
            let mut changed = self.plans[index].finish_tasks(now, &workflow);
            if was_open && self.plans[index].status == PlanStatus::Completed {
                self.ticket_plan_completed(index);
            }
            let completed = was_open && self.plans[index].status == PlanStatus::Completed;
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
            if completed {
                self.cleanup_closed_plan(index);
            }
        }
    }

    pub(super) fn task_action(&mut self, index: usize, task_index: usize, action: &str) {
        if action == "acknowledge_refs" {
            self.plans[index].tasks[task_index].ref_alert = None;
            self.save_plan(index);
            return;
        }
        if action == "resume" && self.plans[index].status != PlanStatus::Open {
            self.toast("Reopen the plan before resuming a task");
            return;
        }
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
                let resume = Some(task.resume_id.unwrap_or_default());
                match self.start_as(&ctx, agent, root, resume, task.args, None, Role::Agent(task.isolation)) {
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
        let workflow = self.plan_workflow(&self.plans[index]);
        if status == PlanStatus::Completed
            && workflow.done != "accepted"
            && (!self.plans[index].workflow_done(&workflow)
                || self.plans[index].tasks.iter().any(|t| t.status != TaskStatus::Done))
        {
            return Err(format!("Plan completion waits for workflow.done = {:?}.", workflow.done));
        }
        let mut closed = self.plans[index].clone();
        closed.status = status;
        closed.save_in(&paths::imacs_home()).map_err(|err| format!("Closing plan failed: {err:#}"))?;
        self.plans[index] = closed;
        self.changed_plans.remove(&self.plans[index].id);
        self.cleanup_closed_plan(index);
        if status == PlanStatus::Completed {
            self.ticket_plan_completed(index);
        }
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
        Ok(())
    }

    pub(super) fn cleanup_closed_plan(&mut self, index: usize) {
        let plan = self.plans[index].clone();
        let enabled = match plan.status {
            PlanStatus::Completed => self.config.cleanup.completed_plans,
            PlanStatus::Archived => self.config.cleanup.archived_plans,
            PlanStatus::Open => false,
        };
        if !enabled {
            return;
        }
        let ids: Vec<_> = plan.tasks.iter().filter_map(|task| task.session.clone()).collect();
        for id in &ids {
            if self.sessions.iter().any(|session| session.id == *id && session.chief)
                || (plan.status == PlanStatus::Completed && !self.config.plan.close_done_agents)
            {
                continue;
            }
            self.remember(id);
            self.close(id);
        }
        let protected: Vec<_> = self
            .sessions
            .iter()
            .map(Session::root)
            .chain(
                self.plans
                    .iter()
                    .filter(|p| p.id != plan.id && p.is_open())
                    .flat_map(|p| p.tasks.iter())
                    .filter_map(|t| t.worktree.clone()),
            )
            .collect();
        let mut preparations = Vec::new();
        for pending in std::mem::take(&mut self.pending_sandbox) {
            if ids.contains(&pending.0.id) && !self.sessions.iter().any(|session| session.id == pending.0.id) {
                preparations.push(pending);
            } else {
                self.pending_sandbox.push(pending);
            }
        }
        let sbx = resolve_program(&self.sbx);
        let actions = self.action_tx.clone();
        // What it may stop or remove: its tasks' sandboxes, and their worktrees where IMACS put or found them.
        let mut scope = Scope::none();
        for task in &plan.tasks {
            let repo = task.repo.as_deref().map(paths::expand).or_else(|| plan.project.clone());
            let made = repo
                .zip(task.branch.as_ref())
                .map(|(repo, branch)| git::worktree_path(&self.config.worktree_path, &repo, branch));
            scope.worktrees.get_or_insert_default().extend(task.worktree.iter().cloned().chain(made));
            scope.sandboxes.extend(task.session.as_deref().map(imacs_core::isolation::sandbox_name));
        }
        scope.sandboxes.extend(preparations.iter().filter_map(|(launch, _)| match &launch.runtime {
            AgentRuntime::Sandbox(sandbox) => Some(sandbox.name.clone()),
            AgentRuntime::Bare => None,
        }));
        let closing = self.running_lifecycle();
        self.lifecycle(scope, move || {
            // Final ref-guard checks must finish before deleting any task refs.
            for job in closing {
                job.wait();
            }
            for (launch, ready) in preparations {
                let _ = ready.recv();
                if let AgentRuntime::Sandbox(sandbox) = launch.runtime
                    && let Err(err) = crate::sandbox::stop(&paths::imacs_home(), &sbx, &sandbox.name)
                {
                    crate::lifecycle::record_leftovers(
                        &paths::imacs_home(),
                        &[format!("Stopping sandbox {}: {err:#}", sandbox.name)],
                    );
                }
            }
            let notes = crate::lifecycle::clean_plan(&paths::imacs_home(), &plan, &sbx, &protected);
            crate::lifecycle::record_leftovers(&paths::imacs_home(), &notes);
            let _ = actions.send(Box::new(move |state| {
                state.worktrees_at = None;
                if !notes.is_empty() {
                    state.toast(notes.join("\n"));
                }
            }));
        });
    }

    /// Never waits for the plan's cleanup: a task started again waits for it in its own worktree
    /// and sandbox jobs.
    pub(super) fn reopen_plan(&mut self, index: usize) -> Result<(), String> {
        let mut plan = self.plans[index].clone();
        plan.reopen();
        plan.save_in(&paths::imacs_home()).map_err(|err| format!("Reopening plan failed: {err:#}"))?;
        self.plans[index] = plan;
        self.integration_revision += 1;
        let id = self.plans[index].id.clone();
        self.refresh_tickets(Some(&id), None);
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
        value["workflow"] = json!(self.plan_workflow(&self.plans[index]));
        value["ready"] = json!(self.plans[index].ready().iter().map(|t| &t.id).collect::<Vec<_>>());
        value["integration"] = json!(self.plans[index].integration_branch());
        if let Some(tasks) = value["tasks"].as_array_mut() {
            for (task, value) in self.plans[index].tasks.iter().zip(tasks) {
                if task.integrated.is_some() {
                    value["integration_verified"] = json!(task.integration_verified);
                }
                if let Some(note) = self.integration_check_note(index, task) {
                    value["integration_check"] = json!(note);
                }
            }
        }
        if let Some(board) = self.ticket_summary(&self.plans[index].id).and_then(|s| s.get("board").cloned()) {
            value["ticket_board"] = board;
        }
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

    fn ticket_branch(
        &self,
        index: usize,
        link: Option<&imacs_core::tickets::TicketLink>,
        repo: &Path,
        task: Option<&str>,
    ) -> Option<String> {
        let config = self.project_config(repo).tickets;
        let plan_ticket = link.is_none();
        let link = link.or_else(|| {
            (config.mode == imacs_core::tickets::TicketMode::Ticket)
                .then_some(self.plans[index].ticket.as_ref())
                .flatten()
        })?;
        let mut branch = config.branch_name(link)?;
        if let Some(task) = task.filter(|_| plan_ticket || branch == self.plans[index].integration_branch()) {
            branch.push_str(&format!("-{task}"));
        }
        git::check_branch_name(repo, &branch, &self.project_config(repo).base_ref).ok()?;
        Some(branch)
    }

    /// Include pinned names even before their asynchronous worktree jobs finish.
    pub(super) fn branch_owners(&self, common: &Path, branch: &str) -> Vec<(usize, Option<&str>)> {
        let mut owners = Vec::new();
        for (index, plan) in self.plans.iter().enumerate() {
            if plan.status == PlanStatus::Archived {
                continue;
            }
            let repo = plan.project.clone().or_else(|| plan.tasks.iter().find_map(|t| self.task_repo(index, t)));
            if plan.integration_branch() == branch
                && repo.as_deref().is_some_and(|repo| git::common_dir(repo).is_ok_and(|dir| dir == common))
            {
                owners.push((index, None));
            }
            for task in &plan.tasks {
                if task.branch.as_deref() == Some(branch)
                    && self
                        .task_repo(index, task)
                        .is_some_and(|repo| git::common_dir(&repo).is_ok_and(|dir| dir == common))
                {
                    owners.push((index, Some(task.id.as_str())));
                }
            }
        }
        owners
    }

    fn available_plan_branch(
        &self,
        index: usize,
        task: Option<&str>,
        repo: &Path,
        preferred: &str,
        pinned: bool,
    ) -> Result<String, String> {
        let common = git::common_dir(repo).map_err(|err| format!("{err:#}"))?;
        let base = self.project_config(repo).base_ref;
        let preferred_path = git::worktree_path(&self.config.worktree_path, repo, preferred);
        let fixed_path =
            preferred_path == git::worktree_path(&self.config.worktree_path, repo, &format!("{preferred}-2"));
        for attempt in 0..MAX_BRANCH_ALLOCATION_ATTEMPTS {
            let branch = if attempt == 0 { preferred.to_string() } else { format!("{preferred}-{}", attempt + 1) };
            git::check_branch_name(repo, &branch, &base).map_err(|err| format!("{err:#}"))?;
            let owners = self.branch_owners(&common, &branch);
            if pinned && let Some((other, task)) = owners.iter().find(|owner| **owner != (index, task)) {
                let owner = task.map(|id| format!("task {id} in ")).unwrap_or_default();
                return Err(format!(
                    "Branch {branch} is already owned by {owner}plan {} in {}. Resolve the ownership conflict before retrying; the recorded branch was kept.",
                    self.plans[*other].id,
                    repo.display()
                ));
            }
            let owned = owners.as_slice() == [(index, task)];
            let path = git::worktree_path(&self.config.worktree_path, repo, &branch);
            let reserved_path = self
                .creating
                .iter()
                .any(|(dir, name, pending_path)| *pending_path == path && (*dir != common || *name != branch));
            if fixed_path
                && (!pinned || task.is_some())
                && (path.exists() || reserved_path)
                && !(owned
                    && !reserved_path
                    && git::common_dir(&path).is_ok_and(|dir| dir == common)
                    && git::worktree_of(&path).and_then(|tree| tree.branch).as_deref() == Some(&branch))
            {
                return Err(format!(
                    "Worktree path {} is occupied or reserved by other work. The worktree_path template does not vary with the branch; add {{slug}} to worktree_path or choose a free path.",
                    path.display()
                ));
            }
            if pinned {
                return Ok(branch);
            }
            let exists = git::branch_ref_exists(repo, &branch).map_err(|err| format!("{err:#}"))?
                || path.exists()
                || reserved_path
                || self.creating.iter().any(|(dir, name, _)| *dir == common && *name == branch);
            if owners.iter().all(|owner| *owner == (index, task)) && (!exists || owned) {
                return Ok(branch);
            }
        }
        Err(format!(
            "Could not allocate a branch for {preferred} in {} after {MAX_BRANCH_ALLOCATION_ATTEMPTS} attempts. Choose a distinct branch name or update worktree_path to use a free location.",
            repo.display()
        ))
    }

    /// Pin before the first branch is created; never rename work already in progress.
    pub(super) fn pin_ticket_integration(&mut self, index: usize) -> Result<(), String> {
        let plan = &self.plans[index];
        let existing =
            plan.integration.is_some() || plan.tasks.iter().any(|t| t.branch.is_some() || t.integrated.is_some());
        let repo =
            plan.project.clone().or_else(|| plan.tasks.iter().find_map(|t| t.repo.as_deref().map(paths::expand)));
        let ticket_mode = self.ticket_config().is_some_and(|c| c.mode == imacs_core::tickets::TicketMode::Ticket);
        let existing = existing
            || match repo.as_deref() {
                Some(repo) => {
                    git::branch_ref_exists(repo, &plan.integration_branch()).map_err(|err| format!("{err:#}"))?
                }
                None => false,
            };
        let branch = if !existing && ticket_mode {
            repo.as_deref().and_then(|repo| self.ticket_branch(index, plan.ticket.as_ref(), repo, None))
        } else {
            None
        };
        let branch = branch.unwrap_or_else(|| self.plans[index].integration_branch());
        let branch = if let Some(repo) = repo {
            self.available_plan_branch(index, None, &repo, &branch, existing)?
        } else {
            branch
        };
        self.plans[index].integration = Some(branch);
        self.save_plan(index);
        Ok(())
    }

    /// Starts `only`, or every ready task, each in a new worktree with its brief as
    /// the agent's first message.
    pub(super) fn dispatch(&mut self, index: usize, only: Option<&str>) -> Result<Vec<String>, String> {
        if self.plans[index].status != PlanStatus::Open {
            return Err("Reopen the plan before dispatching tasks.".into());
        }
        if !self.plans[index].approved {
            return Err("The plan is not approved yet: the user approves it on the plan board.".into());
        }
        self.pin_ticket_integration(index)?;
        self.refresh_integrations();
        let ready: Vec<Task> =
            self.plans[index].ready().into_iter().filter(|t| only.is_none_or(|id| t.id == id)).cloned().collect();
        if ready.is_empty() {
            let plan = &self.plans[index];
            if let Some(task) = plan.tasks.iter().find(|task| {
                only.is_none_or(|id| task.id == id)
                    && task.status == TaskStatus::Planned
                    && !plan.unintegrated_dependencies(task).is_empty()
            }) {
                let note = |id: &str| self.integration_check_note(index, plan.tasks.iter().find(|t| t.id == id)?);
                let dependencies: Vec<String> = plan
                    .unintegrated_dependencies(task)
                    .into_iter()
                    .map(|id| note(id).map_or_else(|| id.to_string(), |note| format!("{id} ({note})")))
                    .collect();
                return Err(format!(
                    "Task {} needs integrated dependencies: {}. Use imacs_plan_integrate on them first.",
                    task.id,
                    dependencies.join(", ")
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
                .or_else(|| self.ticket_branch(index, task.ticket.as_ref(), &repo, Some(&task.id)))
                .unwrap_or_else(|| format!("imacs/{}-{}-{}", self.plans[index].id, task.id, short_slug(&task.title)));
            let branch = self.available_plan_branch(index, Some(&task.id), &repo, &branch, task.branch.is_some())?;
            let agent = if task.agent.trim().is_empty() { "claude".to_string() } else { task.agent.clone() };
            let config = self.project_config(&repo);
            let sandbox = config
                .agent(&agent)
                .filter(|agent| {
                    config.isolation.mode_for(agent, task.isolation) == imacs_core::isolation::IsolationMode::Sandbox
                })
                .map(|_| config.isolation.sandbox.with_defaults(&config.resources, self.governor.cores));
            let mut brief = task.compose_brief_for(&self.plans[index].goal, sandbox.as_ref());
            if task.id == imacs_core::plan::REVIEW_TASK {
                // The reviewer reads every task's branch from the shared repository.
                let base = self.plan_base_ref(&self.plans[index]);
                brief.push_str("\n\nBranches to review (all in this repository):\n");
                let integration = self.plans[index].integration_branch();
                brief.push_str(&format!(
                    "- integration branch {integration} (git diff {base}...{integration}): all accepted work, review this first\n"
                ));
                for other in self.plans[index].tasks.iter().filter(|t| task.depends_on.contains(&t.id)) {
                    let branch = other.branch.clone().unwrap_or_else(|| "(not dispatched)".into());
                    brief.push_str(&format!("- {}: {} (git diff {base}...{branch})\n", other.id, other.title));
                }
                if sandbox.is_some() {
                    // The chief checks sandboxed work inside sandboxes, never on the host; so does its reviewer.
                    brief.push_str(
                        "\nRun the plan's builds and tests yourself, here in your own sandbox: your worktree starts \
                         from the integration branch.\n",
                    );
                }
            }
            let launch = Launch {
                agent,
                prompt: Some(brief),
                args: task.args.clone(),
                reply: None,
                task: Some((self.plans[index].id.clone(), task.id.clone())),
                isolation: task.isolation,
            };
            let start = self.plans[index].start_branch(&task);
            if let Some(t) = self.plans[index].task_mut(&task.id) {
                t.set_status(TaskStatus::Running);
                t.branch = Some(branch.clone());
                t.finished_at = None;
                t.dispatched_at = Some(imacs_core::store::now());
                t.session = None;
                t.report = None;
                t.repo.get_or_insert_with(|| repo.to_string_lossy().into_owned());
            }
            self.save_plan(index);
            self.create_worktree(repo.clone(), branch, start, launch);
            self.ticket_dispatched(index, &task.id);
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
            if self.sessions[found].sandbox.is_some() && self.sessions[found].status != AgentStatus::Exited {
                let session = &mut self.sessions[found];
                session.status = match status {
                    "done" => AgentStatus::Done,
                    "question" | "blocked" => AgentStatus::NeedsInput,
                    _ => session.status,
                };
                if status != "progress" {
                    session.screen_busy = true;
                }
                session.activity.observe(session.status);
            }
        }
        let session = found.map(|found| &self.sessions[found]);
        let session_id = pane.unwrap_or_default().to_string();
        if status == "question" {
            self.pending_questions.insert(session_id.clone());
        } else if status != "progress" {
            self.pending_questions.remove(&session_id);
        }
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
            config: None,
            ticket: None,
        });
        self.serve_waiters();
        if let Some((index, id)) = &owner {
            self.ticket_reported(*index, id, status, summary);
        }
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
            let mut blocked = Vec::new();
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
                    blocked.push(task.id.clone());
                    changed = true;
                }
            }
            if changed {
                self.save_plan(index);
            }
            for id in blocked {
                let (note, author) = (Some("the agent exited before reporting"), imacs_core::tickets::Author::Imacs);
                self.ticket_status_changed(index, &id, TaskStatus::Running, TaskStatus::Blocked, note, author);
            }
        }
        self.push_ticket_states(None);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{repository, request, session, with_app};
    use super::*;

    #[test]
    fn ctl_approval_survives_branch_edits_and_archives_release_branch_ownership() {
        with_app(
            "app::plans::tests::ctl_approval_survives_branch_edits_and_archives_release_branch_ownership",
            |app, _| {
                let (_dir, repo) = repository();
                let created = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"Approval", "integration":"shared",
                "tasks":[{"id":"a", "title":"Implement", "repo":repo}]}),
                );
                let id = created.result["plan"].clone();
                let index = app.runtime.plans.iter().position(|p| p.id == id).unwrap();
                assert!(!app.runtime.plans[index].approved);
                assert!(request(app, json!({"cmd":"plan_approve", "plan":id})).ok);
                assert!(
                    Plan::load(
                        &paths::imacs_home().join("plans").join(format!("{}.json", app.runtime.plans[index].id))
                    )
                    .unwrap()
                    .approved
                );
                let changed = request(
                    app,
                    json!({"cmd":"plan_set", "plan":id, "goal":"Approval", "integration":"renamed",
                "tasks":[{"id":"a", "title":"Implement", "repo":repo, "branch":"worker"}]}),
                );
                assert_eq!(changed.result["approved"], true);
                let common = git::common_dir(&repo).unwrap();
                assert_eq!(app.runtime.branch_owners(&common, "renamed"), [(index, None)]);
                app.runtime.config.cleanup.archived_plans = false;
                assert!(request(app, json!({"cmd":"plan_close", "plan":id, "status":"archived"})).ok);
                assert!(app.runtime.branch_owners(&common, "renamed").is_empty());
                assert!(app.runtime.branch_owners(&common, "worker").is_empty());
                assert!(
                    !request(app, json!({"cmd":"plan_approve", "plan":id})).ok,
                    "closed plans must be reopened explicitly"
                );
            },
        );
    }

    #[test]
    fn explicitly_reopened_plan_waits_for_fresh_lifecycle_progress() {
        with_app("app::plans::tests::explicitly_reopened_plan_waits_for_fresh_lifecycle_progress", |app, _| {
            app.runtime.config.workflow.done = "ci_passed".into();
            app.runtime.plans = vec![Plan {
                id: "reopen".into(),
                tasks: vec![Task { id: "a".into(), status: TaskStatus::Done, ..Default::default() }],
                ..Default::default()
            }];
            app.runtime.finish_plan_agents();
            let event = json!({"cmd":"stage_event", "plan":"reopen", "stage":"ci_passed"});
            assert!(request(app, event.clone()).ok);
            assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
            app.runtime.reopen_plan(0).unwrap();
            for _ in 0..2 {
                app.runtime.finish_plan_agents();
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
            }
            let duplicate = request(app, event);
            assert!(duplicate.ok);
            assert_eq!(duplicate.result["recorded"], false);
            assert_eq!(app.runtime.plans[0].status, PlanStatus::Open);
            assert!(request(app, json!({"cmd":"stage_event", "plan":"reopen", "stage":"merged"})).ok);
            assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
        });
    }

    #[test]
    fn retry_keeps_a_pinned_branch_in_an_independent_repository() {
        with_app("app::plans::tests::retry_keeps_a_pinned_branch_in_an_independent_repository", |app, _| {
            let (_a, repo_a) = repository();
            let (b, repo_b) = repository();
            app.runtime.config.base_ref = "main".into();
            app.runtime.config.worktree_path = "{repo_parent}/{repo}-{slug}".into();
            let branch = "issue/42-parser";
            let worktree = b.path().join("repo-issue-42-parser");
            git::add_worktree(&repo_b, &worktree, branch, "main").unwrap();
            std::fs::write(worktree.join("existing-work.txt"), "unfinished implementation").unwrap();
            app.runtime.plans = vec![
                Plan {
                    id: "project-a".into(),
                    project: Some(repo_a),
                    tasks: vec![Task { id: "a".into(), branch: Some(branch.into()), ..Default::default() }],
                    ..Default::default()
                },
                Plan {
                    id: "project-b".into(),
                    project: Some(repo_b),
                    approved: true,
                    tasks: vec![Task {
                        id: "b".into(),
                        branch: Some(branch.into()),
                        worktree: Some(worktree.clone()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ];
            app.runtime.dispatch(1, Some("b")).unwrap();
            let (allocated, prepared, _, _) = app
                .runtime
                .worktree_rx
                .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                .unwrap();
            let prepared = prepared.unwrap();
            assert_eq!(allocated, branch, "another repository must not rename this task's pinned branch");
            assert_eq!(app.runtime.plans[1].tasks[0].branch.as_deref(), Some(branch));
            assert_eq!(prepared.path, worktree);
            assert_eq!(
                std::fs::read_to_string(prepared.path.join("existing-work.txt")).unwrap(),
                "unfinished implementation"
            );
        });
    }

    #[test]
    fn linked_checkouts_refuse_conflicting_pinned_task_and_integration_branches() {
        with_app(
            "app::plans::tests::linked_checkouts_refuse_conflicting_pinned_task_and_integration_branches",
            |app, _| {
                let (dir, repo) = repository();
                let linked = dir.path().join("linked");
                git::add_worktree(&repo, &linked, "linked", "main").unwrap();
                app.runtime.config.base_ref = "main".into();
                let branch = "issue/42-parser";
                app.runtime.plans = vec![
                    Plan {
                        id: "owner".into(),
                        project: Some(repo),
                        tasks: vec![Task { id: "a".into(), branch: Some(branch.into()), ..Default::default() }],
                        ..Default::default()
                    },
                    Plan {
                        id: "retry".into(),
                        project: Some(linked),
                        approved: true,
                        tasks: vec![Task { id: "b".into(), branch: Some(branch.into()), ..Default::default() }],
                        ..Default::default()
                    },
                ];
                let err = app.runtime.dispatch(1, Some("b")).unwrap_err();
                assert!(err.contains(branch) && err.contains("task a") && err.contains("plan owner"), "{err}");
                assert_eq!(app.runtime.plans[1].tasks[0].branch.as_deref(), Some(branch));
                assert_eq!(app.runtime.plans[1].tasks[0].status, TaskStatus::Planned);
                assert!(app.runtime.creating.is_empty());
                app.runtime.plans[1].integration = Some(branch.into());
                let err = app.runtime.dispatch(1, Some("b")).unwrap_err();
                assert!(err.contains("ownership conflict"), "{err}");
                assert_eq!(app.runtime.plans[1].integration.as_deref(), Some(branch));
            },
        );
    }

    #[test]
    fn pending_branch_reservations_use_the_canonical_repository() {
        with_app("app::plans::tests::pending_branch_reservations_use_the_canonical_repository", |app, _| {
            let (dir, repo) = repository();
            let (_other, other) = repository();
            let linked = dir.path().join("linked");
            git::add_worktree(&repo, &linked, "linked", "main").unwrap();
            app.runtime.config.base_ref = "main".into();
            app.runtime.config.worktree_path = "{repo_parent}/{repo}-{slug}".into();
            app.runtime.plans = vec![Plan::default()];
            app.runtime.creating.push((
                git::common_dir(&repo).unwrap(),
                "reserved".into(),
                dir.path().join("reserved"),
            ));
            assert_eq!(
                app.runtime.available_plan_branch(0, Some("a"), &linked, "reserved", false).unwrap(),
                "reserved-2"
            );
            assert_eq!(app.runtime.available_plan_branch(0, Some("a"), &other, "reserved", false).unwrap(), "reserved");
        });
    }

    #[test]
    fn existing_fixed_worktree_path_returns_in_bounded_time() {
        with_app("app::plans::tests::existing_fixed_worktree_path_returns_in_bounded_time", |app, _| {
            let (dir, repo) = repository();
            app.runtime.config.base_ref = "main".into();
            let fixed = dir.path().join("fixed-worktree");
            app.runtime.config.worktree_path = fixed.to_string_lossy().into_owned();
            std::fs::create_dir(&fixed).unwrap();
            app.runtime.plans = vec![Plan {
                id: "fixed".into(),
                project: Some(repo),
                approved: true,
                tasks: vec![Task { id: "a".into(), ..Default::default() }],
                ..Default::default()
            }];
            let (done, monitor) = std::sync::mpsc::channel::<()>();
            let watchdog = std::thread::spawn(move || {
                if matches!(
                    monitor.recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5))),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    eprintln!("allocation never returns when an occupied worktree_path cannot vary with the branch");
                    std::process::exit(91);
                }
            });
            let started = Instant::now();
            let result = app.runtime.dispatch(0, Some("a"));
            done.send(()).unwrap();
            watchdog.join().unwrap();
            assert!(started.elapsed() < Duration::from_secs(5));
            let err = result.unwrap_err();
            assert!(
                err.contains(fixed.to_str().unwrap()) && err.contains("worktree_path") && err.contains("{slug}"),
                "{err}"
            );
            assert!(app.runtime.plans[0].tasks[0].branch.is_none());
            assert!(app.runtime.creating.is_empty());
        });
    }

    #[test]
    fn fixed_worktree_path_reserves_pending_work_and_allows_its_owner_to_retry() {
        with_app(
            "app::plans::tests::fixed_worktree_path_reserves_pending_work_and_allows_its_owner_to_retry",
            |app, _| {
                let (dir, repo) = repository();
                app.runtime.config.base_ref = "main".into();
                let fixed = dir.path().join("fixed-worktree");
                app.runtime.config.worktree_path = fixed.to_string_lossy().into_owned();
                app.runtime.plans = vec![Plan {
                    id: "fixed".into(),
                    project: Some(repo.clone()),
                    approved: true,
                    tasks: vec![Task { id: "a".into(), branch: Some("pinned".into()), ..Default::default() }],
                    ..Default::default()
                }];
                app.runtime.creating.push((git::common_dir(&repo).unwrap(), "other".into(), fixed.clone()));
                let err = app.runtime.dispatch(0, Some("a")).unwrap_err();
                assert!(err.contains("worktree_path"), "{err}");
                assert!(!fixed.exists(), "the path is reserved before the worker creates it");
                assert_eq!(app.runtime.plans[0].tasks[0].branch.as_deref(), Some("pinned"));
                app.runtime.creating.clear();
                git::add_worktree(&repo, &fixed, "pinned", "main").unwrap();
                app.runtime.dispatch(0, Some("a")).unwrap();
                let (branch, prepared, _, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                assert_eq!(branch, "pinned");
                assert_eq!(prepared.unwrap().path, fixed);
                app.runtime.plans[0].tasks[0].branch = None;
                assert!(
                    app.runtime
                        .available_plan_branch(0, Some("a"), &repo, "fresh", false)
                        .unwrap_err()
                        .contains("worktree_path")
                );
            },
        );
    }

    #[test]
    fn branch_allocation_stops_after_the_attempt_limit() {
        with_app("app::plans::tests::branch_allocation_stops_after_the_attempt_limit", |app, _| {
            let (dir, repo) = repository();
            app.runtime.config.base_ref = "main".into();
            app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
            app.runtime.plans = vec![Plan::default()];
            for attempt in 0..MAX_BRANCH_ALLOCATION_ATTEMPTS {
                let branch = if attempt == 0 { "taken".into() } else { format!("taken-{}", attempt + 1) };
                std::fs::create_dir(dir.path().join(branch)).unwrap();
            }
            let err = app.runtime.available_plan_branch(0, Some("a"), &repo, "taken", false).unwrap_err();
            assert!(err.contains(&format!("after {MAX_BRANCH_ALLOCATION_ATTEMPTS} attempts")), "{err}");
        });
    }

    #[test]
    fn colliding_ticket_branches_reserve_ownership_and_only_retry_their_own_worktree() {
        with_app(
            "app::plans::tests::colliding_ticket_branches_reserve_ownership_and_only_retry_their_own_worktree",
            |app, _| {
                use imacs_core::tickets::{McpServer, TicketConfig, TicketLink, TicketMode};
                let (dir, repo) = repository();
                app.runtime.config.base_ref = "main".into();
                app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.runtime.config.tickets = TicketConfig {
                    system: "example".into(),
                    branch: Some("issue/{key_number}-{slug}".into()),
                    server: McpServer { command: vec!["unused".into()], ..Default::default() },
                    ..Default::default()
                };
                app.runtime.apply_ticket_config();
                let link =
                    |key: &str| TicketLink { id: key.into(), title: Some("Fix parser".into()), ..Default::default() };
                app.runtime.plans = vec![Plan {
                    id: "collisions".into(),
                    project: Some(repo.clone()),
                    approved: true,
                    tasks: vec![
                        Task { id: "eng".into(), ticket: Some(link("ENG-42")), ..Default::default() },
                        Task { id: "ops".into(), ticket: Some(link("OPS-42")), ..Default::default() },
                    ],
                    ..Default::default()
                }];
                assert_eq!(app.runtime.dispatch(0, None).unwrap(), ["eng", "ops"]);
                let mut trees = HashMap::new();
                for _ in 0..2 {
                    let (branch, result, _, _) = app
                        .runtime
                        .worktree_rx
                        .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                        .unwrap();
                    trees.insert(branch, result.unwrap().path);
                }
                let eng = "issue/42-fix-parser";
                let ops = "issue/42-fix-parser-2";
                assert_eq!(app.runtime.plans[0].tasks[0].branch.as_deref(), Some(eng));
                assert_eq!(app.runtime.plans[0].tasks[1].branch.as_deref(), Some(ops));
                assert_ne!(trees[eng], trees[ops]);
                for (branch, path) in &trees {
                    assert_eq!(git::worktree_of(path).unwrap().branch.as_ref(), Some(branch));
                }

                app.runtime.plans[0].tasks[0].set_status(TaskStatus::Planned);
                app.runtime.config.tickets.branch = Some("changed/{key}".into());
                app.runtime.dispatch(0, Some("eng")).unwrap();
                let (branch, result, _, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                assert_eq!(branch, eng);
                assert_eq!(result.unwrap().path, trees[eng]);

                // Even a caller that skips branch allocation cannot reuse another task's worktree.
                app.runtime.create_worktree(
                    repo.clone(),
                    eng.into(),
                    None,
                    Launch {
                        agent: "unused".into(),
                        prompt: None,
                        args: vec![],
                        reply: None,
                        task: Some(("collisions".into(), "ops".into())),
                        isolation: None,
                    },
                );
                let (_, result, _, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                assert!(result.is_err());

                // Reserve another plan's name and an unmanaged branch, including the next suffix.
                app.runtime.config.tickets.mode = TicketMode::Ticket;
                app.runtime.config.tickets.branch = Some("issue/{key_number}-{slug}".into());
                app.runtime.apply_ticket_config();
                let unmanaged = "issue/42-fix-parser-3";
                git::ensure_branch(&repo, unmanaged, "main").unwrap();
                app.runtime.plans.push(Plan {
                    id: "another".into(),
                    project: Some(repo.clone()),
                    ticket: Some(link("OTHER-42")),
                    ..Default::default()
                });
                app.runtime.pin_ticket_integration(1).unwrap();
                assert_eq!(app.runtime.plans[1].integration_branch(), "issue/42-fix-parser-4");
                app.runtime.plans[1].approved = true;
                app.runtime.plans[1].tasks.push(Task {
                    id: "third".into(),
                    ticket: Some(link("THIRD-42")),
                    ..Default::default()
                });
                app.runtime.dispatch(1, None).unwrap();
                let (branch, result, _, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                assert_eq!(branch, "issue/42-fix-parser-5");
                assert!(!trees.values().any(|path| *path == result.as_ref().unwrap().path));
                assert_eq!(git::worktree_of(&result.unwrap().path).unwrap().branch, Some(branch));
            },
        );
    }

    #[test]
    fn ticket_branches_validate_fallback_and_preserve_existing_names() {
        with_app("app::plans::tests::ticket_branches_validate_fallback_and_preserve_existing_names", |app, _| {
            use imacs_core::tickets::{McpServer, TicketConfig, TicketLink, TicketMode};
            let (_dir, repo) = repository();
            app.runtime.config.tickets = TicketConfig {
                system: "example".into(),
                mode: TicketMode::Ticket,
                branch: Some("issue/{key_number}-{slug}".into()),
                server: McpServer { command: vec!["unused".into()], ..Default::default() },
                ..Default::default()
            };
            app.runtime.apply_ticket_config();
            let link = TicketLink { id: "ENG-42".into(), title: Some("Fix parser".into()), ..Default::default() };
            app.runtime.plans = vec![Plan {
                id: "branches".into(),
                project: Some(repo.clone()),
                ticket: Some(link.clone()),
                ..Default::default()
            }];
            assert_eq!(app.runtime.ticket_branch(0, None, &repo, Some("a")).as_deref(), Some("issue/42-fix-parser-a"));
            assert_eq!(
                app.runtime.ticket_branch(0, Some(&link), &repo, Some("a")).as_deref(),
                Some("issue/42-fix-parser")
            );
            app.runtime.pin_ticket_integration(0).unwrap();
            assert_eq!(app.runtime.plans[0].integration_branch(), "issue/42-fix-parser");
            app.runtime.config.tickets.branch = Some("changed/{key}".into());
            app.runtime.apply_ticket_config();
            app.runtime.pin_ticket_integration(0).unwrap();
            assert_eq!(app.runtime.plans[0].integration_branch(), "issue/42-fix-parser");
            for invalid in ["bad..name", "main", "-option", "refs/heads/bad name", "{branch}"] {
                app.runtime.config.tickets.branch = Some(invalid.into());
                app.runtime.apply_ticket_config();
                assert!(app.runtime.ticket_branch(0, Some(&link), &repo, None).is_none(), "{invalid}");
            }
            app.runtime.plans[0].integration = None;
            app.runtime.plans[0].tasks = vec![Task { branch: Some("existing".into()), ..Default::default() }];
            app.runtime.pin_ticket_integration(0).unwrap();
            assert_eq!(app.runtime.plans[0].integration_branch(), "imacs/plan-branches");
            std::fs::create_dir(repo.join(".imacs")).unwrap();
            std::fs::write(
                repo.join(".imacs/config.toml"),
                "base_ref = 'main'\n[tickets]\nbranch = 'repo/{key_lower}/{slug}'",
            )
            .unwrap();
            app.runtime.reload_repo_workflows();
            assert_eq!(
                app.runtime.ticket_branch(0, Some(&link), &repo, None).as_deref(),
                Some("repo/eng-42/fix-parser")
            );
            app.runtime.pin_ticket_integration(0).unwrap();
            assert_eq!(
                app.runtime.plans[0].integration_branch(),
                "imacs/plan-branches",
                "repo edits never rename existing branches"
            );
        });
    }

    #[test]
    fn dispatched_review_records_branch_closes_on_acceptance_and_can_be_cleaned_up() {
        with_app(
            "app::plans::tests::dispatched_review_records_branch_closes_on_acceptance_and_can_be_cleaned_up",
            |app, ctx| {
                // Exercise the optional manual preview after disabling cleanup on completion.
                app.runtime.config.cleanup.completed_plans = false;
                let (_dir, repo) = repository();
                app.runtime.plans = vec![Plan {
                    id: "review-cleanup".into(),
                    project: Some(repo.clone()),
                    approved: true,
                    tasks: vec![Task { id: plan::REVIEW_TASK.into(), ..Task::default() }],
                    ..Plan::default()
                }];
                app.runtime.selected_plan = 0;
                app.runtime.config.isolation.mode = imacs_core::isolation::IsolationMode::Sandbox;
                app.runtime.dispatch(0, Some(plan::REVIEW_TASK)).unwrap();
                let (branch, worktree, launch, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                let brief = launch.prompt.unwrap();
                assert!(
                    brief.contains("Run the plan's builds and tests yourself, here in your own sandbox"),
                    "{brief}"
                );
                let worktree = worktree.unwrap().path;
                assert_eq!(app.runtime.plans[0].tasks[0].branch.as_deref(), Some(branch.as_str()));
                assert_eq!(git::worktree_of(&worktree).unwrap().branch.as_deref(), Some(branch.as_str()));
                assert_eq!(plan::load_plans(&paths::imacs_home()).unwrap().0[0].tasks[0].branch, Some(branch.clone()));
                app.runtime.sessions = vec![session("reviewer", AgentKind::Claude, AgentStatus::Done, &worktree)];
                app.runtime.plans[0].tasks[0].session = Some("reviewer".into());
                app.runtime.plans[0].tasks[0].status = TaskStatus::Review;
                for (id, can_integrate) in [("code", true), (plan::REVIEW_TASK, false)] {
                    app.runtime.plans[0].tasks[0].id = id.into();
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1600.0, 1000.0))),
                            ..Default::default()
                        },
                        |ui| {
                            egui::CentralPanel::default().show(ui, |ui| app.plan_panel(ui, ui.max_rect()));
                        },
                    );
                    app.runtime.drain_actions();
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
                assert!(app.runtime.find("reviewer").is_none());
                assert_eq!(app.runtime.plans[0].status, PlanStatus::Completed);
                assert!(app.runtime.plans[0].tasks[0].integrated.is_none());
                assert_eq!(app.runtime.plans[0].tasks[0].worktree.as_ref(), Some(&worktree));
                let rows = imacs_core::cleanup::preview(&app.runtime.plans[0], "main").unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].branch, branch);
                imacs_core::cleanup::remove(&app.runtime.plans[0], "main", &rows[0]).unwrap();
                assert!(!worktree.exists());
                assert!(git::git(&repo, &["show-ref", "--verify", &format!("refs/heads/{branch}")]).is_err());
            },
        );
    }

    #[test]
    fn accepted_agents_exit_keep_results_and_complete_without_closing_chief() {
        with_app(
            "app::plans::tests::accepted_agents_exit_keep_results_and_complete_without_closing_chief",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let root = dir.path();
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.palette.terminal_colors(),
                    "/bin/sh".into(),
                    vec!["-c".into(), "exec sleep 600".into()],
                    root,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let pid = terminal.process_id();
                app.runtime.sessions = vec![
                    session("worker", AgentKind::Claude, AgentStatus::Done, root),
                    session("reviewer", AgentKind::Claude, AgentStatus::Idle, root),
                    session("chief", AgentKind::Claude, AgentStatus::Working, root),
                    session("other-plan", AgentKind::Generic, AgentStatus::Working, root),
                ];
                app.runtime.sessions[0].terminal = Some(terminal);
                app.runtime.sessions[2].chief = true;
                let index = app.runtime.selected_plan;
                app.runtime.plans[index] = Plan {
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
                let into = app.runtime.plans[index].integration_branch();
                app.runtime.plans[index].tasks[0].status = TaskStatus::Running;
                app.runtime.plans[index].tasks[0].integrated = Some(into.clone());
                assert_eq!(app.runtime.report(Some("worker"), "done", "Last report\nSecond line"), Some("code".into()));
                app.runtime.sync_plan();
                assert_eq!(app.runtime.plans[index].tasks[0].status, TaskStatus::Review);
                assert!(app.runtime.plans[index].tasks[0].finished_at.is_none());
                assert!(app.runtime.completed_tasks().is_empty());
                assert!(
                    app.runtime.sessions[app.runtime.find("worker").unwrap()]
                        .terminal
                        .as_ref()
                        .is_some_and(|t| !t.exited())
                );
                app.runtime.plans[index].tasks[0].integrated = None;
                assert!(
                    request(app, json!({"cmd":"plan_update", "plan":"lifecycle", "id":"code", "status":"done"})).ok
                );
                assert!(app.runtime.find("worker").is_some(), "acceptance alone must not close unintegrated work");
                app.runtime.diffs.entry(root.into()).or_default().prepared = Some(Arc::new(diff_view::Prepared {
                    diff: imacs_core::diff::WorktreeDiff::default(),
                    highlights: vec![],
                    longest_line: 0,
                    additions: 412,
                    deletions: 88,
                    all_files: Arc::default(),
                }));
                app.runtime.finish_integration(Integrated {
                    commit: None,
                    plan: "lifecycle".into(),
                    task: "code".into(),
                    branch: "code".into(),
                    into,
                    result: Ok(Integration { committed: false, merge: Merge::Merged, worktree: root.into() }),
                    reply: None,
                });
                assert!(app.runtime.find("worker").is_none());
                assert_eq!(app.runtime.report(Some("worker"), "done", "Final late report"), Some("code".into()));
                assert_eq!(app.runtime.plans[index].tasks[0].status, TaskStatus::Done);
                let event = imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":"Stop", "session_id":"worker", "cwd":root,
                }))
                .unwrap();
                app.runtime.on_hook(HookMessage {
                    pane: Some("worker".into()),
                    ancestors: vec![],
                    event,
                    delivered: 0,
                    approval_reply: None,
                });
                assert!(app.runtime.find("worker").is_none(), "late shutdown hooks must not recreate a closed agent");
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
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
                assert_eq!(app.runtime.completed_tasks(), [(index, 0)]);
                let task = &app.runtime.plans[index].tasks[0];
                assert_eq!(task.worktree.as_deref(), Some(root));
                assert_eq!(task.resume_id.as_deref(), Some("worker"));
                assert_eq!((task.additions, task.deletions), (412, 88));
                assert_eq!(task.report.as_ref().unwrap().summary, "Final late report");
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Open);
                assert_eq!(app.runtime.report(Some("reviewer"), "done", "Review passed"), Some("review".into()));
                app.runtime.sync_plan();
                assert!(app.runtime.find("reviewer").is_some(), "the reviewer report must also wait for acceptance");
                assert!(app.runtime.plans[index].tasks[1].finished_at.is_none());
                assert!(
                    request(app, json!({"cmd":"plan_update", "plan":"lifecycle", "id":"review", "status":"done"})).ok
                );
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Completed);
                assert!(app.runtime.find("reviewer").is_none());
                assert!(app.runtime.find("chief").is_some());
                assert!(app.runtime.find("other-plan").is_some());
                app.runtime.task_action(index, 0, "diff");
                assert_eq!(app.runtime.diff_target().as_deref(), Some(root));
                assert!(app.runtime.panel == PanelMode::Diff);
                app.runtime.reopen_plan(index).unwrap();
                app.runtime.sync_plan();
                assert_eq!(
                    app.runtime.plans[index].status,
                    PlanStatus::Open,
                    "reopening must survive a lifecycle sweep"
                );
                app.runtime.close_plan(index, PlanStatus::Archived).unwrap();
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Archived);
                assert!(app.runtime.completed_tasks().is_empty());
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
            app.runtime.config.plan.close_done_agents = false;
            app.runtime.sessions = vec![session("worker", AgentKind::Claude, AgentStatus::Idle, Path::new("/work"))];
            app.runtime.plans[app.runtime.selected_plan] = Plan {
                id: "disabled".into(),
                tasks: vec![Task { status: TaskStatus::Done, session: Some("worker".into()), ..Task::default() }],
                ..Plan::default()
            };
            app.runtime.finish_plan_agents();
            assert!(app.runtime.find("worker").is_some());
            assert_eq!(app.runtime.plans[app.runtime.selected_plan].status, plan::PlanStatus::Completed);
            assert!(app.runtime.completed_tasks().is_empty());
            app.runtime.config.plan.close_done_agents = true;
            app.runtime.finish_plan_agents();
            assert!(app.runtime.find("worker").is_none());
            assert!(app.runtime.completed_tasks().is_empty());
            app.runtime.plan_view = true;
            assert_eq!(app.runtime.completed_tasks(), [(app.runtime.selected_plan, 0)]);
        });
    }

    #[test]
    fn completed_task_resume_uses_claude_or_codex_resume_and_reopens_plan() {
        with_app("app::plans::tests::completed_task_resume_uses_claude_or_codex_resume_and_reopens_plan", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
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
                app.runtime.config.agents = vec![imacs_core::config::AgentConfig {
                    name: agent.into(),
                    command: vec![
                        "/bin/sh".into(),
                        script.to_string_lossy().into_owned(),
                        output.to_string_lossy().into_owned(),
                    ],
                    kind,
                    busy_text: None,
                    isolation: None,
                }];
                let index = app.runtime.selected_plan;
                app.runtime.plans[index] = Plan {
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
                app.runtime.task_action(index, 0, "resume");
                assert!(!output.exists(), "a completed plan cannot resume without an explicit reopen");
                app.runtime.reopen_plan(index).unwrap();
                app.runtime.task_action(index, 0, "resume");
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                let args = loop {
                    let args = std::fs::read_to_string(&output).unwrap_or_default();
                    if args.contains("test-model") {
                        break args;
                    }
                    assert!(Instant::now() < deadline, "resume did not start: {:?}", app.runtime.toast);
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(args.contains(expected), "{args}");
                assert_eq!(app.runtime.plans[index].status, plan::PlanStatus::Open);
                let task = &app.runtime.plans[index].tasks[0];
                assert_eq!(task.status, TaskStatus::Running);
                assert!(task.finished_at.is_none() && task.dispatched_at.is_some() && task.integrated.is_none());
                let session = task.session.clone().unwrap();
                assert_eq!(app.runtime.sessions[app.runtime.find(&session).unwrap()].root(), dir.path());
                assert!(app.runtime.completed_tasks().is_empty());
                let count = app.runtime.sessions.len();
                app.runtime.task_action(index, 0, "resume");
                assert_eq!(app.runtime.sessions.len(), count, "a live session must never be resumed into a duplicate");
                assert_eq!(app.runtime.plans[index].tasks[0].session.as_deref(), Some(session.as_str()));
                app.runtime.close(&session);
            }
        });
    }

    #[test]
    fn redispatch_and_late_exit_events_preserve_the_current_task_session() {
        with_app(
            "app::plans::tests::redispatch_and_late_exit_events_preserve_the_current_task_session",
            |app, _ctx| {
                let (_dir, repo) = repository();
                app.runtime.plans[0] = Plan {
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
                app.runtime.sessions = vec![session("old", AgentKind::Generic, AgentStatus::Exited, &repo)];
                app.runtime.dispatch(0, Some("task")).unwrap();
                assert!(app.runtime.plans[0].tasks[0].session.is_none());
                assert!(app.runtime.plans[0].tasks[0].report.is_none());
                app.runtime.sync_plan();
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Running);
                app.runtime.sessions.push(session("new", AgentKind::Generic, AgentStatus::Working, &repo));
                app.runtime.sessions[1].terminal = Some(
                    Terminal::spawn(
                        app.runtime.wake.clone(),
                        app.runtime.palette.terminal_colors(),
                        "/bin/sleep".into(),
                        vec!["10".into()],
                        &repo,
                        HashMap::new(),
                        100,
                    )
                    .unwrap(),
                );
                app.runtime.plans[0].tasks[0].session = Some("new".into());
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
                app.runtime.on_hook(exit("new", "old"));
                app.runtime.on_hook(exit("old", "old"));
                let mut stale = exit("new", "retired");
                stale.pane = None;
                stale.ancestors = vec![app.runtime.sessions[1].terminal.as_ref().unwrap().process_id()];
                app.runtime.on_hook(stale);
                app.runtime.sync_plan();
                assert_eq!(app.runtime.sessions[1].status, AgentStatus::Working);
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Running);
                app.runtime.on_hook(exit("new", "new"));
                app.runtime.sync_plan();
                assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Blocked);
                assert_eq!(app.runtime.plans[0].tasks[0].notes, ["the agent exited before reporting"]);
                // Finish the worktree job before its temporary repository is removed.
                let (_, result, _, _) = app
                    .runtime
                    .worktree_rx
                    .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                    .unwrap();
                result.unwrap();
            },
        );
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
                            {"id":"a", "brief":brief, "branch":"task-a"},
                            {"id":"b", "depends_on":["a"]}
                        ]
                    }),
                )
                .result["plan"]
                    .clone();
                let index = app.runtime.selected_plan;
                app.runtime.plans[index].tasks[0].notes = vec!["old note".into(); 1000];
                app.runtime.plans[index].tasks[0].report =
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
                let into = app.runtime.plans[index].integration_branch();
                for (merge, committed, merged, note) in [
                    (Merge::Merged, true, true, format!("committed and merged into {into}")),
                    (Merge::UpToDate, false, false, format!("already in {into}")),
                ] {
                    let (reply, received) = channel();
                    app.runtime.finish_integration(Integrated {
                        commit: None,
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
                app.runtime.sync_plan();
                assert_eq!(request(app, json!({"cmd":"plan_get", "plan":two})).result["status"], "completed");
                assert!(request(app, json!({"cmd":"plan_update", "id":"a", "status":"done"})).ok);
                assert_eq!(app.runtime.plans[app.runtime.selected_plan].id, one.as_str().unwrap());
                assert_eq!(request(app, json!({"cmd":"plan_get"})).result["plans"].as_array().unwrap().len(), 1);
                assert!(request(app, json!({"cmd":"plan_close", "plan":one, "status":"archived"})).ok);
                assert!(!app.runtime.plan_view);
                assert!(app.runtime.plans.iter().all(|p| !p.is_open()));
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
                app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.runtime.config.agents.push(imacs_core::config::AgentConfig {
                    name: "fixture".into(),
                    command: vec!["/bin/cat".into()],
                    kind: AgentKind::Shell,
                    busy_text: None,
                    isolation: None,
                });
                for dock in [ChiefDock::Floating, ChiefDock::Docked] {
                    request(
                        app,
                        json!({"cmd":"plan_set", "goal":format!("Board {dock:?}"), "tasks":[{"id":"a", "repo":repo, "agent":"fixture"}]}),
                    );
                    let index = app.runtime.selected_plan;
                    app.runtime.plans[index].approved = true;
                    app.runtime.plan_view = false;
                    app.runtime.panel = PanelMode::Plan;
                    app.runtime.diff_open = true;
                    app.runtime.chief_open = true;
                    app.runtime.store.chief_dock = Some(dock);
                    assert_eq!(app.runtime.dispatch(index, Some("a")).unwrap(), ["a"]);
                    assert!(app.runtime.plan_view && app.runtime.panel == PanelMode::Diff);
                    assert_eq!(app.runtime.chief_open, dock == ChiefDock::Docked);
                    assert!(!app.runtime.changed_plans.contains(&app.runtime.plans[index].id));
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while !app.runtime.creating.is_empty() {
                        assert!(Instant::now() < deadline);
                        app.runtime.runtime_tick(ctx);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let session = app.runtime.plans[index].tasks[0].session.as_ref().unwrap();
                    assert_eq!(
                        app.runtime.visible_sessions().iter().map(|&i| &app.runtime.sessions[i].id).collect::<Vec<_>>(),
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
                app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
                app.runtime.config.agents.push(imacs_core::config::AgentConfig {
                    name: "fixture".into(),
                    command: vec!["/bin/cat".into()],
                    kind: AgentKind::Shell,
                    busy_text: None,
                    isolation: None,
                });
                let first = request(app, json!({"cmd":"plan_set", "goal":"Background", "tasks":[{"id":"a", "repo":repo, "agent":"fixture"}]})).result["plan"].clone();
                let one = app.runtime.selected_plan;
                request(app, json!({"cmd":"plan_set", "goal":"Foreground", "tasks":[{"id":"a"}]}));
                let two = app.runtime.selected_plan;
                app.runtime.sessions.push(session("foreground", AgentKind::Generic, AgentStatus::Idle, &repo));
                app.runtime.active = Some("foreground".into());
                app.runtime.diff_root = Some(repo.clone());
                app.runtime.focus_terminal = Some("foreground".into());
                app.runtime.chief_open = true;
                app.runtime.panel = PanelMode::Editor;
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Background revised", "tasks":[
                {"id":"a", "repo":repo, "agent":"fixture"}]})
                    )
                    .ok
                );
                assert_eq!(app.runtime.selected_plan, two);
                assert!(app.runtime.chief_open && app.runtime.panel == PanelMode::Editor);
                assert_eq!(app.runtime.active.as_deref(), Some("foreground"));
                assert_eq!(app.runtime.diff_root.as_ref(), Some(&repo));
                assert!(app.runtime.changed_plans.contains(first.as_str().unwrap()));
                assert!(app.runtime.toast.as_ref().unwrap().0.starts_with("Plan updated:"));
                app.runtime.plans[one].approved = true;
                assert!(request(app, json!({"cmd":"plan_dispatch", "plan":first, "id":"a"})).ok);
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.creating.is_empty() {
                    assert!(Instant::now() < deadline);
                    app.runtime.runtime_tick(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(app.runtime.plans[one].tasks[0].session.is_some());
                assert_eq!(app.runtime.selected_plan, two);
                assert!(app.runtime.chief_open && app.runtime.panel == PanelMode::Editor);
                assert_eq!(app.runtime.active.as_deref(), Some("foreground"));
                assert_eq!(app.runtime.diff_root.as_ref(), Some(&repo));
                assert_eq!(app.runtime.focus_terminal.as_deref(), Some("foreground"));
                assert!(app.runtime.changed_plans.contains(first.as_str().unwrap()));
                app.runtime.selected_plan = one;
                app.runtime.show_plan_fleet();
                assert!(!app.runtime.changed_plans.contains(first.as_str().unwrap()));
                assert!(
                    request(
                        app,
                        json!({"cmd":"plan_set", "plan":first, "goal":"Selected update", "tasks":[{"id":"a"}]})
                    )
                    .ok
                );
                assert!(app.runtime.panel == PanelMode::Plan);
                assert!(!app.runtime.chief_open);
            },
        );
    }

    #[test]
    fn multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate() {
        with_app(
            "app::plans::tests::multiple_plans_keep_boards_fleets_tools_reports_and_integrations_separate",
            |app, _| {
                app.runtime.config.plan.close_done_agents = false;
                let first = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"First task"}]}),
                );
                assert!(first.ok);
                let first_id = first.result["plan"].as_str().unwrap().to_string();
                let one = app.runtime.selected_plan;
                app.runtime.plans[one].approved = true;
                app.runtime.plans[one].tasks[0].status = TaskStatus::Blocked;
                app.runtime.plans[one].tasks[0].session = Some("one-agent".into());
                let second = request(
                    app,
                    json!({"cmd":"plan_set", "goal":"First goal", "tasks":[{"id":"a", "title":"Second task"}]}),
                );
                assert!(second.ok);
                let second_id = second.result["plan"].as_str().unwrap().to_string();
                assert_ne!(first_id, second_id);
                let two = app.runtime.selected_plan;
                assert!(app.runtime.plans[one].approved);
                assert!(!app.runtime.plans[two].approved);
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Planned);
                assert_ne!(app.runtime.plans[one].integration_branch(), app.runtime.plans[two].integration_branch());
                assert_eq!(app.runtime.plans[one].tasks.last().unwrap().id, "review");
                assert_eq!(app.runtime.plans[two].tasks.last().unwrap().id, "review");
                app.runtime.plans[two].tasks[0].session = Some("two-agent".into());
                app.runtime.sessions = vec![
                    session("one-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                    session("two-agent", AgentKind::Generic, AgentStatus::Working, Path::new("/project")),
                    session("outside", AgentKind::Generic, AgentStatus::Idle, Path::new("/project")),
                    session("chief", AgentKind::Claude, AgentStatus::Idle, Path::new("/project")),
                ];
                app.runtime.sessions[3].chief = true;
                app.runtime.repos = vec!["/project".into()];
                for (index, visible) in [(one, vec![0]), (two, vec![1])] {
                    app.runtime.selected_plan = index;
                    app.runtime.show_plan_fleet();
                    assert_eq!(app.runtime.visible_sessions(), visible);
                    app.runtime.toggle_plan();
                    assert!(app.runtime.panel == PanelMode::Plan);
                    app.runtime.toggle_plan();
                    assert!(app.runtime.panel == PanelMode::Diff);
                }
                app.runtime.set_view(None);
                assert_eq!(app.runtime.visible_sessions(), [0, 1, 2]);
                app.runtime.set_view(Some("/project".into()));
                assert_eq!(app.runtime.visible_sessions(), [0, 1, 2]);
                app.runtime.selected_plan = two;
                app.runtime.plans[one].project = Some("/one".into());
                app.runtime.plans[two].project = Some("/two".into());
                assert_eq!(app.runtime.task_repo(one, &Task::default()), Some("/one".into()));
                assert_eq!(app.runtime.task_repo(two, &Task::default()), Some("/two".into()));
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
                assert_eq!(app.runtime.selected_plan, two);
                let update = request(
                    app,
                    json!({"cmd":"plan_update", "plan":first_id, "id":"a", "status":"done", "note":"verified"}),
                );
                assert!(update.ok);
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Done);
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Planned);
                assert_eq!(app.runtime.selected_plan, two);
                let into = app.runtime.plans[one].integration_branch();
                app.runtime.integrating.insert((first_id.clone(), "a".into()));
                app.runtime.integrating.insert((second_id.clone(), "a".into()));
                app.runtime.finish_integration(Integrated {
                    commit: None,
                    plan: first_id.clone(),
                    task: "a".into(),
                    branch: "first".into(),
                    into: into.clone(),
                    result: Ok(Integration { committed: true, merge: Merge::Merged, worktree: "/integration".into() }),
                    reply: None,
                });
                assert_eq!(app.runtime.plans[one].tasks[0].integrated.as_deref(), Some(into.as_str()));
                assert!(app.runtime.plans[two].tasks[0].integrated.is_none());
                assert!(app.runtime.integrating.contains(&(second_id.clone(), "a".into())));
                assert_eq!(app.runtime.report(Some("one-agent"), "done", "first report"), Some("a".into()));
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Done);
                assert!(app.runtime.plans[two].tasks[0].report.is_none());
                let waited = request(app, json!({"cmd":"wait", "after":0, "statuses":["reported"]}));
                assert_eq!(waited.result["events"][0]["plan"], first_id);
                assert_eq!(waited.result["events"][0]["task"], "a");
                app.runtime.record_events();
                for (session, plan) in [("one-agent", &first_id), ("two-agent", &second_id)] {
                    let event = app
                        .runtime
                        .events
                        .iter()
                        .find(|e| e.session == session && e.report.is_none())
                        .unwrap()
                        .to_json();
                    assert_eq!(event["plan"], *plan);
                    assert_eq!(event["task"], "a");
                }
                app.runtime.sessions[0].status = AgentStatus::Exited;
                app.runtime.plans[one].tasks[0].status = TaskStatus::Running;
                app.runtime.sync_plan();
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Blocked);
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Planned);
                let old = app.runtime.plans[two].clone();
                assert!(request(app, json!({"cmd":"plan_set", "plan":first_id, "goal":"First goal", "tasks":[{"id":"a", "title":"Revised"}]})).ok);
                assert_eq!(app.runtime.plans[two], old);
                assert_eq!(app.runtime.plans[one].tasks[0].session.as_deref(), Some("one-agent"));
                let loaded = plan::load_plans(&paths::imacs_home()).unwrap().0;
                assert_eq!(loaded.iter().find(|p| p.id == first_id).unwrap(), &app.runtime.plans[one]);
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
                app.runtime.plans = plan::load_plans(&paths::imacs_home()).unwrap().0;
                app.runtime.selected_plan = 0;
                let id = app.runtime.plans[0].id.clone();
                let progress = app.runtime.plans[0].tasks.clone();
                let tasks =
                    json!([{"id":"a", "title":"Revised API task"}, {"id":"review", "title":"Review revised plan"}]);
                let updated =
                    request(app, json!({"cmd":"plan_set", "plan":id, "goal":"Deliver the API", "tasks":tasks}));
                assert!(updated.ok);
                assert_eq!(app.runtime.plans[0].goal, "Deliver the API");
                assert!(!app.runtime.plans[0].approved);
                assert_eq!(app.runtime.plans[0].integration, legacy.integration);
                assert_eq!(updated.result["integration"], "imacs/already-integrated");
                for (task, old) in app.runtime.plans[0].tasks.iter().zip(&progress) {
                    assert_eq!(task.status, old.status);
                    assert_eq!(task.session, old.session);
                    assert_eq!(task.branch, old.branch);
                    assert_eq!(task.repo, old.repo);
                    assert_eq!(task.notes, old.notes);
                    assert_eq!(task.report, old.report);
                    assert_eq!(task.integrated, old.integrated);
                }
                let original = app.runtime.plans[0].clone();
                for goal in ["Deliver the API", "Another goal"] {
                    let created = request(app, json!({"cmd":"plan_set", "goal":goal, "tasks":tasks}));
                    assert!(created.ok);
                    let fresh = &app.runtime.plans[app.runtime.selected_plan];
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
                    assert_eq!(app.runtime.plans[0], original);
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
                let dir = crate::test_support::tempdir().unwrap();
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
                app.runtime.config.worktree_path = format!("{}/{{slug}}", dir.path().display());
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
                    indices.push(app.runtime.selected_plan);
                }
                let (one, two) = (indices[0], indices[1]);
                assert!(app.runtime.dispatch(one, Some("a")).unwrap_err().contains("not approved"));
                app.runtime.plans[one].approved = true;
                assert_eq!(app.runtime.dispatch(one, Some("a")).unwrap(), ["a"]);
                assert!(app.runtime.plans[one].tasks[0].dispatched_at.is_some());
                assert_eq!(app.runtime.selected_plan, two);
                assert!(app.runtime.plan_view);
                assert!(app.runtime.changed_plans.contains(&app.runtime.plans[one].id));
                app.runtime.selected_plan = two;
                app.runtime.show_plan_fleet();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.creating.is_empty() && Instant::now() < deadline {
                    app.runtime.runtime_tick(ctx);
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(app.runtime.creating.is_empty());
                assert_eq!(app.runtime.selected_plan, two);
                assert_eq!(app.runtime.plans[one].tasks[0].status, TaskStatus::Blocked);
                assert!(app.runtime.plans[one].tasks[0].notes.last().unwrap().contains("could not start"));
                assert_eq!(app.runtime.plans[two].tasks[0].status, TaskStatus::Planned);
                assert!(app.runtime.plans[two].tasks[0].notes.is_empty());
            },
        );
    }
}
