use super::*;
use atomics_core::hook::HookThread;

impl State {
    // ── background events ─────────────────────────────────────────────────

    pub(crate) fn runtime_tick(&mut self) -> bool {
        if self.restarting || self.shutting_down {
            return false;
        }
        let before = self.runtime_view_revision();
        let mut changed = self.drain_actions();
        let roots = self
            .sessions
            .iter()
            .filter(|s| s.status != AgentStatus::Exited)
            .map(|s| (s.id.clone(), s.terminal.as_ref().map(|t| t.process_id())))
            .collect();
        changed |= self.previews.tick(roots, self.wake.clone());
        let machine_roots = self
            .sessions
            .iter()
            .filter(|s| s.status != AgentStatus::Exited && s.sandbox.is_none())
            .map(|s| (s.id.clone(), s.terminal.as_ref().map(|t| t.process_id())))
            .collect();
        let mut machine_paths = self.open_projects.clone();
        machine_paths.push(paths::atomics_home());
        changed |= self.machine.tick(machine_paths, machine_roots, self.wake.clone());
        changed |= self.poll_sandbox_check();
        changed |= self.poll_programs_check();
        changed |= self.poll_setup_look_again();
        changed |= self.poll_connections();
        changed |= self.github_tick();
        for record in std::mem::take(&mut self.resume) {
            self.resume_record(record);
            changed = true;
        }
        changed |= self.poll_build_caches();
        if std::mem::take(&mut self.chief_autostart) && !self.sessions.iter().any(|s| s.chief) {
            match self.start_chief(None) {
                Ok(_) => self.chief_open = true,
                Err(error) => self.toast(format!("Could not start the chief: {error}")),
            }
            changed = true;
        }
        // Links from macOS share the slot the command line fills; a newer one replaces an older.
        #[cfg(target_os = "macos")]
        if let Some(url) = crate::open_link::macos::take().pop() {
            self.hold_link(crate::open_link::parse(&url));
        }
        changed |= self.release_pending_link();
        self.flush_chief_draft();
        self.refresh_integrations();
        self.serve_dispatches();
        changed |= self.poll_cleanup();
        changed |= self.poll_plan_cleanup();
        self.recheck_deferred_cleanup();
        changed |= self.usage.drain();
        changed |= self.poll_resources();
        changed |= self.poll_update();
        for session in &mut self.sessions {
            if let Some(ready) = &session.worktree_ready
                && let Ok(worktree) = ready.try_recv()
            {
                changed = true;
                session.worktree = worktree;
                session.worktree_ready = None;
                session.follow_folder();
            }
        }
        for (agent, dir) in std::mem::take(&mut self.pending_open) {
            changed = true;
            self.interactive_start = true;
            self.spawn(&agent, dir, None);
            self.interactive_start = false;
        }
        let pending = std::mem::take(&mut self.pending_codex);
        for (launch, ready) in pending {
            if self.find(&launch.id).is_none() {
                if let Some(granted) = &launch.relay {
                    self.relay.revoke(granted);
                }
                continue;
            }
            if let Some(supported) = ready.get().copied() {
                changed = true;
                let id = launch.id.clone();
                let granted = launch.relay.clone();
                if let Err(error) = self.launch_codex(launch, supported) {
                    if let Some(granted) = &granted {
                        self.relay.revoke(granted);
                    }
                    if let Some(index) = self.find(&id) {
                        self.sessions[index].status = AgentStatus::Exited;
                        self.sessions[index].last_message = Some(error.clone());
                    }
                    self.toast(error);
                }
            } else {
                self.pending_codex.push((launch, ready));
            }
        }
        for (launch, ready) in std::mem::take(&mut self.pending_sandbox) {
            let result = match ready.try_recv() {
                Ok(result) => result,
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    self.pending_sandbox.push((launch, ready));
                    continue;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Err(super::NotPrepared::Other("Sandbox preparation stopped".into()))
                }
            };
            changed = true;
            if !self.sessions.iter().any(|s| s.id == launch.id) {
                // Closed while its sandbox was made: leave the sandbox stopped for reuse or cleanup,
                // or remove a scratch session's, which nothing resumes. Its placeholder goes either way.
                if let Ok((_, Some(granted), _, _)) = &result {
                    self.relay.revoke(granted);
                }
                if let AgentRuntime::Sandbox(sandboxed) = &launch.runtime {
                    match paths::scratch_of(&launch.cwd) {
                        Some(_) => self.remove_sandbox(sandboxed.name.clone()),
                        None => self.stop_sandbox(sandboxed.name.clone()),
                    }
                }
                continue;
            }
            let id = launch.id.clone();
            let name = match &launch.runtime {
                AgentRuntime::Sandbox(sandbox) => Some(sandbox.name.clone()),
                AgentRuntime::Bare => None,
            };
            // A person pressed Start, for this agent alone, and it has no plan task to block: they get to say.
            let result = match result {
                Err(super::NotPrepared::Sandbox(why))
                    if launch.interactive
                        && launch.resume.is_none()
                        && !self
                            .plans
                            .iter()
                            .any(|plan| plan.tasks.iter().any(|t| t.session.as_deref() == Some(&id))) =>
                {
                    self.ask_about_start(launch, why);
                    continue;
                }
                result => result.map_err(String::from),
            };
            let launched = result.and_then(|(guard, granted, needed, cache_env)| {
                let mut launch = launch;
                if let AgentRuntime::Sandbox(sandbox) = &mut launch.runtime {
                    sandbox.cache_env = cache_env;
                }
                let (guard, alerts) = guard.unzip();
                if let Some(alerts) = alerts.filter(|alerts| !alerts.is_empty()) {
                    self.ref_alert_at(
                        &id,
                        alerts.join("\n"),
                        guard.as_ref().and_then(|guard| guard.alert_generation()),
                    );
                }
                let relay = granted.clone();
                let launched = self.launch_prepared(AgentLaunch { ref_guard: guard, relay, ..launch });
                if launched.is_err()
                    && let Some(granted) = granted
                {
                    self.relay.revoke(&granted);
                }
                if launched.is_ok()
                    && let Some(index) = self.find(&id)
                {
                    if !needed.is_empty() {
                        // Its banner shows what sbx says about each server.
                        self.refresh_connections();
                    }
                    self.sessions[index].mcp_needed =
                        needed.into_iter().map(super::connections::Needed::missing).collect();
                }
                launched
            });
            if let Err(error) = launched {
                if let Some(name) = name {
                    self.stop_sandbox(name);
                }
                self.launch_failed(&id, error);
            }
        }
        while let Ok(message) = self.hook_rx.try_recv() {
            changed = true;
            self.on_hook(message);
        }
        while let Ok((branch, result, launch, common)) = self.worktree_rx.try_recv() {
            changed = true;
            if let Ok(prepared) = &result {
                let identity = self.project_identity(&prepared.path);
                if prepared.identity.as_ref() != Some(&identity) && self.tickets.workspace_identity_error.is_none() {
                    let tx = self.worktree_tx.clone();
                    self.identity_updates.run(move || {
                        let result = result.and_then(|mut prepared| {
                            atomics_core::identity::sync_worktree_identity(&prepared.path, &identity)
                                .map_err(|e| format!("{e:#}"))?;
                            prepared.identity = Some(identity);
                            Ok(prepared)
                        });
                        let _ = tx.send((branch, result, launch, common));
                    });
                    continue;
                }
                self.identity_worktrees.insert(prepared.path.clone());
            }
            self.worktrees_at = None;
            if let Some(i) = self.creating.iter().position(|(repo, b, _)| *repo == common && *b == branch) {
                self.creating.remove(i);
            }
            if let Some((plan_id, task_id)) = &launch.task
                && let Some(index) = self.plans.iter().position(|p| &p.id == plan_id && !p.is_open())
            {
                if let Ok(prepared) = &result
                    && let Some(task) = self.plans[index].task_mut(task_id)
                {
                    task.worktree = Some(prepared.path.clone());
                    self.save_plan(index);
                }
                self.cleanup_closed_plan(index);
                continue;
            }
            // A control start belongs to its caller; the desktop selects only replies to its own starts.
            let background = launch.reply.is_some()
                || launch
                    .task
                    .as_ref()
                    .is_some_and(|(plan, _)| !self.plan_view || self.plans[self.selected_plan].id != *plan);
            let selection = (self.active.clone(), self.diff_root.clone(), self.focus_terminal.clone());
            let start = result.as_ref().ok().and_then(|prepared| prepared.start.clone());
            let warning = result.as_ref().ok().and_then(|prepared| prepared.warning.clone());
            if let Some(warning) = &warning {
                self.toast(format!("{branch}: {warning}"));
            }
            let outcome = result.and_then(|PreparedWorktree { path, warmed, .. }| {
                let role = Role::Agent(launch.isolation);
                self.interactive_start = launch.interactive;
                let outcome = self.start_as(&launch.agent, path, None, launch.args, launch.prompt, role);
                self.interactive_start = false;
                if warmed && outcome.is_ok() {
                    self.toast("warm cache from main");
                }
                outcome
            });
            if background {
                (self.active, self.diff_root, self.focus_terminal) = selection;
            }
            if let Some((plan_id, task_id)) = &launch.task
                && let Some(index) = self.plans.iter().position(|p| &p.id == plan_id)
                && let Some(task) = self.plans[index].task_mut(task_id)
            {
                let old = task.status;
                match &outcome {
                    Ok(id) => {
                        task.session = Some(id.clone());
                        task.worktree = self.sessions.iter().find(|s| &s.id == id).map(Session::root);
                        if start.is_some() {
                            task.start_commit = start;
                        }
                        if let Some(warning) = &warning {
                            task.notes.push(warning.clone());
                        }
                    }
                    Err(err) => {
                        task.set_status(TaskStatus::Blocked);
                        task.notes.push(format!("could not start: {err}"));
                    }
                }
                self.save_plan(index);
                if let Err(err) = &outcome {
                    let note = format!("could not start: {err}");
                    let author = atomics_core::tickets::Author::Atomics;
                    self.ticket_status_changed(index, task_id, old, TaskStatus::Blocked, Some(&note), author);
                }
            }
            let response = match outcome {
                Ok(id) => {
                    let mut started = self.describe(self.find(&id).expect("just started"));
                    if let Some(warning) = warning {
                        started["warning"] = json!(warning);
                    }
                    ControlResponse::ok(started)
                }
                Err(err) => {
                    self.toast(err.clone());
                    ControlResponse::error(err)
                }
            };
            if let Some(reply) = launch.reply {
                let _ = reply.send(ControlOutcome::Done(response));
            }
        }
        while let Ok(integrated) = self.integrate_rx.try_recv() {
            changed = true;
            self.finish_integration(integrated);
        }
        changed |= self.ticket_results();
        self.remote_tick();
        changed |= self.poll_signin();
        self.expire_approvals(Instant::now());
        while let Ok(message) = self.control_rx.try_recv() {
            changed = true;
            self.control(message.request, message.reply);
            if self.restarting {
                return true;
            }
        }
        while let Ok(request) = self.op_rx.try_recv() {
            changed = true;
            self.serve_op(request);
        }
        changed |= self.poll_ops();
        for session in &mut self.sessions {
            let Some(terminal) = &session.terminal else { continue };
            if terminal.shared.changed.swap(false, Ordering::AcqRel) {
                session.activity.last_event = atomics_core::store::now();
                let delay = terminal.repaint_delay();
                self.output_delay = Some(self.output_delay.map_or(delay, |pending| pending.min(delay)));
            }
        }
        for terminal in self.editors.values() {
            if terminal.shared.changed.swap(false, Ordering::AcqRel) {
                let delay = terminal.repaint_delay();
                self.output_delay = Some(self.output_delay.map_or(delay, |pending| pending.min(delay)));
            }
        }
        let exiting =
            |s: &Session| s.status != AgentStatus::Exited && s.terminal.as_ref().is_some_and(Terminal::exited);
        // Gathered while agents that just exited still count as live, so every guard accepts their last commits.
        let exempt = self.sessions.iter().any(exiting).then(|| self.exempt_branches());
        let mut idle = Vec::new();
        for session in &mut self.sessions {
            if session.terminal.as_ref().is_some_and(Terminal::exited) {
                // A sandbox stops once its agent exits; the next `sbx run` starts it again.
                let credential_error = session
                    .sandbox
                    .as_ref()
                    .and(session.terminal.as_ref())
                    .and_then(|t| super::sessions::credential_failure(&t.screen_text(60)));
                if session.status != AgentStatus::Exited
                    && !(session.status == AgentStatus::NeedsInput && credential_error.is_some())
                {
                    idle.extend(session.sandbox.clone());
                }
                // The agent is gone either way, and its placeholder with it.
                if let Some(granted) = session.relay.take() {
                    self.relay.revoke(&granted);
                }
                if credential_error.is_some() {
                    session.status = AgentStatus::NeedsInput;
                } else {
                    session.status = AgentStatus::Exited;
                }
            }
            if let Some(title) = session.terminal.as_ref().and_then(|t| t.shared.title.lock().unwrap().take())
                && session.kind == AgentKind::Shell
                && session.naming == Naming::Folder
                && !title.trim().is_empty()
            {
                session.title = title;
            }
            // Sessions without hooks, sandboxed ones among them, are named after what was typed.
            if session.naming == Naming::FirstPrompt
                && let Some(line) = session.terminal.as_ref().and_then(|t| t.shared.submitted.lock().unwrap().take())
            {
                session.first_prompt(&line);
            }
        }
        if let Some(exempt) = exempt {
            self.check_all_refs(&exempt);
        }
        for name in idle {
            self.stop_sandbox(name);
        }
        let names = self.sessions.iter().filter_map(|s| Some((s.sandbox.clone()?, s.root()))).collect();
        changed |= self.sandbox_usage.poll(names, &self.window);
        // Only sandboxes with an attached agent are running; `sbx exec` would start a stopped one.
        let live = self
            .sessions
            .iter()
            .filter(|s| s.status != AgentStatus::Starting && s.terminal.as_ref().is_some_and(|t| !t.exited()))
            .filter_map(|s| s.sandbox.clone())
            .collect();
        self.sandbox_pull.poll(&self.sbx, live, &self.window);
        self.track_busy();
        self.verify_deliveries();
        for index in 0..self.sessions.len() {
            self.release_held(index);
        }
        self.record_events();
        self.sync_plan();
        self.heartbeat();
        self.wake_chief();
        while let Ok(result) = self.worker.rx.try_recv() {
            changed = true;
            match result {
                WorkerResult::Diff { root, result } => {
                    let state = self.diffs.entry(root.clone()).or_default();
                    state.loading = false;
                    state.revision += 1;
                    match result {
                        Ok(prepared) => {
                            for (index, plan) in self.plans.iter_mut().enumerate() {
                                let mut changed = false;
                                for task in &mut plan.tasks {
                                    if task.finished_at.is_some()
                                        && task.worktree.as_ref() == Some(&root)
                                        && (task.additions != prepared.additions
                                            || task.deletions != prepared.deletions)
                                    {
                                        task.additions = prepared.additions;
                                        task.deletions = prepared.deletions;
                                        changed = true;
                                    }
                                }
                                if changed && let Err(err) = plan.save_in(&paths::atomics_home()) {
                                    eprintln!("ATOMICS: saving plan {index} failed: {err:#}");
                                }
                            }
                            state.prepared = Some(prepared);
                            state.error = None;
                        }
                        Err(err) => state.error = Some(err),
                    }
                    // A file open in the viewer changes with the worktree too.
                    if let Some(path) = state.viewing.clone() {
                        self.worker.request_file(root, path, self.highlighter.clone());
                    }
                }
                WorkerResult::File { root, path, result } => {
                    let state = self.diffs.entry(root).or_default();
                    if state.viewing.as_deref() == Some(path.as_str()) {
                        match result {
                            Ok(file) => state.file = Some(file),
                            Err(err) => state.file_error = Some(err),
                        }
                    }
                }
            }
        }
        while let Ok(root) = self.watch_rx.try_recv() {
            changed = true;
            self.diffs.entry(root).or_default().mark_dirty();
        }

        if self.config_checked.elapsed() > Duration::from_secs(1) {
            self.reload_config();
            changed |= self.reload_repo_workflows();
        }

        // A refresh asked for while a listing runs starts once it is in.
        if self.worktree_listing.is_none() && self.worktrees_at.is_none_or(|at| at.elapsed() > WORKTREE_REFRESH) {
            self.worktrees_at = Some(Instant::now());
            let (tx, rx) = self.wake.channel();
            let repos = self.repos.clone();
            std::thread::spawn(move || {
                let _ = tx.send(
                    repos
                        .into_iter()
                        .map(|repo| {
                            let value = |key| {
                                git::git(&repo, &["config", "--get", key]).ok().map(|value| value.trim().to_string())
                            };
                            let identity = atomics_core::identity::Identity {
                                name: value("user.name"),
                                email: value("user.email"),
                                forbidden_words: None,
                                github: None,
                            };
                            (repo.clone(), git::list_worktrees(&repo).unwrap_or_default(), identity)
                        })
                        .collect(),
                );
            });
            self.worktree_listing = Some(rx);
        }
        let listed = self.worktree_listing.as_ref().map(|listing| listing.try_recv());
        if let Some(Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected)) = &listed {
            self.worktree_listing = None;
        }
        if let Some(Ok(listed)) = listed {
            changed = true;
            self.worktrees.clear();
            self.worktree_project.clear();
            self.git_identities.clear();
            for (repo, worktrees, identity) in listed {
                self.git_identities.insert(repo.clone(), identity);
                // One listed from files that do not lead back to it is never opened.
                for worktree in worktrees.into_iter().filter(|worktree| worktree.trusted) {
                    self.worktree_project.insert(worktree.path.clone(), repo.clone());
                    self.worktrees.push(worktree);
                }
                // The explorer lists the main checkout's files.
                let state = self.diffs.entry(repo).or_default();
                if !state.loading {
                    state.mark_dirty();
                }
            }
        }
        let names: Vec<String> =
            self.repos.iter().filter_map(|r| r.file_name()).map(|n| n.to_string_lossy().into_owned()).collect();
        let title = match &self.workspace {
            Some(ws) => format!("ATOMICS · {ws} · {}", names.join(", ")),
            None if names.is_empty() => "ATOMICS".to_string(),
            None => format!("ATOMICS · {}", names.join(", ")),
        };
        if title != self.title {
            self.client_events.push(ClientEvent::Viewport(egui::ViewportCommand::Title(title.clone())));
            self.title = title;
        }

        let target = self.diff_target();
        self.watch(target.as_deref());
        if let Some(root) = &target {
            let state = self.diffs.entry(root.clone()).or_default();
            // A diff seen off screen may lack untracked content; on screen it gets all of it.
            let missing = state.prepared.as_ref().is_none_or(|p| p.diff.partial);
            if missing && !state.loading && state.error.is_none() && state.dirty_since.is_none() {
                state.dirty_since = Some(Instant::now() - Duration::from_secs(1));
            }
        }
        // File changes and hooks keep the diffs of agents off screen current; this
        // catches what they miss while an agent works.
        for session in &self.sessions {
            let state = self.diffs.entry(session.root()).or_default();
            let stale = state.last_request.is_none_or(|at| at.elapsed() > FALLBACK_REFRESH);
            if (session.status == AgentStatus::Working || state.prepared.is_none()) && stale && !state.loading {
                state.mark_dirty();
            }
        }
        let now = Instant::now();
        let roots: Vec<_> = self.diffs.keys().cloned().collect();
        for root in &roots {
            self.resolve_project_root(root);
        }
        let bases: HashMap<_, _> = roots.into_iter().map(|root| (root.clone(), self.diff_base_ref(&root))).collect();
        for (root, state) in &mut self.diffs {
            // The diff on screen follows every change; the others at most every few seconds,
            // with only some of their untracked content.
            let on_screen = target.as_ref() == Some(root);
            let (interval, budget) =
                if on_screen { (Duration::ZERO, None) } else { (BACKGROUND_REFRESH, Some(OFF_SCREEN_UNTRACKED)) };
            match state.due_at(interval) {
                Some(at) if at <= now => {
                    state.requested();
                    let (base_ref, highlighter) = (bases[root].clone(), self.highlighter.clone());
                    self.worker.request(root.clone(), base_ref, budget, highlighter);
                }
                _ => {}
            }
        }
        if let Some((_, at)) = &self.toast
            && at.elapsed() > Duration::from_secs(5)
        {
            self.toast = None;
        }
        changed || before != self.runtime_view_revision()
    }

    /// Reads the config files again when they changed, and the workspace's sections.
    pub(super) fn reload_config(&mut self) {
        self.management_config.take();
        self.config_checked = Instant::now();
        let stamp = config_fingerprints(&self.config_path);
        self.apply_ticket_config();
        if stamp == self.config_stamp {
            return;
        }
        let themes_changed = stamp[2] != self.config_stamp[2];
        self.config_stamp = stamp;
        match Config::load(&self.config_path) {
            Ok(mut config) => {
                super::with_remote_port(&mut config, self.remote_port);
                let (theme, theme_errors) =
                    Theme::load(&config.theme, &atomics_core::theme::themes_dir(&self.config_path));
                let syntax_changed = theme.syntax != self.theme.syntax;
                let theme_reloaded = config.theme != self.config.theme || themes_changed;
                if config.terminal.scrollback != self.config.terminal.scrollback {
                    let terminals =
                        self.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.editors.values());
                    for terminal in terminals {
                        terminal.set_scrollback(config.terminal.scrollback);
                    }
                }
                let github_changed = config.github != self.config.github;
                if config.chief.dock != self.config.chief.dock {
                    self.store.chief_dock = None;
                    self.save_chief_preferences();
                }
                self.config = config;
                self.config_error = None;
                self.config_problems = github::config_problems(&self.config);
                if github_changed {
                    self.github_poll_now();
                }
                self.theme_error = theme_error(&theme_errors);
                if syntax_changed {
                    self.highlighter = Arc::new(Highlighter::new(&theme.syntax));
                }
                self.theme = Arc::new(theme);
                self.refresh_themes();
                let colors = self.theme.terminal_colors();
                let terminals = self.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.editors.values());
                for terminal in terminals {
                    terminal.set_colors(colors.clone());
                }
                self.client_events.push(ClientEvent::ConfigChanged);
                self.apply_config();
                for state in self.diffs.values_mut() {
                    state.mark_dirty();
                }
                self.toast(if theme_reloaded { "Theme reloaded" } else { "Config reloaded" });
            }
            Err(err) => self.config_error = Some(format!("{err:#}")),
        }
    }

    /// Filesystem/config and terminal fallback checks have bounded polling
    /// deadlines; messages and worker results wake the host immediately.
    pub(crate) fn take_output_delay(&mut self) -> Option<Duration> {
        self.output_delay.take()
    }

    /// Runtime sampling continues with no desktop or remote client attached.
    fn poll_resources(&mut self) -> bool {
        use atomics_core::projection::{ResourceInfo, SlotInfo};
        let before = self.resources.clone();
        if self.resources_checked.elapsed() >= Duration::from_secs(2) {
            self.resources.load = crate::governor::load_average();
            self.resources.sampled_at = crate::usage::now();
            self.resources_checked = Instant::now();
        }
        let (running, waiting) = self.governor.snapshot();
        let pool = |class| SlotInfo {
            slots: self.governor.slots(class),
            running: running.iter().filter(|ticket| ticket.class == class).count(),
            waiting: waiting.iter().filter(|ticket| ticket.class == class).count(),
        };
        self.resources = ResourceInfo {
            claude: self.usage.claude.clone(),
            codex: self.usage.codex.clone(),
            cores: self.governor.cores,
            compiler_jobs: self.jobserver.as_ref().map(|pool| pool.tokens()),
            test: pool(SlotClass::Test),
            qa: pool(SlotClass::Qa),
            ..self.resources.clone()
        };
        resources_need_repaint(before, &self.resources)
    }

    pub(crate) fn runtime_deadline(&self) -> Instant {
        let now = Instant::now();
        if self.restarting {
            return now + Duration::from_secs(1);
        }
        let mut next = self.config_checked + Duration::from_secs(1);
        next = next.min(self.resources_checked + Duration::from_secs(2));
        next = next.min(self.heartbeat.deadline(Duration::from_secs(self.config.chief.heartbeat_secs.max(1))));
        for waiter in &self.waiters {
            next = next.min(waiter.deadline);
        }
        if let Some(interval) = self.approval_poll_interval() {
            next = next.min(self.approvals_checked + interval);
        }
        if self.sessions.iter().any(|s| s.pending.is_some()) {
            next = next.min(now + PULSE_STEP);
        }
        if self.sessions.iter().any(|s| s.terminal.is_some()) {
            next = next.min(self.busy_checked + Duration::from_millis(400));
        }
        let target = self.diff_target();
        for (root, state) in &self.diffs {
            let interval = if target.as_ref() == Some(root) { Duration::ZERO } else { BACKGROUND_REFRESH };
            if let Some(at) = state.due_at(interval) {
                next = next.min(at);
            }
        }
        if let Some((_, at)) = &self.toast {
            next = next.min(*at + Duration::from_secs(5));
        }
        // An already-due timer is serviced on the next turn, without spinning
        // when the sampled clock is exactly equal to its deadline.
        next.max(now + Duration::from_millis(1))
    }

    fn runtime_view_revision(&self) -> impl PartialEq + use<> {
        (
            self.event_seq,
            self.config_stamp,
            self.worktrees_at,
            self.toast.as_ref().map(|(_, at)| *at),
            self.integration_revision,
            self.integration_checks.len(),
            self.agent_cleanup.revision,
            self.sessions
                .iter()
                .map(|s| {
                    (
                        s.id.clone(),
                        s.title.clone(),
                        s.status,
                        s.first_run_hint,
                        s.approvals.len(),
                        s.approval_hold,
                        s.held.len(),
                        s.pending.is_some(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }

    pub(super) fn on_hook(&mut self, message: HookMessage) {
        let event = message.event;
        // An agent's background threads inherit its pane, but what they do is not the session's.
        if event.thread == HookThread::Background || self.dismissed.contains(&event.session_id) {
            leave_to_agent(message.approval_reply);
            return;
        }
        let main = event.thread == HookThread::Main;
        let found = message
            .pane
            .as_ref()
            .and_then(|pane| self.sessions.iter().position(|s| &s.id == pane))
            .or_else(|| self.sessions.iter().position(|s| s.claude_id.as_deref() == Some(event.session_id.as_str())))
            .or_else(|| {
                message.ancestors.iter().find_map(|pid| {
                    self.sessions
                        .iter()
                        .position(|s| s.terminal.as_ref().is_some_and(|terminal| terminal.process_id() == *pid))
                })
            });
        let index = match found {
            Some(index) => index,
            None if event.name == "SessionEnd" => return,
            None => {
                let cwd = event.cwd.clone().unwrap_or_else(paths::home);
                let worktree = git::worktree_of(&cwd);
                let title = title_for(worktree.as_ref().and_then(|w| w.branch.as_deref()), &cwd);
                self.sessions.push(Session {
                    id: format!("ext:{}", event.session_id),
                    agent: "claude".into(),
                    kind: AgentKind::Claude,
                    terminal: None,
                    claude_id: Some(event.session_id.clone()),
                    cwd,
                    worktree,
                    worktree_ready: None,
                    title,
                    status: AgentStatus::Idle,
                    last_prompt: None,
                    last_message: None,
                    hooks_active: false,
                    first_run_hint: None,
                    activity: heartbeat::Activity::default(),
                    approvals: Vec::new(),
                    allowed: Vec::new(),
                    held: Vec::new(),
                    approval_hold: false,
                    menu_absent_since: None,
                    answered_menu: None,
                    chief: false,
                    pending: None,
                    sandbox: None,
                    cache_env: Vec::new(),
                    relay: None,
                    screen_busy: false,
                    busy_absent_since: None,
                    ref_guard: None,
                    scratch: None,
                    naming: Naming::Folder,
                    mcp_needed: Vec::new(),
                    warning: None,
                });
                if self.active.is_none() {
                    self.active = Some(format!("ext:{}", event.session_id));
                }
                self.sessions.len() - 1
            }
        };

        // A command that drops shared stash entries says whose, for whoever decides.
        let (stash_warning, stash) = (event.name == "PermissionRequest" && self.sessions[index].sandbox.is_none())
            .then(|| self.stash_warning(index, &ApprovalRequest::from_event(&event)))
            .flatten()
            .unzip();
        let session = &mut self.sessions[index];
        // A sandboxed session's requests are not ATOMICS's to answer, nor are those of a thread it
        // no longer follows.
        if session.sandbox.is_some()
            || (event.name != "SessionStart" && session.claude_id.as_ref().is_some_and(|id| id != &event.session_id))
        {
            leave_to_agent(message.approval_reply);
            return;
        }
        if main && event.name == "SessionStart" && session.claude_id.as_deref() != Some(&event.session_id) {
            self.traces.remove(&session.id);
        }
        session.claude_id = Some(event.session_id.clone());
        session.hooks_active = true;
        session.first_run_hint = None;
        session.activity.last_event = atomics_core::store::now();
        if event.name == "PermissionRequest" {
            let request = ApprovalRequest { warning: stash_warning, ..ApprovalRequest::from_event(&event) };
            if (self.config.approvals.allows(&request, &session.root())
                || session.allowed.iter().any(|allowed| allowed.same_command(&request)))
                && message
                    .approval_reply
                    .as_ref()
                    .is_some_and(|reply| reply.answer(HookReply { stdout: Some(Decision::Allow.output(None)) }))
            {
                return;
            }
            session.approval_hold = true;
            session.menu_absent_since = None;
            // The hook is the request itself: a menu read off the screen only stood in for it.
            session.approvals.retain(|pending| pending.native.is_none());
            session.approvals.push(PendingApproval {
                id: uuid::Uuid::new_v4().to_string(),
                request,
                reply: message.approval_reply,
                native: None,
                stash: stash.unwrap_or_default(),
            });
            self.last_status.remove(&session.id);
        } else if event.name == "PreToolUse" {
            let running = ApprovalRequest::from_event(&event);
            session.approvals.retain(|pending| {
                if pending.request.same_command(&running) {
                    if let Some(reply) = &pending.reply {
                        reply.answer(HookReply::default());
                    }
                    false
                } else {
                    true
                }
            });
        } else if event.name == "PostToolUse" {
            let completed = ApprovalRequest::from_event(&event);
            session.approvals.retain(|pending| !pending.request.same_command(&completed));
        } else if main && matches!(event.name.as_str(), "Stop" | "SessionEnd" | "Interrupt" | "UserPromptSubmit") {
            session.approvals.clear();
        }
        // An agent's session follows its shell into another folder. The chief's stays where it was
        // started: it looks into agents' worktrees to review them, which never makes their work its own.
        if main
            && !session.chief
            && let Some(cwd) = &event.cwd
            && *cwd != session.cwd
        {
            session.cwd = cwd.clone();
            session.worktree_ready = None;
            session.worktree = git::worktree_of(cwd);
            session.follow_folder();
        }
        // A spawned thread moves the status only by asking, and once what it asked is answered.
        if let Some(status) = event.status()
            && (main || session.status == AgentStatus::NeedsInput || !session.approvals.is_empty())
        {
            session.status = if session.approvals.is_empty() { status } else { AgentStatus::NeedsInput };
        }
        match event.name.as_str() {
            "UserPromptSubmit" if main => {
                self.pending_questions.remove(&session.id);
                if let Some(pending) = &mut session.pending
                    && pending.codex
                    && event.prompt.as_deref() == Some(&pending.text)
                {
                    if let Some(reply) = pending.reply.take() {
                        let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(json!({"sent":true}))));
                    }
                    session.pending = None;
                }
                session.last_prompt = event.prompt.clone();
                if let Some(prompt) = &event.prompt {
                    session.first_prompt(prompt);
                }
            }
            "Stop" if main => session.last_message = event.last_assistant_message.clone(),
            _ => {}
        }
        session.activity.observe(session.status);
        let root = session.root();
        let id = session.id.clone();
        let title = self.session_title(&self.sessions[index]);
        self.capture_chief_trace(index, &event);
        if main && self.sessions[index].chief && matches!(event.name.as_str(), "UserPromptSubmit" | "Stop") {
            self.tickets.remote.chief_turn = Some(Instant::now());
        }
        if matches!(event.name.as_str(), "PostToolUse" | "Stop" | "UserPromptSubmit" | "CwdChanged")
            && let Some(state) = self.diffs.get_mut(&root)
        {
            state.mark_dirty();
        }
        if message.delivered > 0 {
            self.toast(format!("Delivered {} queued message(s) to {title}", message.delivered));
        }
        if matches!(event.name.as_str(), "SessionStart" | "UserPromptSubmit" | "Stop") {
            self.remember(&id);
        }
        if matches!(event.name.as_str(), "PostToolUse" | "Stop" | "Interrupt" | "UserPromptSubmit") {
            let session = &mut self.sessions[index];
            if session.approvals.is_empty() && session.approval_hold {
                let menu = session
                    .terminal
                    .as_ref()
                    .is_some_and(|terminal| atomics_core::approvals::menu_visible(&terminal.screen_text(60)));
                session.approval_hold = menu;
                session.menu_absent_since = None;
            }
            self.release_held(index);
        }
        if event.name == "PermissionRequest" {
            self.last_status.insert(id.clone(), AgentStatus::NeedsInput);
            let at = atomics_core::store::now();
            let approval = self.sessions[index].approvals.last().map(|p| self.approval_json(index, p, at));
            self.event_seq += 1;
            self.events.push(StatusEvent {
                seq: self.event_seq,
                plan: self.plans.iter().find(|p| p.contains_session(&id)).map(|p| p.id.clone()),
                task: self
                    .plans
                    .iter()
                    .flat_map(|p| &p.tasks)
                    .find(|t| t.session.as_deref() == Some(&id))
                    .map(|t| t.id.clone()),
                session: id,
                title,
                status: AgentStatus::NeedsInput,
                at,
                report: None,
                approval,
                findings: None,
                config: None,
                ticket: None,
                change: None,
            });
            self.serve_waiters();
        }
    }

    /// Watches the worktree of every agent and the one on screen, so their diffs follow
    /// file changes without polling.
    fn watch(&mut self, target: Option<&Path>) {
        let mut wanted: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|s| s.worktree.is_some() || s.scratch.is_some())
            .map(Session::root)
            .chain(target.map(Path::to_path_buf))
            .collect();
        for (index, plan) in self.plans.iter().enumerate().filter(|(_, p)| p.is_open()) {
            wanted.extend(plan.project.clone());
            wanted.extend(
                plan.tasks
                    .iter()
                    .filter(|task| task.integrated.is_some())
                    .filter_map(|task| self.task_repo(index, task)),
            );
        }
        wanted.sort();
        wanted.dedup();
        if self.watcher.as_ref().map_or(&[][..], Watcher::roots) == wanted {
            return;
        }
        if self.watcher.is_none() {
            let (tx, integration_dirty, wake) =
                (self.watch_tx.clone(), self.integration_dirty.clone(), self.wake.clone());
            let cleanup_files = self.agent_cleanup.files.clone();
            let on_change = move |change| match change {
                Change::Files(root) => {
                    *cleanup_files.lock().unwrap().entry(root.clone()).or_default() += 1;
                    drop(tx.send(root));
                }
                Change::Refs => {
                    integration_dirty.store(true, Ordering::Relaxed);
                    wake.notify();
                }
            };
            match Watcher::new(on_change) {
                Ok(watcher) => self.watcher = Some(watcher),
                Err(err) => eprintln!("ATOMICS: file watcher failed: {err}"),
            }
        }
        if let Some(watcher) = &mut self.watcher {
            watcher.set_roots(&wanted);
        }
    }
}

/// Keep the full sample for protocol clients, but only wake the desktop for visible changes.
fn resources_need_repaint(
    before: atomics_core::projection::ResourceInfo,
    after: &atomics_core::projection::ResourceInfo,
) -> bool {
    use atomics_core::projection::ResourceInfo;
    // Match resources::load's one-decimal text and its load-per-core color thresholds.
    let gauge = |resources: &ResourceInfo| {
        resources.load.map(|load| {
            let per_core = load / resources.cores.max(1) as f64;
            (format!("{load:.1}"), per_core >= 0.7, per_core >= 1.0)
        })
    };
    let load_changed = gauge(&before) != gauge(after);
    // Fresh timestamps still reach usage age/reset labels through host projections.
    let before = ResourceInfo { sampled_at: after.sampled_at, load: after.load, ..before };
    load_changed || before != *after
}

/// Gives a request nobody here can answer no decision at once: the agent then asks in its own
/// terminal, which it draws only once the hook has returned.
fn leave_to_agent(reply: Option<Arc<ipc::ApprovalReply>>) {
    if let Some(reply) = reply {
        reply.answer(HookReply::default());
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{answer_pending, repository, request, session, with_app};
    use super::*;
    use atomics_core::projection::{HostInfo, Projections, ResourceInfo};

    #[test]
    fn resource_samples_publish_fresh_data_without_repainting_an_unchanged_view() {
        let before = ResourceInfo { sampled_at: 100, load: Some(4.21), cores: 8, ..Default::default() };
        let after = ResourceInfo { sampled_at: 102, load: Some(4.24), ..before.clone() };
        assert!(!resources_need_repaint(before.clone(), &after));
        assert!(!resources_need_repaint(after.clone(), &before));

        let projections = Projections::default();
        assert!(projections.publish_host(HostInfo { resources: Some(before), ..Default::default() }));
        let revision = projections.host.load().revision;
        assert!(projections.publish_host(HostInfo { resources: Some(after.clone()), ..Default::default() }));
        let published = projections.host.load();
        assert_eq!(published.revision, revision + 1);
        assert_eq!(published.data.resources.as_ref(), Some(&after), "publish the timestamp and unrounded load");
    }

    #[test]
    fn resource_samples_repaint_for_load_text_or_color_changes_in_either_direction() {
        for (old, new, repaint) in [
            (None, None, false),
            (None, Some(0.0), true),
            (Some(4.21), Some(4.24), false),
            (Some(4.24), Some(4.26), true),
            // Same text (5.6 / 8.0), different warning / error color on eight cores.
            (Some(5.59), Some(5.6), true),
            (Some(7.99), Some(8.0), true),
        ] {
            let before = ResourceInfo { sampled_at: 100, load: old, cores: 8, ..Default::default() };
            let after = ResourceInfo { sampled_at: 102, load: new, ..before.clone() };
            assert_eq!(resources_need_repaint(before.clone(), &after), repaint, "{old:?} -> {new:?}");
            assert_eq!(resources_need_repaint(after, &before), repaint, "{new:?} -> {old:?}");
        }
    }

    #[test]
    fn resource_samples_repaint_for_slot_and_account_changes_in_either_direction() {
        let before = ResourceInfo { cores: 8, ..Default::default() };
        let changes: [fn(&mut ResourceInfo); 10] = [
            |r| r.test.slots += 1,
            |r| r.test.running += 1,
            |r| r.test.waiting += 1,
            |r| r.qa.slots += 1,
            |r| r.qa.running += 1,
            |r| r.qa.waiting += 1,
            |r| r.cores += 1,
            |r| r.compiler_jobs = Some(6),
            |r| r.claude.observed_at = 100,
            |r| r.codex.observed_at = 100,
        ];
        for change in changes {
            let mut after = before.clone();
            change(&mut after);
            assert!(resources_need_repaint(before.clone(), &after), "{before:?} -> {after:?}");
            assert!(resources_need_repaint(after.clone(), &before), "{after:?} -> {before:?}");
            assert!(!resources_need_repaint(after.clone(), &after));
        }
    }

    #[test]
    fn needs_input_without_a_pending_menu_uses_slow_screen_checks() {
        with_app("app::background::tests::needs_input_without_a_pending_menu_uses_slow_screen_checks", |app, _ctx| {
            let terminal = Terminal::spawn(
                app.runtime.wake.clone(),
                app.runtime.theme.terminal_colors(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf 'Working (esc to interrupt)'; exec sleep 30".into()],
                &app.runtime.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            while !terminal.screen_text(60).contains("esc to interrupt") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Generic,
                busy_text: Some("esc to interrupt".into()),
                isolation: None,
            });
            app.runtime.sessions =
                vec![session("waiting", AgentKind::Generic, AgentStatus::NeedsInput, &app.runtime.launch_dir)];
            app.runtime.sessions[0].terminal = Some(terminal);
            let now = Instant::now();
            app.runtime.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.runtime.needs_input_checked = now;
            assert_eq!(app.runtime.approval_poll_interval(), Some(NEEDS_INPUT_CHECK_INTERVAL));
            app.runtime.expire_approvals(now);
            assert!(
                app.runtime.sessions[0].menu_absent_since.is_none(),
                "a frame must not read an idle needs-input screen"
            );
            app.runtime.expire_approvals(now + NEEDS_INPUT_CHECK_INTERVAL);
            assert!(
                app.runtime.sessions[0].menu_absent_since.is_some(),
                "the slow check still recovers missed lifecycle events"
            );
            app.runtime.sessions[0].approval_hold = true;
            assert_eq!(app.runtime.approval_poll_interval(), Some(Duration::from_millis(400)));
            app.runtime.expire_approvals(now + NEEDS_INPUT_CHECK_INTERVAL + APPROVAL_MENU_GRACE);
            assert!(!app.runtime.sessions[0].approval_hold);
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
            assert_eq!(app.runtime.approval_poll_interval(), None);
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn pre_tool_use_retires_only_the_matching_approval() {
        with_app("app::background::tests::pre_tool_use_retires_only_the_matching_approval", |app, _| {
            app.runtime.sessions =
                vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
            let hook = |name: &str, command: &str, tool: &str, cwd: &Path, reply| HookMessage {
                pane: Some("worker".into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: reply,
                event: atomics_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":"worker", "cwd":cwd,
                    "tool_name":tool, "tool_input":{"command":command}
                }))
                .unwrap(),
            };
            let root = app.runtime.launch_dir.clone();
            let matching = Arc::new(ipc::ApprovalReply::default());
            let other = Arc::new(ipc::ApprovalReply::default());
            for (command, reply) in [("touch first", matching.clone()), ("touch second", other.clone())] {
                app.runtime.on_hook(hook("PermissionRequest", command, "Bash", &root, Some(reply)));
            }
            for (command, tool, cwd) in [
                ("touch third", "Bash", root.clone()),
                ("touch first", "Edit", root.clone()),
                ("touch first", "Bash", root.join("elsewhere")),
            ] {
                app.runtime.on_hook(hook("PreToolUse", command, tool, &cwd, None));
                assert_eq!(app.runtime.sessions[0].approvals.len(), 2);
                assert!(!matching.expired() && !other.expired());
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
            }
            app.runtime.on_hook(hook("PreToolUse", "touch first", "Bash", &root, None));
            assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
            assert_eq!(app.runtime.sessions[0].approvals[0].request.command.as_deref(), Some("touch second"));
            assert!(!matching.answer(HookReply { stdout: Some("must already be answered".into()) }));
            assert!(matching.wait(Duration::ZERO).stdout.is_none());
            assert!(!other.expired(), "the other hook must still be waiting for its own decision");
            assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
            app.runtime
                .approve(
                    0,
                    &app.runtime.sessions[0].approvals.first().map(|pending| pending.id.clone()).unwrap_or_default(),
                    Decision::Deny,
                    None,
                )
                .unwrap();
            let output = other.wait(Duration::ZERO).stdout.unwrap();
            assert!(output.contains("deny"));
        });
    }

    /// One thread's hook as Codex 0.166 sends it. Every thread of an agent inherits its pane; a
    /// thread Codex keeps no rollout of, its memory agent among them, has a null `transcript_path`.
    fn thread_hook(
        name: &str,
        thread: &str,
        cwd: &Path,
        transcript: Option<&str>,
        reply: Option<Arc<ipc::ApprovalReply>>,
    ) -> HookMessage {
        HookMessage {
            pane: Some("worker".into()),
            ancestors: vec![],
            delivered: 0,
            approval_reply: reply,
            event: atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":name, "session_id":thread, "cwd":cwd, "transcript_path":transcript,
                "source":"startup", "reason":"other", "prompt":"a thread's own prompt",
                "tool_name":"Bash", "tool_input":{"command":"touch approval-file"},
                "last_assistant_message":"a thread's own answer",
            }))
            .unwrap(),
        }
    }

    const MAIN: Option<&str> = Some("/sessions/rollout-main-thread.jsonl");

    /// A worker in its own repository, following its main thread, and a folder elsewhere.
    fn main_thread_worker(app: &mut Desktop<'_>, repo: &Path) {
        let mut worker = session("worker", AgentKind::Generic, AgentStatus::Working, repo);
        worker.claude_id = Some("main-thread".into());
        worker.worktree = git::worktree_of(repo);
        worker.follow_folder();
        app.runtime.sessions = vec![worker];
    }

    /// A repository on a branch of its own, so a session moved there reads differently.
    fn other_repository() -> (crate::test_support::TempDir, PathBuf) {
        let (dir, repo) = repository();
        git::git(&repo, &["checkout", "-qb", "elsewhere"]).unwrap();
        (dir, repo)
    }

    /// Everything a session takes from the thread it follows.
    fn followed(session: &Session) -> impl PartialEq + std::fmt::Debug + use<> {
        (
            session.claude_id.clone(),
            session.cwd.clone(),
            session.root(),
            session.branch().map(str::to_string),
            session.title.clone(),
            session.status,
            session.last_prompt.clone(),
            session.last_message.clone(),
        )
    }

    /// A background thread's whole life, from its own folder: Codex's memory agent sent its start,
    /// prompt and end in a real run; the tool events and Stop are what a longer run could add.
    fn background_thread_runs(app: &mut Desktop<'_>, elsewhere: &Path) {
        let before = followed(&app.runtime.sessions[0]);
        let approvals = app.runtime.sessions[0].approvals.len();
        for name in ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "SessionEnd"] {
            app.runtime.on_hook(thread_hook(name, "background-thread", elsewhere, None, None));
            assert_eq!(followed(&app.runtime.sessions[0]), before, "after its {name}");
            assert_eq!(app.runtime.sessions[0].approvals.len(), approvals, "after its {name}");
            assert_eq!(app.runtime.sessions.len(), 1, "after its {name}");
        }
    }

    #[test]
    fn a_background_thread_leaves_its_session_on_the_main_thread() {
        with_app("app::background::tests::a_background_thread_leaves_its_session_on_the_main_thread", |app, _| {
            let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
            main_thread_worker(app, &repo);
            background_thread_runs(app, &elsewhere);
            let worker = &app.runtime.sessions[0];
            assert_eq!(worker.claude_id.as_deref(), Some("main-thread"));
            assert_eq!(worker.cwd, repo);
            assert_eq!(worker.root(), std::fs::canonicalize(&repo).unwrap());
            assert_eq!(worker.status, AgentStatus::Working);
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn after_a_background_thread_the_main_thread_is_approved_and_ends_its_turn() {
        with_app(
            "app::background::tests::after_a_background_thread_the_main_thread_is_approved_and_ends_its_turn",
            |app, _| {
                let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
                main_thread_worker(app, &repo);
                background_thread_runs(app, &elsewhere);
                let reply = Arc::new(ipc::ApprovalReply::default());
                app.runtime.on_hook(thread_hook("PermissionRequest", "main-thread", &repo, MAIN, Some(reply.clone())));
                assert_eq!(app.runtime.sessions[0].approvals.len(), 1);
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                // A real run: the memory agent ended while the main thread's hook waited for its answer.
                background_thread_runs(app, &elsewhere);
                let answered = answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"}));
                assert!(answered.ok, "{answered:?}");
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                assert!(app.runtime.sessions[0].approvals.is_empty());
                app.runtime.on_hook(thread_hook("PostToolUse", "main-thread", &repo, MAIN, None));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                app.runtime.on_hook(thread_hook("Stop", "main-thread", &repo, MAIN, None));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Done);
                assert_eq!(app.runtime.sessions[0].last_message.as_deref(), Some("a thread's own answer"));
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn a_background_thread_neither_binds_a_new_session_nor_starts_one() {
        with_app("app::background::tests::a_background_thread_neither_binds_a_new_session_nor_starts_one", |app, _| {
            let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
            main_thread_worker(app, &repo);
            // An interactive Codex starts its main thread with the first prompt, so this can come first.
            app.runtime.sessions[0].claude_id = None;
            app.runtime.on_hook(thread_hook("SessionStart", "background-thread", &elsewhere, None, None));
            assert_eq!(app.runtime.sessions[0].claude_id, None);
            assert!(!app.runtime.sessions[0].hooks_active);
            assert_eq!(app.runtime.sessions[0].cwd, repo);
            let outside = HookMessage {
                pane: None,
                ..thread_hook("SessionStart", "outside-background-thread", &elsewhere, None, None)
            };
            app.runtime.on_hook(outside);
            assert_eq!(app.runtime.sessions.len(), 1, "no session of its own either");
            app.runtime.on_hook(thread_hook("SessionStart", "main-thread", &repo, MAIN, None));
            assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some("main-thread"));
            assert!(app.runtime.sessions[0].hooks_active);
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn a_spawned_thread_can_be_approved_but_the_session_stays_on_the_main_thread() {
        with_app(
            "app::background::tests::a_spawned_thread_can_be_approved_but_the_session_stays_on_the_main_thread",
            |app, _| {
                let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
                main_thread_worker(app, &repo);
                // Codex and Claude send a spawned thread's events under the main thread's session id.
                let spawned = |name: &str, reply: Option<Arc<ipc::ApprovalReply>>| {
                    let mut hook =
                        thread_hook(name, "main-thread", &elsewhere, Some("/sessions/rollout-spawned.jsonl"), reply);
                    hook.event = atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":name, "session_id":"main-thread", "cwd":&elsewhere,
                        "transcript_path":"/sessions/rollout-spawned.jsonl",
                        "agent_id":"spawned-thread", "agent_type":"default",
                        "prompt":"a spawned thread's prompt", "last_assistant_message":"a spawned thread's answer",
                        "tool_name":"Bash", "tool_input":{"command":"touch spawned-file"},
                    }))
                    .unwrap();
                    hook
                };
                let main_reply = Arc::new(ipc::ApprovalReply::default());
                app.runtime.on_hook(thread_hook(
                    "PermissionRequest",
                    "main-thread",
                    &repo,
                    MAIN,
                    Some(main_reply.clone()),
                ));
                let waiting = followed(&app.runtime.sessions[0]);
                app.runtime.on_hook(spawned("UserPromptSubmit", None));
                assert_eq!(followed(&app.runtime.sessions[0]), waiting);
                assert_eq!(app.runtime.sessions[0].approvals.len(), 1, "its start must not retire the main request");
                assert!(answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"})).ok);
                assert!(main_reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                app.runtime.on_hook(thread_hook("PostToolUse", "main-thread", &repo, MAIN, None));
                let before = followed(&app.runtime.sessions[0]);
                app.runtime.on_hook(spawned("PostToolUse", None));
                assert_eq!(followed(&app.runtime.sessions[0]), before);

                let reply = Arc::new(ipc::ApprovalReply::default());
                app.runtime.on_hook(spawned("PermissionRequest", Some(reply.clone())));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                assert_eq!(app.runtime.sessions[0].cwd, repo);
                assert!(answer_pending(app, json!({"cmd":"approve", "session":"worker", "decision":"allow"})).ok);
                assert!(reply.wait(Duration::ZERO).stdout.unwrap().contains("allow"));
                app.runtime.on_hook(spawned("PostToolUse", None));
                assert_eq!(followed(&app.runtime.sessions[0]), before, "back to where its request found the session");
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn the_main_thread_still_changes_its_id_and_follows_its_folder() {
        with_app("app::background::tests::the_main_thread_still_changes_its_id_and_follows_its_folder", |app, _| {
            let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
            main_thread_worker(app, &repo);
            let hook = |payload: serde_json::Value| HookMessage {
                pane: Some("worker".into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: None,
                event: atomics_core::hook::HookEvent::from_payload(&payload).unwrap(),
            };
            // `/clear`, `/resume`, compaction and `/fork` start a thread with a transcript and a new id.
            let mut previous = "main-thread".to_string();
            for source in ["clear", "resume", "compact", "fork"] {
                app.runtime.on_hook(hook(json!({
                    "hook_event_name":"SessionStart", "session_id":source, "cwd":&repo, "source":source,
                    "transcript_path":format!("/sessions/{source}.jsonl"),
                })));
                assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some(source));
                // The thread it left has no say any more.
                app.runtime.on_hook(hook(json!({
                    "hook_event_name":"UserPromptSubmit", "session_id":previous, "cwd":&elsewhere, "prompt":"stale",
                    "transcript_path":format!("/sessions/{previous}.jsonl"),
                })));
                assert_eq!(app.runtime.sessions[0].cwd, repo, "{source}");
                assert_eq!(app.runtime.sessions[0].last_prompt, None, "{source}");
                previous = source.to_string();
            }
            // Resuming a Codex thread keeps its id.
            app.runtime.on_hook(hook(json!({
                "hook_event_name":"SessionStart", "session_id":"fork", "cwd":&repo, "source":"resume",
                "transcript_path":"/sessions/fork.jsonl",
            })));
            assert_eq!(app.runtime.sessions[0].claude_id.as_deref(), Some("fork"));
            // A `cd` moves the session, reported either way the agents report it.
            let elsewhere_title = title_for(Some("elsewhere"), &elsewhere);
            let follows = [
                json!({"hook_event_name":"CwdChanged", "session_id":"fork", "cwd":&repo, "new_cwd":&elsewhere,
                       "transcript_path":"/sessions/fork.jsonl"}),
                json!({"hook_event_name":"PostToolUse", "session_id":"fork", "cwd":&elsewhere,
                       "transcript_path":"/sessions/fork.jsonl"}),
            ];
            for payload in follows {
                app.runtime.on_hook(hook(json!({"hook_event_name":"PostToolUse", "session_id":"fork", "cwd":&repo})));
                assert_eq!(app.runtime.sessions[0].cwd, repo);
                app.runtime.on_hook(hook(payload));
                let worker = &app.runtime.sessions[0];
                assert_eq!(worker.cwd, elsewhere);
                assert_eq!(worker.branch(), Some("elsewhere"));
                assert_eq!(worker.title, elsewhere_title);
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn the_chief_stays_in_its_own_folder_when_its_shell_looks_into_an_agents_worktree() {
        with_app(
            "app::background::tests::the_chief_stays_in_its_own_folder_when_its_shell_looks_into_an_agents_worktree",
            |app, _| {
                let ((_repo_dir, repo), (_other_dir, worktree)) = (repository(), other_repository());
                main_thread_worker(app, &repo);
                let chief = &mut app.runtime.sessions[0];
                chief.chief = true;
                chief.rename("chief");
                let root = chief.root();
                let hook = |payload: serde_json::Value| HookMessage {
                    pane: Some("worker".into()),
                    ancestors: vec![],
                    delivered: 0,
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent::from_payload(&payload).unwrap(),
                };
                // `cd` into a plan agent's worktree to review it, reported either way Claude reports it.
                for payload in [
                    json!({"hook_event_name":"CwdChanged", "session_id":"main-thread", "cwd":&repo,
                           "new_cwd":&worktree, "transcript_path":MAIN}),
                    json!({"hook_event_name":"PostToolUse", "session_id":"main-thread", "cwd":&worktree,
                           "transcript_path":MAIN}),
                ] {
                    app.runtime.on_hook(hook(payload));
                    let chief = &app.runtime.sessions[0];
                    assert_eq!((&chief.cwd, chief.root(), chief.branch()), (&repo, root.clone(), Some("main")));
                    assert_eq!(chief.title, "chief");
                }
                // atomics_list shows it where it is, not on the agent's worktree and branch.
                let listed = request(app, json!({"cmd":"list"})).result["sessions"][0].clone();
                assert_eq!(listed["chief"], json!(true));
                assert_eq!(listed["folder"], json!(repo));
                assert_eq!(listed["worktree"], json!(root));
                assert_eq!(listed["branch"], json!("main"));
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn a_claude_turn_interrupted_while_a_tool_runs_is_no_longer_working() {
        with_app(
            "app::background::tests::a_claude_turn_interrupted_while_a_tool_runs_is_no_longer_working",
            |app, _| {
                let (_repo_dir, repo) = repository();
                main_thread_worker(app, &repo);
                app.runtime.sessions[0].kind = AgentKind::Claude;
                let hook = |payload: serde_json::Value| HookMessage {
                    pane: Some("worker".into()),
                    ancestors: vec![],
                    delivered: 0,
                    approval_reply: None,
                    event: atomics_core::hook::HookEvent::from_payload(&payload).unwrap(),
                };
                let failure = |interrupt: bool| {
                    json!({"hook_event_name":"PostToolUseFailure", "session_id":"main-thread", "cwd":&repo,
                       "transcript_path":MAIN, "tool_name":"Bash", "tool_input":{"command":"sleep 600"},
                       "error":"failed", "is_interrupt":interrupt})
                };
                app.runtime.on_hook(hook(json!({"hook_event_name":"UserPromptSubmit", "session_id":"main-thread",
                                            "cwd":&repo, "transcript_path":MAIN, "prompt":"run it"})));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                // A command that fails leaves the turn going.
                app.runtime.on_hook(hook(failure(false)));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working);
                // Esc while it runs: Claude sends no Stop, only the tool's interrupted failure.
                app.runtime.on_hook(hook(failure(true)));
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::Idle);
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn a_request_from_a_thread_the_session_does_not_follow_is_left_to_the_agent_at_once() {
        with_app(
            "app::background::tests::a_request_from_a_thread_the_session_does_not_follow_is_left_to_the_agent_at_once",
            |app, _| {
                let ((_repo_dir, repo), (_other_dir, elsewhere)) = (repository(), other_repository());
                main_thread_worker(app, &repo);
                let before = followed(&app.runtime.sessions[0]);
                for (thread, transcript) in
                    [("background-thread", None), ("earlier-thread", Some("/sessions/rollout-earlier.jsonl"))]
                {
                    let reply = Arc::new(ipc::ApprovalReply::default());
                    app.runtime.on_hook(thread_hook(
                        "PermissionRequest",
                        thread,
                        &elsewhere,
                        transcript,
                        Some(reply.clone()),
                    ));
                    // Nothing could ever answer it: the hook must not wait out its whole window.
                    assert!(!reply.answer(HookReply { stdout: Some("too late".into()) }), "{thread}");
                    assert!(reply.wait(Duration::ZERO).stdout.is_none(), "{thread}");
                    assert!(app.runtime.sessions[0].approvals.is_empty(), "{thread}");
                    assert_eq!(followed(&app.runtime.sessions[0]), before, "{thread}");
                }
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn watcher_retries_missing_roots_and_receives_events_under_symlinked_parents() {
        with_app(
            "app::background::tests::watcher_retries_missing_roots_and_receives_events_under_symlinked_parents",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let real = dir.path().join("real");
                std::fs::create_dir(&real).unwrap();
                let alias = dir.path().join("alias");
                std::os::unix::fs::symlink(&real, &alias).unwrap();
                let root = alias.join("worktree");
                app.runtime.sessions.clear();
                app.runtime.plans.clear();
                app.runtime.watch(Some(&root));
                let watched = |app: &Desktop<'_>| app.runtime.watcher.as_ref().unwrap().roots().to_vec();
                assert!(watched(app).is_empty(), "a failed watch must remain pending");
                std::fs::create_dir(&root).unwrap();
                app.runtime.watch(Some(&root));
                assert_eq!(watched(app), std::slice::from_ref(&root));
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                loop {
                    std::fs::write(root.join("changed.txt"), "changed").unwrap();
                    if let Ok(changed) = app
                        .runtime
                        .watch_rx
                        .recv_timeout(crate::test_support::scaled_timeout(Duration::from_millis(100)))
                    {
                        assert_eq!(changed, root, "events must retain the application's worktree path");
                        break;
                    }
                    assert!(Instant::now() < deadline, "no event for symlinked worktree");
                }
                app.runtime.watch(None);
                assert!(watched(app).is_empty());
            },
        );
    }

    #[test]
    fn ref_updates_mark_integrations_dirty() {
        with_app("app::background::tests::ref_updates_mark_integrations_dirty", |app, _ctx| {
            let (_dir, repo) = repository();
            app.runtime.sessions.clear();
            app.runtime.plans.clear();
            app.runtime.watch(Some(&repo));
            // The last one moves a branch inside a refs folder created after watching began.
            for args in [
                &["branch", "feature/one"][..],
                &["commit", "--allow-empty", "-qm", "second"],
                &["update-ref", "refs/heads/feature/one", "HEAD"],
            ] {
                // Earlier commands settle first, so only this one can mark integrations dirty.
                std::thread::sleep(crate::test_support::scaled_timeout(Duration::from_millis(750)));
                app.runtime.integration_dirty.store(false, Ordering::Relaxed);
                git::git(&repo, args).unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !app.runtime.integration_dirty.load(Ordering::Relaxed) {
                    assert!(Instant::now() < deadline, "git {} must mark integrations dirty", args.join(" "));
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            app.runtime.watch(None);
        });
    }

    #[test]
    fn startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans() {
        with_app(
            "app::background::tests::startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans",
            |app, ctx| {
                request(app, json!({"cmd":"plan_set", "goal":"Valid", "tasks":[]}));
                let folder = paths::atomics_home().join("plans");
                std::fs::write(folder.join("broken.json"), "{").unwrap();
                std::fs::create_dir(folder.join("unreadable.json")).unwrap();
                let cc = eframe::CreationContext::_new_kittest(ctx.clone());
                let reopened = State::new(&cc, Options::default(), &crate::load_icon());
                let toast = &reopened.toast.as_ref().unwrap().0;
                assert!(toast.contains("broken.json") && toast.contains("unreadable.json"), "{toast}");
                assert_eq!(reopened.plans.iter().filter(|p| p.is_open()).count(), 1);
            },
        );
    }
}
