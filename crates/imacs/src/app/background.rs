use super::*;

impl App {
    // ── background events ─────────────────────────────────────────────────

    pub(super) fn pump(&mut self, ctx: &egui::Context) {
        self.refresh_integrations();
        self.usage.drain();
        for session in &mut self.sessions {
            if let Some(ready) = &session.worktree_ready
                && let Ok(worktree) = ready.try_recv()
            {
                session.worktree = worktree;
                session.worktree_ready = None;
                if !session.chief {
                    session.title = title_for(session.branch(), &session.cwd);
                }
            }
        }
        for (agent, dir) in std::mem::take(&mut self.pending_open) {
            self.spawn(ctx, &agent, dir, None);
        }
        let pending = std::mem::take(&mut self.pending_codex);
        for (launch, ready) in pending {
            if self.find(&launch.id).is_none() {
                continue;
            }
            if let Some(supported) = ready.get().copied() {
                let id = launch.id.clone();
                if supported.is_none() {
                    self.toast(
                        "Codex version probe failed; starting without hooks and retrying detection on the next start",
                    );
                }
                if let Err(error) = self.launch_agent(ctx, launch, Some(supported.unwrap_or(false))) {
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
        while let Ok(message) = self.hook_rx.try_recv() {
            self.on_hook(message);
        }
        while let Ok((branch, result, launch)) = self.worktree_rx.try_recv() {
            self.worktrees_at = None;
            if let Some(i) = self.creating.iter().position(|b| *b == branch) {
                self.creating.remove(i);
            }
            let background = launch
                .task
                .as_ref()
                .is_some_and(|(plan, _)| !self.plan_view || self.plans[self.selected_plan].id != *plan);
            let selection = (self.active.clone(), self.diff_root.clone(), self.focus_terminal.clone());
            let outcome =
                result.and_then(|path| self.start(ctx, &launch.agent, path, None, launch.args, launch.prompt));
            if background {
                (self.active, self.diff_root, self.focus_terminal) = selection;
            }
            if let Some((plan_id, task_id)) = &launch.task
                && let Some(index) = self.plans.iter().position(|p| &p.id == plan_id)
                && let Some(task) = self.plans[index].task_mut(task_id)
            {
                match &outcome {
                    Ok(id) => {
                        task.session = Some(id.clone());
                        task.worktree = self.sessions.iter().find(|s| &s.id == id).map(Session::root);
                    }
                    Err(err) => {
                        task.set_status(TaskStatus::Blocked);
                        task.notes.push(format!("could not start: {err}"));
                    }
                }
                self.save_plan(index);
            }
            let response = match outcome {
                Ok(id) => ControlResponse::ok(self.describe(self.find(&id).expect("just started"))),
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
            self.finish_integration(integrated);
        }
        self.expire_approvals(Instant::now());
        if let Some(interval) = self.approval_poll_interval() {
            ctx.request_repaint_after(interval);
        }
        while let Ok(message) = self.control_rx.try_recv() {
            self.control(ctx, message.request, message.reply);
        }
        while let Ok(event) = self.voice_rx.try_recv() {
            match event {
                VoiceEvent::Opened(cancelled, result) => {
                    if matches!(&self.voice, Voice::Opening(current) if Arc::ptr_eq(current, &cancelled)) {
                        match result {
                            Ok(recorder) => self.voice = Voice::Recording(recorder),
                            Err(err) => {
                                self.voice = Voice::Idle;
                                self.toast(format!("Voice: {err}"));
                            }
                        }
                    }
                }
                VoiceEvent::Transcript(result) => {
                    self.voice = Voice::Idle;
                    match result {
                        Ok(text) => self.send_voice(ctx, &text),
                        Err(err) => self.toast(format!("Voice: {err}")),
                    }
                }
            }
        }
        if let Voice::Recording(recorder) = &self.voice {
            if recorder.elapsed() >= MAX_RECORDING {
                self.stop_voice();
                self.toast("Voice: stopped after 5 minutes, transcribing");
            } else {
                // Keeps the recording clock moving.
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        self.track_busy();
        self.verify_deliveries();
        for index in 0..self.sessions.len() {
            self.release_held(index);
        }
        if self.sessions.iter().any(|session| session.pending.is_some()) {
            ctx.request_repaint_after(PULSE_STEP);
        }
        self.record_events();
        self.sync_plan();
        self.heartbeat();
        // Events wake waiters as they come in; this only times them out.
        if let Some(deadline) = self.waiters.iter().map(|w| w.deadline).min() {
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
        }
        if self.sessions.iter().any(|s| s.status == AgentStatus::Working) {
            // Keeps the status pulse of working agents moving, in a few steps a second.
            ctx.request_repaint_after(PULSE_STEP);
        }
        while let Ok(result) = self.worker.rx.try_recv() {
            match result {
                WorkerResult::Diff { root, result } => {
                    let state = self.diffs.entry(root.clone()).or_default();
                    state.loading = false;
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
                                if changed && let Err(err) = plan.save_in(&paths::imacs_home()) {
                                    eprintln!("IMACS: saving plan {index} failed: {err:#}");
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
            self.diffs.entry(root).or_default().mark_dirty();
        }
        for session in &mut self.sessions {
            if session.terminal.as_ref().is_some_and(Terminal::exited) {
                session.status = AgentStatus::Exited;
            }
            if let Some(title) = session.terminal.as_ref().and_then(|t| t.shared.title.lock().unwrap().take())
                && session.kind == AgentKind::Shell
                && !title.trim().is_empty()
            {
                session.title = title;
            }
        }

        if self.config_checked.elapsed() > Duration::from_secs(1) {
            self.config_checked = Instant::now();
            let stamp = modified(&self.config_path);
            if stamp != self.config_stamp {
                self.config_stamp = stamp;
                match Config::load(&self.config_path) {
                    Ok(config) => {
                        let syntax = Palette::from_config(&config.theme).syntax;
                        let theme_changed = syntax != self.palette.syntax;
                        if config.terminal.scrollback != self.config.terminal.scrollback {
                            let terminals =
                                self.sessions.iter().filter_map(|s| s.terminal.as_ref()).chain(self.editors.values());
                            for terminal in terminals {
                                terminal.set_scrollback(config.terminal.scrollback);
                            }
                        }
                        self.config = config;
                        self.config_error = None;
                        self.palette = Arc::new(Palette::from_config(&self.config.theme));
                        if theme_changed {
                            self.highlighter = Arc::new(Highlighter::new(&syntax));
                        }
                        apply_style(ctx, &self.config);
                        self.apply_config();
                        for state in self.diffs.values_mut() {
                            state.mark_dirty();
                        }
                        self.toast("Config reloaded");
                    }
                    Err(err) => self.config_error = Some(format!("{err:#}")),
                }
            }
        }

        if self.worktrees_at.is_none_or(|at| at.elapsed() > WORKTREE_REFRESH) {
            self.worktrees_at = Some(Instant::now());
            self.worktrees.clear();
            self.worktree_project.clear();
            for repo in self.repos.clone() {
                for worktree in git::list_worktrees(&repo).unwrap_or_default() {
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
            Some(ws) => format!("IMACS · {ws} · {}", names.join(", ")),
            None if names.is_empty() => "IMACS".to_string(),
            None => format!("IMACS · {}", names.join(", ")),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let target = self.diff_target();
        self.watch(ctx, target.as_deref());
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
        let mut next: Option<Instant> = None;
        for (root, state) in &mut self.diffs {
            // The diff on screen follows every change; the others at most every few seconds,
            // with only some of their untracked content.
            let on_screen = target.as_ref() == Some(root);
            let (interval, budget) =
                if on_screen { (Duration::ZERO, None) } else { (BACKGROUND_REFRESH, Some(OFF_SCREEN_UNTRACKED)) };
            match state.due_at(interval) {
                Some(at) if at <= now => {
                    state.requested();
                    let (base_ref, highlighter) = (self.config.base_ref.clone(), self.highlighter.clone());
                    self.worker.request(root.clone(), base_ref, budget, highlighter);
                }
                Some(at) => next = Some(next.map_or(at, |next| next.min(at))),
                None => {}
            }
        }
        if let Some(at) = next {
            ctx.request_repaint_after(at - now);
        }
        if let Some((_, at)) = &self.toast
            && at.elapsed() > Duration::from_secs(5)
        {
            self.toast = None;
        }
    }

    pub(super) fn on_hook(&mut self, message: HookMessage) {
        let event = message.event;
        if self.dismissed.contains(&event.session_id) {
            return;
        }
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
                    activity: heartbeat::Activity::default(),
                    approvals: Vec::new(),
                    allowed: Vec::new(),
                    held: Vec::new(),
                    approval_hold: false,
                    menu_absent_since: None,
                    selected: false,
                    chief: false,
                    pending: None,
                });
                if self.active.is_none() {
                    self.active = Some(format!("ext:{}", event.session_id));
                }
                self.sessions.len() - 1
            }
        };

        let session = &mut self.sessions[index];
        if event.name != "SessionStart" && session.claude_id.as_ref().is_some_and(|id| id != &event.session_id) {
            return;
        }
        session.claude_id = Some(event.session_id.clone());
        session.hooks_active = true;
        session.activity.last_event = imacs_core::store::now();
        if event.name == "PermissionRequest" {
            let request = ApprovalRequest::from_event(&event);
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
            session.approvals.push(PendingApproval { request, reply: message.approval_reply });
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
        } else if matches!(event.name.as_str(), "Stop" | "SessionEnd" | "Interrupt" | "UserPromptSubmit") {
            session.approvals.clear();
        }
        if let Some(cwd) = &event.cwd
            && *cwd != session.cwd
        {
            session.cwd = cwd.clone();
            session.worktree_ready = None;
            session.worktree = git::worktree_of(cwd);
            session.title = title_for(session.branch(), cwd);
        }
        if let Some(status) = event.status() {
            session.status = if session.approvals.is_empty() { status } else { AgentStatus::NeedsInput };
        }
        match event.name.as_str() {
            "UserPromptSubmit" => {
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
            }
            "Stop" => session.last_message = event.last_assistant_message.clone(),
            _ => {}
        }
        session.activity.observe(session.status);
        let root = session.root();
        let (id, title) = (session.id.clone(), session.title.clone());
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
                    .is_some_and(|terminal| imacs_core::approvals::menu_visible(&terminal.screen_text(60)));
                session.approval_hold = menu;
                session.menu_absent_since = None;
            }
            self.release_held(index);
        }
        if event.name == "PermissionRequest" {
            self.last_status.insert(id.clone(), AgentStatus::NeedsInput);
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
                at: imacs_core::store::now(),
                report: None,
                approval: self.sessions[index].approvals.last().map(|pending| pending.request.clone()),
                findings: None,
            });
            self.serve_waiters();
        }
    }

    /// Watches the worktree of every agent and the one on screen, so their diffs follow
    /// file changes without polling.
    fn watch(&mut self, ctx: &egui::Context, target: Option<&Path>) {
        let mut wanted: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|s| s.worktree.is_some())
            .map(Session::root)
            .chain(target.map(Path::to_path_buf))
            .collect();
        wanted.extend(self.plans.iter().filter(|p| p.is_open()).filter_map(|p| p.project.clone()));
        wanted.sort();
        wanted.dedup();
        let current: Vec<PathBuf> = self.watched.lock().unwrap().iter().map(|(root, _)| root.clone()).collect();
        if current == wanted {
            return;
        }
        if self.watcher.is_none() {
            let (tx, ctx, watched) = (self.watch_tx.clone(), ctx.clone(), self.watched.clone());
            let integration_dirty = self.integration_dirty.clone();
            let handler = move |result: DebounceEventResult| {
                let Ok(events) = result else { return };
                let watched = watched.lock().unwrap();
                let mut changed: Vec<&PathBuf> = Vec::new();
                for event in &events {
                    if event.path.components().any(|c| c.as_os_str() == ".git")
                        && (event.path.components().any(|c| c.as_os_str() == "refs")
                            || event.path.file_name().is_some_and(|n| n == "HEAD" || n == "packed-refs"))
                    {
                        integration_dirty.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                        continue;
                    }
                    // The innermost worktree, for worktrees inside another.
                    let Some((root, relative)) = watched
                        .iter()
                        .filter_map(|(root, real)| Some((root, event.path.strip_prefix(real).ok()?)))
                        .min_by_key(|(_, relative)| relative.components().count())
                    else {
                        continue;
                    };
                    let ignored = relative.components().any(|c| IGNORED_DIRS.iter().any(|d| c.as_os_str() == *d));
                    if !ignored && !changed.contains(&root) {
                        changed.push(root);
                    }
                }
                for root in &changed {
                    let _ = tx.send((*root).clone());
                }
                if !changed.is_empty() {
                    ctx.request_repaint();
                }
            };
            match new_debouncer(Duration::from_millis(250), handler) {
                Ok(debouncer) => self.watcher = Some(debouncer),
                Err(err) => eprintln!("IMACS: file watcher failed: {err}"),
            }
        }
        if let Some(debouncer) = &mut self.watcher {
            let watcher = debouncer.watcher();
            for root in current.iter().filter(|root| !wanted.contains(root)) {
                if let Some((_, real)) = self.watched.lock().unwrap().iter().find(|(path, _)| path == root) {
                    let _ = watcher.unwatch(real);
                }
            }
            self.watched.lock().unwrap().retain(|(root, _)| wanted.contains(root));
            for root in wanted.iter().filter(|root| !current.contains(root)) {
                let Ok(real) = std::fs::canonicalize(root) else { continue };
                if watcher.watch(&real, RecursiveMode::Recursive).is_ok() {
                    self.watched.lock().unwrap().push((root.clone(), real));
                }
            }
            self.watched.lock().unwrap().sort();
        }
        if self.watched.lock().unwrap().len() != wanted.len() {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn needs_input_without_a_pending_menu_uses_slow_screen_checks() {
        with_app("app::background::tests::needs_input_without_a_pending_menu_uses_slow_screen_checks", |app, ctx| {
            let terminal = Terminal::spawn(
                ctx,
                app.palette.clone(),
                "/bin/sh".into(),
                vec!["-c".into(), "printf 'Working (esc to interrupt)'; exec sleep 30".into()],
                &app.launch_dir,
                HashMap::new(),
                100,
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !terminal.screen_text(60).contains("esc to interrupt") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Generic,
                busy_text: Some("esc to interrupt".into()),
            });
            app.sessions = vec![session("waiting", AgentKind::Generic, AgentStatus::NeedsInput, &app.launch_dir)];
            app.sessions[0].terminal = Some(terminal);
            let now = Instant::now();
            app.approvals_checked = now - APPROVAL_CHECK_INTERVAL;
            app.needs_input_checked = now;
            assert_eq!(app.approval_poll_interval(), Some(NEEDS_INPUT_CHECK_INTERVAL));
            app.expire_approvals(now);
            assert!(app.sessions[0].menu_absent_since.is_none(), "a frame must not read an idle needs-input screen");
            app.expire_approvals(now + NEEDS_INPUT_CHECK_INTERVAL);
            assert!(
                app.sessions[0].menu_absent_since.is_some(),
                "the slow check still recovers missed lifecycle events"
            );
            app.sessions[0].approval_hold = true;
            assert_eq!(app.approval_poll_interval(), Some(Duration::from_millis(400)));
            app.expire_approvals(now + NEEDS_INPUT_CHECK_INTERVAL + APPROVAL_MENU_GRACE);
            assert!(!app.sessions[0].approval_hold);
            assert_eq!(app.sessions[0].status, AgentStatus::Working);
            assert_eq!(app.approval_poll_interval(), None);
            app.sessions.clear();
        });
    }

    #[test]
    fn pre_tool_use_retires_only_the_matching_approval() {
        with_app("app::background::tests::pre_tool_use_retires_only_the_matching_approval", |app, _| {
            app.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.launch_dir)];
            let hook = |name: &str, command: &str, tool: &str, cwd: &Path, reply| HookMessage {
                pane: Some("worker".into()),
                ancestors: vec![],
                delivered: 0,
                approval_reply: reply,
                event: imacs_core::hook::HookEvent::from_payload(&json!({
                    "hook_event_name":name, "session_id":"worker", "cwd":cwd,
                    "tool_name":tool, "tool_input":{"command":command}
                }))
                .unwrap(),
            };
            let root = app.launch_dir.clone();
            let matching = Arc::new(ipc::ApprovalReply::default());
            let other = Arc::new(ipc::ApprovalReply::default());
            for (command, reply) in [("touch first", matching.clone()), ("touch second", other.clone())] {
                app.on_hook(hook("PermissionRequest", command, "Bash", &root, Some(reply)));
            }
            for (command, tool, cwd) in [
                ("touch third", "Bash", root.clone()),
                ("touch first", "Edit", root.clone()),
                ("touch first", "Bash", root.join("elsewhere")),
            ] {
                app.on_hook(hook("PreToolUse", command, tool, &cwd, None));
                assert_eq!(app.sessions[0].approvals.len(), 2);
                assert!(!matching.expired() && !other.expired());
                assert_eq!(app.sessions[0].status, AgentStatus::NeedsInput);
            }
            app.on_hook(hook("PreToolUse", "touch first", "Bash", &root, None));
            assert_eq!(app.sessions[0].approvals.len(), 1);
            assert_eq!(app.sessions[0].approvals[0].request.command.as_deref(), Some("touch second"));
            assert!(!matching.answer(HookReply { stdout: Some("must already be answered".into()) }));
            assert!(matching.wait(Duration::ZERO).stdout.is_none());
            assert!(!other.expired(), "the other hook must still be waiting for its own decision");
            assert_eq!(app.sessions[0].status, AgentStatus::NeedsInput);
            app.approve(0, Decision::Deny, None).unwrap();
            let output = other.wait(Duration::ZERO).stdout.unwrap();
            assert!(output.contains("deny"));
        });
    }

    #[test]
    fn watcher_retries_missing_roots_and_receives_events_under_symlinked_parents() {
        with_app(
            "app::background::tests::watcher_retries_missing_roots_and_receives_events_under_symlinked_parents",
            |app, ctx| {
                let dir = tempfile::tempdir().unwrap();
                let real = dir.path().join("real");
                std::fs::create_dir(&real).unwrap();
                let alias = dir.path().join("alias");
                std::os::unix::fs::symlink(&real, &alias).unwrap();
                let root = alias.join("worktree");
                app.sessions.clear();
                app.plans.clear();
                app.watch(ctx, Some(&root));
                assert!(app.watched.lock().unwrap().is_empty(), "a failed watch must remain pending");
                std::fs::create_dir(&root).unwrap();
                app.watch(ctx, Some(&root));
                assert_eq!(*app.watched.lock().unwrap(), [(root.clone(), std::fs::canonicalize(&root).unwrap())]);
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    std::fs::write(root.join("changed.txt"), "changed").unwrap();
                    if let Ok(changed) = app.watch_rx.recv_timeout(Duration::from_millis(100)) {
                        assert_eq!(changed, root, "events must retain the application's worktree path");
                        break;
                    }
                    assert!(Instant::now() < deadline, "no event for symlinked worktree");
                }
                app.watch(ctx, None);
                assert!(app.watched.lock().unwrap().is_empty());
            },
        );
    }

    #[test]
    fn startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans() {
        with_app(
            "app::background::tests::startup_toast_lists_unreadable_plan_files_without_hiding_valid_plans",
            |app, ctx| {
                request(app, json!({"cmd":"plan_set", "goal":"Valid", "tasks":[]}));
                let folder = paths::imacs_home().join("plans");
                std::fs::write(folder.join("broken.json"), "{").unwrap();
                std::fs::create_dir(folder.join("unreadable.json")).unwrap();
                let cc = eframe::CreationContext::_new_kittest(ctx.clone());
                let reopened = App::new(&cc, Options::default(), &crate::load_icon());
                let toast = &reopened.toast.as_ref().unwrap().0;
                assert!(toast.contains("broken.json") && toast.contains("unreadable.json"), "{toast}");
                assert_eq!(reopened.plans.iter().filter(|p| p.is_open()).count(), 1);
            },
        );
    }
}
