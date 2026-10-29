use super::*;

impl Session {
    pub(super) fn starting(
        id: String,
        agent: &imacs_core::config::AgentConfig,
        cwd: PathBuf,
        chief: bool,
        wake: &crate::host::Wake,
    ) -> Self {
        let (tx, rx) = wake.channel();
        let dir = cwd.clone();
        std::thread::spawn(move || {
            let _ = tx.send(git::worktree_of(&dir));
        });
        let title = if chief { "chief".into() } else { title_for(None, &cwd) };
        Self {
            id,
            agent: agent.name.clone(),
            kind: agent.kind,
            terminal: None,
            claude_id: None,
            cwd,
            worktree: None,
            worktree_ready: Some(rx),
            title,
            status: if agent.kind == AgentKind::Shell { AgentStatus::Idle } else { AgentStatus::Starting },
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
            chief,
            pending: None,
        }
    }

    pub(super) fn root(&self) -> PathBuf {
        self.worktree.as_ref().map(|w| w.root.clone()).unwrap_or_else(|| self.cwd.clone())
    }

    pub(super) fn branch(&self) -> Option<&str> {
        self.worktree.as_ref().and_then(|w| w.branch.as_deref())
    }

    pub(super) fn is_agent(&self) -> bool {
        self.kind != AgentKind::Shell
    }

    pub(super) fn can_receive(&self) -> bool {
        match &self.terminal {
            Some(t) => !t.exited(),
            None => self.claude_id.is_some() && self.status != AgentStatus::Exited,
        }
    }
}

impl State {
    // ── sessions ──────────────────────────────────────────────────────────

    pub(super) fn spawn(&mut self, ctx: &egui::Context, agent_name: &str, cwd: PathBuf, resume: Option<String>) {
        if let Err(err) = self.start(ctx, agent_name, cwd, resume, Vec::new(), None) {
            self.toast(err);
        }
    }

    /// Starts an agent in a terminal and returns its session id.
    pub(super) fn start(
        &mut self,
        ctx: &egui::Context,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
    ) -> Result<String, String> {
        self.start_as(ctx, agent_name, cwd, resume, extra_args, prompt, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_as(
        &mut self,
        ctx: &egui::Context,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
        chief: bool,
    ) -> Result<String, String> {
        let agent = if chief {
            imacs_core::config::AgentConfig {
                name: "chief".into(),
                command: self.config.chief.command.clone(),
                kind: AgentKind::Claude,
                busy_text: None,
            }
        } else {
            let Some(agent) = self.config.agent(agent_name).cloned() else {
                return Err(format!("No agent named {agent_name} in the config"));
            };
            agent
        };
        if !cwd.is_dir() {
            return Err(format!("{} is not a folder", cwd.display()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let codex = agent.is_codex();
        let launch = AgentLaunch { id: id.clone(), agent, cwd, resume, extra_args, prompt, chief };
        let version = if codex {
            let program = resolve_program(&launch.agent.command[0]);
            let ready = self.codex_version(ctx, program);
            match ready.get().copied() {
                Some(supported) => {
                    if supported.is_none() {
                        self.toast("Codex version probe failed; starting without hooks and retrying detection on the next start");
                    }
                    Some(supported.unwrap_or(false))
                }
                None => {
                    self.sessions.push(Session::starting(
                        id.clone(),
                        &launch.agent,
                        launch.cwd.clone(),
                        chief,
                        &self.wake,
                    ));
                    self.pending_codex.push((launch, ready));
                    self.activate(&id, false);
                    return Ok(id);
                }
            }
        } else {
            None
        };
        self.launch_agent(ctx, launch, version)
    }

    fn codex_version(&mut self, _ctx: &egui::Context, program: String) -> CodexVersion {
        if self.codex_versions.get(&program).is_some_and(|ready| ready.get() == Some(&None)) {
            self.codex_versions.remove(&program);
        }
        self.codex_versions
            .entry(program.clone())
            .or_insert_with(|| {
                let ready = Arc::new(OnceLock::new());
                let result = ready.clone();
                let wake = self.wake.clone();
                std::thread::spawn(move || {
                    let supported = crate::setup::codex_supports_hooks(&program, CODEX_VERSION_TIMEOUT);
                    let _ = result.set(supported);
                    wake.notify();
                });
                ready
            })
            .clone()
    }

    pub(super) fn launch_agent(
        &mut self,
        _ctx: &egui::Context,
        launch: AgentLaunch,
        codex: Option<bool>,
    ) -> Result<String, String> {
        let AgentLaunch { id, agent, cwd, resume, extra_args, prompt, chief } = launch;
        let agent_name = agent.name.clone();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color".into()), ("COLORTERM", "truecolor".into()), ("TERM_PROGRAM", "imacs".into())]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        if let Some(path) = SHELL_PATH.as_ref() {
            env.insert("PATH".into(), path.clone());
        }
        // `imacs queue` in an agent resolves to this IMACS, and its compilers share one job pool.
        if let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
            let path = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
            // An empty entry would put the current directory on PATH.
            let path = if path.is_empty() { dir.display().to_string() } else { format!("{}:{path}", dir.display()) };
            env.insert("PATH".into(), path);
        }
        if let Some(jobserver) = &self.jobserver {
            env.extend(jobserver.env());
        }
        env.extend(compile_cache_env(&cwd));
        env.extend(crate::governor::toolchain_env(&cwd, &self.config.resources, self.governor.cores));

        let (program, mut args) = match agent.command.split_first() {
            Some((program, rest)) => (program.clone(), rest.to_vec()),
            None => (shell, vec!["-l".to_string()]),
        };
        let mut claude_id = None;
        // Every agent IMACS starts can report back to the plan with `imacs_report`.
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "imacs".into());
        let socket = paths::socket_path().to_string_lossy().into_owned();
        match agent.kind {
            AgentKind::Claude if chief => {
                // The chief gets every IMACS tool and knows how to run a plan.
                let config = json!({ "mcpServers": { "imacs": {
                    "command": exe, "args": ["mcp"], "env": { "IMACS_PANE": id, "IMACS_SOCKET": socket }
                } } });
                let projects: Vec<String> = self.repos.iter().map(|r| paths::display(r)).collect();
                let prompt = self
                    .config
                    .chief
                    .system_prompt
                    .replace("{projects}", &projects.join(", "))
                    .replace("{base_ref}", &self.config.base_ref);
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    CHIEF_ALLOWED_TOOLS.join(","),
                    "--append-system-prompt".to_string(),
                    prompt,
                ]);
            }
            AgentKind::Claude => {
                let config = json!({ "mcpServers": { "imacs": {
                    "command": exe, "args": ["mcp", "--agent"], "env": { "IMACS_PANE": id, "IMACS_SOCKET": socket }
                } } });
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    "mcp__imacs__imacs_report".to_string(),
                ]);
            }
            AgentKind::Generic if codex.is_some() => {
                let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
                if codex == Some(true) {
                    if !args.iter().any(|arg| arg == "--no-daemon") {
                        args.push("--no-daemon".into());
                    }
                    args.extend(imacs_core::hook::codex_hook_args(Path::new(&exe), Path::new(&socket), &id));
                }
                args.extend([
                    "-c".to_string(),
                    format!("mcp_servers.imacs.command={}", quoted(&exe)),
                    "-c".to_string(),
                    "mcp_servers.imacs.args=[\"mcp\", \"--agent\"]".to_string(),
                    "-c".to_string(),
                    format!(
                        "mcp_servers.imacs.env={{ IMACS_PANE = {}, IMACS_SOCKET = {} }}",
                        quoted(&id),
                        quoted(&socket)
                    ),
                    "-c".to_string(),
                    "mcp_servers.imacs.default_tools_approval_mode=\"approve\"".to_string(),
                ]);
            }
            _ => {}
        }
        if agent.kind == AgentKind::Claude {
            args.push("--settings".into());
            args.push(imacs_core::hook::session_settings(Path::new(&exe), Path::new(&socket), &id, false).to_string());
            let session_id = match resume {
                Some(id) => {
                    args.extend(["--resume".to_string(), id.clone()]);
                    id
                }
                None => {
                    let id = uuid::Uuid::new_v4().to_string();
                    args.extend(["--session-id".to_string(), id.clone()]);
                    id
                }
            };
            claude_id = Some(session_id);
        } else if let Some(id) = resume {
            args.extend(["resume".into(), if id.is_empty() { "--last".into() } else { id.clone() }]);
            claude_id = (!id.is_empty()).then_some(id);
        }
        args.extend(extra_args);
        let prompt = match prompt {
            Some(text) if agent.kind != AgentKind::Claude && text.len() > LONG_PROMPT => Some(brief_file(&id, &text)?),
            other => other,
        };
        let shell_prompt = match (&prompt, agent.kind) {
            (Some(prompt), AgentKind::Shell) => Some(prompt.clone()),
            (Some(prompt), _) => {
                args.push(prompt.clone());
                None
            }
            _ => None,
        };
        let program = resolve_program(&program);
        let nice = self.config.resources.agent_nice;
        let (program, args) = if !chief && agent.kind != AgentKind::Shell && nice != 0 {
            ("nice".to_string(), ["-n".to_string(), nice.to_string(), program].into_iter().chain(args).collect())
        } else {
            (program, args)
        };
        match Terminal::spawn(
            self.wake.clone(),
            self.palette.terminal_colors(),
            program,
            args,
            &cwd,
            env,
            self.config.terminal.scrollback,
        ) {
            Ok(terminal) => {
                if let Some(id) = &claude_id {
                    self.dismissed.remove(id);
                }
                let starting = self.sessions.iter().position(|session| session.id == id);
                let index = starting.unwrap_or_else(|| {
                    self.sessions.push(Session::starting(id.clone(), &agent, cwd, chief, &self.wake));
                    self.sessions.len() - 1
                });
                self.sessions[index].terminal = Some(terminal);
                self.sessions[index].claude_id = claude_id;
                if chief {
                    self.hub.lock().unwrap().chief_running = true;
                }
                if let Some(text) = shell_prompt
                    && let Some(terminal) = self.sessions.iter().find(|s| s.id == id).and_then(|s| s.terminal.as_ref())
                {
                    terminal.submit(&text);
                }
                if !chief && starting.is_none() {
                    self.activate(&id, false);
                }
                self.remember(&id);
                Ok(id)
            }
            Err(err) => Err(format!("Could not start {agent_name}: {err:#}")),
        }
    }

    /// Finds a session by id, id prefix, or title.
    pub(super) fn find(&self, reference: &str) -> Option<usize> {
        let reference = reference.trim();
        self.sessions
            .iter()
            .position(|s| s.id == reference)
            .or_else(|| self.sessions.iter().position(|s| s.title.eq_ignore_ascii_case(reference)))
            .or_else(|| {
                let matches: Vec<usize> =
                    (0..self.sessions.len()).filter(|&i| self.sessions[i].id.starts_with(reference)).collect();
                (matches.len() == 1).then(|| matches[0])
            })
    }

    /// Confirm terminal delivery before answering Codex sends; busy turns can drop input.
    pub(super) fn verify_deliveries(&mut self) {
        let mut warnings = Vec::new();
        for session in &mut self.sessions {
            if !session.terminal.as_ref().is_some_and(Terminal::exited)
                && (session.approval_hold || !session.approvals.is_empty())
            {
                continue;
            }
            let (Some(pending), Some(terminal)) = (session.pending.as_mut(), session.terminal.as_ref()) else {
                continue;
            };
            let started = !pending.was_working && session.status == AgentStatus::Working;
            let snippet: String = pending.text.lines().next().unwrap_or("").chars().take(24).collect();
            let screen = terminal.screen_text(200);
            let lines: Vec<&str> = screen.lines().collect();
            let input_box = lines.len().saturating_sub(6);
            let composer_empty = pending.codex.then(|| codex_composer_empty(&screen, &pending.screen_before)).flatten();
            let in_transcript = if pending.codex {
                let marker = codex_composer(&screen).is_some_and(|line| line.contains("[Pasted "));
                pending.paste_seen |=
                    marker && !codex_composer(&pending.screen_before).is_some_and(|line| line.contains("[Pasted "));
                codex_transcript_receipt(&screen, &pending.screen_before, &pending.text)
                    || pending.paste_seen && !marker && composer_empty == Some(true)
            } else {
                started || !snippet.trim().is_empty() && lines[..input_box].iter().any(|l| l.contains(snippet.trim()))
            };
            if in_transcript {
                if let Some(reply) = pending.reply.take() {
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(json!({"sent":true}))));
                }
                session.pending = None;
                continue;
            }
            if !terminal.exited() && pending.sent.elapsed() < DELIVERY_TIMEOUT {
                continue;
            }
            if terminal.exited() || pending.attempts >= DELIVERY_ATTEMPTS {
                if let Some(reply) = pending.reply.take() {
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::error(if pending.codex {
                        "message delivery unconfirmed: no receipt from the agent; check its terminal"
                    } else {
                        "message not delivered: no receipt from the agent; check its terminal"
                    })));
                }
                warnings.push(session.title.clone());
                session.pending = None;
                continue;
            }
            if pending.codex {
                // The paste is sent exactly once. Retry only the submit key,
                // and only while the composer differs from its pre-paste line.
                if composer_empty == Some(false) {
                    terminal.write(if screen.contains("tab to queue") { b"\t".to_vec() } else { b"\r".to_vec() });
                }
            } else if lines[input_box..].iter().any(|l| l.contains(snippet.trim())) {
                terminal.write(b"\r".to_vec());
            } else {
                terminal.submit(&pending.text);
            }
            pending.attempts += 1;
            pending.sent = Instant::now();
            pending.was_working = session.status == AgentStatus::Working;
        }
        for title in warnings {
            self.toast(format!("{title} has not picked up your message; check its terminal"));
        }
    }

    /// Status for agents without hooks, read from what their terminal shows.
    pub(super) fn track_busy(&mut self) {
        if self.busy_checked.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.busy_checked = Instant::now();
        let mut finished = Vec::new();
        for session in &mut self.sessions {
            let Some(busy_text) = self.config.agent(&session.agent).and_then(|a| a.busy_text.clone()) else { continue };
            let Some(terminal) = &session.terminal else { continue };
            if session.hooks_active || terminal.exited() || session.status == AgentStatus::NeedsInput {
                continue;
            }
            let busy = terminal.screen_text(12).contains(&busy_text);
            session.status = match (busy, session.status) {
                (true, _) => AgentStatus::Working,
                (false, AgentStatus::Working) => {
                    finished.push(session.root());
                    AgentStatus::Done
                }
                (false, AgentStatus::Starting) => AgentStatus::Idle,
                (false, other) => other,
            };
        }
        for root in finished {
            self.diffs.entry(root).or_default().mark_dirty();
        }
    }

    pub(super) fn close(&mut self, id: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            let mut session = self.sessions.remove(i);
            if session.chief {
                self.hub.lock().unwrap().chief_running = false;
            }
            if let Some(reply) = session.pending.as_mut().and_then(|pending| pending.reply.take()) {
                let _ =
                    reply.send(ControlOutcome::Done(ControlResponse::error("message not delivered: session closed")));
            }
            if session.terminal.is_none()
                && let Some(cid) = session.claude_id
            {
                self.dismissed.insert(cid);
            }
        }
        if self.active.as_deref() == Some(id) {
            self.active = self.sessions.first().map(|s| s.id.clone());
        }
    }

    pub(super) fn remember(&mut self, id: &str) {
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else { return };
        let Some(claude_id) = s.claude_id.clone() else { return };
        self.store.upsert(SessionRecord {
            claude_session_id: claude_id,
            agent: s.agent.clone(),
            cwd: s.cwd.clone(),
            title: s.title.clone(),
            last_prompt: s.last_prompt.clone(),
            updated: 0,
        });
        if let Err(err) = self.store.save(&paths::store_file()) {
            eprintln!("IMACS: saving sessions failed: {err:#}");
        }
    }

    /// Sends text to a session: typed into IMACS's own terminals, queued for the
    /// next hook of sessions running elsewhere.
    pub(super) fn deliver(&mut self, index: usize, text: &str) -> Delivery {
        let session = &mut self.sessions[index];
        if session.approval_hold
            || !session.approvals.is_empty()
            || session.pending.as_ref().is_some_and(|pending| pending.codex)
        {
            session.activity.held_since.get_or_insert_with(imacs_core::store::now);
            session.held.push(text.to_string());
            return Delivery::Held;
        }
        if let Some(terminal) = &session.terminal {
            if terminal.exited() {
                return Delivery::Unavailable;
            }
            let codex = self.config.agent(&session.agent).is_some_and(|agent| agent.is_codex());
            let screen_before = if codex { terminal.screen_text(200) } else { String::new() };
            terminal.submit(text);
            let trackable = session.kind == AgentKind::Claude
                || codex
                || self.config.agent(&session.agent).is_some_and(|a| a.busy_text.is_some());
            if trackable {
                let was_working = session.status == AgentStatus::Working;
                session.pending = Some(Pending {
                    text: text.to_string(),
                    sent: Instant::now(),
                    attempts: 1,
                    was_working,
                    codex,
                    screen_before,
                    paste_seen: false,
                    reply: None,
                });
            } else if session.is_agent() {
                session.status = AgentStatus::Working;
            }
            return if codex { Delivery::Pending } else { Delivery::Sent };
        }
        let Some(claude_id) = session.claude_id.clone() else { return Delivery::Unavailable };
        self.hub.lock().unwrap().outbox.entry(claude_id).or_default().push(text.to_string());
        Delivery::Queued
    }

    fn steer_targets(&self) -> Vec<usize> {
        let indices = 0..self.sessions.len();
        match self.steer_target {
            SteerTarget::Active => self.active_index().into_iter().collect::<Vec<usize>>(),
            SteerTarget::Agents => {
                indices.filter(|&i| self.sessions[i].is_agent() && !self.sessions[i].chief).collect::<Vec<usize>>()
            }
            SteerTarget::Selected => indices.filter(|&i| self.sessions[i].selected).collect::<Vec<usize>>(),
        }
        .into_iter()
        .filter(|&i| self.sessions[i].can_receive())
        .collect()
    }

    pub(super) fn send_steer(&mut self) {
        let text = self.steer_text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let targets = self.steer_targets();
        if targets.is_empty() {
            self.toast("No session to send to");
            return;
        }
        let mut typed = 0;
        let mut queued = 0;
        let mut held = 0;
        let mut verifying = 0;
        for i in targets {
            match self.deliver(i, &text) {
                Delivery::Sent => typed += 1,
                Delivery::Pending => verifying += 1,
                Delivery::Queued => queued += 1,
                Delivery::Held => held += 1,
                Delivery::Unavailable => {}
            }
        }
        self.steer_text.clear();
        let mut parts = Vec::new();
        if verifying > 0 {
            parts.push(format!("Verifying delivery to {verifying} session(s)"));
        }
        if typed > 0 {
            parts.push(format!("Sent to {typed} session{}", if typed == 1 { "" } else { "s" }));
        }
        if queued > 0 {
            parts.push(format!("Queued for {queued} outside IMACS"));
        }
        if held > 0 {
            parts.push(format!("Held for {held} until the session is ready"));
        }
        let summary = if parts.is_empty() { "No session could receive the message".into() } else { parts.join(", ") };
        self.toast(summary);
    }

    pub(super) fn send_review(&mut self, root: &Path) -> Delivery {
        let comments = self.diffs.get(root).map(|s| s.comments.clone()).unwrap_or_default();
        if comments.is_empty() {
            return Delivery::Unavailable;
        }
        let header = self.config.fill(&self.config.steer.review_header);
        let message = steer::compose_review(&header, &comments);
        let target = self
            .active_index()
            .filter(|&i| self.sessions[i].root() == root)
            .or_else(|| self.sessions.iter().position(|s| s.root() == root && s.is_agent() && s.can_receive()));
        let Some(index) = target else {
            self.toast("No agent is working in this worktree");
            return Delivery::Unavailable;
        };
        let count = comments.len();
        let delivery = self.deliver(index, &message);
        if delivery != Delivery::Unavailable {
            if let Some(state) = self.diffs.get_mut(root) {
                state.comments.clear();
            }
            let title = self.sessions[index].title.clone();
            let action = match delivery {
                Delivery::Held => "Held",
                Delivery::Pending => "Sending",
                _ => "Sent",
            };
            self.toast(format!("{action} {count} comment{} to {title}", if count == 1 { "" } else { "s" }));
        }
        delivery
    }
}

/// The last prompt belongs to the composer, unless a permission menu is open.
fn codex_composer(screen: &str) -> Option<&str> {
    if imacs_core::approvals::menu_visible(screen) {
        return None;
    }
    screen.lines().rev().find_map(|line| line.trim_start().strip_prefix(['›', '❯']).map(str::trim))
}

/// Empty composers contain a rotating placeholder. Compare with the line captured
/// before this paste instead of mistaking that placeholder for unsent text.
fn codex_composer_empty(screen: &str, before: &str) -> Option<bool> {
    Some(codex_composer(screen)? == codex_composer(before)?)
}

/// A first or last message line newly visible above the composer is a receipt.
/// Count occurrences so sending the same message again needs new evidence too.
fn codex_transcript_receipt(screen: &str, before: &str, text: &str) -> bool {
    let transcript = |screen: &str| {
        let lines: Vec<_> = screen.lines().collect();
        let end = lines.iter().rposition(|line| line.trim_start().starts_with(['›', '❯'])).unwrap_or(0);
        lines[..end].join("\n")
    };
    if imacs_core::approvals::menu_visible(screen) {
        return false;
    }
    let current = transcript(screen);
    let previous = transcript(before);
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let current_compact = compact(&current);
    let previous_compact = compact(&previous);
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let Some(first) = lines.next() else { return false };
    [first, lines.next_back().unwrap_or(first)].into_iter().any(|line| {
        let message = compact(line);
        if message.chars().count() >= 20
            && current_compact.matches(&message).count() > previous_compact.matches(&message).count()
        {
            return true;
        }
        // Codex clips queued messages to a bullet and an ellipsis. Only accept a
        // newly visible preview with a substantial matching prefix.
        current.lines().any(|preview| {
            let Some(prefix) = preview.trim().strip_prefix('•').and_then(|s| s.trim().strip_suffix('…')) else {
                return false;
            };
            let prefix = compact(prefix);
            prefix.chars().count() >= 20
                && message.starts_with(&prefix)
                && current.lines().filter(|line| *line == preview).count()
                    > previous.lines().filter(|line| *line == preview).count()
        })
    })
}

/// Longest first prompt passed on the command line to agents other than Claude.
const LONG_PROMPT: usize = 700;

/// Writes a long prompt to a brief file and returns the short prompt that points to it.
fn brief_file(session: &str, text: &str) -> Result<String, String> {
    let dir = paths::briefs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let path = dir.join(format!("{session}.md"));
    std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(format!("Your task brief is in {}. Read it first and follow it exactly.", path.display()))
}

/// RUSTC_WRAPPER for sccache when it is installed and neither the user nor the project under
/// `cwd` chose a wrapper (an exported variable would override their cargo config).
fn compile_cache_env(cwd: &Path) -> Vec<(String, String)> {
    let user_config = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| paths::home().join(".cargo"));
    let configs = cwd.ancestors().map(|dir| dir.join(".cargo")).chain([user_config]);
    let wrapper_set = ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"].iter().any(|var| std::env::var_os(var).is_some())
        || configs.into_iter().any(|dir| {
            ["config.toml", "config"]
                .iter()
                .any(|name| std::fs::read_to_string(dir.join(name)).is_ok_and(|text| text.contains("rustc-wrapper")))
        });
    let sccache = resolve_program("sccache");
    if wrapper_set || !Path::new(&sccache).is_absolute() {
        return Vec::new();
    }
    vec![("RUSTC_WRAPPER".into(), sccache), ("SCCACHE_IGNORE_SERVER_IO_ERROR".into(), "1".into())]
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn short_messages_cannot_be_confirmed_by_agent_output() {
        let before = "Working\n› Ask Codex to do anything";
        for (message, output) in
            [("ok", "I looked at the code"), ("yes", "yes, I will check"), ("continue", "I will continue now")]
        {
            let screen = format!("• {output}\n› Ask Codex to do anything");
            assert!(!codex_transcript_receipt(&screen, before, message), "{message}: {output}");
        }
    }

    #[test]
    fn codex_receipts_require_new_evidence_outside_the_composer() {
        let before = "Working\n› Ask Codex to do anything";
        assert_eq!(codex_composer_empty(before, before), Some(true));
        let other = "Working\n› Explain this codebase";
        assert_eq!(codex_composer_empty(other, other), Some(true));
        assert_eq!(codex_composer_empty("Working\n› unsent input", before), Some(false));
        assert_eq!(codex_composer_empty("Working", before), None);
        let text = "First line of the message\nMiddle line\nLast line of the message";
        assert!(!codex_transcript_receipt(&format!("Working\n› {text}"), before, text));
        assert!(!codex_transcript_receipt(before, before, text));
        for line in ["First line of the message", "Last line of the message"] {
            let receipt = format!("• {line}\n› Ask Codex to do anything");
            assert!(codex_transcript_receipt(&receipt, before, text));
            assert!(!codex_transcript_receipt(&receipt, &receipt, text));
            let repeated = format!("• {line}\n{receipt}");
            assert!(codex_transcript_receipt(&repeated, &receipt, text));
        }
        let preview = "• First line of the messag…\n› Ask Codex to do anything";
        assert!(codex_transcript_receipt(preview, before, text));
        assert!(!codex_transcript_receipt(preview, preview, text));
    }

    #[test]
    fn agents_share_the_job_pool_at_lower_priority() {
        with_app("app::sessions::tests::agents_share_the_job_pool_at_lower_priority", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            let out = dir.path().join("probe");
            let script = format!("ps -o nice= -p $$ > {0}.nice; env > {0}.tmp; mv {0}.tmp {0}; sleep 5", out.display());
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "probe".into(),
                command: vec!["/bin/sh".into(), "-c".into(), script],
                kind: AgentKind::Generic,
                busy_text: None,
            });
            app.config.resources.agent_nice = 7;
            // SAFETY: querying this process's priority has no preconditions.
            let inherited_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
            app.start(ctx, "probe", dir.path().into(), None, vec![], None).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !out.exists() {
                assert!(Instant::now() < deadline, "the probe agent never wrote its environment");
                std::thread::sleep(Duration::from_millis(20));
            }
            let env = std::fs::read_to_string(&out).unwrap();
            let pool = format!("-j --jobserver-auth=fifo:{}", Jobserver::path_for(&paths::imacs_home()).display());
            assert!(env.lines().any(|line| line == format!("CARGO_MAKEFLAGS={pool}")), "{env}");
            // MAKEFLAGS may be inherited from whatever started the tests; this pool must never be in it.
            let pool_path = Jobserver::path_for(&paths::imacs_home()).display().to_string();
            assert!(
                !env.lines().any(|line| line.starts_with("MAKEFLAGS=") && line.contains(&pool_path)),
                "plain make must not see the pool: {env}"
            );
            let exe_dir = std::env::current_exe().unwrap().parent().unwrap().display().to_string();
            assert!(env.lines().any(|line| line.starts_with(&format!("PATH={exe_dir}:"))), "{env}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("probe.nice")).unwrap().trim(),
                (inherited_nice + 7).min(19).to_string()
            );
            app.sessions.clear();
        });
    }

    #[test]
    fn dotnet_agents_get_the_budget_in_their_terminal() {
        with_app("app::sessions::tests::dotnet_agents_get_the_budget_in_their_terminal", |app, ctx| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/Example.csproj"), "").unwrap();
            let out = dir.path().join("env");
            app.config.resources.dotnet.processor_count = Some(2);
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "dotnet-probe".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("env > {0}.tmp; mv {0}.tmp {0}; sleep 5", out.display()),
                ],
                kind: AgentKind::Generic,
                busy_text: None,
            });
            app.start(ctx, "dotnet-probe", dir.path().into(), None, vec![], None).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while !out.exists() {
                assert!(Instant::now() < deadline, "the .NET probe never wrote its environment");
                std::thread::sleep(Duration::from_millis(20));
            }
            let env = std::fs::read_to_string(out).unwrap();
            for (key, value) in app.config.resources.env(imacs_core::governor::Toolchain::Dotnet, app.governor.cores) {
                assert!(env.lines().any(|line| line == format!("{key}={value}")), "{env}");
            }
            app.sessions.clear();
        });
    }

    #[test]
    fn codex_version_is_cached_per_program_without_blocking_launch() {
        with_app("app::sessions::tests::codex_version_is_cached_per_program_without_blocking_launch", |app, ctx| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let script = r#"#!/bin/sh
dir=${0%/*}
if [ "$1" = --version ]; then
  printf 'checked\n' >> "$dir/checks"
  attempts=0
  while [ ! -e "$dir/ready" ]; do
    attempts=$((attempts + 1))
    [ "$attempts" -lt 1000 ] || exit 1
    sleep 0.01
  done
  printf 'codex-cli 0.165.0\n'
else
  printf '%s\n' "$@" > "$dir/launch-args"
fi
"#;
            for name in ["one", "two"] {
                let folder = dir.path().join(name);
                std::fs::create_dir(&folder).unwrap();
                let program = folder.join("codex");
                std::fs::write(
                    &program,
                    if name == "two" { script.replace("0.165.0", "0.164.0") } else { script.to_string() },
                )
                .unwrap();
                std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: name.into(),
                    command: vec![program.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                });
            }
            let probe_dir = dir.path().to_path_buf();
            let release = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while ["one", "two"].iter().any(|name| !probe_dir.join(name).join("checks").exists()) {
                    assert!(Instant::now() < deadline, "both asynchronous version probes must start");
                    std::thread::sleep(Duration::from_millis(10));
                }
                std::thread::sleep(Duration::from_millis(3200));
                for name in ["one", "two"] {
                    std::fs::write(probe_dir.join(name).join("ready"), "").unwrap();
                }
            });
            let program = |name: &str| dir.path().join(name).join("codex").to_string_lossy().into_owned();
            let one = app.codex_version(ctx, program("one"));
            assert!(Arc::ptr_eq(&one, &app.codex_version(ctx, program("one"))));
            let two = app.codex_version(ctx, program("two"));
            release.join().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while one.get().is_none() || two.get().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(one.get(), Some(&Some(true)));
            assert_eq!(two.get(), Some(&Some(false)));
            assert_eq!(app.codex_versions.len(), 2);
            // A gated probe must finish before start() runs git: concurrent children can
            // inherit pipe writers on macOS and otherwise keep git's output() waiting for EOF.
            let mut ids = Vec::new();
            for name in ["one", "one", "two"] {
                ids.push(app.start(ctx, name, dir.path().into(), None, vec![], None).unwrap());
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while ids.iter().any(|id| !app.sessions[app.find(id).unwrap()].terminal.as_ref().unwrap().exited()) {
                assert!(Instant::now() < deadline, "finite fake agents must exit before teardown");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                std::fs::read_to_string(dir.path().join("one/launch-args"))
                    .unwrap()
                    .contains("hooks.PermissionRequest")
            );
            assert!(
                !std::fs::read_to_string(dir.path().join("two/launch-args"))
                    .unwrap()
                    .contains("hooks.PermissionRequest")
            );
            assert!(Arc::ptr_eq(&one, &app.codex_version(ctx, program("one"))));
            for name in ["one", "two"] {
                assert_eq!(std::fs::read_to_string(dir.path().join(name).join("checks")).unwrap().lines().count(), 1);
            }
            app.sessions.clear();
        });
    }

    #[test]
    fn failed_codex_probes_are_retried_and_recovered_launches_get_hooks() {
        with_app(
            "app::sessions::tests::failed_codex_probes_are_retried_and_recovered_launches_get_hooks",
            |app, ctx| {
                use std::os::unix::fs::PermissionsExt;
                let dir = tempfile::tempdir().unwrap();
                let program = dir.path().join("codex");
                let name = program.to_string_lossy().into_owned();
                app.config.agents.push(imacs_core::config::AgentConfig {
                    name: "flaky".into(),
                    command: vec![name.clone()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                });
                for script in ["#!/bin/sh\nexec sleep 30\n", "#!/bin/sh\nexit 1\n"] {
                    std::fs::write(&program, script).unwrap();
                    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
                    let failed = Arc::new(OnceLock::new());
                    failed.set(crate::setup::codex_supports_hooks(&name, Duration::from_millis(100))).unwrap();
                    assert_eq!(failed.get(), Some(&None));
                    app.codex_versions.insert(name.clone(), failed.clone());
                    std::fs::write(
                        &program,
                        r#"#!/bin/sh
if [ "$1" = --version ]; then
  printf 'codex-cli 0.165.0\n'
else
  printf '%s\n' "$@" > launch-args
fi
"#,
                    )
                    .unwrap();
                    let id = app.start(ctx, "flaky", dir.path().into(), None, vec![], None).unwrap();
                    let retried = app.codex_versions[&name].clone();
                    assert!(!Arc::ptr_eq(&failed, &retried), "failed probes must not be reused as old versions");
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while app.sessions[app.find(&id).unwrap()]
                        .terminal
                        .as_ref()
                        .is_none_or(|terminal| !terminal.exited())
                    {
                        assert!(Instant::now() < deadline, "recovered fake agent must launch and exit");
                        app.runtime_tick(ctx);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let args = std::fs::read_to_string(dir.path().join("launch-args")).unwrap();
                    assert!(args.contains("--no-daemon"));
                    assert!(args.contains("hooks.PermissionRequest"));
                    assert_eq!(retried.get(), Some(&Some(true)));
                    assert!(Arc::ptr_eq(&retried, &app.codex_version(ctx, name.clone())));
                }
            },
        );
    }

    #[test]
    fn busy_codex_sends_wait_for_receipts_retry_and_report_failure() {
        with_app("app::sessions::tests::busy_codex_sends_wait_for_receipts_retry_and_report_failure", |app, ctx| {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
            });
            let text = "Please check the first change and then the second change";
            for mode in ["queue", "retry", "drop", "drop-paste", "drop-empty", "exit", "hook"] {
                let terminal = Terminal::spawn(
                    app.wake.clone(),
                    app.palette.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), mode.into()],
                    &app.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.sessions = vec![session(mode, AgentKind::Generic, AgentStatus::Working, &app.launch_dir)];
                app.sessions[0].agent = "fake-codex".into();
                app.sessions[0].terminal = Some(terminal);
                let (tx, rx) = channel();
                app.control(ctx, ControlRequest::Send { session: mode.into(), text: text.into(), raw: false }, tx);
                assert!(rx.try_recv().is_err(), "an unverified send must not claim success");
                let held = request(app, json!({"cmd":"send", "session":mode, "text":"second message"}));
                assert!(held.ok && held.result["held"] == true, "another send must wait for the pending receipt");
                assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, text);
                // Let the initial paste and its delayed Enter reach the fake busy terminal.
                std::thread::sleep(Duration::from_millis(250));
                app.sessions[0].status = AgentStatus::Done;
                app.verify_deliveries();
                if mode != "exit" {
                    assert!(rx.try_recv().is_err(), "composer text or a shared prefix is not a receipt");
                }
                if mode == "hook" {
                    let event = imacs_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":"UserPromptSubmit", "session_id":mode, "prompt":text,
                    }))
                    .unwrap();
                    app.on_hook(HookMessage {
                        pane: Some(mode.into()),
                        ancestors: vec![],
                        event,
                        delivered: 0,
                        approval_reply: None,
                    });
                }
                let mut response = None;
                for _ in 0..DELIVERY_ATTEMPTS {
                    if let Some(pending) = &mut app.sessions[0].pending {
                        pending.sent = Instant::now() - DELIVERY_TIMEOUT;
                    }
                    app.verify_deliveries();
                    std::thread::sleep(Duration::from_millis(200));
                    app.verify_deliveries();
                    if let Ok(ControlOutcome::Done(result)) = rx.try_recv() {
                        response = Some(result);
                        break;
                    }
                }
                let response = response.expect("every send must receive a delivery result");
                assert_eq!(response.ok, matches!(mode, "queue" | "retry" | "hook"), "{mode}: {response:?}");
                if response.ok {
                    assert_eq!(response.result["sent"], true);
                    assert!(
                        mode == "hook"
                            || steer::message_in_transcript(
                                &app.sessions[0].terminal.as_ref().unwrap().screen_text(60),
                                text
                            )
                    );
                } else {
                    assert!(response.error.unwrap().contains("unconfirmed"));
                }
                if matches!(mode, "drop-paste" | "drop-empty") {
                    let screen = app.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                    assert!(screen.contains("Pastes: 1"), "{screen}");
                    assert!(screen.contains("Enters: 1"), "{screen}");
                    assert!(screen.contains("Tabs: 0"), "{screen}");
                    assert_eq!(response.result.get("sent"), None);
                }
                if mode == "hook" {
                    assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "second message");
                    assert!(app.sessions[0].held.is_empty());
                } else {
                    assert!(app.sessions[0].pending.is_none());
                }
                app.sessions.clear();
            }
        });
    }

    #[test]
    fn codex_clipped_and_repeated_receipts_never_repaste() {
        with_app("app::sessions::tests::codex_clipped_and_repeated_receipts_never_repaste", |app, ctx| {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
            });
            for mode in ["long", "preview", "repeat", "marker", "collapsed", "no-prompt"] {
                let text = if mode == "long" {
                    "A long multiline message that cannot fit on screen.\n".repeat(100) + "The final line is visible."
                } else {
                    "Please check the first change and then the second change".into()
                };
                let terminal = Terminal::spawn(
                    app.wake.clone(),
                    app.palette.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), mode.into()],
                    &app.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.sessions = vec![session(mode, AgentKind::Generic, AgentStatus::Working, &app.launch_dir)];
                app.sessions[0].agent = "fake-codex".into();
                app.sessions[0].terminal = Some(terminal);
                let (tx, rx) = channel();
                app.control(ctx, ControlRequest::Send { session: mode.into(), text, raw: false }, tx);
                let mut paste_seen = false;
                while !app.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Enters: 1") {
                    assert!(Instant::now() < deadline, "{mode}: the original paste and Enter must reach the fixture");
                    app.verify_deliveries();
                    paste_seen |= app.sessions[0].pending.as_ref().is_some_and(|pending| pending.paste_seen);
                    std::thread::sleep(Duration::from_millis(10));
                }
                if mode == "marker" {
                    assert!(paste_seen, "the marker must be observed");
                }
                let mut response = None;
                for _ in 0..=DELIVERY_ATTEMPTS {
                    if let Some(pending) = &mut app.sessions[0].pending {
                        pending.sent = Instant::now() - DELIVERY_TIMEOUT;
                    }
                    app.verify_deliveries();
                    if let Ok(ControlOutcome::Done(result)) = rx.try_recv() {
                        response = Some(result);
                        break;
                    }
                    if mode == "collapsed" {
                        while !app.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Queued") {
                            assert!(Instant::now() < deadline, "{mode}: the retry must submit with Tab");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
                let response = response.expect("a send must eventually resolve");
                assert_eq!(response.ok, mode != "no-prompt", "{mode}: {response:?}");
                // A missing prompt cannot confirm delivery, but must never cause
                // a second paste after the original text has left the composer.
                let screen = app.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                assert!(screen.contains("Pastes: 1"), "{mode}: {screen}");
                assert!(screen.contains("Enters: 1"), "{mode}: {screen}");
                assert!(screen.contains(if mode == "collapsed" { "Tabs: 1" } else { "Tabs: 0" }), "{mode}: {screen}");
                assert!(rx.try_recv().is_err(), "{mode}: a send resolves only once");
                app.sessions.clear();
            }
        });
    }

    #[test]
    fn steering_during_a_pending_codex_send_is_held_and_released() {
        with_app("app::sessions::tests::steering_during_a_pending_codex_send_is_held_and_released", |app, _ctx| {
            let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex");
            app.config.agents.push(imacs_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
            });
            app.sessions = vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.launch_dir)];
            app.sessions[0].agent = "fake-codex".into();
            app.sessions[0].terminal = Some(
                Terminal::spawn(
                    app.wake.clone(),
                    app.palette.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec![],
                    &app.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
            assert_eq!(app.deliver(0, "first"), Delivery::Pending);
            app.active = Some("worker".into());
            app.steer_target = SteerTarget::Active;
            app.steer_text = "second".into();
            app.send_steer();
            app.steer_text = "third".into();
            app.send_steer();
            assert!(app.steer_text.is_empty());
            assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "first");
            assert_eq!(app.sessions[0].held, ["second", "third"]);
            assert!(app.sessions[0].activity.held_since.is_some());
            app.release_held(0);
            assert_eq!(app.sessions[0].held.len(), 2);
            let event = imacs_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"UserPromptSubmit", "session_id":"worker", "prompt":"first"
            }))
            .unwrap();
            app.on_hook(HookMessage {
                pane: Some("worker".into()),
                ancestors: vec![],
                event,
                delivered: 0,
                approval_reply: None,
            });
            app.release_held(0);
            assert!(app.sessions[0].held.is_empty());
            assert!(app.sessions[0].activity.held_since.is_none());
            assert_eq!(app.sessions[0].pending.as_ref().unwrap().text, "second\n\nthird");
            app.sessions.clear();
        });
    }

    #[test]
    fn session_start_does_not_wait_for_git() {
        if std::env::var_os("IMACS_START_GIT_TEST").is_some() {
            let dir = std::env::current_dir().unwrap();
            let agent = imacs_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Shell,
                busy_text: None,
            };
            let session = Session::starting("start".into(), &agent, dir.clone(), false, &crate::host::Wake::default());
            assert!(!dir.join("timed-out").exists(), "session construction waited for git on the UI thread");
            assert!(session.worktree.is_none());
            std::fs::write(dir.join("ready"), "").unwrap();
            let worktree = session.worktree_ready.unwrap().recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(worktree.branch.as_deref(), Some("feature"));
            assert_eq!(worktree.root, dir);
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("git");
        std::fs::write(
            &git,
            r#"#!/bin/sh
attempts=0
while [ ! -e ready ]; do
  attempts=$((attempts + 1))
  if [ "$attempts" -gt 200 ]; then
    /usr/bin/touch timed-out
    exit 1
  fi
  /bin/sleep 0.01
done
case "$5" in
  rev-parse) pwd ;;
  symbolic-ref) printf 'feature\n' ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app::sessions::tests::session_start_does_not_wait_for_git", "--nocapture"])
                .env("IMACS_START_GIT_TEST", "1")
                .env("PATH", dir.path())
                .env("HOME", dir.path())
                .env("IMACS_HOME", dir.path().join("state"))
                .env("IMACS_CONFIG", dir.path().join("config.toml"))
                .env("IMACS_SOCKET", dir.path().join("imacs.sock"))
                .env_remove("IMACS_PANE")
                .current_dir(dir.path()),
        );
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
