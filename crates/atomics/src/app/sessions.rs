use super::*;
use atomics_core::approvals::{FOLDER_ACCESS_HINT, codex_folder_access_visible, codex_restricted_folder};
use atomics_core::isolation::{self, IsolationMode};

pub(super) const CODEX_OPENED_RESTRICTED: &str = "codex_opened_restricted";
const CODEX_CHIEF_BYPASS: &str = "--dangerously-bypass-approvals-and-sandbox";
pub(super) const CODEX_DELIVERY_WARNING: &str = "Could not confirm message delivery. Check the chief's terminal.";

impl Session {
    pub(super) fn starting(
        id: String,
        agent: &atomics_core::config::AgentConfig,
        cwd: PathBuf,
        chief: bool,
        managed_worktree: Option<bool>,
        wake: &crate::host::Wake,
        input_access: InputAccess,
    ) -> Self {
        let (tx, rx) = wake.channel();
        let dir = cwd.clone();
        std::thread::spawn(move || {
            let _ = tx.send(git::worktree_of(&dir));
        });
        let scratch = if chief { None } else { paths::scratch_of(&cwd) };
        let (title, naming) = match (chief, &scratch) {
            (true, _) => ("chief".into(), Naming::Named),
            (false, Some(_)) => (UNTITLED.into(), Naming::FirstPrompt),
            (false, None) => (title_for(None, &cwd), Naming::Folder),
        };
        Self {
            id,
            agent: agent.name.clone(),
            kind: agent.kind,
            input_access,
            terminal: None,
            claude_id: None,
            cwd,
            worktree: None,
            worktree_ready: Some(rx),
            title,
            status: AgentStatus::Starting,
            last_prompt: None,
            simple_turn_plan: None,
            simple_chat: None,
            simple_context_for: None,
            last_message: None,
            hooks_active: false,
            first_run_hint: None,
            activity: heartbeat::Activity::default(),
            work: Default::default(),
            live_read: None,
            approvals: Vec::new(),
            allowed: Vec::new(),
            held: Vec::new(),
            approval_hold: false,
            menu_absent_since: None,
            answered_menu: None,
            chief,
            chief_selection: None,
            chief_model_label: None,
            chief_send_masks: Default::default(),
            managed_worktree,
            restricted_entry: None,
            pending: None,
            sandbox: None,
            cache_env: Vec::new(),
            relay: None,
            screen_busy: false,
            busy_absent_since: None,
            ref_guard: None,
            scratch,
            naming,
            mcp_needed: Vec::new(),
            warning: None,
            unsaved: None,
            send_now: None,
            held_now: false,
            resumed_at: None,
        }
    }

    /// Names a scratch session after its first prompt, once.
    pub(super) fn first_prompt(&mut self, prompt: &str) {
        if self.naming == Naming::FirstPrompt
            && let Some(title) = prompt_title(prompt)
        {
            self.title = title;
            self.naming = Naming::Named;
        }
    }

    /// The title of a session named after its folder, once the folder or branch changes.
    pub(super) fn follow_folder(&mut self) {
        if self.naming == Naming::Folder {
            self.title = title_for(self.branch(), &self.cwd);
        }
    }

    pub(super) fn rename(&mut self, title: &str) {
        let title = title.trim();
        if !title.is_empty() {
            self.title = title.to_string();
            self.naming = Naming::Named;
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

    /// It ran here and stopped without a terminal to show: a resume or launch that failed before its agent ran.
    pub(super) fn stopped_here(&self) -> bool {
        self.terminal.is_none() && self.status == AgentStatus::Exited && !self.id.starts_with(EXTERNAL)
    }

    pub(super) fn current_chief(&self) -> bool {
        self.chief && (self.status == AgentStatus::Starting || self.can_receive())
    }

    pub(super) fn send_masks_ready(&self, config: Option<&atomics_core::tickets::TicketConfig>) -> bool {
        match &self.chief_send_masks {
            ChiefSendMasks::Unchecked => config.is_none(),
            ChiefSendMasks::Reading | ChiefSendMasks::Failed => false,
            ChiefSendMasks::Ready(verified) => config.is_none_or(|current| current == verified.as_ref()),
        }
    }

    pub(super) fn can_receive(&self) -> bool {
        match &self.terminal {
            Some(t) => !t.exited(),
            None => self.claude_id.is_some() && self.status != AgentStatus::Exited,
        }
    }
}

/// The id prefix of a session ATOMICS follows by its hooks but did not start: nothing of it is resumed here.
pub(super) const EXTERNAL: &str = "ext:";

/// "Bare" also describes agents inside the ATOMICS VM and the Mac bundle's
/// wrappers. Only a host-owned guest marker or the installed wrapper path
/// identifies those runtimes. Agent names and provider kinds do not.
pub(super) fn bare_remote_agent(agent: &atomics_core::config::AgentConfig) -> bool {
    crate::vm::is_guest()
        || agent.command.first().is_some_and(|program| crate::vm::is_agent_wrapper(Path::new(program)))
}

pub(super) fn bare_input_access(agent: &atomics_core::config::AgentConfig, wrappers: &[PathBuf]) -> InputAccess {
    if bare_remote_agent(agent) {
        return InputAccess::Controller;
    }
    let Some(program) = agent.command.first().filter(|_| agent.kind != AgentKind::Shell) else {
        return InputAccess::Desktop;
    };
    let command = Path::new(program);
    let declared = command.is_absolute() && wrappers.iter().any(|path| path == command);
    let path = PathBuf::from(resolve_program(program));
    if !path.is_absolute() {
        return InputAccess::Desktop;
    }
    let Ok(target) = path.canonicalize() else { return InputAccess::Desktop };
    // An arbitrary symlink may later point somewhere else. Trust the installed
    // path and its resolved executable, or a wrapper declared by the host.
    let recognized = ["claude", "codex"].into_iter().any(|name| {
        crate::setup::find_program(name).is_some_and(|installed| {
            installed.is_absolute()
                && (path == installed || (path == target && installed.canonicalize().is_ok_and(|p| p == target)))
        })
    });
    let interactive = |path: &Path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(
                "env"
                    | "sh"
                    | "bash"
                    | "dash"
                    | "zsh"
                    | "fish"
                    | "ksh"
                    | "csh"
                    | "tcsh"
                    | "nu"
                    | "pwsh"
                    | "hx"
                    | "helix"
                    | "vim"
                    | "nvim"
                    | "vi"
                    | "emacs"
                    | "nano"
            )
        )
    };
    if !(recognized || declared) || interactive(&path) || interactive(&target) {
        InputAccess::Desktop
    } else {
        InputAccess::Host
    }
}

impl State {
    pub(super) fn bare_input_access(&self, agent: &atomics_core::config::AgentConfig) -> InputAccess {
        bare_input_access(agent, &self.config.remote.host_input_wrappers)
    }

    pub(super) fn chief_agent(&self, provider: ChiefAgent) -> atomics_core::config::AgentConfig {
        atomics_core::config::AgentConfig {
            name: if provider == ChiefAgent::Claude { "chief" } else { provider.name() }.into(),
            command: self.chief_command(provider),
            kind: if provider == ChiefAgent::Claude { AgentKind::Claude } else { AgentKind::Generic },
            busy_text: None,
            isolation: None,
        }
    }

    // ── sessions ──────────────────────────────────────────────────────────

    pub(super) fn spawn(&mut self, agent_name: &str, cwd: PathBuf, resume: Option<String>) {
        if let Err(err) = self.start(agent_name, cwd, resume, Vec::new(), None) {
            self.toast(err);
        }
    }

    /// Starts an agent in a terminal and returns its session id.
    pub(super) fn start(
        &mut self,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
    ) -> Result<String, String> {
        self.start_as(agent_name, cwd, resume, extra_args, prompt, Role::Agent(None, ControlOrigin::Local))
    }

    /// Starts `agent_name` outside any project, in a new scratch folder of its own. It takes the
    /// launch every agent takes, so it gets the isolation, credentials and MCP servers they get.
    pub(super) fn start_scratch(
        &mut self,
        agent_name: &str,
        args: Vec<String>,
        prompt: Option<String>,
        role: Role<'_>,
    ) -> Result<String, String> {
        let folder = new_scratch_folder(&paths::scratch_dir())
            .map_err(|err| format!("Could not make a scratch folder in {}: {err}", paths::scratch_dir().display()))?;
        // A repository of its own, without commits, so the diff panel shows everything written there.
        if let Err(err) = git::git(&folder, &["init", "-q"]) {
            eprintln!("ATOMICS: git init in {} failed: {err:#}", folder.display());
        }
        let started = self.start_as(agent_name, folder.clone(), None, args, prompt, role);
        if started.is_err() {
            let _ = std::fs::remove_dir_all(&folder);
        }
        started
    }

    /// Starts an agent as `role`. A prompt names a scratch session.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_as(
        &mut self,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
        role: Role<'_>,
    ) -> Result<String, String> {
        let first = prompt.clone();
        let id = self.launch_as(agent_name, cwd, resume, extra_args, prompt, role)?;
        if let Some(prompt) = first
            && let Some(session) = self.sessions.iter_mut().find(|s| s.id == id)
        {
            session.first_prompt(&prompt);
        }
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_as(
        &mut self,
        agent_name: &str,
        cwd: PathBuf,
        resume: Option<String>,
        extra_args: Vec<String>,
        prompt: Option<String>,
        role: Role<'_>,
    ) -> Result<String, String> {
        if self.governor.admissions_paused() {
            return Err("The host is preparing to stop".into());
        }
        let chief = matches!(role, Role::Chief(_) | Role::ChiefRestart(_) | Role::ChiefHandoff { .. });
        self.session_disk_guard(
            &cwd,
            chief,
            resume.is_some(),
            self.config.agent(agent_name).is_some_and(|agent| agent.kind == AgentKind::Shell),
        )?;
        self.cancel_cache_cleanup(&cwd);
        let interactive = std::mem::take(&mut self.interactive_start);
        let chief_masks_from =
            chief.then(|| self.ticket_config().filter(|config| config.server.uses_oauth())).flatten();
        let replacing = self.next_session_id.as_deref().is_some_and(|id| self.is_chief_replacement(id));
        if chief && !replacing && (self.chief_switch.is_some() || self.sessions.iter().any(|s| s.chief)) {
            return Err("A chief session already exists".into());
        }
        if !chief && let Some(error) = &self.tickets.workspace_identity_error {
            return Err(error.clone());
        }
        let agent = if chief {
            let provider = if agent_name == "chief" {
                self.preferred_chief().agent
            } else {
                ChiefAgent::ALL
                    .into_iter()
                    .find(|provider| provider.name() == agent_name)
                    .ok_or_else(|| format!("Unknown chief provider {agent_name}"))?
            };
            if provider == ChiefAgent::Opencode {
                return Err(ChiefAgent::OPENCODE_UNAVAILABLE.into());
            }
            let agent = self.chief_agent(provider);
            if provider == ChiefAgent::Codex {
                let (program, args) = agent.command.split_first().ok_or("The Codex chief command is empty")?;
                if program.trim().is_empty() {
                    return Err("The Codex chief command is empty".into());
                }
                if resume.as_deref().is_some_and(|id| !ipc::safe_chief_root_id(id)) {
                    return Err("A Codex chief can only resume an explicit root thread ID".into());
                }
                codex_chief_options(args, false)?;
                codex_chief_options(&extra_args, true)?;
            }
            agent
        } else {
            let Some(agent) = self.config.agent(agent_name).cloned() else {
                return Err(format!("No agent named {agent_name} in the config"));
            };
            agent
        };
        let managed_worktree = match &role {
            Role::WorktreeAgent(..) => true,
            Role::Recover { managed_worktree, .. } if agent.is_codex() => {
                managed_worktree.unwrap_or_else(|| self.known_codex_worktree(&cwd) || self.legacy_codex_worktree(&cwd))
            }
            Role::Recover { managed_worktree, .. } => managed_worktree.unwrap_or_default(),
            Role::Agent(..) if agent.is_codex() => self.known_codex_worktree(&cwd),
            _ => false,
        };
        if managed_worktree
            && agent.is_codex()
            && agent.command.iter().skip(1).chain(&extra_args).any(|arg| {
                matches!(arg.as_str(), "--" | "--cd" | "--worktree" | "--remote")
                    || arg.starts_with("-C")
                    || arg.starts_with("--cd=")
                    || arg.starts_with("--worktree=")
                    || arg.starts_with("--remote=")
            })
        {
            return Err("A Codex task worktree must start in the local folder ATOMICS prepared.".into());
        }
        if !cwd.is_dir() {
            return Err(format!("{} is not a folder", cwd.display()));
        }
        let id = self
            .next_session_id
            .take()
            .filter(|id| !self.sessions.iter().any(|s| &s.id == id))
            .filter(|id| !chief || !self.hub.lock().unwrap().retired_chief_pane(id))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        // The chief is never sandboxed, whatever the isolation settings or a repository say: it needs
        // this machine's tools (atomics ctl, git across repositories, builds and screenshots).
        let job = self.sandbox_job(&agent, &cwd, &id, resume.as_deref(), prompt.is_some(), &role)?;
        let origin = match &role {
            Role::Agent(_, origin)
            | Role::WorktreeAgent(_, origin)
            | Role::Chief(origin)
            | Role::ChiefRestart(origin)
            | Role::ChiefHandoff { origin, .. } => origin.clone(),
            Role::Recover { .. } => ControlOrigin::Local,
        };
        let input_access = if job.is_some() { InputAccess::Controller } else { self.bare_input_access(&agent) };
        self.check_input_access(input_access, &origin).map_err(|e| e.message)?;
        let runtime = job.as_ref().map_or(AgentRuntime::Bare, |job| AgentRuntime::Sandbox(job.sandboxed()));
        // Codex's hooks call ATOMICS on this machine, which a sandboxed Codex cannot reach.
        if (agent.is_codex() || chief && agent.kind == AgentKind::Generic) && job.is_none() {
            self.codex_version(resolve_program(&agent.command[0]));
        }
        let launch = AgentLaunch {
            origin,
            id: id.clone(),
            agent,
            cwd,
            resume,
            extra_args,
            prompt,
            chief,
            restart_notice: matches!(role, Role::ChiefRestart(_)),
            managed_worktree,
            handoff: match role {
                Role::ChiefHandoff { provider, conversation, .. } => Some((provider, conversation)),
                _ => None,
            },
            chief_masks_from: chief_masks_from.clone(),
            runtime,
            ref_guard: None,
            relay: None,
            interactive,
        };
        if let Some(job) = job {
            let workspace =
                crate::sandbox::current_pins(&job.cwd).map_or_else(|_| job.cwd.clone(), |pins| pins.work_tree);
            let pending_mount = crate::sandbox::editor_mounts::reserve([workspace])?;
            self.sessions.push(Session::starting(
                id.clone(),
                &launch.agent,
                launch.cwd.clone(),
                chief,
                Some(managed_worktree),
                &self.wake,
                input_access,
            ));
            let (tx, rx) = self.wake.channel();
            let (checks, exempt) = (self.ref_checks.clone(), self.exempt_branches());
            let snapshot = paths::atomics_home().join("ref-guards").join(format!("{}.json", job.name));
            let jobs = self.running_lifecycle();
            std::thread::spawn(move || {
                let _pending_mount = pending_mount;
                // A previous close may still be stopping this session's resumable sandbox, or a cleanup
                // removing its worktree. This thread waits for that, never the UI thread.
                let root = crate::build_cache::root(&job.cwd);
                crate::lifecycle::wait_for(&jobs, Some(&job.name), Some(&root));
                let ready = crate::sandbox::prepare(&job)
                    .map_err(|err| super::NotPrepared::Sandbox(format!("Could not start the sandbox: {err:#}")))
                    .and_then(|granted| {
                        let opened = crate::ref_guard::open_and_check(&checks, &job.cwd, snapshot, exempt)
                            .map_err(super::NotPrepared::Other);
                        if opened.is_err()
                            && let (Some(relay), Some(granted)) = (&job.relay, &granted)
                        {
                            relay.revoke(granted);
                        }
                        opened.map(|opened| {
                            // Every connected server, on a new or reattached sandbox; it never stops the agent.
                            let (sbx, mcp) = (&job.sbx, &job.config.mcp);
                            let needed =
                                crate::connections::load_into(&job.home, sbx, &job.name, mcp, job.config.ssh_agent);
                            super::PreparedLaunch {
                                guard: opened,
                                grant: granted,
                                connections: needed,
                                cache_env: crate::sandbox::cache_env(&job),
                                ..Default::default()
                            }
                        })
                    });
                let _ = tx.send(ready);
            });
            self.pending_sandbox.push((launch, rx));
            self.activate(&id, false);
            return Ok(id);
        }
        self.sessions.push(Session::starting(
            id.clone(),
            &launch.agent,
            launch.cwd.clone(),
            chief,
            Some(managed_worktree),
            &self.wake,
            input_access,
        ));
        let (tx, rx) = self.wake.channel();
        let relay = self.relay.clone();
        let session = id.clone();
        let workspace = self.workspace.clone();
        let secret_cwd = launch.cwd.clone();
        let github_account = self.project_identity(&launch.cwd).github;
        let claude = launch.agent.kind == AgentKind::Claude && self.effective_isolation().bare_relay;
        let verify_masks = launch.prompt.is_some()
            || launch.handoff.is_some()
            || launch.resume.as_ref().is_some_and(|id| {
                self.store.sessions.iter().any(|record| {
                    &record.claude_session_id == id
                        && record.unsaved.as_ref().is_some_and(|unsaved| unsaved.prompt.is_some())
                })
            });
        let handoff = launch.handoff.is_some();
        let oauth = self.tickets.sign.oauth();
        std::thread::spawn(move || {
            let project = workspace::project_root(&secret_cwd).ok();
            let ready = (|| {
                let masks = match chief_masks_from.filter(|_| verify_masks) {
                    Some(config) => oauth.secrets(&config.server).map_err(|_| {
                        let purpose = if handoff { "handoff" } else { "prompt" };
                        anyhow::anyhow!("Could not read chief {purpose} credentials. Try starting the chief again.")
                    })?,
                    None => Vec::new(),
                };
                let grant = if claude {
                    Some(relay.grant_claude(&session)?.ok_or_else(|| {
                        anyhow::anyhow!("bare relay requires a Claude subscription in Settings → Credentials")
                    })?)
                } else {
                    None
                };
                relay
                    .grant_secrets(
                        &session,
                        workspace.as_deref(),
                        project.as_deref().and_then(Path::to_str),
                        false,
                        grant.clone(),
                    )
                    .and_then(|granted| {
                        if let (Some(account), Some(project)) = (&github_account, &project) {
                            relay.grant_project_github(&session, account, project, workspace.as_deref(), false, granted)
                        } else {
                            Ok(granted)
                        }
                    })
                    .inspect_err(|_| {
                        if let Some(grant) = &grant {
                            relay.revoke(grant);
                        }
                    })
                    .map(|grant| super::PreparedLaunch { grant, masks, ..Default::default() })
            })()
            .map_err(|error: anyhow::Error| super::NotPrepared::Other(error.to_string()));
            let _ = tx.send(ready);
        });
        self.pending_sandbox.push((launch, rx));
        if !chief {
            self.activate(&id, false);
        }
        Ok(id)
    }

    pub(super) fn launch_prepared(&mut self, launch: AgentLaunch) -> Result<String, String> {
        let codex = (launch.agent.is_codex() || launch.chief && launch.agent.kind == AgentKind::Generic)
            && matches!(launch.runtime, AgentRuntime::Bare);
        if codex {
            let program = resolve_program(&launch.agent.command[0]);
            let ready = self.codex_version(program);
            let Some(supported) = ready.get().copied() else {
                let id = launch.id.clone();
                self.pending_codex.push((launch, ready));
                return Ok(id);
            };
            return self.launch_codex(launch, supported);
        }
        self.launch_agent(launch, None)
    }

    fn known_codex_worktree(&self, cwd: &Path) -> bool {
        self.plans.iter().flat_map(|plan| &plan.tasks).any(|task| task.worktree.as_deref() == Some(cwd))
            || self.sessions.iter().any(|session| session.managed_worktree == Some(true) && session.cwd == cwd)
            || self.store.sessions.iter().any(|record| record.managed_worktree == Some(true) && record.cwd == cwd)
            || atomics_core::identity::is_managed_worktree(cwd)
    }

    fn legacy_codex_worktree(&self, cwd: &Path) -> bool {
        // Older ad-hoc launches without a Git identity only kept their folder. Match the
        // configured destination for their branch, and only in a linked worktree.
        let Some(worktree) = git::worktree_of(cwd) else { return false };
        let Some(branch) = worktree.branch else { return false };
        let Some(repo) = git::main_checkout(&worktree.root).ok().filter(|repo| repo != &worktree.root) else {
            return false;
        };
        git::worktree_path(&self.config.worktree_path, &repo, &branch)
            .canonicalize()
            .is_ok_and(|path| path == worktree.root)
    }

    /// The sandbox `agent` gets in `cwd`, or `None` to run it bare: the isolation in effect for
    /// its project, which the repository may have tightened, and its plan `task`'s own. A resumed
    /// session reattaches to the sandbox it ran in.
    fn sandbox_job(
        &self,
        agent: &atomics_core::config::AgentConfig,
        cwd: &Path,
        id: &str,
        resume: Option<&str>,
        prompt: bool,
        role: &Role<'_>,
    ) -> Result<Option<crate::sandbox::Prepare>, String> {
        let (mode, prior) = match role {
            Role::Chief(_) | Role::ChiefRestart(_) | Role::ChiefHandoff { .. } => return Ok(None),
            Role::Recover { sandbox, .. } => {
                (Some(if sandbox.is_some() { IsolationMode::Sandbox } else { IsolationMode::Bare }), *sandbox)
            }
            Role::Agent(task, _) | Role::WorktreeAgent(task, _) => {
                let prior = resume
                    .filter(|id| !id.is_empty())
                    .and_then(|id| self.store.sessions.iter().find(|record| record.claude_session_id == id))
                    .and_then(|record| record.sandbox.as_deref());
                (*task, prior)
            }
        };
        let config = self.current_project_config(cwd);
        if config.isolation.mode_for(agent, mode) == IsolationMode::Bare {
            return Ok(None);
        }
        let sbx_agent = isolation::sbx_agent(agent).ok_or_else(|| {
            let program = agent.command.first().map_or("an empty command", String::as_str);
            let fix = if config.isolation.required {
                "Its repository requires sandboxed agents in .atomics/config.toml; use claude, codex or shell there."
            } else {
                "Set isolation = \"bare\" on it in [[agents]] to run it on this machine."
            };
            format!("Could not start {}: Docker Sandboxes has no agent for {program}. {fix}", agent.name)
        })?;
        let name = prior.map_or_else(|| isolation::sandbox_name(id), str::to_owned);
        let briefs = (prompt && agent.kind != AgentKind::Claude)
            .then(|| paths::briefs_dir().join(name.strip_prefix(isolation::NAME_PREFIX).unwrap_or(&name)));
        let exe = crate::setup::executable_path().map_err(|err| format!("Could not locate ATOMICS: {err}"))?;
        Ok(Some(crate::sandbox::Prepare {
            home: paths::atomics_home(),
            sbx: resolve_program(&self.sbx),
            name,
            agent: sbx_agent,
            cwd: cwd.to_path_buf(),
            warm: config.worktrees.warm.clone(),
            cache: config.cache.clone(),
            config: config.isolation.sandbox.with_defaults(&config.resources, self.governor.cores),
            coordination: config.plan.coordination,
            exe,
            socket: paths::socket_path(),
            // Claude takes any prompt as an argument; other agents may get a brief file.
            briefs,
            pins: paths::git_pins_dir(),
            relay: Some(self.relay.clone()),
            secret_workspace: self.workspace.clone(),
            github_account: self.project_identity(cwd).github,
        }))
    }

    /// Stops a sandbox in the background: its agent exited or its tile closed.
    pub(super) fn stop_sandbox(&mut self, name: String) {
        let sbx = resolve_program(&self.sbx);
        self.lifecycle(Scope::sandbox(name.clone()), move || {
            if let Err(err) = crate::sandbox::stop(&paths::atomics_home(), &sbx, &name) {
                eprintln!("ATOMICS: stopping sandbox {name} failed: {err:#}");
                crate::lifecycle::record_leftovers(
                    &paths::atomics_home(),
                    &[format!("Stopping sandbox {name}: {err:#}")],
                );
            }
        });
    }

    /// Stops and removes a sandbox in the background: a closed scratch session's.
    pub(super) fn remove_sandbox(&mut self, name: String) {
        let sbx = resolve_program(&self.sbx);
        self.lifecycle(Scope::sandbox(name.clone()), move || {
            let home = paths::atomics_home();
            if let Err(err) =
                crate::sandbox::stop(&home, &sbx, &name).and_then(|()| crate::sandbox::remove(&home, &sbx, &name))
            {
                crate::lifecycle::record_leftovers(&home, &[format!("Removing sandbox {name}: {err:#}")]);
            }
        });
    }

    /// A launch that failed after its tile appeared: the tile and a toast say why, and the
    /// plan task it was started for is blocked with the reason.
    pub(super) fn launch_failed(&mut self, id: &str, error: String) {
        self.discard_chief_replacement(id);
        if let Some(index) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions[index].status = AgentStatus::Exited;
            self.sessions[index].last_message = Some(error.clone());
        }
        let note = format!("could not start: {error}");
        let tasks: Vec<(usize, String)> = self
            .plans
            .iter()
            .enumerate()
            .flat_map(|(index, plan)| {
                plan.tasks.iter().filter(|t| t.session.as_deref() == Some(id)).map(move |t| (index, t.id.clone()))
            })
            .collect();
        for (index, task_id) in tasks {
            let Some(task) = self.plans[index].task_mut(&task_id) else { continue };
            let old = task.status;
            task.set_status(TaskStatus::Blocked);
            task.notes.push(note.clone());
            self.save_plan(index);
            let author = atomics_core::tickets::Author::Atomics;
            self.ticket_status_changed(index, &task_id, old, TaskStatus::Blocked, Some(&note), author);
        }
        self.toast(error);
    }

    /// The session an agent's MCP server speaks for. A sandboxed agent's server is registered
    /// with sbx under its sandbox's name, which outlives ATOMICS restarts and new session ids.
    pub(super) fn pane_session(&self, pane: Option<String>) -> Option<String> {
        pane.map(|pane| {
            self.sessions.iter().find(|s| s.sandbox.as_deref() == Some(pane.as_str())).map_or(pane, |s| s.id.clone())
        })
    }

    fn codex_version(&mut self, program: String) -> CodexVersion {
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
                    // `codex` can be a Node script: `env node` needs the login shell's PATH, which
                    // a window started from the Dock may not have taken on yet.
                    let path = crate::setup::child_path();
                    let supported =
                        crate::setup::codex_supports_hooks(&program, path.as_deref(), CODEX_VERSION_TIMEOUT);
                    let _ = result.set(supported);
                    wake.notify();
                });
                ready
            })
            .clone()
    }

    /// Starts a Codex agent with ATOMICS's hooks when its version probe found them supported. A
    /// probe that failed or timed out leaves support unknown: Codex starts without the hook flags,
    /// which an older Codex refuses, and its tile says that its approvals stay in its terminal.
    /// The probe runs again at the next start.
    pub(super) fn launch_codex(&mut self, launch: AgentLaunch, supported: Option<bool>) -> Result<String, String> {
        if launch.chief && supported != Some(true) {
            return Err("Codex chief requires a Codex CLI with session and permission hooks (0.165 or later)".into());
        }
        let id = self.launch_agent(launch, Some(supported == Some(true)))?;
        let warning = supported.is_none().then(|| CODEX_HOOKS_UNKNOWN.to_string());
        if let Some(index) = self.find(&id) {
            self.sessions[index].warning = warning.clone();
        }
        if let Some(warning) = warning {
            self.toast(warning);
        }
        Ok(id)
    }

    pub(super) fn launch_agent(&mut self, launch: AgentLaunch, codex: Option<bool>) -> Result<String, String> {
        let AgentLaunch {
            id,
            agent,
            cwd,
            resume,
            extra_args,
            prompt,
            chief,
            restart_notice,
            managed_worktree,
            handoff,
            chief_masks_from,
            runtime,
            ref_guard,
            relay,
            origin,
            ..
        } = launch;
        // An agent reads its project's settings, hooks among them, from the folder it starts in.
        if let Some(why) = workspace::untrusted(&cwd) {
            return Err(why);
        }
        self.session_disk_guard(&cwd, chief, resume.is_some(), agent.kind == AgentKind::Shell)?;
        self.cancel_cache_cleanup(&cwd);
        let worktree_root = crate::build_cache::root(&cwd);
        if self.lifecycle_jobs.iter().any(|job| !job.is_finished() && job.touches(None, Some(&worktree_root))) {
            return Err("Worktree cleanup is still running; start the session again once it finishes.".into());
        }
        let sandboxed = match runtime {
            AgentRuntime::Sandbox(sandboxed) => Some(sandboxed),
            AgentRuntime::Bare => None,
        };
        let input_access = if sandboxed.is_some() { InputAccess::Controller } else { self.bare_input_access(&agent) };
        self.check_input_access(input_access, &origin).map_err(|e| e.message)?;
        // Claude saves a conversation once its first prompt is submitted. One that never got that far, its prompt
        // held back by a first-run screen such as the folder-trust question, has nothing to resume: it starts again
        // under its own id, with the prompt it was started with.
        let unsaved = match (&resume, agent.kind) {
            (Some(id), AgentKind::Claude) => self.unsaved_conversation(id, sandboxed.is_some()),
            _ => None,
        };
        let fresh = unsaved.is_some();
        let prompt = match unsaved {
            Some(unsaved) => unsaved.prompt.or(prompt),
            None => prompt,
        };
        let new_conversation = agent.kind == AgentKind::Claude && (resume.is_none() || fresh);
        let chief_cleaner = chief.then(|| {
            self.chief_cleaner(&self.cleaner()).with(
                relay
                    .iter()
                    .flat_map(|grant| grant.secrets.values().chain(std::iter::once(&grant.placeholder)))
                    .cloned(),
            )
        });
        let has_chief_prompt = chief && (prompt.is_some() || handoff.is_some());
        let ticket_masks_from =
            chief.then(|| self.ticket_config().filter(|config| config.server.uses_oauth())).flatten();
        let defer_instructions = !has_chief_prompt && (ticket_masks_from.is_some() || chief_masks_from.is_some());
        let chief_instructions = chief_cleaner.as_ref().map(|cleaner| {
            if defer_instructions {
                format!("{}{}", atomics_core::config::CHIEF_PROMPT, atomics_core::config::ATTACHMENT_INSTRUCTIONS)
            } else {
                cleaner.text(&self.chief_prompt())
            }
        });
        let prompt = if has_chief_prompt {
            if ticket_masks_from != chief_masks_from {
                return Err(
                    "Ticket settings changed before the chief prompt could be sent. Start the chief again.".into()
                );
            }
            if let Some((provider, conversation)) = handoff {
                Some(self.chief_handoff(Some(&conversation), provider).0)
            } else {
                prompt.map(|text| chief_cleaner.as_ref().expect("chief cleaner").text(&text))
            }
        } else {
            prompt.or_else(|| {
                restart_notice
                    .then(|| "ATOMICS restarted. Call atomics_wait to receive the restart event, then carry on.".into())
            })
        };
        let started_with = prompt.clone();
        let choose_conversation = resume.as_deref() == Some("") && agent.kind == AgentKind::Generic;
        let prompt = if choose_conversation {
            // With no id, Codex's resume command opens its picker. A positional prompt would be read as an id.
            None
        } else if !chief && agent.kind != AgentKind::Shell {
            let root = workspace::project_root(&cwd).unwrap_or_else(|_| cwd.clone());
            let section = atomics_core::repo_workflow::RepoWorkflow::read(&root).agent_section();
            if section.is_empty() { prompt } else { Some(format!("{}{section}", prompt.unwrap_or_default())) }
        } else {
            prompt
        };
        let agent_name = agent.name.clone();
        let chief_provider = chief.then_some(if codex.is_some() { ChiefAgent::Codex } else { ChiefAgent::Claude });
        let chief_selection = chief_provider
            .map(|provider| super::chief_provider::launch_selection(provider, &agent.command, &extra_args));
        let chief_model_label = chief_provider.map(|provider| {
            let args = agent.command.iter().skip(1).cloned().chain(extra_args.iter().cloned()).collect::<Vec<_>>();
            atomics_core::tickets::model_label(provider.name(), &args)
        });
        let shell = crate::setup::shell_program();
        let mut env: HashMap<String, String> =
            [("TERM", "xterm-256color".into()), ("COLORTERM", "truecolor".into()), ("TERM_PROGRAM", "atomics".into())]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        env.insert("COLORFGBG".into(), self.theme.colorfgbg().into());
        if sandboxed.is_none() && agent.kind != AgentKind::Shell {
            env.extend(crate::setup::instance_agent_env());
        }
        if sandboxed.is_none()
            && let Some(granted) = relay.as_ref().filter(|g| !g.placeholder.is_empty())
        {
            env.insert("CLAUDE_CODE_OAUTH_TOKEN".into(), granted.placeholder.clone());
            env.insert("ANTHROPIC_API_KEY".into(), String::new());
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), String::new());
            env.insert("ANTHROPIC_BASE_URL".into(), format!("http://127.0.0.1:{}/api.anthropic.com", granted.port));
        }
        if let Some(path) = crate::setup::login_path_now().as_ref() {
            env.insert("PATH".into(), path.clone());
        }
        // `atomics queue` in an agent resolves to this ATOMICS, and its compilers share one job pool.
        // None of this reaches into a sandbox, whose toolchain and limits are its own.
        if sandboxed.is_none()
            && let Some(dir) = crate::setup::executable_path().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            let path = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
            // An empty entry would put the current directory on PATH.
            let path = if path.is_empty() { dir.display().to_string() } else { format!("{}:{path}", dir.display()) };
            env.insert("PATH".into(), path);
        }
        if sandboxed.is_none() {
            if let Some(jobserver) = &self.jobserver {
                env.extend(jobserver.env());
            }
            if crate::build_cache::lean_worktree(&cwd) {
                let cache = self.project_config(&cwd).cache;
                let lean = !chief && agent.kind != AgentKind::Shell;
                let policy = if lean { cache.clone() } else { Default::default() };
                let setup = crate::build_cache::link(
                    &paths::atomics_home(),
                    &crate::build_cache::root(&cwd),
                    &policy,
                    crate::build_cache::Domain::Host,
                );
                let mut cache_env = if lean { crate::build_cache::agent_env(&cwd, &cache) } else { Default::default() };
                match setup {
                    Ok(setup) => setup.filter_env(&mut cache_env),
                    Err(error) => {
                        eprintln!("ATOMICS: cache setup skipped: {error:#}");
                        crate::build_cache::CacheSetup { skipped: vec![PathBuf::from("/")] }.filter_env(&mut cache_env);
                    }
                }
                env.extend(cache_env);
            }
            env.extend(crate::governor::toolchain_env(&cwd, &self.config.resources, self.governor.cores));
        }

        // A sandboxed agent's own and task arguments lose the host's sandbox and permission flags.
        let own_args = |args: &[String]| match &sandboxed {
            Some(sandboxed) => isolation::sandboxed_args(sandboxed.agent, args),
            None => args.to_vec(),
        };
        let extra_args = own_args(&extra_args);
        let (program, mut args) = match agent.command.split_first() {
            Some((program, rest)) => (program.clone(), own_args(rest)),
            None if sandboxed.is_some() => (shell, Vec::new()),
            None => (shell, vec!["-l".to_string()]),
        };
        let mut claude_id = (agent.kind == AgentKind::Claude)
            .then(|| resume.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
        // Every agent ATOMICS starts can report back to the plan with `atomics_report`.
        let exe = crate::setup::executable_path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "atomics".into());
        let socket = paths::socket_path().to_string_lossy().into_owned();
        // Hooks name a path that survives updates where there is one, so Codex's trust in them lasts.
        let hooks_exe = crate::setup::hook_executable().unwrap_or_else(|_| PathBuf::from(&exe));
        let notebook_capability = crate::notebook_auth::issue(
            &paths::atomics_home(),
            sandboxed.as_ref().map_or(id.as_str(), |s| s.name.as_str()),
        )
        .map_err(|error| format!("Notebook session identity: {error:#}"))?;
        let notebook_mode = self.current_project_config(&cwd).plan.coordination.name();
        if sandboxed.is_none() {
            env.insert(crate::notebook_auth::MODE_ENV.into(), notebook_mode.into());
            env.insert(crate::notebook_auth::ENV.into(), notebook_capability.clone());
        }
        let mut codex_chief_home = None;
        match agent.kind {
            AgentKind::Claude if chief => {
                // The chief gets every ATOMICS tool and knows how to run a plan.
                let config = json!({ "mcpServers": { "atomics": {
                    "command": exe, "args": ["mcp", "--chief"], "env": { "ATOMICS_CHIEF_CONVERSATION": claude_id, "ATOMICS_PANE": id, "ATOMICS_SOCKET": socket, "ATOMICS_NOTE_MODE": notebook_mode, "ATOMICS_NOTE_CAPABILITY": "${ATOMICS_NOTE_CAPABILITY}" }
                } } });
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    CHIEF_ALLOWED_TOOLS.join(","),
                    "--append-system-prompt".to_string(),
                    chief_instructions.expect("chief instructions"),
                ]);
            }
            // A sandboxed agent reaches ATOMICS's MCP server through sbx, registered with its sandbox.
            AgentKind::Claude if sandboxed.is_none() => {
                let config = json!({ "mcpServers": { "atomics": {
                    "command": exe, "args": ["mcp", "--agent"], "env": { "ATOMICS_PANE": id, "ATOMICS_SOCKET": socket, "ATOMICS_NOTE_CAPABILITY": "${ATOMICS_NOTE_CAPABILITY}", "ATOMICS_NOTE_MODE": notebook_mode }
                } } });
                args.extend([
                    "--mcp-config".to_string(),
                    config.to_string(),
                    "--allowedTools".to_string(),
                    "mcp__atomics__atomics_preview,mcp__atomics__atomics_report,mcp__atomics__atomics_note,mcp__atomics__atomics_notes,mcp__atomics__atomics_rules,mcp__atomics__atomics_agent_ticket_get,mcp__atomics__atomics_agent_ticket_comment".to_string(),
                ]);
            }
            AgentKind::Generic if codex.is_some() => {
                // Linux runtime storage is outside Codex's workspace and /tmp
                // writable roots. Its compilers must be able to open our FIFO.
                // A writable root, unlike --add-dir, applies only once Codex writes
                // the workspace: in a folder it does not trust yet, Codex 0.166 quits
                // on --add-dir before it can ask for trust.
                if !chief
                    && self.jobserver.is_some()
                    && let Some(runtime) = paths::runtime_dir_for(&paths::atomics_home())
                {
                    let pool = runtime.join("jobserver");
                    args.extend([
                        "-c".to_string(),
                        format!("sandbox_workspace_write.writable_roots=[{}]", quoted(&pool.to_string_lossy())),
                    ]);
                }
                if codex == Some(true) {
                    if !args.iter().chain(&extra_args).any(|arg| arg == "--no-daemon") {
                        args.push("--no-daemon".into());
                    }
                    let hook_args = if chief {
                        atomics_core::hook::codex_chief_hook_args
                    } else {
                        atomics_core::hook::codex_hook_args
                    };
                    args.extend(hook_args(&hooks_exe, Path::new(&socket), &id));
                    // The handlers find this pane without process ancestry, which a wrapper
                    // such as ssh into a VM would hide.
                    env.insert(atomics_core::hook::HOOK_PANE_ENV.into(), id.clone());
                }
                let role = if chief { "--chief" } else { "--agent" };
                args.extend([
                    "-c".into(),
                    format!("mcp_servers.atomics.command={}", quoted(&exe)),
                    "-c".into(),
                    format!("mcp_servers.atomics.args=[\"mcp\", {}]", quoted(role)),
                    "-c".into(),
                    format!(
                        "mcp_servers.atomics.env={{ ATOMICS_PANE = {}, ATOMICS_SOCKET = {}, ATOMICS_NOTE_MODE = {} }}",
                        quoted(&id),
                        quoted(&socket),
                        quoted(notebook_mode)
                    ),
                    "-c".into(),
                    "mcp_servers.atomics.env_vars=[\"ATOMICS_NOTE_CAPABILITY\"]".into(),
                ]);
                if chief {
                    let home = crate::setup::prepare_chief_codex_home(&hooks_exe, Path::new(&socket), &id)
                        .map_err(|error| format!("Could not prepare the Codex chief home: {error}"))?;
                    codex_chief_home = Some(home);
                    if !extra_args.iter().any(|arg| arg == CODEX_CHIEF_BYPASS) {
                        args.extend(["--sandbox".into(), "read-only".into(), "-a".into(), "on-request".into()]);
                    }
                    args.extend([
                        "-c".into(),
                        "approvals_reviewer=\"user\"".into(),
                        // Snapshots persist inherited environment credentials in the Codex home.
                        "-c".into(),
                        "features.shell_snapshot=false".into(),
                        "-c".into(),
                        format!(
                            "shell_environment_policy.exclude=[{}]",
                            quoted(atomics_core::hook::CHIEF_HOOK_KEY_ENV)
                        ),
                    ]);
                    args.extend([
                        "-c".into(),
                        format!("developer_instructions={}", quoted(&chief_instructions.expect("chief instructions"))),
                        "-c".into(),
                        "mcp_servers.atomics.default_tools_approval_mode=\"prompt\"".into(),
                        "-c".into(),
                        "mcp_servers.atomics.approvals_reviewer=\"user\"".into(),
                    ]);
                    for tool in CHIEF_ALLOWED_TOOLS.iter().filter_map(|tool| tool.strip_prefix("mcp__atomics__")) {
                        args.extend([
                            "-c".into(),
                            format!("mcp_servers.atomics.tools.{tool}.approval_mode=\"approve\""),
                        ]);
                    }
                    if self.config.chief.native_messaging == atomics_core::config::NativeMessaging::Refuse {
                        for feature in
                            ["multi_agent", "multi_agent_v2", "agent_message_board", "agent_message_board_mcp"]
                        {
                            args.extend(["-c".into(), format!("features.{feature}=false")]);
                        }
                    }
                } else {
                    args.extend(["-c".into(), "mcp_servers.atomics.default_tools_approval_mode=\"approve\"".into()]);
                }
            }
            // Codex asks whether to trust a folder it has no setting for, and in a new sandbox it has
            // none. The folder sbx mounts, as the whole table: a folder's dots would split a dotted key.
            AgentKind::Generic if sandboxed.as_ref().is_some_and(|sandboxed| sandboxed.agent == "codex") => {
                let mounted = match git::pinned(&cwd) {
                    Some(pins) if pins.git_dir.is_some() => pins.work_tree,
                    _ => cwd.clone(),
                };
                let folder = serde_json::to_string(&mounted.to_string_lossy()).unwrap_or_default();
                args.extend(["-c".to_string(), format!("projects={{ {folder} = {{ trust_level = \"trusted\" }} }}")]);
                if self.current_project_config(&cwd).plan.coordination != atomics_core::config::CoordinationMode::Off {
                    args.push("--no-daemon".into());
                    args.extend(atomics_core::hook::codex_settings_args(
                        &atomics_core::hook::sandbox_notebook_settings("codex"),
                    ));
                }
            }
            _ => {}
        }
        if agent.kind == AgentKind::Claude {
            // Hooks run `atomics hook` on this machine, which a sandbox does not have.
            let mut settings = match sandboxed {
                None => {
                    let mut settings = if chief {
                        atomics_core::hook::chief_session_settings(&hooks_exe, Path::new(&socket), &id)
                    } else {
                        atomics_core::hook::session_settings(&hooks_exe, Path::new(&socket), &id, false)
                    };
                    if let Some(granted) = relay.as_ref().filter(|g| !g.placeholder.is_empty()) {
                        settings["apiKeyHelper"] = "".into();
                        settings["env"] = json!({
                            "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:{}/api.anthropic.com", granted.port),
                            "CLAUDE_CODE_OAUTH_TOKEN": granted.placeholder,
                            "ANTHROPIC_API_KEY": "",
                            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1"
                        });
                    }
                    settings
                }
                // Its status line only saves the account's limits in the VM, for ATOMICS to read back.
                Some(_) => match &relay {
                    Some(relay) if !relay.placeholder.is_empty() => {
                        isolation::claude_relay_settings(relay.port, &relay.placeholder)
                    }
                    _ => isolation::claude_settings(),
                },
            };
            let allow = if chief {
                self.config.chief.native_messaging == atomics_core::config::NativeMessaging::Allow
            } else {
                self.current_project_config(&cwd).plan.native_agent_messaging
            };
            atomics_core::hook::native_messaging(&mut settings, allow);
            if sandboxed.is_some()
                && self.current_project_config(&cwd).plan.coordination != atomics_core::config::CoordinationMode::Off
            {
                settings["hooks"] = atomics_core::hook::sandbox_notebook_settings("claude")["hooks"].clone();
            }
            let settings = if sandboxed.is_none() && relay.as_ref().is_some_and(|grant| !grant.placeholder.is_empty()) {
                let file = claude_settings_path(&id);
                atomics_core::store::write_private(&file, settings.to_string().as_bytes())
                    .map_err(|err| format!("Could not prepare Claude settings: {err}"))?;
                file.to_string_lossy().into_owned()
            } else {
                settings.to_string()
            };
            args.extend(["--settings".into(), settings]);
            let flag = if resume.is_some() && !fresh { "--resume" } else { "--session-id" };
            args.extend([flag.to_string(), claude_id.clone().expect("Claude conversation selected")]);
        } else if let Some(id) = resume {
            args.push("resume".into());
            if !id.is_empty() {
                if id.starts_with('-') {
                    return Err("Invalid conversation ID".into());
                }
                args.push(id.clone());
                claude_id = Some(id);
            }
        }
        args.extend(extra_args);
        let restricted_entry = (codex.is_some() && (chief || managed_worktree)).then(|| {
            let entry = RestrictedEntry::new(&cwd);
            args.extend(["-c".into(), codex_restricted_projects(&entry.cwd)]);
            if !chief {
                codex_task_permissions(&mut args);
                if codex == Some(true) {
                    let state = atomics_core::hook::codex_hook_trust(&hooks_exe, Path::new(&socket))
                        .into_iter()
                        .map(|hook| format!("{:?} = {{ trusted_hash = {:?} }}", hook.key, hook.hash))
                        .collect::<Vec<_>>()
                        .join(", ");
                    args.extend(["-c".into(), format!("hooks.state={{ {state} }}")]);
                }
            }
            entry
        });
        let prompt = match prompt {
            Some(text) if chief && text.starts_with(super::chief_provider::HANDOFF_PREAMBLE) => {
                Some(chief_prompt_input(&id, &text, chief_provider.expect("chief provider"))?)
            }
            Some(text) if agent.kind != AgentKind::Claude && text.len() > LONG_PROMPT => {
                let session = sandboxed.as_ref().map_or(id.as_str(), |sandbox| {
                    sandbox.name.strip_prefix(isolation::NAME_PREFIX).unwrap_or(&sandbox.name)
                });
                Some(brief_file(session, &text, sandboxed.is_some())?)
            }
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
        if let Some(granted) = &relay {
            env.extend(granted.secrets.clone());
        }
        if let Some(home) = &codex_chief_home {
            env.insert("CODEX_HOME".into(), home.path().to_string_lossy().into_owned());
            env.insert("TMPDIR".into(), home.path().join("process-tmp").to_string_lossy().into_owned());
        }
        let nice = self.config.resources.agent_nice;
        #[cfg(target_os = "linux")]
        let nice = nice.max(1);
        let program =
            if chief && codex.is_some() { crate::setup::native_codex(&resolve_program(&program)) } else { program };
        let (program, args) = match &sandboxed {
            Some(sandboxed) => {
                let (program, mut run) = sandboxed.command(&args);
                if let Some(relay) = &relay {
                    let position = run.iter().position(|arg| arg == "--").unwrap_or(run.len());
                    let env = relay
                        .azure_env()
                        .into_iter()
                        .flat_map(|(name, value)| ["--env".into(), format!("{name}={value}")]);
                    run.splice(position..position, env);
                    let position = run.iter().position(|arg| arg == "--").unwrap_or(run.len());
                    // Bare names copy the session environment; values never enter argv or sandbox config.
                    run.splice(
                        position..position,
                        relay.secrets.keys().flat_map(|name| ["--env".into(), name.clone()]),
                    );
                }
                #[cfg(target_os = "linux")]
                let (program, run) = (
                    "nice".to_string(),
                    ["-n".to_string(), nice.to_string(), resolve_program(&program)].into_iter().chain(run).collect(),
                );
                (program, run)
            }
            None if (cfg!(target_os = "linux") || (!chief && agent.kind != AgentKind::Shell)) && nice != 0 => (
                "nice".to_string(),
                ["-n".to_string(), nice.to_string(), resolve_program(&program)].into_iter().chain(args).collect(),
            ),
            None => (resolve_program(&program), args),
        };
        let chief_key = chief.then(|| uuid::Uuid::new_v4().to_string());
        if let Some(key) = &chief_key {
            env.insert(atomics_core::hook::CHIEF_HOOK_KEY_ENV.into(), key.clone());
        }
        // Hooks can reach the listener immediately after exec. Keep admission locked until the PTY PID is known.
        let hub = self.hub.clone();
        let mut chief_hub = chief.then(|| hub.lock().unwrap());
        match Terminal::spawn_on(
            (self.wake.clone(), self.governor.restarting.clone()),
            self.theme.terminal_colors(),
            program,
            args,
            &cwd,
            env,
            self.config.terminal.scrollback,
        ) {
            Ok(mut terminal) => {
                terminal.codex_chief_home = codex_chief_home;
                if let Some(mut hub) = chief_hub.take() {
                    hub.register_chief(
                        &id,
                        claude_id.clone(),
                        chief_key.expect("chief launch key"),
                        self.is_chief_replacement(&id),
                        terminal.chief_process.expect("chief process identity"),
                        // The launcher execs the native binary before opening the hook socket.
                        std::env::current_exe().unwrap_or(hooks_exe),
                    );
                }
                if let Some(id) = &claude_id {
                    self.dismissed.remove(id);
                }
                let starting = self.sessions.iter().position(|session| session.id == id);
                let index = starting.unwrap_or_else(|| {
                    self.sessions.push(Session::starting(
                        id.clone(),
                        &agent,
                        cwd,
                        chief,
                        Some(managed_worktree),
                        &self.wake,
                        input_access,
                    ));
                    self.sessions.len() - 1
                });
                self.sessions[index].input_access = input_access;
                self.sessions[index].terminal = Some(terminal);
                if agent.kind == AgentKind::Shell {
                    self.sessions[index].status = AgentStatus::Idle;
                }
                self.sessions[index].claude_id = claude_id;
                self.sessions[index].chief_selection = chief_selection;
                self.sessions[index].chief_model_label = chief_model_label;
                self.sessions[index].restricted_entry = restricted_entry;
                self.sessions[index].unsaved =
                    new_conversation.then_some(atomics_core::store::Unsaved { prompt: started_with });
                self.sessions[index].cache_env =
                    sandboxed.as_ref().map(|sandbox| sandbox.cache_env.clone()).unwrap_or_default();
                self.sessions[index].sandbox = sandboxed.map(|sandboxed| sandboxed.name);
                self.sessions[index].ref_guard = ref_guard;
                self.sessions[index].relay = relay;
                if self.active.as_deref() == Some(&id) {
                    self.focus_terminal = Some(id.clone());
                }
                if chief {
                    let replacing = self.is_chief_replacement(&id);
                    if defer_instructions || !replacing {
                        let instructions = defer_instructions.then(|| self.chief_prompt_additions());
                        self.load_chief_trace(&id, instructions);
                    }
                    if !replacing {
                        self.hub.lock().unwrap().chief_running = true;
                    }
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
        if reference.is_empty() {
            return None;
        }
        if let Some(index) = self.sessions.iter().position(|s| s.id == reference) {
            return Some(index);
        }
        let mut matches = self.sessions.iter().enumerate().filter(|(_, s)| s.id.starts_with(reference));
        if let Some((index, _)) = matches.next() {
            // A title cannot override even an ambiguous id prefix.
            return matches.next().is_none().then_some(index);
        }
        let mut titles = self.sessions.iter().enumerate().filter(|(_, s)| {
            self.session_title(s).eq_ignore_ascii_case(reference) || s.title.eq_ignore_ascii_case(reference)
        });
        let (index, _) = titles.next()?;
        titles.next().is_none().then_some(index)
    }

    /// Confirm terminal delivery before answering Codex sends; busy turns can drop input.
    pub(super) fn verify_deliveries(&mut self) {
        let mut warnings = Vec::new();
        let mut delivered = Vec::new();
        for session in &mut self.sessions {
            if !session.terminal.as_ref().is_some_and(Terminal::exited)
                && (session.approval_hold || !session.approvals.is_empty())
            {
                continue;
            }
            let (Some(pending), Some(terminal)) = (session.pending.as_mut(), session.terminal.as_ref()) else {
                continue;
            };
            if pending.attachment.as_ref().is_some_and(|ticket| !ticket.live()) {
                session.pending = None;
                continue;
            }
            let started = !pending.was_working && session.status == AgentStatus::Working;
            let snippet: String = pending.text.lines().next().unwrap_or("").chars().take(24).collect();
            let screen = terminal.screen_text(200);
            let queued = pending.codex && codex_queue_receipt(&screen, &pending.screen_before, &pending.text);
            let lines: Vec<&str> = screen.lines().collect();
            let input_box = lines.len().saturating_sub(6);
            let composer_empty = pending.codex.then(|| codex_composer_empty(&screen, &pending.screen_before)).flatten();
            let in_transcript = if pending.codex {
                let marker = composer_line(&screen).is_some_and(|line| line.contains("[Pasted "));
                pending.paste_seen |=
                    marker && !composer_line(&pending.screen_before).is_some_and(|line| line.contains("[Pasted "));
                !queued
                    && (codex_transcript_receipt(&screen, &pending.screen_before, &pending.text)
                        || pending.paste_seen && !marker && composer_empty == Some(true))
            } else {
                started || !snippet.trim().is_empty() && lines[..input_box].iter().any(|l| l.contains(snippet.trim()))
            };
            // A generic screen preview can quote a previous attachment. The provider's
            // exact submission event acknowledges the delivery tagged on this prompt.
            if in_transcript && pending.attachment.is_none() {
                if let Some(reply) = pending.reply.take() {
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(json!({"sent":true}))));
                }
                if pending.queued {
                    delivered.push(session.id.clone());
                }
                session.pending = None;
                continue;
            }
            if queued && !terminal.exited() {
                pending.queued = true;
                pending.sent = Instant::now();
                if let Some(reply) = pending.reply.take() {
                    let _ = reply.send(ControlOutcome::Done(ControlResponse::ok(json!({"sent":false,"queued":true}))));
                }
                // The provider owns the visible queue. Give it time to draw a receipt once the queue disappears.
                continue;
            }
            if !terminal.exited() && pending.sent.elapsed() < DELIVERY_TIMEOUT {
                continue;
            }
            if terminal.exited() || pending.queued || pending.attempts >= DELIVERY_ATTEMPTS {
                if let Some(reply) = pending.reply.take() {
                    let mut response = ControlResponse::error(if pending.codex {
                        "message delivery unconfirmed: no receipt from the agent; check its terminal"
                    } else {
                        "message not delivered: no receipt from the agent; check its terminal"
                    });
                    // The paste may have landed: the protocol answers `uncertain`, not refused.
                    if pending.codex {
                        response.result = json!({ "unconfirmed": true });
                    }
                    let _ = reply.send(ControlOutcome::Done(response));
                }
                if let Some(ticket) = &pending.attachment {
                    ticket.unconfirmed();
                } else if pending.codex && session.chief {
                    session.warning = Some(CODEX_DELIVERY_WARNING.into());
                }
                if pending.codex {
                    session.send_now = None;
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
            } else if pending.attachment.is_some() {
                // A file delivery is pasted once. Its provider hook acknowledges the tracked prompt.
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
        for id in delivered {
            self.delivery_event(&id);
        }
    }

    pub(super) fn delivery_event(&mut self, id: &str) {
        self.session_event(id, "message_delivered");
    }

    fn session_event(&mut self, id: &str, change: &'static str) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else { return };
        self.event_seq += 1;
        let plan = self.plans.iter().find(|plan| plan.contains_session(id));
        self.events.push(StatusEvent {
            seq: self.event_seq,
            session: id.into(),
            title: session.title.clone(),
            status: session.status,
            at: atomics_core::store::now(),
            plan: plan.map(|plan| plan.id.clone()),
            task: plan
                .and_then(|plan| plan.tasks.iter().find(|task| task.session.as_deref() == Some(id)))
                .map(|task| task.id.clone()),
            report: None,
            approval: None,
            findings: None,
            ticket: None,
            config: None,
            change: Some(change),
        });
    }

    /// Presses the key that makes a busy agent take a message sent with `now` at once, once its screen shows
    /// the message waiting ([`send_now_keys`]). A turn that ends first takes the message by itself.
    pub(super) fn send_now_keys(&mut self) {
        let mut notes = Vec::new();
        for session in &mut self.sessions {
            let Some(send_now) = &session.send_now else { continue };
            let Some(terminal) = session.terminal.as_ref().filter(|terminal| !terminal.exited()) else {
                session.send_now = None;
                continue;
            };
            let screen = terminal.screen_text(60);
            // Until it is submitted, what waits is someone else's; the key would leave this message behind.
            let submitted = send_now.since.elapsed() >= SUBMITTED && !composer_holds(&screen, &send_now.snippet);
            match send_now_keys(&screen, send_now.codex).filter(|_| submitted) {
                Some(Ok(keys)) => {
                    terminal.write(keys);
                    session.send_now = None;
                }
                Some(Err(key)) => {
                    notes.push(format!(
                        "{} takes waiting messages with {key}, which ATOMICS cannot press: yours waits for the end of its step",
                        session.title
                    ));
                    session.send_now = None;
                }
                None if send_now.since.elapsed() >= SEND_NOW_WAIT => {
                    if session.status == AgentStatus::Working {
                        notes.push(format!(
                            "{} did not show your message waiting, so it could not be sent at once; it waits for the end of its step",
                            session.title
                        ));
                    }
                    session.send_now = None;
                }
                None => {}
            }
        }
        for note in notes {
            self.toast(note);
        }
    }

    /// Status for agents without hooks, read from what their terminal shows.
    pub(super) fn track_busy(&mut self) {
        if self.busy_checked.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.busy_checked = Instant::now();
        let mut finished = Vec::new();
        let mut credential_errors = Vec::new();
        let mut ref_checks = Vec::new();
        let mut menus = Vec::new();
        let mut restricted_opened = Vec::new();
        for (index, session) in self.sessions.iter_mut().enumerate() {
            session.first_run_hint = None;
            let Some(terminal) = &session.terminal else {
                take_live(&mut session.work.live, &mut session.live_read, None, Instant::now());
                continue;
            };
            // What its own status line says while it works, for the simple view's working line.
            let reading = (session.is_agent() && session.status == AgentStatus::Working && !terminal.exited())
                .then(|| {
                    atomics_core::status_line::parse(&terminal.screen_text(atomics_core::status_line::SEARCH_LINES))
                })
                .flatten();
            take_live(&mut session.work.live, &mut session.live_read, reading, Instant::now());
            if !session.approvals.is_empty() {
                continue;
            }
            if session.sandbox.is_some()
                && let Some(reason) = credential_failure(&terminal.screen_text(60))
            {
                session.status = AgentStatus::NeedsInput;
                if session.last_message.as_deref() != Some(&reason) {
                    credential_errors.push((session.id.clone(), reason.clone()));
                }
                session.last_message = Some(reason);
                session.activity.observe(session.status);
                continue;
            }
            // A command menu drawn with no hook asking is the session's pending approval.
            if session.is_agent()
                && session.sandbox.is_none()
                && !terminal.exited()
                && matches!(
                    session.status,
                    AgentStatus::Starting | AgentStatus::Idle | AgentStatus::Done | AgentStatus::NeedsInput
                )
            {
                let screen = terminal.screen_text(usize::MAX);
                if atomics_core::approvals::native_menu(&screen).is_some() {
                    menus.push((index, screen));
                    continue;
                }
            }
            // Trust and hook menus can precede hooks, including in bare Codex.
            let check_startup = matches!(
                session.status,
                AgentStatus::Starting | AgentStatus::Idle | AgentStatus::Done | AgentStatus::NeedsInput
            );
            if session.is_agent() && !terminal.exited() && (check_startup || session.restricted_entry.is_some()) {
                let screen = terminal.screen_text(60);
                let folder_access = codex_folder_access_visible(&screen);
                if let Some(entry) = &mut session.restricted_entry {
                    if !folder_access {
                        entry.answered = false;
                    } else if entry.open(terminal, &screen) {
                        restricted_opened.push(session.id.clone());
                    }
                }
                if folder_access {
                    session.first_run_hint = Some(FOLDER_ACCESS_HINT);
                    continue;
                }
                let before_turn = !session.hooks_active
                    && session.last_prompt.is_none()
                    && session.last_message.is_none()
                    && session.activity.working_since.is_none()
                    && session.status != AgentStatus::Done;
                if check_startup
                    && (before_turn || atomics_core::approvals::first_run_menu_visible(&screen))
                    && atomics_core::approvals::first_run_prompt(&screen)
                {
                    session.first_run_hint = atomics_core::approvals::first_run_hint(&screen);
                    session.status = AgentStatus::NeedsInput;
                    session.activity.observe(session.status);
                    continue;
                }
            }
            let busy_text = self.config.agent(&session.agent).and_then(|a| a.busy_text.clone());
            let busy_text =
                busy_text.or_else(|| (session.sandbox.is_some() && session.is_agent()).then(|| SANDBOX_BUSY.into()));
            let Some(busy_text) = busy_text else { continue };
            if terminal.exited() {
                continue;
            }
            // A question answered in the agent's own terminal, such as Codex's "Hooks need review",
            // asked nothing a hook or request could close, and with its hooks untrusted none reports
            // the turn going on. With no request waiting and no menu left, its busy line says it works.
            if session.sandbox.is_none() && session.status == AgentStatus::NeedsInput {
                let full = terminal.screen_text(60);
                if !session.approval_hold && view_busy(&full, &screen_tail(&full, 12), &busy_text) {
                    session.status = AgentStatus::Working;
                    session.activity.observe(session.status);
                }
                continue;
            }
            if session.hooks_active && session.sandbox.is_none() {
                continue;
            }
            let full = terminal.screen_text(60);
            let screen = screen_tail(&full, 12);
            let busy = view_busy(&full, &screen, &busy_text);
            if !busy && session.sandbox.is_some() {
                if screen.trim().is_empty() && session.status == AgentStatus::Starting {
                    continue;
                }
                let since = session.busy_absent_since.get_or_insert_with(Instant::now);
                if since.elapsed() < SCREEN_QUIET {
                    continue;
                }
            } else {
                session.busy_absent_since = None;
            }
            let new_turn = busy && !session.screen_busy;
            if new_turn || session.screen_busy && !busy {
                ref_checks.push(index);
            }
            session.screen_busy = busy;
            if session.status == AgentStatus::NeedsInput && !new_turn {
                continue;
            }
            session.status = match (busy, session.status) {
                (true, AgentStatus::Done) if !new_turn => AgentStatus::Done,
                (true, _) => AgentStatus::Working,
                (false, AgentStatus::Working) => {
                    finished.push(session.root());
                    AgentStatus::Done
                }
                (false, AgentStatus::Starting) => AgentStatus::Idle,
                (false, other) => other,
            };
            if new_turn {
                self.pending_questions.remove(&session.id);
            }
            session.activity.observe(session.status);
        }
        for (index, screen) in menus {
            self.sync_native(index, &screen, Instant::now());
        }
        for id in restricted_opened {
            self.session_event(&id, CODEX_OPENED_RESTRICTED);
        }
        for root in finished {
            self.diffs.entry(root).or_default().mark_dirty();
        }
        for (id, reason) in credential_errors {
            self.report(Some(&id), "blocked", &reason);
        }
        for index in ref_checks {
            self.check_refs(index);
        }
    }

    pub(super) fn close(&mut self, id: &str) {
        self.close_session(id, !self.restarting);
    }

    pub(super) fn close_for_resume(&mut self, id: &str) {
        if let Some(session) = self.sessions.iter().find(|s| s.id == id) {
            self.cancel_cache_cleanup(&session.root());
        }
        self.close_session(id, false);
    }

    pub(super) fn close_session(&mut self, id: &str, cleanup: bool) {
        let replacing = self.is_chief_replacement(id);
        if replacing {
            self.chief_switch = None;
        }
        self.pending_questions.remove(id);
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.keep_pane_masks(i);
            if self.sessions[i].chief {
                let mut hub = self.hub.lock().unwrap();
                hub.retire_chief(id, self.sessions[i].claude_id.as_deref());
                hub.chief_running = self.sessions.iter().any(|s| s.id != id && s.chief && s.can_receive());
            }
            if let Some(granted) = self.sessions[i].relay.take() {
                self.relay.revoke(&granted);
            }
            // Accept legitimate concurrent task commits before releasing this branch's ownership.
            let exempt = self.exempt_branches();
            let guard = self.sessions[i].ref_guard.take();
            self.check_all_refs(&exempt);
            let mut session = self.sessions.remove(i);
            // A closed plan's task worktree, kept while this session used it, may go now.
            let worktree = (!session.chief && session.scratch.is_none()).then(|| session.root());
            self.traces.remove(&session.id);
            if let Some(reply) = session.pending.as_mut().and_then(|pending| pending.reply.take()) {
                let _ =
                    reply.send(ControlOutcome::Done(ControlResponse::error("message not delivered: session closed")));
            }
            // A scratch folder goes with the last session in it, and so does its sandbox: neither is resumed.
            let scratch =
                session.scratch.clone().filter(|dir| !self.sessions.iter().any(|s| s.scratch.as_ref() == Some(dir)));
            // An exited agent's sandbox was stopped when it exited; one still preparing stops when ready.
            if let Some(name) = session.sandbox.clone() {
                let sbx = resolve_program(&self.sbx);
                let (actions, checks) = (self.action_tx.clone(), self.ref_checks.clone());
                let id = id.to_string();
                session.terminal.take();
                let scope = Scope {
                    sandboxes: vec![name.clone()],
                    worktrees: Some(
                        scratch.iter().cloned().chain(worktree.iter().filter(|_| cleanup).cloned()).collect(),
                    ),
                };
                self.lifecycle(scope, move || {
                    let home = paths::atomics_home();
                    let stopped = crate::sandbox::stop(&home, &sbx, &name);
                    let mut alerts = stopped
                        .err()
                        .map(|err| format!("Stopping sandbox FAILED: {err:#}"))
                        .into_iter()
                        .collect::<Vec<_>>();
                    if let Some(dir) = scratch {
                        let removed = crate::sandbox::remove(&home, &sbx, &name)
                            .and_then(|()| crate::lifecycle::remove_scratch(&home, &dir));
                        alerts.extend(
                            removed.err().map(|err| format!("Removing scratch session {name} FAILED: {err:#}")),
                        );
                    }
                    // The last check, once the agent cannot write any more.
                    let (done, finished) = std::sync::mpsc::channel();
                    checks.run(move || {
                        if let Some(guard) = &guard {
                            match guard.check(&exempt) {
                                Ok(found) => alerts.extend(found),
                                Err(err) => alerts.push(format!("Final ref guard check FAILED: {err:#}")),
                            }
                        }
                        if !alerts.is_empty() {
                            crate::lifecycle::record_leftovers(&paths::atomics_home(), &alerts);
                            let generation = guard.as_ref().and_then(|guard| guard.alert_generation());
                            let _ = actions
                                .send(Box::new(move |state| state.ref_alert_at(&id, alerts.join("\n"), generation)));
                        }
                        let _ = done.send(());
                    });
                    let _ = finished.recv();
                });
            } else if let Some(dir) = scratch {
                // Its processes are gone once the terminal is: closing it kills the process group.
                session.terminal.take();
                self.lifecycle(Scope { sandboxes: Vec::new(), worktrees: Some(vec![dir.clone()]) }, move || {
                    let home = paths::atomics_home();
                    if let Err(err) = crate::lifecycle::remove_scratch(&home, &dir) {
                        crate::lifecycle::record_leftovers(&home, &[format!("Scratch folder: {err:#}")]);
                    }
                });
            }
            if !session.chief
                && session.terminal.is_none()
                && let Some(cid) = session.claude_id
            {
                self.dismissed.insert(cid);
            }
            session.terminal.take();
            if ipc::safe_chief_root_id(id) {
                let _ = std::fs::remove_file(claude_settings_path(id));
            }
            if cleanup && let Some(worktree) = worktree {
                self.schedule_cache_cleanup(&worktree);
                self.clean_after_session(id, &worktree);
            }
        }
        if self.active.as_deref() == Some(id) {
            self.active = self.sessions.first().map(|s| s.id.clone());
        }
    }

    pub(super) fn remember(&mut self, id: &str) {
        if self.is_chief_replacement(id) {
            return;
        }
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else { return };
        // Scratch sessions end with ATOMICS; only a restart's handoff brings them back.
        if s.scratch.is_some() {
            return;
        }
        let Some(claude_id) = s.claude_id.clone() else { return };
        let record = self.session_record(s, claude_id, s.last_prompt.as_deref());
        if s.chief && s.hooks_active {
            self.store.chief_recovery = None;
        }
        self.store.upsert(record);
        if let Err(err) = self.store.save(&paths::store_file()) {
            eprintln!("ATOMICS: saving sessions failed: {err:#}");
        }
    }

    /// What resuming `s` needs, its conversation being `claude_session_id`: a restart's handoff, the record of
    /// what was running, a tile's Resume and the recent sessions all keep this.
    pub(super) fn session_record(
        &self,
        s: &Session,
        claude_session_id: String,
        last_prompt: Option<&str>,
    ) -> SessionRecord {
        let mut record = self.session_record_fields(s, claude_session_id, last_prompt);
        if s.chief {
            Self::mask_session_record(&mut record, &self.chief_cleaner(&self.cleaner()));
        }
        record
    }

    pub(super) fn session_record_fields(
        &self,
        s: &Session,
        claude_session_id: String,
        last_prompt: Option<&str>,
    ) -> SessionRecord {
        SessionRecord {
            chief: s.chief,
            pane: Some(s.id.clone()),
            claude_session_id,
            agent: s.agent.clone(),
            cwd: s.cwd.clone(),
            title: self.session_title(s),
            last_prompt: last_prompt.map(str::to_owned),
            updated: 0,
            sandbox: s.sandbox.clone(),
            managed_worktree: s.managed_worktree,
            named: s.naming == Naming::Named,
            unsaved: s.unsaved.clone(),
        }
    }

    pub(super) fn mask_session_record(record: &mut SessionRecord, cleaner: &atomics_core::tickets::Cleaner) {
        record.title = cleaner.text(&record.title);
        if let Some(prompt) = &mut record.last_prompt {
            *prompt = cleaner.text(prompt);
        }
        if let Some(prompt) = record.unsaved.as_mut().and_then(|unsaved| unsaved.prompt.as_mut()) {
            *prompt = cleaner.text(prompt);
        }
    }

    /// The first turn of conversation `id` started: Claude has saved it, and a resume continues it.
    pub(super) fn conversation_saved(&mut self, index: usize) {
        if self.sessions[index].unsaved.take().is_some() {
            let id = self.sessions[index].id.clone();
            self.remember(&id);
        }
    }

    /// How conversation `id` was started, if Claude never saved it ([`SessionRecord::unsaved`]): a first-run
    /// screen held its first prompt back, and then it ended. A bare agent's transcript is on this machine, and
    /// one found there wins: Claude may have saved it just before ATOMICS saw the turn start.
    fn unsaved_conversation(&self, id: &str, sandboxed: bool) -> Option<atomics_core::store::Unsaved> {
        let unsaved = self.store.sessions.iter().find(|record| record.claude_session_id == id)?.unsaved.clone()?;
        (sandboxed || !crate::setup::claude_conversation_saved(id)).then_some(unsaved)
    }

    /// Sends text to a session: typed into ATOMICS's own terminals, queued for the
    /// next hook of sessions running elsewhere.
    pub(super) fn deliver(&mut self, index: usize, text: &str) -> Delivery {
        self.deliver_with(index, text, false)
    }

    /// [`Self::deliver`]; with `now`, an agent busy in a turn takes the message at once rather than at the end
    /// of the step it is on. Claude and Codex queue what is typed while they work and say so on screen; once
    /// that shows, [`Self::send_now_keys`] presses the key that sends it now.
    pub(super) fn deliver_with(&mut self, index: usize, text: &str, now: bool) -> Delivery {
        let gate = self.governor.restarting.clone();
        let paused = gate.read().unwrap();
        if *paused {
            return Delivery::Paused;
        }
        if self.is_chief_replacement(&self.sessions[index].id) {
            return Delivery::Unavailable;
        }
        if !self.prepare_chief_send(index) {
            let session = &mut self.sessions[index];
            session.activity.held_since.get_or_insert_with(atomics_core::store::now);
            session.held.push(text.to_owned());
            session.held_now |= now;
            return Delivery::Held;
        }
        drop(paused);
        self.deliver_in_terminal(index, text, now)
    }

    pub(super) fn prepare_chief_send(&mut self, index: usize) -> bool {
        let config = self.chief_send_config(index);
        let session = &mut self.sessions[index];
        if session.send_masks_ready(config.as_deref()) {
            return true;
        }
        if let Some(config) = config
            && !matches!(session.chief_send_masks, ChiefSendMasks::Reading)
        {
            session.chief_send_masks = ChiefSendMasks::Reading;
            let id = session.id.clone();
            self.queue_trace(config, super::trace::TraceAction::Send(id));
        }
        false
    }

    pub(super) fn chief_send_config(&self, index: usize) -> Option<Arc<atomics_core::tickets::TicketConfig>> {
        self.sessions[index].chief.then(|| self.ticket_config().filter(|config| config.server.uses_oauth())).flatten()
    }

    pub(super) fn deliver_in_terminal(&mut self, index: usize, text: &str, now: bool) -> Delivery {
        let gate = self.governor.restarting.clone();
        let paused = gate.read().unwrap();
        if *paused {
            return Delivery::Paused;
        }
        let cleaner = self.sessions[index].chief.then(|| self.chief_cleaner(&self.cleaner()));
        let cleaned = cleaner.as_ref().map(|cleaner| cleaner.text(text));
        let text = cleaned.as_deref().unwrap_or(text);
        self.check_refs(index);
        let session = &mut self.sessions[index];
        if session.is_agent()
            && !session.approval_hold
            && session.terminal.as_ref().is_some_and(Terminal::permission_pending)
        {
            session.approval_hold = true;
        }
        if (session.chief && session.status == AgentStatus::Starting && session.terminal.is_none())
            || session.approval_hold
            || !session.approvals.is_empty()
            || session.pending.as_ref().is_some_and(|pending| pending.codex || pending.attachment.is_some())
        {
            session.activity.held_since.get_or_insert_with(atomics_core::store::now);
            session.first_prompt(text);
            session.held.push(text.to_string());
            // Held messages go together, at once if any was sent now. Held behind a message that waits in Codex's own
            // queue, they wait for it: Escape sends that one now, and these follow.
            session.held_now |= now;
            if let Some(pending) = session.pending.as_ref().filter(|pending| now && pending.codex && pending.queued) {
                let snippet = pending.text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();
                let snippet = snippet.trim().chars().take(24).collect();
                session.send_now = Some(SendNow { codex: true, snippet, since: Instant::now() });
            }
            return Delivery::Held;
        }
        if session.can_receive() {
            if let Some(root) = session.simple_context_for.take()
                && session.claude_id.as_ref() == Some(&root)
                && let Some(cleaner) = &cleaner
            {
                let line = cleaner.text(super::mutations::SIMPLE_CONTEXT);
                self.hub.lock().unwrap().prompt_context.insert(root, line);
            }
            session.first_prompt(text);
        }
        if let Some(terminal) = &session.terminal {
            if terminal.exited() {
                return Delivery::Unavailable;
            }
            let codex = session.chief && session.kind == AgentKind::Generic
                || self.config.agent(&session.agent).is_some_and(|agent| agent.is_codex());
            let screen_before = if codex { terminal.screen_text(200) } else { String::new() };
            terminal.submit_under(&paused, text);
            // An idle agent starts a turn with it anyway.
            if now
                && (session.kind == AgentKind::Claude || codex)
                && (session.status == AgentStatus::Working || session.screen_busy)
            {
                let snippet = text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();
                let snippet = snippet.trim().chars().take(24).collect();
                session.send_now = Some(SendNow { codex, snippet, since: Instant::now() });
            }
            let trackable = session.kind == AgentKind::Claude
                || codex
                || self.config.agent(&session.agent).is_some_and(|a| a.busy_text.is_some());
            if trackable {
                let was_working = session.status == AgentStatus::Working;
                session.pending = Some(Pending {
                    attachment: None,
                    text: text.to_string(),
                    sent: Instant::now(),
                    attempts: 1,
                    was_working,
                    codex,
                    screen_before,
                    paste_seen: false,
                    queued: false,
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

    /// Send `root`'s comments to its agent as one message. When they cannot go they stay, and the error says why.
    pub(super) fn send_review(&mut self, root: &Path, origin: &ControlOrigin) -> Result<Delivery, String> {
        let comments = self.diffs.get(root).map(|s| s.comments.clone()).unwrap_or_default();
        if comments.is_empty() {
            return Err("no pending comments".into());
        }
        let header = self.config.fill(&self.config.steer.review_header);
        let message = steer::compose_review(&header, &comments);
        let target = self
            .active_index()
            .filter(|&i| self.sessions[i].root() == root && self.sessions[i].is_agent())
            .or_else(|| self.sessions.iter().position(|s| s.root() == root && s.is_agent() && s.can_receive()));
        let Some(index) = target else {
            let reason = "No agent is working in this worktree";
            self.toast(reason);
            return Err(reason.into());
        };
        self.check_input_access(self.sessions[index].input_access, origin).map_err(|e| e.message)?;
        let count = comments.len();
        let delivery = self.deliver(index, &message);
        if delivery == Delivery::Paused {
            return Err("The host is preparing to stop".into());
        }
        if delivery == Delivery::Unavailable {
            return Err("could not deliver the review".into());
        }
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
        Ok(delivery)
    }
}

/// The last prompt belongs to the composer, unless a permission menu is open.
pub(super) fn composer_line(screen: &str) -> Option<&str> {
    if atomics_core::approvals::menu_visible(screen) {
        return None;
    }
    screen.lines().rev().find_map(|line| line.trim_start().strip_prefix(['›', '❯']).map(str::trim))
}

/// Empty composers contain a rotating placeholder. Compare with the line captured
/// before this paste instead of mistaking that placeholder for unsent text.
fn codex_composer_empty(screen: &str, before: &str) -> Option<bool> {
    Some(composer_line(screen)? == composer_line(before)?)
}

/// Codex renders pending steers/follow-ups with a header and indented arrow previews.
/// These can appear above or below the composer, including during compaction.
fn codex_queue_receipt(screen: &str, before: &str, text: &str) -> bool {
    fn previews(screen: &str) -> String {
        let mut queue = false;
        let legacy = screen.lines().any(|line| line.trim() == "Queued");
        screen
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.starts_with(['›', '❯']) {
                    queue = false;
                    return None;
                }
                if [
                    "• Queued follow-up inputs",
                    "• Messages to be submitted at end of turn",
                    "• Messages to be submitted after next tool call",
                ]
                .iter()
                .any(|header| line.starts_with(header))
                {
                    queue = true;
                    return None;
                }
                if queue {
                    Some(line.trim_start_matches('↳').trim())
                } else if legacy {
                    line.strip_prefix('•').map(str::trim)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    if atomics_core::approvals::menu_visible(screen) {
        return false;
    }
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    let current = previews(screen);
    let previous = previews(before);
    let current_compact = compact(&current);
    let previous_compact = compact(&previous);
    text.lines().filter(|line| !line.trim().is_empty()).any(|line| {
        let message = compact(line);
        let complete = if message.chars().count() >= 20 {
            current_compact.matches(&message).count() > previous_compact.matches(&message).count()
        } else {
            current.lines().filter(|line| compact(line) == message).count()
                > previous.lines().filter(|line| compact(line) == message).count()
        };
        complete
            || current.lines().any(|preview| {
                let prefix = compact(preview.trim_end_matches('…'));
                prefix.chars().count() >= 20
                    && message.starts_with(&prefix)
                    && current.lines().filter(|line| *line == preview).count()
                        > previous.lines().filter(|line| *line == preview).count()
            })
    })
}

/// A first or last message line newly visible above the composer is a receipt.
/// Count occurrences so sending the same message again needs new evidence too.
fn codex_transcript_receipt(screen: &str, before: &str, text: &str) -> bool {
    let transcript = |screen: &str| {
        let lines: Vec<_> = screen.lines().collect();
        let end = lines.iter().rposition(|line| line.trim_start().starts_with(['›', '❯'])).unwrap_or(0);
        lines[..end].join("\n")
    };
    if atomics_core::approvals::menu_visible(screen) {
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
        if message.chars().count() < 20 {
            // Short text can appear in the reply. Require a full user prompt above the composer.
            let prompts = |transcript: &str| {
                transcript
                    .lines()
                    .filter_map(|row| row.trim_start().strip_prefix(['›', '❯']))
                    .filter(|prompt| prompt.trim() == line.trim())
                    .count()
            };
            return prompts(&current) > prompts(&previous);
        }
        if current_compact.matches(&message).count() > previous_compact.matches(&message).count() {
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

/// How long a message sent with `now` may take to show up waiting before it is left to wait.
const SEND_NOW_WAIT: Duration = Duration::from_secs(10);
/// A submitted message's Enter follows its paste after 120 ms ([`Terminal::submit`]).
const SUBMITTED: Duration = Duration::from_millis(300);

/// Whether the agent's composer, its last prompt line, still holds text starting with `snippet`.
fn composer_holds(screen: &str, snippet: &str) -> bool {
    let composer = screen.lines().rev().find_map(|line| line.trim_start().strip_prefix(['>', '❯', '›']));
    composer.is_some_and(|line| !snippet.is_empty() && line.contains(snippet))
}

/// The keys that make a busy agent take what waits in its queue at once, read off what `screen` shows while a
/// message waits: Claude's "Queued · ctrl+x ctrl+s to send now" names its send-now chord (a person can bind
/// another), and Codex's "Messages to be submitted after next tool call (esc to interrupt and send immediately)"
/// its key. `None` while nothing waits; an error names a key that is not a control key ATOMICS can press.
fn send_now_keys(screen: &str, codex: bool) -> Option<Result<Vec<u8>, String>> {
    let (marker, separator) =
        if codex { ("to interrupt and send immediately", '(') } else { ("to send now", '\u{b7}') };
    // A narrow terminal wraps the hint: in the middle of a word, or between words.
    let lines: Vec<&str> = screen.lines().collect();
    let line = lines
        .iter()
        .rev()
        .find(|line| line.contains(marker))
        .map(|line| line.to_string())
        .or_else(|| [lines.join(""), lines.join(" ")].into_iter().find(|joined| joined.contains(marker)))?;
    let before = &line[..line.rfind(marker)?];
    let key = before.rsplit(separator).next().unwrap_or(before).trim();
    let key = key.strip_prefix("press ").unwrap_or(key).trim();
    let bytes: Option<Vec<u8>> = key
        .split_whitespace()
        .map(|part| {
            let part = part.to_ascii_lowercase();
            if part == "esc" {
                return Some(0x1b);
            }
            let letter = part.strip_prefix("ctrl+")?;
            (letter.len() == 1 && letter.as_bytes()[0].is_ascii_lowercase()).then(|| letter.as_bytes()[0] & 0x1f)
        })
        .collect();
    Some(bytes.filter(|bytes| !bytes.is_empty()).ok_or_else(|| key.to_string()))
}

/// How often a working agent's status line reaches clients when only its numbers move.
const LIVE_EVERY: Duration = Duration::from_secs(2);

/// Takes a reading of an agent's status line into `shown`, what clients are sent: at once when the line appears,
/// goes or changes its words; when only its numbers move, at most every [`LIVE_EVERY`], and not for its time
/// ticking on as a client counts it from the last reading. True when `shown` changed.
fn take_live(
    shown: &mut Option<atomics_core::status_line::LiveLine>,
    read: &mut Option<Instant>,
    reading: Option<atomics_core::status_line::LiveLine>,
    now: Instant,
) -> bool {
    let changed = match (&*shown, &reading) {
        (None, None) => false,
        (Some(old), Some(new)) if old.verb == new.verb => {
            let since = read.map_or(Duration::MAX, |at| now.saturating_duration_since(at));
            let counted = old.elapsed.map(|elapsed| elapsed.saturating_add(since.as_secs()));
            let drift = match (counted, new.elapsed) {
                (Some(counted), Some(elapsed)) => counted.abs_diff(elapsed) > 2,
                (None, None) => false,
                _ => true,
            };
            since >= LIVE_EVERY && (old.tokens != new.tokens || old.thought != new.thought || drift)
        }
        _ => true,
    };
    if changed {
        *shown = reading;
        *read = Some(now);
    }
    changed
}

/// Longest first prompt passed on the command line to agents other than Claude.
const LONG_PROMPT: usize = 700;

/// What Claude Code and Codex show while they work: a sandboxed agent's status, since its hooks
/// cannot reach ATOMICS.
const SANDBOX_BUSY: &str = "esc to interrupt";
const SCREEN_QUIET: Duration = Duration::from_millis(800);

/// The last `lines` lines of `screen`.
fn screen_tail(screen: &str, lines: usize) -> String {
    let all: Vec<_> = screen.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// Busy by the footer, or, in an agent view scrolled up from its live end (it shows a "Jump to
/// bottom" pill instead of its footer), by the most recent spinner or status line on the screen: busy
/// text the transcript quotes never counts there. The terminal's own scroll position never matters:
/// the screen read is always the live one.
fn view_busy(full: &str, tail: &str, busy: &str) -> bool {
    if !screen_busy(tail, "jump to bottom") {
        return screen_busy(tail, busy);
    }
    let lines: Vec<_> = full.lines().collect();
    let Some(at) = lines.iter().rposition(|line| status_line(line)) else { return false };
    let mut status = lines[at].to_string();
    // A status too long for the screen wraps onto the next line.
    if status.rfind('(') > status.rfind(')')
        && let Some(next) = lines.get(at + 1)
    {
        status.push_str(next);
    }
    screen_busy(&status, busy)
}

/// A spinner or status line, as Claude Code and Codex draw them below the transcript: a glyph, a word, then
/// an ellipsis or a status in parentheses (`✻ Building… (5m · esc to interrupt)`, `• Working (5s • esc to
/// interrupt)`), or what a finished turn leaves (`✻ Worked for 5m 3s`, `─ Worked for 1m 05s ─`).
fn status_line(line: &str) -> bool {
    let mut chars = line.trim().chars();
    let Some(glyph) = chars.next() else { return false };
    if glyph.is_alphanumeric() || glyph.is_ascii() && glyph != '*' {
        return false;
    }
    let Some(rest) = chars.as_str().strip_prefix(' ') else { return false };
    let word = rest.split(|c: char| !c.is_alphabetic()).next().unwrap_or_default();
    if !word.starts_with(char::is_uppercase) {
        return false;
    }
    let after = &rest[word.len()..];
    after.starts_with('…')
        || after.starts_with("...")
        || after.starts_with(" (")
        || after.strip_prefix(" for ").is_some_and(|time| time.starts_with(|c: char| c.is_ascii_digit()))
}

/// How much of an agent's screen is its footer and the prompt it sits around: Claude Code's line at the very bottom,
/// Codex's above its prompt.
pub(super) const FOOTER_LINES: usize = 12;

/// What an agent's footer says runs in the background, which a restart would lose: Claude Code's `1 shell`,
/// `2 local agents`, `1 shell, 1 monitor`, and Codex's `1 background terminal (cargo test) running`. A footer item is
/// the whole of what stands between its `·` separators, so a line of the transcript that mentions a shell is not one.
pub(super) fn background_work(screen: &str) -> Option<String> {
    let lines: Vec<_> = screen.lines().collect();
    let items: Vec<_> = lines[lines.len().saturating_sub(FOOTER_LINES)..]
        .iter()
        .flat_map(|line| line.split('·'))
        .map(str::trim)
        .filter(|item| background_item(item))
        .collect();
    (!items.is_empty()).then(|| items.join(", "))
}

/// What `3 things` counts.
fn counted(part: &str) -> Option<&str> {
    part.split_once(' ').filter(|(count, _)| count.parse::<u32>().is_ok()).map(|(_, what)| what)
}

fn background_item(item: &str) -> bool {
    if counted(item).is_some_and(|what| what.starts_with("background terminal")) {
        return true;
    }
    item.split(", ").all(|part| {
        counted(part).is_some_and(|what| {
            matches!(
                what,
                "shell"
                    | "shells"
                    | "monitor"
                    | "monitors"
                    | "local agent"
                    | "local agents"
                    | "team"
                    | "teams"
                    | "background task"
                    | "background tasks"
                    | "MCP task"
                    | "MCP tasks"
            )
        })
    })
}

/// Footers can wrap or change case, including Codex's `esc to interrupt` and Claude's `Esc to interrupt`.
fn screen_busy(screen: &str, busy: &str) -> bool {
    let compact =
        |text: &str| text.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect::<String>();
    !busy.trim().is_empty() && compact(screen).contains(&compact(busy))
}

/// Writes a long prompt to a brief file and returns the short prompt that points to it. A
/// sandboxed session's brief goes in its own folder, the one its sandbox mounts read-only.
/// A new, empty folder in `root` for one scratch session.
fn new_scratch_folder(root: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    loop {
        let dir = root.join(&uuid::Uuid::new_v4().simple().to_string()[..8]);
        match std::fs::create_dir(&dir) {
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            other => return other.map(|()| dir),
        }
    }
}

fn claude_settings_path(id: &str) -> PathBuf {
    paths::atomics_home().join("claude-settings").join(format!("{id}.json"))
}

fn codex_restricted_projects(cwd: &Path) -> String {
    let projects = cwd
        .ancestors()
        .map(|dir| format!("{} = {{ trust_level = \"untrusted\" }}", quoted(&dir.to_string_lossy())))
        .collect::<Vec<_>>()
        .join(", ");
    format!("projects={{ {projects} }}")
}

pub(super) struct RestrictedEntry {
    cwd: PathBuf,
    answered: bool,
}

impl RestrictedEntry {
    pub(super) fn new(cwd: &Path) -> Self {
        Self { cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()), answered: false }
    }

    fn open(&mut self, terminal: &Terminal, screen: &str) -> bool {
        let Some(folder) = codex_restricted_folder(screen)
            .filter(|folder| !self.answered && self.cwd.ancestors().any(|dir| dir.to_str() == Some(folder)))
        else {
            return false;
        };
        if terminal.write_if_screen(60, |current| codex_restricted_folder(current) == Some(folder), b"\r") {
            self.answered = true;
            return true;
        }
        false
    }
}

/// Keep repository config disabled while allowing routine commands inside the task sandbox.
fn codex_task_permissions(args: &mut Vec<String>) {
    let mut approval = false;
    let mut sandbox = false;
    let mut options = args.iter().map(String::as_str);
    while let Some(arg) = options.next() {
        if matches!(arg, "--approve-for-me" | "--not-so-yolo" | "--dangerously-bypass-approvals-and-sandbox" | "--yolo")
        {
            return;
        }
        let flag = arg.split_once('=').map_or(arg, |(name, _)| name);
        approval |= flag == "--ask-for-approval" || arg.starts_with("-a");
        sandbox |= flag == "--sandbox" || arg.starts_with("-s");
        let setting = match arg {
            "-c" | "--config" => options.next(),
            _ => arg.strip_prefix("--config=").or_else(|| arg.strip_prefix("-c").map(|s| s.trim_start_matches('='))),
        };
        if let Some((key, _)) = setting.and_then(|value| value.split_once('=')) {
            let key = key.trim();
            approval |= key == "approval_policy" || key.starts_with("approval_policy.");
            sandbox |= matches!(key, "sandbox_mode" | "default_permissions");
        }
    }
    if !approval {
        args.extend(["-a".into(), "on-request".into()]);
    }
    if !sandbox {
        args.extend(["--sandbox".into(), "workspace-write".into()]);
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn codex_chief_options(args: &[String], allow_bypass: bool) -> Result<(), String> {
    let reasoning = |setting: &str| {
        setting.split_once('=').is_some_and(|(key, value)| {
            key.trim() == "model_reasoning_effort"
                && matches!(
                    value.trim().trim_matches(['\'', '"']),
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
        })
    };
    let model = |value: &str| atomics_core::config::ChiefConfig::model_problem(value).is_none();
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        let allowed = match arg {
            "--no-daemon" => true,
            CODEX_CHIEF_BYPASS => allow_bypass,
            "--model" | "-m" => args.next().is_some_and(model),
            "--config" | "-c" => args.next().is_some_and(reasoning),
            _ => {
                arg.strip_prefix("--model=").or_else(|| arg.strip_prefix("-m=")).is_some_and(model)
                    || arg.strip_prefix("--config=").or_else(|| arg.strip_prefix("-c=")).is_some_and(reasoning)
            }
        };
        if !allowed {
            return Err(format!(
                "Codex chief options may only set the model or reasoning effort, disable the daemon, or use {CODEX_CHIEF_BYPASS} in [chief] args"
            ));
        }
    }
    Ok(())
}

pub(super) fn chief_prompt_input(session: &str, text: &str, provider: ChiefAgent) -> Result<String, String> {
    if provider != ChiefAgent::Codex || text.len() <= LONG_PROMPT {
        return Ok(text.to_owned());
    }
    let path = write_brief(session, text, false)?;
    Ok(format!("{} Read the data in {}.", super::chief_provider::HANDOFF_PREAMBLE, path.display()))
}

fn brief_file(session: &str, text: &str, sandboxed: bool) -> Result<String, String> {
    let path = write_brief(session, text, sandboxed)?;
    Ok(format!("Your task brief is in {}. Read it first and follow it exactly.", path.display()))
}

fn write_brief(session: &str, text: &str, sandboxed: bool) -> Result<PathBuf, String> {
    let dir = if sandboxed { paths::briefs_dir().join(session) } else { paths::briefs_dir() };
    atomics_core::store::private_dir(&paths::atomics_home()).map_err(|e| format!("creating state: {e}"))?;
    atomics_core::store::private_dir(&paths::briefs_dir()).map_err(|e| format!("creating briefs: {e}"))?;
    let path = if sandboxed { dir.join("brief.md") } else { dir.join(format!("{session}.md")) };
    if sandboxed {
        use std::os::unix::fs::PermissionsExt;
        // sbx mounts this session directory read-only. Preserve its previous 0755/0644
        // access: the guest agent may have a different uid from the host user. Bare briefs
        // remain private; do not require host ownership through an unverified uid mapping.
        std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("setting permissions on {}: {e}", dir.display()))?;
        std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .map_err(|e| format!("setting permissions on {}: {e}", path.display()))?;
    } else {
        atomics_core::store::write_private(&path, text.as_bytes())
            .map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// Authentication failures must remain actionable even when the provider exits normally.
pub(super) fn credential_failure(screen: &str) -> Option<String> {
    screen
        .lines()
        .rev()
        .find(|line| {
            let text = line.to_ascii_lowercase();
            text.contains("authentication_failed")
                || text.contains("authentication failed")
                || text.contains("invalid api key")
                || text.contains("invalid_api_key")
                || text.contains("401")
                    && (text.contains("error") || text.contains("unauthorized") || text.contains("http"))
        })
        .map(|line| format!("Credential error: {}", line.trim()))
}

#[cfg(test)]
impl Desktop<'_> {
    /// Sends the steer bar's message as the bar's Send does (`atomics_ui::chief::send_steer`), for tests.
    pub(super) fn send_steer(&mut self) {
        self.draw(|backend, client| atomics_ui::chief::send_steer(backend, client, false));
    }
}

impl State {
    /// Steers each of `sessions`, by id, with `text`, and says in a toast how it went. Each target is one
    /// steer operation, through the journal as a protocol client's is.
    pub(super) fn steer_sessions(&mut self, sessions: Vec<String>, text: String, now: bool, simple: bool) {
        use atomics_core::ops::{Command, Outcome, SessionAction, SessionCommand};
        let mut typed = 0;
        let mut queued = 0;
        let mut held = 0;
        let mut verifying = 0;
        for id in sessions {
            let steer = |op| {
                let action = SessionAction::Steer { session: id.clone(), text: text.clone(), now, simple };
                Command::SessionCommand(SessionCommand { op, action })
            };
            match self.mutate_desktop(steer) {
                Outcome::Accepted => verifying += 1,
                Outcome::Done { result } if result["held"] == true => held += 1,
                Outcome::Done { result } if result["queued_until_next_hook"] == true => queued += 1,
                Outcome::Done { .. } => typed += 1,
                _ => {}
            }
        }
        let mut parts = Vec::new();
        if verifying > 0 {
            parts.push(format!("Verifying delivery to {verifying} session(s)"));
        }
        if typed > 0 {
            parts.push(format!("Sent to {typed} session{}", if typed == 1 { "" } else { "s" }));
        }
        if queued > 0 {
            parts.push(format!("Queued for {queued} outside ATOMICS"));
        }
        if held > 0 {
            parts.push(format!("Held for {held} until the session is ready"));
        }
        let summary = if parts.is_empty() { "No session could receive the message".into() } else { parts.join(", ") };
        self.toast(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app, with_app_env};
    use super::*;

    #[test]
    fn generic_host_launchers_require_the_desktop() {
        with_app_env(
            "app::sessions::tests::generic_host_launchers_require_the_desktop",
            |dir| {
                let bin = dir.join("bin");
                std::fs::create_dir(&bin).unwrap();
                for name in ["claude", "codex", "unknown-wrapper"] {
                    crate::test_support::write_program(bin.join(name), "#!/bin/sh\nexit 0\n");
                }
                vec![("PATH", crate::test_support::path_with(&bin))]
            },
            |app, _| {
                if crate::vm::is_guest() {
                    return;
                }
                let mut agent = app.runtime.config.agent("codex").unwrap().clone();
                let other = app.runtime.launch_dir.join("other");
                std::fs::create_dir(&other).unwrap();
                let named_claude = other.join("claude");
                crate::test_support::write_program(&named_claude, "#!/bin/sh\nexec /bin/sh\n");
                for command in [
                    &["env", "bash"][..],
                    &["sh", "-c", "true"],
                    &["unknown-wrapper"],
                    &["./codex"],
                    &[named_claude.to_str().unwrap()],
                ] {
                    agent.command = command.iter().map(|arg| (*arg).into()).collect();
                    let access = app.runtime.bare_input_access(&agent);
                    assert_eq!(access, InputAccess::Desktop, "{command:?}");
                    assert!(access.check(true).is_err(), "{command:?}");
                }
                let wrapper = app.runtime.launch_dir.join("bin/unknown-wrapper");
                app.runtime.config.remote =
                    toml::from_str(&format!("host_input_wrappers = {}", json!([wrapper, named_claude]),)).unwrap();
                let resolved_claude = resolve_program("claude");
                for command in
                    ["claude", "codex", &resolved_claude, wrapper.to_str().unwrap(), named_claude.to_str().unwrap()]
                {
                    agent.command = vec![command.into()];
                    let access = app.runtime.bare_input_access(&agent);
                    assert_eq!(access, InputAccess::Host, "{command}");
                    assert!(access.check(true).is_ok(), "{command}");
                    assert!(access.check(false).is_err(), "{command}");
                }
            },
        );
    }

    #[test]
    fn a_provider_name_alone_does_not_establish_vm_isolation() {
        let agent = atomics_core::config::AgentConfig {
            name: "vm assistant".into(),
            kind: AgentKind::Generic,
            isolation: Some(IsolationMode::Bare),
            command: vec!["./codex".into()],
            busy_text: None,
        };
        assert_eq!(bare_remote_agent(&agent), crate::vm::is_guest());
    }

    #[test]
    fn p5_review_finished_agents_ignore_startup_prose() {
        with_app("app::sessions::tests::p5_review_finished_agents_ignore_startup_prose", |app, _| {
            use atomics_ui::Backend;
            let dir = crate::test_support::tempdir().unwrap();
            for text in [
                "Next: review hooks in hook.rs",
                "Added a Login with GitHub button",
                "Users can sign in with your account",
                "Review hooks",
            ] {
                for status in [AgentStatus::Idle, AgentStatus::Done] {
                    let terminal = Terminal::spawn(
                        app.runtime.wake.clone(),
                        app.runtime.theme.terminal_colors(),
                        "/bin/sh".into(),
                        vec!["-c".into(), "printf '%s' \"$1\"; exec sleep 600".into(), "screen".into(), text.into()],
                        dir.path(),
                        HashMap::new(),
                        100,
                    )
                    .unwrap();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while !terminal.screen_text(60).contains(text) {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let mut worker = session("worker", AgentKind::Generic, status, dir.path());
                    worker.hooks_active = true;
                    worker.last_prompt = Some("Build the page".into());
                    worker.last_message = Some(text.into());
                    worker.terminal = Some(terminal);
                    app.runtime.sessions = vec![worker];
                    for _ in 0..2 {
                        app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                        app.runtime.track_busy();
                        let row = app.runtime.backend().session_row(0).unwrap();
                        assert_eq!(row.status, status, "{text}");
                        assert_eq!(row.status_hint, None);
                        assert!(!row.needs_user());
                    }
                }
            }
        });
    }

    #[test]
    fn p5_review_rows_use_the_last_sampled_startup_hint() {
        with_app("app::sessions::tests::p5_review_rows_use_the_last_sampled_startup_hint", |app, _| {
            use atomics_ui::Backend;
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Generic,
                busy_text: Some("esc to interrupt".into()),
                isolation: None,
            });
            let dir = crate::test_support::tempdir().unwrap();
            let terminal = Terminal::spawn(app.runtime.wake.clone(), app.runtime.theme.terminal_colors(), "/bin/sh".into(),
                vec!["-c".into(), "printf 'Choose a theme\\n'; read answer; printf '\\033[2J\\033[HWorking (esc to interrupt)\\n› '; exec sleep 600".into()],
                dir.path(), HashMap::new(), 100).unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            while !terminal.screen_text(60).contains("Choose a theme") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            let mut worker = session("worker", AgentKind::Generic, AgentStatus::Starting, dir.path());
            worker.terminal = Some(terminal);
            app.runtime.sessions = vec![worker];
            for _ in 0..2 {
                app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                app.runtime.track_busy();
                assert_eq!(app.runtime.backend().session_row(0).unwrap().status_label(), "first-run setup");
            }
            let terminal = app.runtime.sessions[0].terminal.as_ref().unwrap();
            terminal.write(b"2\r".to_vec());
            while !terminal.screen_text(60).contains("esc to interrupt") {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            for _ in 0..10 {
                assert_eq!(
                    app.runtime.backend().session_row(0).unwrap().status_label(),
                    "first-run setup",
                    "a row reads the sample, not the terminal"
                );
            }
            app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
            app.runtime.track_busy();
            let row = app.runtime.backend().session_row(0).unwrap();
            assert_eq!(row.status, AgentStatus::Working);
            assert_eq!(row.status_hint, None);
        });
    }

    #[test]
    fn audit_p7_briefs_keep_bare_prompts_private_and_restore_sandbox_mount_permissions() {
        with_app(
            "app::sessions::tests::audit_p7_briefs_keep_bare_prompts_private_and_restore_sandbox_mount_permissions",
            |_app, _| {
                use std::os::unix::fs::PermissionsExt;
                for sandboxed in [false, true] {
                    if sandboxed {
                        // Repair a session directory and brief tightened by the previous version too.
                        atomics_core::store::write_private(&paths::briefs_dir().join("worker/brief.md"), b"old")
                            .unwrap();
                    }
                    brief_file("worker", "private prompt", sandboxed).unwrap();
                    let path = if sandboxed {
                        paths::briefs_dir().join("worker/brief.md")
                    } else {
                        paths::briefs_dir().join("worker.md")
                    };
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "private prompt");
                    assert_eq!(
                        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                        if sandboxed { 0o644 } else { 0o600 }
                    );
                    assert_eq!(
                        std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
                        if sandboxed { 0o755 } else { 0o700 }
                    );
                    for dir in [paths::atomics_home(), paths::briefs_dir()] {
                        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
                    }
                }
            },
        );
    }

    #[test]
    fn a_status_line_reaches_clients_when_it_changes_and_its_numbers_at_most_every_two_seconds() {
        use atomics_core::status_line::LiveLine;
        let at = Instant::now();
        let line = |verb: &str, elapsed: u64, tokens: u64| {
            Some(LiveLine { verb: verb.into(), elapsed: Some(elapsed), tokens: Some(tokens), thought: None })
        };
        let (mut shown, mut read) = (None, None);
        assert!(take_live(&mut shown, &mut read, line("Fermenting", 14, 1100), at), "it appears");
        // Its time ticking on as a client counts it, and tokens within two seconds: nothing new to send.
        assert!(!take_live(&mut shown, &mut read, line("Fermenting", 15, 1200), at + Duration::from_secs(1)));
        assert!(!take_live(&mut shown, &mut read, line("Fermenting", 17, 1100), at + Duration::from_secs(3)));
        assert!(take_live(&mut shown, &mut read, line("Fermenting", 17, 1500), at + Duration::from_secs(3)));
        assert_eq!(shown.as_ref().and_then(|l| l.tokens), Some(1500));
        // New words at once; a time that jumped (a new turn) once due; gone at once.
        assert!(take_live(&mut shown, &mut read, line("Pondering", 17, 1500), at + Duration::from_millis(3400)));
        assert!(take_live(&mut shown, &mut read, line("Pondering", 1, 1500), at + Duration::from_millis(5500)));
        assert!(take_live(&mut shown, &mut read, None, at + Duration::from_millis(5600)));
        assert_eq!(shown, None);
        assert!(!take_live(&mut shown, &mut read, None, at + Duration::from_secs(9)));
    }

    #[test]
    fn background_work_is_what_a_footer_lists_and_nothing_a_transcript_says() {
        // Claude Code's footer, as it stood with one background shell running.
        let live = "❯\n─────\n  Claude · session 96% · weekly 14%\n  ⏵⏵ auto mode on · 1 shell · ← 1 agent";
        assert_eq!(background_work(live).as_deref(), Some("1 shell"));
        for item in [
            "2 shells",
            "1 shell, 1 monitor",
            "3 monitors",
            "1 local agent",
            "2 local agents",
            "1 team",
            "2 background tasks",
            "1 MCP task",
        ] {
            assert_eq!(background_work(&format!("  ⏵⏵ auto mode on · {item}")).as_deref(), Some(item));
        }
        // Codex's, above its prompt.
        let codex =
            "  1 background terminal (cargo test) running · /ps to view · /stop to close\n\n› \n\n  gpt · 80% left";
        assert_eq!(background_work(codex).as_deref(), Some("1 background terminal (cargo test) running"));
        let codex = "  2 background terminals running · /ps to view · /stop to close";
        assert_eq!(background_work(codex).as_deref(), Some("2 background terminals running"));

        for quiet in [
            "",
            "  ⏵⏵ auto mode on · ← 1 agent",
            "  Claude · session 96% · weekly 14%",
            "  • No background terminals running.",
            "  1 cloud session",
            "It left 1 shell running",
            "fixed 2 shells and more",
            "shell",
        ] {
            assert_eq!(background_work(quiet), None, "{quiet:?}");
        }
        // Scrolled out of the footer, it is the transcript's.
        let old = format!("1 shell\n{}", "line\n".repeat(FOOTER_LINES));
        assert_eq!(background_work(&old), None);
    }

    #[test]
    fn two_instances_do_not_overwrite_each_others_provider_trust_slots() {
        let dir = crate::test_support::tempdir().unwrap();
        let user = dir.path().join("user");
        std::fs::create_dir(&user).unwrap();
        for instance in ["a", "b"] {
            let home = dir.path().join(instance);
            std::fs::create_dir(&home).unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "app::sessions::tests::isolated_agents_write_provider_trust_only_inside_the_instance",
                        "--nocapture",
                    ])
                    .env("ATOMICS_APP_TEST_CHILD", "1")
                    .env("HOME", &user)
                    .env("CODEX_HOME", user.join("codex"))
                    .env("CLAUDE_CONFIG_DIR", user.join("claude"))
                    .env("ATOMICS_HOME", &home)
                    .env("ATOMICS_CONFIG", home.join("config.toml"))
                    .env("ATOMICS_SOCKET", home.join("socket")),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        // The provider has one trust slot per event; separate homes keep those slots separate.
        std::fs::write(dir.path().join("a/codex/config.toml"), "instance a trust").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("b/codex/config.toml")).unwrap(), "trusted\n");
        assert!(!user.join("codex/config.toml").exists());
        assert!(!user.join("claude/.claude.json").exists());
    }

    #[test]
    fn sandbox_credential_errors_block_the_task_with_the_reason_even_after_exit() {
        with_app(
            "app::sessions::tests::sandbox_credential_errors_block_the_task_with_the_reason_even_after_exit",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                for (text, exit) in [("API Error: 401 authentication_failed", false), ("HTTP 401 Unauthorized", true)] {
                    let terminal = Terminal::spawn(
                        app.runtime.wake.clone(),
                        app.runtime.theme.terminal_colors(),
                        "/bin/sh".into(),
                        vec![
                            "-c".into(),
                            format!("printf '%s' '{text}'; {}", if exit { "exit 1" } else { "exec sleep 600" }),
                        ],
                        dir.path(),
                        HashMap::new(),
                        100,
                    )
                    .unwrap();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
                    while !terminal.screen_text(60).contains(text) || exit && !terminal.exited() {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let mut worker = session("worker", AgentKind::Claude, AgentStatus::Working, dir.path());
                    worker.sandbox = Some("atomics-worker".into());
                    worker.terminal = Some(terminal);
                    app.runtime.sessions = vec![worker];
                    app.runtime.plans[0].tasks = vec![Task {
                        id: "a".into(),
                        session: Some("worker".into()),
                        status: TaskStatus::Running,
                        ..Default::default()
                    }];
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
                    loop {
                        app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                        app.runtime.runtime_tick();
                        if app.runtime.sessions[0].status == AgentStatus::NeedsInput
                            && app.runtime.plans[0].tasks[0].status == TaskStatus::Blocked
                            && app.runtime.plans[0].tasks[0].report.as_ref().is_some_and(|r| r.summary.contains(text))
                            && app.runtime.sessions[0].last_message.as_ref().is_some_and(|m| m.contains(text))
                        {
                            break;
                        }
                        assert!(Instant::now() < deadline, "credential failure did not block the task");
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    // It stays blocked on the following tick, including after the terminal exits.
                    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                    app.runtime.runtime_tick();
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                    assert_eq!(app.runtime.plans[0].tasks[0].status, TaskStatus::Blocked);
                    assert!(app.runtime.plans[0].tasks[0].report.as_ref().unwrap().summary.contains(text));
                    assert!(app.runtime.sessions[0].last_message.as_ref().unwrap().contains(text));
                    app.runtime.sessions.clear();
                }
            },
        );
    }

    #[test]
    fn explicit_codex_task_permissions_win_independently() {
        let approval = &["-a", "on-request"][..];
        let sandbox = &["--sandbox", "workspace-write"][..];
        for (given, defaults) in [
            (&["-a", "never"][..], sandbox),
            (&["--ask-for-approval=never"], sandbox),
            (&["-s", "read-only"], approval),
            (&["--sandbox=read-only"], approval),
            (&["-anever", "-s=read-only"], &[]),
            (&["-c", "approval_policy = 'never'"], sandbox),
            (&["--config=sandbox_mode='read-only'"], approval),
            (&["-cdefault_permissions=':read-only'"], approval),
            (&["--approve-for-me"], &[]),
            (&["--dangerously-bypass-approvals-and-sandbox"], &[]),
        ] {
            let mut args: Vec<_> = given.iter().map(|arg| arg.to_string()).collect();
            codex_task_permissions(&mut args);
            assert_eq!(args, [given, defaults].concat(), "{given:?}");
        }
    }

    #[test]
    fn fresh_and_restored_codex_task_worktrees_reach_the_prompt_with_only_their_generated_hooks() {
        super::super::tests::with_app_env(
            "app::sessions::tests::fresh_and_restored_codex_task_worktrees_reach_the_prompt_with_only_their_generated_hooks",
            |_| vec![("ATOMICS_AGENT_CONFIG", "user".into())],
            |app, _| {
                let root = app.runtime.launch_dir.join("task-repository");
                std::fs::create_dir_all(root.join(".codex")).unwrap();
                std::fs::write(root.join(".codex/config.toml"), "developer_instructions = 'repo instructions'\n")
                    .unwrap();
                std::fs::write(root.join(".codex/hooks.json"), "{}").unwrap();
                git::git(&root, &["init", "-q", "-b", "main"]).unwrap();
                git::git(&root, &["add", "."]).unwrap();
                git::git(
                    &root,
                    &[
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.invalid",
                        "commit",
                        "-qm",
                        "fixture",
                    ],
                )
                .unwrap();
                app.runtime.config.base_ref = "main".into();
                std::fs::create_dir_all(crate::setup::user_codex_home()).unwrap();
                let programs = paths::home().join("test-agents");
                std::fs::create_dir_all(&programs).unwrap();
                let codex = programs.join("codex");
                crate::test_support::write_program(
                    &codex,
                    r#"#!/usr/bin/env python3
import json, pathlib, sys, time
if "--version" in sys.argv:
    print("codex-cli 0.169.0")
    sys.exit(0)
args = sys.argv[1:]
pathlib.Path("fixture-args.json").write_text(json.dumps(args))
if any(arg.startswith("projects=") and "untrusted" in arg for arg in args):
    print(f"Folder access\n{pathlib.Path.cwd().parent}\n\n› 1. Open restricted\n  2. Back to Agent Command Center\n\nenter continue · esc back", flush=True)
    sys.stdin.readline()
    if any(arg.startswith("hooks.state=") for arg in args):
        print("\033[2J\033[H› Ask Codex to do anything", flush=True)
    else:
        print("Hooks need review", flush=True)
else:
    print("Trust this folder?\n› 1. Trust and continue\n  2. Cancel", flush=True)
time.sleep(60)
"#,
                );
                app.runtime.config.agents.iter_mut().find(|agent| agent.name == "codex").unwrap().command =
                    vec![codex.to_string_lossy().into_owned()];
                let (tx, rx) = channel();
                app.runtime.control(
                    serde_json::from_value(json!({
                        "cmd":"start", "agent":"codex", "repo":root, "branch":"fixture-work", "isolation":"bare"
                    }))
                    .unwrap(),
                    tx,
                );
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                let outcome = loop {
                    app.runtime.runtime_tick();
                    if let Ok(ControlOutcome::Done(outcome)) = rx.try_recv() {
                        break outcome;
                    }
                    assert!(Instant::now() < deadline, "the task worktree did not start");
                    std::thread::sleep(Duration::from_millis(10));
                };
                assert!(outcome.ok, "{:?}", outcome.error);
                let state = &mut *app.runtime;
                let id = outcome.result["id"].as_str().unwrap();
                let index = state.find(id).unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                loop {
                    state.runtime_tick();
                    if let Some(terminal) = &state.sessions[index].terminal {
                        let screen = terminal.screen_text(60);
                        if screen.contains("Ask Codex to do anything") {
                            assert!(!screen.contains("Folder access") && !screen.contains("Hooks need review"));
                            break;
                        }
                        assert!(
                            !screen.contains("Trust this folder?") && !screen.contains("Hooks need review"),
                            "{screen}"
                        );
                    }
                    assert!(Instant::now() < deadline, "the task never reached its composer");
                    std::thread::sleep(Duration::from_millis(10));
                }
                let cwd = &state.sessions[index].cwd;
                let args: Vec<String> =
                    serde_json::from_slice(&std::fs::read(cwd.join("fixture-args.json")).unwrap()).unwrap();
                let projects = codex_restricted_projects(&cwd.canonicalize().unwrap());
                assert!(args.windows(2).any(|pair| pair == ["-c", &projects]));
                assert!(args.windows(2).any(|pair| pair == ["-a", "on-request"]));
                assert!(args.windows(2).any(|pair| pair == ["--sandbox", "workspace-write"]));
                let trust = args.iter().find_map(|arg| arg.strip_prefix("hooks.state=")).unwrap();
                let decoded = toml::from_str::<toml::Table>(&format!("trust = {trust}")).unwrap();
                let actual = decoded["trust"].as_table().unwrap();
                let expected = atomics_core::hook::codex_hook_trust(
                    &crate::setup::hook_executable().unwrap(),
                    &paths::socket_path(),
                );
                assert_eq!(actual.len(), expected.len());
                for hook in expected {
                    assert_eq!(actual[&hook.key]["trusted_hash"].as_str(), Some(hook.hash.as_str()));
                }
                assert!(cwd.join(".codex/config.toml").is_file());
                assert!(!crate::setup::user_codex_home().join("config.toml").exists());

                let record = state.session_record_fields(&state.sessions[index], "saved-thread".into(), None);
                let mut legacy = serde_json::to_value(&record).unwrap();
                assert_eq!(legacy.as_object_mut().unwrap().remove("managed_worktree"), Some(json!(true)));
                let legacy: SessionRecord = serde_json::from_value(legacy).unwrap();
                assert!(state.legacy_codex_worktree(&legacy.cwd));
                let args_file = record.cwd.join("fixture-args.json");
                std::fs::remove_file(&args_file).unwrap();
                state.close_for_resume(id);
                state
                    .config
                    .agents
                    .iter_mut()
                    .find(|agent| agent.name == "codex")
                    .unwrap()
                    .command
                    .push("--dangerously-bypass-approvals-and-sandbox".into());
                for record in [legacy, record] {
                    let restored = state.start_restored_session(&record, ControlOrigin::Local).unwrap();
                    assert_eq!(restored, id);
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                    loop {
                        state.runtime_tick();
                        let session = &state.sessions[state.find(&restored).unwrap()];
                        if let Some(terminal) = &session.terminal {
                            let screen = terminal.screen_text(60);
                            if screen.contains("Ask Codex to do anything") {
                                break;
                            }
                            assert!(
                                !screen.contains("Trust this folder?") && !screen.contains("Hooks need review"),
                                "{screen}"
                            );
                        }
                        assert!(Instant::now() < deadline, "the restored task never reached its composer");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let resumed: Vec<String> = serde_json::from_slice(&std::fs::read(&args_file).unwrap()).unwrap();
                    assert!(resumed.windows(2).any(|pair| pair == ["resume", "saved-thread"]));
                    assert!(resumed.windows(2).any(|pair| pair == ["-c", &projects]));
                    assert!(resumed.iter().any(|arg| arg.strip_prefix("hooks.state=") == Some(trust)));
                    assert!(resumed.iter().any(|arg| arg == "--dangerously-bypass-approvals-and-sandbox"));
                    assert!(!resumed.iter().any(|arg| arg == "--sandbox" || arg == "-a"));
                    std::fs::remove_file(&args_file).unwrap();
                    state.close_for_resume(id);
                }
            },
        );
    }

    #[test]
    fn bare_codex_trust_and_hook_menus_need_input_even_with_hooks() {
        with_app("app::sessions::tests::bare_codex_trust_and_hook_menus_need_input_even_with_hooks", |app, _| {
            let dir = crate::test_support::tempdir().unwrap();
            for text in [
                "Trust this folder?\n› 1. Trust and continue\n  2. Cancel",
                "Review hooks\n› 1. Trust hooks\n  2. Cancel",
                "Sign in with ChatGPT\n› 1. Sign in\n  2. Quit",
                "Choose a theme\n› 1. Dark\n  2. Light",
            ] {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    "/bin/sh".into(),
                    vec!["-c".into(), format!("printf '%s' '{}'; exec sleep 600", text)],
                    dir.path(),
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !terminal.screen_text(60).contains(text.lines().next().unwrap()) {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut worker = session("worker", AgentKind::Generic, AgentStatus::Idle, dir.path());
                worker.hooks_active = true;
                worker.terminal = Some(terminal);
                app.runtime.sessions = vec![worker];
                app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                app.runtime.track_busy();
                assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput);
                assert!(app.runtime.sessions[0].approvals.is_empty());
                use atomics_ui::Backend;
                let row = app.runtime.backend().session_row(0).unwrap();
                let expected = if text.starts_with("Sign in") { "needs sign-in" } else { "first-run setup" };
                assert_eq!(row.status_label(), expected);
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn a_codex_agent_working_again_after_its_hook_review_no_longer_needs_you() {
        with_app(
            "app::sessions::tests::a_codex_agent_working_again_after_its_hook_review_no_longer_needs_you",
            |app, _| {
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "test".into(),
                    command: vec![],
                    kind: AgentKind::Generic,
                    busy_text: Some("esc to interrupt".into()),
                    isolation: None,
                });
                let dir = crate::test_support::tempdir().unwrap();
                // Codex's review, answered in its terminal; then it redraws and works on its prompt.
                let script = "printf 'Hooks need review\n› 1. Trust all and continue\n  2. Continue without trusting\n'; \
                              read answer; printf '\\033[2J\\033[H• Working (2s • esc to interrupt)\n\n› '; \
                              exec sleep 600";
                // Untrusted hooks never ran; trusted ones may have, before the hooks changed.
                for hooks_active in [false, true] {
                    let terminal = Terminal::spawn(
                        app.runtime.wake.clone(),
                        app.runtime.theme.terminal_colors(),
                        "/bin/sh".into(),
                        vec!["-c".into(), script.into()],
                        dir.path(),
                        HashMap::new(),
                        100,
                    )
                    .unwrap();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while !terminal.screen_text(60).contains("Continue without trusting") {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let mut worker = session("worker", AgentKind::Generic, AgentStatus::Starting, dir.path());
                    worker.hooks_active = hooks_active;
                    worker.terminal = Some(terminal);
                    app.runtime.sessions = vec![worker];
                    for _ in 0..2 {
                        app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                        app.runtime.track_busy();
                        assert_eq!(app.runtime.sessions[0].status, AgentStatus::NeedsInput, "while it asks");
                    }
                    let terminal = app.runtime.sessions[0].terminal.as_ref().unwrap();
                    terminal.write(b"2\r".to_vec());
                    while !terminal.screen_text(60).contains("esc to interrupt") {
                        assert!(Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    app.runtime.busy_checked = Instant::now() - Duration::from_secs(1);
                    app.runtime.track_busy();
                    assert_eq!(app.runtime.sessions[0].status, AgentStatus::Working, "hooks active: {hooks_active}");
                    app.runtime.sessions.clear();
                }
            },
        );
    }

    #[test]
    fn isolated_agents_write_provider_trust_only_inside_the_instance() {
        with_app("app::sessions::tests::isolated_agents_write_provider_trust_only_inside_the_instance", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "trust-probe".into(),
                kind: AgentKind::Generic,
                command: vec!["/bin/sh".into(), "-c".into(),
                    "mkdir -p \"$CODEX_HOME\" \"$CLAUDE_CONFIG_DIR\"; echo trusted > \"$CODEX_HOME/config.toml\"; echo trusted > \"$CLAUDE_CONFIG_DIR/.claude.json\"".into()],
                busy_text: None,
                isolation: None,
            });
            let id = app.runtime.start("trust-probe", dir.path().into(), None, vec![], None).unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            let home = paths::atomics_home();
            while !home.join("claude/.claude.json").exists() {
                assert!(Instant::now() < deadline);
                app.runtime.runtime_tick();
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(std::fs::read_to_string(home.join("codex/config.toml")).unwrap(), "trusted\n");
            assert!(!paths::home().join(".claude.json").exists());
            assert!(!paths::home().join(".codex/config.toml").exists());
            // Inherited CODEX_HOME belongs to the test harness, not this instance.
            assert!(!PathBuf::from(std::env::var_os("CODEX_HOME").unwrap()).join("config.toml").exists());
            app.runtime.close(&id);
        });
    }

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
    fn codex_queue_receipts_cover_pending_steers_and_followups_without_composer_receipts() {
        let before = "Compacting\n› Ask Codex to do anything";
        let text = "Please check the first change and then the second change";
        for header in [
            "Queued follow-up inputs",
            "Messages to be submitted at end of turn",
            "Messages to be submitted after next tool call (press esc to interrupt and send immediately)",
        ] {
            for preview in
                [text, "Please check the first change…", "Please check the first change\n    and then the\n    …"]
            {
                let screen = format!(
                    "Compacting\n› Ask Codex to do anything\n• {header}\n  ↳ {preview}\n    shift+left edit last queued message"
                );
                assert!(codex_queue_receipt(&screen, before, text), "{screen}");
                assert!(!codex_queue_receipt(&screen, &screen, text), "repeated text needs a new receipt");
            }
        }
        assert!(!codex_queue_receipt(&format!("Compacting\n› {text}\n tab to queue"), before, text));
        assert!(!codex_queue_receipt(
            &format!("› {text}\n• Queued follow-up inputs\n  ↳ unrelated message"),
            before,
            text
        ));
        let queue = "› Ask Codex to do anything\n• Queued follow-up inputs\n  ↳ unrelated message\n    shift+left edit last queued message";
        assert!(!codex_queue_receipt(queue, before, "a"));
        assert!(codex_queue_receipt(&queue.replace("unrelated message", "yes"), before, "yes"));
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
        let short = "› cobalt\n• The answer is blue.\n› Ask Codex to do anything";
        assert!(codex_transcript_receipt(short, before, "cobalt"));
        assert!(!codex_transcript_receipt(short, short, "cobalt"));
        assert!(!codex_transcript_receipt(short, before, "cob"));
        assert!(!codex_transcript_receipt(short, before, "The answer is blue."));
        assert!(!codex_transcript_receipt("Working\n› cobalt", before, "cobalt"));
    }

    #[test]
    fn agents_share_the_job_pool_at_lower_priority() {
        with_app("app::sessions::tests::agents_share_the_job_pool_at_lower_priority", |app, _ctx| {
            // This closure runs in its own test process. Cover an already-niced launcher too:
            // Darwin allows niceness 20, whereas Linux saturates at 19.
            for saturated in [false, true] {
                if saturated {
                    assert_eq!(unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 20) }, 0);
                }
                let dir = crate::test_support::tempdir().unwrap();
                let out = dir.path().join("probe");
                let script = format!(
                    "ps -o nice= -p $$ > {0}.nice; env > {0}.tmp; mv {0}.tmp {0}; exec sleep 600",
                    out.display()
                );
                app.runtime.config.agents.retain(|agent| agent.name != "probe");
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "probe".into(),
                    command: vec!["/bin/sh".into(), "-c".into(), script],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                app.runtime.config.resources.agent_nice = 7;
                // SAFETY: querying this process's priority has no preconditions.
                let inherited_nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
                app.runtime.start("probe", dir.path().into(), None, vec![], None).unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(60));
                while !out.exists() {
                    assert!(Instant::now() < deadline, "the probe agent never wrote its environment");
                    app.runtime.runtime_tick();
                    std::thread::sleep(Duration::from_millis(20));
                }
                let env = std::fs::read_to_string(&out).unwrap();
                let pool =
                    format!("-j --jobserver-auth=fifo:{}", Jobserver::path_for(&paths::atomics_home()).display());
                assert!(env.lines().any(|line| line == format!("CARGO_MAKEFLAGS={pool}")), "{env}");
                // MAKEFLAGS may be inherited from whatever started the tests; this pool must never be in it.
                let pool_path = Jobserver::path_for(&paths::atomics_home()).display().to_string();
                assert!(
                    !env.lines().any(|line| line.starts_with("MAKEFLAGS=") && line.contains(&pool_path)),
                    "plain make must not see the pool: {env}"
                );
                let exe_dir = std::env::current_exe().unwrap().parent().unwrap().display().to_string();
                assert!(env.lines().any(|line| line.starts_with(&format!("PATH={exe_dir}:"))), "{env}");
                assert_eq!(
                    std::fs::read_to_string(dir.path().join("probe.nice")).unwrap().trim(),
                    (inherited_nice + 7).min(if cfg!(target_os = "macos") { 20 } else { 19 }).to_string()
                );
                app.runtime.sessions.clear();
            }
        });
    }

    #[test]
    fn dotnet_agents_get_the_budget_in_their_terminal() {
        with_app("app::sessions::tests::dotnet_agents_get_the_budget_in_their_terminal", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("src")).unwrap();
            std::fs::write(dir.path().join("src/Example.csproj"), "").unwrap();
            let out = dir.path().join("env");
            app.runtime.config.resources.dotnet.processor_count = Some(2);
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "dotnet-probe".into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    format!("env > {0}.tmp; mv {0}.tmp {0}; sleep 5", out.display()),
                ],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            app.runtime.start("dotnet-probe", dir.path().into(), None, vec![], None).unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
            while !out.exists() {
                assert!(Instant::now() < deadline, "the .NET probe never wrote its environment");
                app.runtime.runtime_tick();
                std::thread::sleep(Duration::from_millis(20));
            }
            let env = std::fs::read_to_string(out).unwrap();
            for (key, value) in
                app.runtime.config.resources.env(atomics_core::governor::Toolchain::Dotnet, app.runtime.governor.cores)
            {
                assert!(env.lines().any(|line| line == format!("{key}={value}")), "{env}");
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn codex_version_is_cached_per_program_without_blocking_launch() {
        with_app("app::sessions::tests::codex_version_is_cached_per_program_without_blocking_launch", |app, _ctx| {
            let dir = crate::test_support::tempdir().unwrap();
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
                crate::test_support::write_program(
                    &program,
                    if name == "two" { script.replace("0.165.0", "0.164.0") } else { script.to_string() },
                );
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: name.into(),
                    command: vec![program.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
            }
            let probe_dir = dir.path().to_path_buf();
            let release = std::thread::spawn(move || {
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
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
            let one = app.runtime.codex_version(program("one"));
            assert!(Arc::ptr_eq(&one, &app.runtime.codex_version(program("one"))));
            let two = app.runtime.codex_version(program("two"));
            release.join().unwrap();
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while one.get().is_none() || two.get().is_none() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(one.get(), Some(&Some(true)));
            assert_eq!(two.get(), Some(&Some(false)));
            assert_eq!(app.runtime.codex_versions.len(), 2);
            // A gated probe must finish before start() runs git: concurrent children can
            // inherit pipe writers on macOS and otherwise keep git's output() waiting for EOF.
            let mut ids = Vec::new();
            for name in ["one", "one", "two"] {
                ids.push(app.runtime.start(name, dir.path().into(), None, vec![], None).unwrap());
            }
            let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
            while ids.iter().any(|id| {
                app.runtime.sessions[app.runtime.find(id).unwrap()].terminal.as_ref().is_none_or(|t| !t.exited())
            }) {
                assert!(Instant::now() < deadline, "finite fake agents must exit before teardown");
                app.runtime.runtime_tick();
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
            assert!(Arc::ptr_eq(&one, &app.runtime.codex_version(program("one"))));
            for name in ["one", "two"] {
                assert_eq!(std::fs::read_to_string(dir.path().join(name).join("checks")).unwrap().lines().count(), 1);
                let args = std::fs::read_to_string(dir.path().join(name).join("launch-args")).unwrap();
                let args: Vec<_> = args.lines().collect();
                if let Some(runtime) = paths::runtime_dir_for(&paths::atomics_home()) {
                    let pool = runtime.join("jobserver");
                    let roots = format!("sandbox_workspace_write.writable_roots=[{:?}]", pool.to_str().unwrap());
                    assert!(args.windows(2).any(|pair| pair == ["-c", roots.as_str()]));
                    // Codex 0.166 quits on --add-dir in a folder it does not trust yet.
                    assert!(!args.contains(&"--add-dir"));
                    // The default socket is a sibling of jobserver/, even when
                    // this test instance uses an explicit ATOMICS_SOCKET override.
                    let writable = pool.as_path();
                    assert!(!runtime.join("atomics.sock").starts_with(writable));
                    assert!(!paths::socket_path().starts_with(writable));
                    assert!(Jobserver::path_for(&paths::atomics_home()).starts_with(writable));
                } else {
                    assert!(!args.iter().any(|arg| arg.starts_with("sandbox_workspace_write.")));
                }
            }
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn failed_codex_probes_are_retried_and_recovered_launches_get_hooks() {
        with_app(
            "app::sessions::tests::failed_codex_probes_are_retried_and_recovered_launches_get_hooks",
            |app, _ctx| {
                let dir = crate::test_support::tempdir().unwrap();
                let program = dir.path().join("codex");
                let name = program.to_string_lossy().into_owned();
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "flaky".into(),
                    command: vec![name.clone()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                for script in ["#!/bin/sh\nexec sleep 30\n", "#!/bin/sh\nexit 1\n"] {
                    crate::test_support::write_program(&program, script);
                    let failed = Arc::new(OnceLock::new());
                    failed.set(crate::setup::codex_supports_hooks(&name, None, Duration::from_millis(100))).unwrap();
                    assert_eq!(failed.get(), Some(&None));
                    app.runtime.codex_versions.insert(name.clone(), failed.clone());
                    crate::test_support::write_program(
                        &program,
                        r#"#!/bin/sh
if [ "$1" = --version ]; then
  printf 'codex-cli 0.165.0\n'
else
  printf '%s\n' "$@" > launch-args
fi
"#,
                    );
                    let id = app.runtime.start("flaky", dir.path().into(), None, vec![], None).unwrap();
                    let retried = app.runtime.codex_versions[&name].clone();
                    assert!(!Arc::ptr_eq(&failed, &retried), "failed probes must not be reused as old versions");
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while app.runtime.sessions[app.runtime.find(&id).unwrap()]
                        .terminal
                        .as_ref()
                        .is_none_or(|terminal| !terminal.exited())
                    {
                        assert!(Instant::now() < deadline, "recovered fake agent must launch and exit");
                        app.runtime.runtime_tick();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let args = std::fs::read_to_string(dir.path().join("launch-args")).unwrap();
                    assert!(args.contains("--no-daemon"));
                    assert!(args.contains("hooks.PermissionRequest"));
                    assert_eq!(retried.get(), Some(&Some(true)));
                    assert!(Arc::ptr_eq(&retried, &app.runtime.codex_version(name.clone())));
                }
            },
        );
    }

    #[test]
    fn codex_whose_probe_fails_starts_with_a_warning_on_its_tile_until_a_probe_answers() {
        with_app(
            "app::sessions::tests::codex_whose_probe_fails_starts_with_a_warning_on_its_tile_until_a_probe_answers",
            |app, _ctx| {
                use atomics_ui::Backend;
                let dir = crate::test_support::tempdir().unwrap();
                let program = dir.path().join("codex");
                app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                    name: "probed".into(),
                    command: vec![program.to_string_lossy().into_owned()],
                    kind: AgentKind::Generic,
                    busy_text: None,
                    isolation: None,
                });
                let launch = |app: &mut Desktop<'_>, version: &str| {
                    crate::test_support::write_program(
                        &program,
                        format!(
                            "#!/bin/sh\nif [ \"$1\" = --version ]; then {version}; else printf '%s\\n' \"$@\" > launch-args; fi\n"
                        ),
                    );
                    let _ = std::fs::remove_file(dir.path().join("launch-args"));
                    let id = app.runtime.start("probed", dir.path().into(), None, vec![], None).unwrap();
                    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                    while app.runtime.sessions[app.runtime.find(&id).unwrap()]
                        .terminal
                        .as_ref()
                        .is_none_or(|terminal| !terminal.exited())
                    {
                        assert!(Instant::now() < deadline, "Codex must start whatever its probe found");
                        app.runtime.runtime_tick();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let index = app.runtime.find(&id).unwrap();
                    let warning = app.runtime.backend().tile(index).unwrap().warning;
                    (std::fs::read_to_string(dir.path().join("launch-args")).unwrap(), warning)
                };
                // As Codex fails when the interpreter it names is not on PATH.
                let (args, warning) = launch(app, "echo 'env: node: No such file or directory' >&2; exit 127");
                assert!(!args.contains("hooks.PermissionRequest"), "{args}");
                assert_eq!(warning.as_deref(), Some(CODEX_HOOKS_UNKNOWN));
                assert_eq!(app.runtime.toast.as_ref().unwrap().0, CODEX_HOOKS_UNKNOWN);
                // Unknown is not unsupported: the next start asks again.
                let (args, warning) = launch(app, "printf 'codex-cli 0.165.0\\n'");
                assert!(args.contains("hooks.PermissionRequest"), "{args}");
                assert_eq!(warning, None);
                // A version too old for hooks is known: no hooks, and nothing to warn of.
                app.runtime.codex_versions.clear();
                let (args, warning) = launch(app, "printf 'codex-cli 0.164.0\\n'");
                assert!(!args.contains("hooks.PermissionRequest"), "{args}");
                assert_eq!(warning, None);
                app.runtime.sessions.clear();
            },
        );
    }

    #[test]
    fn compacting_codex_send_is_queued_then_confirmed() {
        with_app("app::sessions::tests::compacting_codex_send_is_queued_then_confirmed", |app, _| {
            let fixture = crate::app::tests::fixture_agent("codex");
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            for receipt in ["hook", "transcript"] {
                app.runtime.events.clear();
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), "compacting".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions =
                    vec![session("busy", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
                app.runtime.sessions[0].agent = "fake-codex".into();
                app.runtime.sessions[0].terminal = Some(terminal);
                let text = "Please check the first change and then the second change";
                let (tx, rx) = channel();
                app.runtime.control(
                    ControlRequest::Send { session: "busy".into(), text: text.into(), raw: false, now: false },
                    tx,
                );
                std::thread::sleep(Duration::from_millis(250));
                let mut response = None;
                for _ in 0..=DELIVERY_ATTEMPTS {
                    app.runtime.sessions[0].pending.as_mut().unwrap().sent = Instant::now() - DELIVERY_TIMEOUT;
                    app.runtime.verify_deliveries();
                    std::thread::sleep(Duration::from_millis(200));
                    app.runtime.verify_deliveries();
                    if let Ok(ControlOutcome::Done(result)) = rx.try_recv() {
                        response = Some(result);
                        break;
                    }
                }
                let response = response.expect("queued send answers");
                assert!(
                    response.ok && response.result["queued"] == true && response.result["sent"] == false,
                    "{response:?}"
                );
                assert!(app.runtime.sessions[0].pending.is_some(), "keep tracking the queued message");
                assert!(!app.runtime.events.iter().any(|event| event.kind() == "delivery"));
                let after = app.runtime.event_seq;
                app.runtime.sessions.push(session(
                    "other",
                    AgentKind::Generic,
                    AgentStatus::Working,
                    &app.runtime.launch_dir,
                ));
                let (other_tx, other_rx) = channel();
                app.runtime.control(
                    serde_json::from_value(json!({"cmd":"wait", "after":after,
                "sessions":["other"], "statuses":["delivery"]}))
                    .unwrap(),
                    other_tx,
                );
                if receipt == "hook" {
                    let event = atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":"UserPromptSubmit", "session_id":"busy", "prompt":text,
                    }))
                    .unwrap();
                    app.runtime.on_hook(HookMessage {
                        pane: Some("busy".into()),
                        ancestors: vec![],
                        event,
                        delivered: Vec::new(),
                        approval_reply: None,
                    });
                } else {
                    app.runtime.sessions[0].terminal.as_ref().unwrap().write(vec![4]);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while app.runtime.sessions[0].pending.is_some() {
                        assert!(Instant::now() < deadline, "transcript confirms the queued message");
                        std::thread::sleep(Duration::from_millis(10));
                        app.runtime.verify_deliveries();
                    }
                }
                assert!(app.runtime.sessions[0].pending.is_none());
                assert!(app.runtime.events.iter().any(|event| event.kind() == "delivery" && event.session == "busy"));
                app.runtime.serve_waiters();
                assert!(other_rx.try_recv().is_err(), "delivery belongs only to its target session");
                let result = request(app, json!({"cmd":"wait", "after":after}));
                assert!(result.ok, "{result:?}");
                assert!(
                    result.result["events"].as_array().unwrap().iter().any(|event| {
                        event["status"] == "delivery" && event["sent"] == true && event["change"] == "message_delivered"
                    }),
                    "default wait confirms delivery: {result:?}"
                );
                app.runtime.sessions.clear();
                app.runtime.waiters.clear();
            }
        });
    }

    #[test]
    fn a_codex_chief_delivery_that_leaves_the_queue_without_receipt_releases_held_messages() {
        with_app(
            "app::sessions::tests::a_codex_chief_delivery_that_leaves_the_queue_without_receipt_releases_held_messages",
            |app, _| {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    crate::app::tests::fixture_agent("codex").to_string_lossy().into_owned(),
                    vec!["--mode".into(), "compacting".into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut chief =
                    session("codex-chief", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir);
                chief.chief = true;
                chief.terminal = Some(terminal);
                app.runtime.sessions = vec![chief];
                let text = "cobalt";
                let (tx, rx) = channel();
                app.runtime.control(
                    ControlRequest::Send { session: "codex-chief".into(), text: text.into(), raw: false, now: false },
                    tx,
                );
                let state = &mut *app.runtime;
                let screen = |state: &State| state.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                while !screen(state).contains("Enters: 1") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                state.sessions[0].pending.as_mut().unwrap().sent = Instant::now() - DELIVERY_TIMEOUT;
                state.verify_deliveries();
                while !screen(state).contains("↳ cobalt") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                state.verify_deliveries();
                let ControlOutcome::Done(result) = rx.try_recv().unwrap() else { panic!("expected a queue receipt") };
                assert_eq!(result.result["queued"], true);
                assert_eq!(state.deliver(0, "amber"), Delivery::Held);
                state.sessions[0].pending.as_mut().unwrap().sent = Instant::now() - DELIVERY_TIMEOUT;
                state.verify_deliveries();
                assert!(state.sessions[0].pending.is_some(), "a visible queue can outlive the receipt timeout");

                state.sessions[0].terminal.as_ref().unwrap().write(vec![5]);
                while screen(state).contains("↳ cobalt") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                state.sessions[0].pending.as_mut().unwrap().sent = Instant::now() - DELIVERY_TIMEOUT;
                state.verify_deliveries();
                assert!(state.sessions[0].pending.is_none(), "the missing queue must expire");
                let (_, hint) = state.session_status(&state.sessions[0]);
                assert!(hint.is_some_and(|hint| hint.to_ascii_lowercase().contains("could not confirm")));
                state.release_held(0);
                assert_eq!(state.sessions[0].pending.as_ref().unwrap().text, "amber");
                assert!(state.sessions[0].held.is_empty());
                state.sessions.clear();
            },
        );
    }

    #[test]
    fn busy_codex_sends_wait_for_receipts_retry_and_report_failure() {
        with_app("app::sessions::tests::busy_codex_sends_wait_for_receipts_retry_and_report_failure", |app, _ctx| {
            let fixture = crate::app::tests::fixture_agent("codex");
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            let text = "Please check the first change and then the second change";
            for mode in ["queue", "retry", "drop", "drop-paste", "drop-empty", "exit", "hook"] {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), mode.into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions =
                    vec![session(mode, AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
                app.runtime.sessions[0].agent = "fake-codex".into();
                app.runtime.sessions[0].terminal = Some(terminal);
                let (tx, rx) = channel();
                app.runtime.control(
                    ControlRequest::Send { session: mode.into(), text: text.into(), raw: false, now: false },
                    tx,
                );
                assert!(rx.try_recv().is_err(), "an unverified send must not claim success");
                let held = request(app, json!({"cmd":"send", "session":mode, "text":"second message"}));
                assert!(held.ok && held.result["held"] == true, "another send must wait for the pending receipt");
                assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, text);
                // Let the initial paste and its delayed Enter reach the fake busy terminal.
                std::thread::sleep(Duration::from_millis(250));
                app.runtime.sessions[0].status = AgentStatus::Done;
                app.runtime.verify_deliveries();
                if mode != "exit" {
                    assert!(rx.try_recv().is_err(), "composer text or a shared prefix is not a receipt");
                }
                if mode == "hook" {
                    let event = atomics_core::hook::HookEvent::from_payload(&json!({
                        "hook_event_name":"UserPromptSubmit", "session_id":mode, "prompt":text,
                    }))
                    .unwrap();
                    app.runtime.on_hook(HookMessage {
                        pane: Some(mode.into()),
                        ancestors: vec![],
                        event,
                        delivered: Vec::new(),
                        approval_reply: None,
                    });
                }
                let mut response = None;
                for _ in 0..DELIVERY_ATTEMPTS {
                    if let Some(pending) = &mut app.runtime.sessions[0].pending {
                        pending.sent = Instant::now() - DELIVERY_TIMEOUT;
                    }
                    app.runtime.verify_deliveries();
                    std::thread::sleep(Duration::from_millis(200));
                    app.runtime.verify_deliveries();
                    if let Ok(ControlOutcome::Done(result)) = rx.try_recv() {
                        response = Some(result);
                        break;
                    }
                }
                let response = response.expect("every send must receive a delivery result");
                assert_eq!(response.ok, matches!(mode, "queue" | "retry" | "hook"), "{mode}: {response:?}");
                if response.ok {
                    assert_eq!(response.result["sent"], mode != "queue");
                    if mode == "queue" {
                        assert_eq!(response.result["queued"], true);
                    }
                    assert!(
                        mode == "hook"
                            || steer::message_in_transcript(
                                &app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60),
                                text
                            )
                    );
                } else {
                    assert!(response.error.unwrap().contains("unconfirmed"));
                }
                if matches!(mode, "drop-paste" | "drop-empty") {
                    let screen = app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                    assert!(screen.contains("Pastes: 1"), "{screen}");
                    assert!(screen.contains("Enters: 1"), "{screen}");
                    assert!(screen.contains("Tabs: 0"), "{screen}");
                    assert_eq!(response.result.get("sent"), None);
                }
                if mode == "hook" {
                    assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "second message");
                    assert!(app.runtime.sessions[0].held.is_empty());
                } else if mode == "queue" {
                    assert!(app.runtime.sessions[0].pending.as_ref().unwrap().queued);
                } else {
                    assert!(app.runtime.sessions[0].pending.is_none());
                }
                app.runtime.sessions.clear();
            }
        });
    }

    #[test]
    fn codex_clipped_and_repeated_receipts_never_repaste() {
        with_app("app::sessions::tests::codex_clipped_and_repeated_receipts_never_repaste", |app, _ctx| {
            let fixture = crate::app::tests::fixture_agent("codex");
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            for mode in ["long", "preview", "repeat", "marker", "collapsed", "no-prompt"] {
                let text = if mode == "long" {
                    "A long multiline message that cannot fit on screen.\n".repeat(100) + "The final line is visible."
                } else {
                    "Please check the first change and then the second change".into()
                };
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec!["--mode".into(), mode.into()],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !terminal.screen_text(60).contains("tab to queue") {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                app.runtime.sessions =
                    vec![session(mode, AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
                app.runtime.sessions[0].agent = "fake-codex".into();
                app.runtime.sessions[0].terminal = Some(terminal);
                let (tx, rx) = channel();
                app.runtime.control(ControlRequest::Send { session: mode.into(), text, raw: false, now: false }, tx);
                let mut paste_seen = false;
                while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Enters: 1") {
                    assert!(Instant::now() < deadline, "{mode}: the original paste and Enter must reach the fixture");
                    app.runtime.verify_deliveries();
                    paste_seen |= app.runtime.sessions[0].pending.as_ref().is_some_and(|pending| pending.paste_seen);
                    std::thread::sleep(Duration::from_millis(10));
                }
                if mode == "marker" {
                    assert!(paste_seen, "the marker must be observed");
                }
                let mut response = None;
                for _ in 0..=DELIVERY_ATTEMPTS {
                    if let Some(pending) = &mut app.runtime.sessions[0].pending {
                        pending.sent = Instant::now() - DELIVERY_TIMEOUT;
                    }
                    app.runtime.verify_deliveries();
                    if let Ok(ControlOutcome::Done(result)) = rx.try_recv() {
                        response = Some(result);
                        break;
                    }
                    if mode == "collapsed" {
                        while !app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60).contains("Queued") {
                            assert!(Instant::now() < deadline, "{mode}: the retry must submit with Tab");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                }
                let response = response.expect("a send must eventually resolve");
                assert_eq!(response.ok, mode != "no-prompt", "{mode}: {response:?}");
                // A missing prompt cannot confirm delivery, but must never cause
                // a second paste after the original text has left the composer.
                let screen = app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                assert!(screen.contains("Pastes: 1"), "{mode}: {screen}");
                assert!(screen.contains("Enters: 1"), "{mode}: {screen}");
                assert!(screen.contains(if mode == "collapsed" { "Tabs: 1" } else { "Tabs: 0" }), "{mode}: {screen}");
                assert!(rx.try_recv().is_err(), "{mode}: a send resolves only once");
                app.runtime.sessions.clear();
            }
        });
    }

    #[test]
    fn steering_during_a_pending_codex_send_is_held_and_released() {
        with_app("app::sessions::tests::steering_during_a_pending_codex_send_is_held_and_released", |app, _ctx| {
            let fixture = crate::app::tests::fixture_agent("codex");
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![fixture.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            app.runtime.sessions =
                vec![session("worker", AgentKind::Generic, AgentStatus::Working, &app.runtime.launch_dir)];
            app.runtime.sessions[0].agent = "fake-codex".into();
            app.runtime.sessions[0].terminal = Some(
                Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    fixture.to_string_lossy().into_owned(),
                    vec![],
                    &app.runtime.launch_dir,
                    HashMap::new(),
                    100,
                )
                .unwrap(),
            );
            assert_eq!(app.runtime.deliver(0, "first"), Delivery::Pending);
            app.runtime.active = Some("worker".into());
            app.client.steer_target = SteerTarget::Active;
            app.client.steer_text = "second".into();
            app.send_steer();
            app.runtime.drain_actions();
            app.client.steer_text = "third".into();
            app.send_steer();
            app.runtime.drain_actions();
            assert!(app.client.steer_text.is_empty());
            assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "first");
            assert_eq!(app.runtime.sessions[0].held, ["second", "third"]);
            assert!(app.runtime.sessions[0].activity.held_since.is_some());
            app.runtime.release_held(0);
            assert_eq!(app.runtime.sessions[0].held.len(), 2);
            let event = atomics_core::hook::HookEvent::from_payload(&json!({
                "hook_event_name":"UserPromptSubmit", "session_id":"worker", "prompt":"first"
            }))
            .unwrap();
            app.runtime.on_hook(HookMessage {
                pane: Some("worker".into()),
                ancestors: vec![],
                event,
                delivered: Vec::new(),
                approval_reply: None,
            });
            app.runtime.release_held(0);
            assert!(app.runtime.sessions[0].held.is_empty());
            assert!(app.runtime.sessions[0].activity.held_since.is_none());
            assert_eq!(app.runtime.sessions[0].pending.as_ref().unwrap().text, "second\n\nthird");
            app.runtime.sessions.clear();
        });
    }

    #[test]
    fn session_start_does_not_wait_for_git() {
        if std::env::var_os("ATOMICS_START_GIT_TEST").is_some() {
            let dir = std::env::current_dir().unwrap();
            let agent = atomics_core::config::AgentConfig {
                name: "test".into(),
                command: vec![],
                kind: AgentKind::Shell,
                busy_text: None,
                isolation: None,
            };
            let session = Session::starting(
                "start".into(),
                &agent,
                dir.clone(),
                false,
                None,
                &crate::host::Wake::default(),
                InputAccess::Desktop,
            );
            assert!(!dir.join("timed-out").exists(), "session construction waited for git on the UI thread");
            assert!(session.worktree.is_none());
            std::fs::write(dir.join("ready"), "").unwrap();
            let worktree = session
                .worktree_ready
                .unwrap()
                .recv_timeout(crate::test_support::scaled_timeout(Duration::from_secs(5)))
                .unwrap()
                .unwrap();
            assert_eq!(worktree.branch.as_deref(), Some("feature"));
            assert_eq!(worktree.root, dir);
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        git::git(dir.path(), &["init", "-q", "-b", "feature"]).unwrap();
        let bin = crate::test_support::tempdir().unwrap();
        let real_git = crate::setup::resolve_program("git").replace('\'', "'\\''");
        crate::test_support::write_program(
            bin.path().join("git"),
            format!(
                r#"#!/bin/sh
attempts=0
while [ ! -e "$HOME/ready" ]; do
  attempts=$((attempts + 1))
  if [ "$attempts" -gt 200 ]; then
    touch "$HOME/timed-out"
    exit 1
  fi
  sleep 0.01
done
exec '{real_git}' "$@"
"#
            ),
        );
        let output = crate::test_support::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app::sessions::tests::session_start_does_not_wait_for_git", "--nocapture"])
                .env("ATOMICS_START_GIT_TEST", "1")
                .env("PATH", crate::test_support::path_with(bin.path()))
                .env("HOME", dir.path())
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", dir.path().join("atomics.sock"))
                .env_remove("ATOMICS_PANE")
                .current_dir(dir.path()),
        );
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn busy_survives_a_scrolled_up_agent_view() {
        let busy = "esc to interrupt";
        let spinner = "✻ Building… (5m · esc to interrupt)";
        let filler = "transcript\n".repeat(30);
        let live = format!("{filler}{spinner}\n> ");
        assert!(view_busy(&live, &screen_tail(&live, 12), busy));
        // Scrolled up: the spinner sits above the tail, under a "Jump to bottom" pill.
        let scrolled = format!("{spinner}\n{filler}Jump to bottom");
        assert!(view_busy(&scrolled, &screen_tail(&scrolled, 12), busy));
        // Quoted busy text in a live transcript is still not busy.
        let quoted = format!("the footer says {busy}\n{filler}> ");
        assert!(!view_busy(&quoted, &screen_tail(&quoted, 12), busy));
    }

    #[test]
    fn a_scrolled_up_view_is_busy_only_by_its_most_recent_status_line() {
        let busy = "esc to interrupt";
        let view = |lines: &[&str]| {
            let full = format!("{}\n{}Jump to bottom", lines.join("\n"), "transcript\n".repeat(3));
            view_busy(&full, &screen_tail(&full, 12), busy)
        };
        // Quoted busy text, here or in the last lines, is transcript, not a status.
        assert!(!view(&["⏺ The sandbox footer reads esc to interrupt", "> SANDBOX_BUSY = \"esc to interrupt\""]));
        // The spinner of a turn that has since finished is history.
        assert!(!view(&["✻ Building… (5m · esc to interrupt)", "⏺ Done.", "✻ Worked for 5m 3s"]));
        assert!(!view(&["• Working (12s • esc to interrupt)", "─ Worked for 1m 05s ─────"]));
        // The latest status still counts below quoted text, in Codex's form, and wrapped.
        assert!(view(&["⏺ Bash(cargo test)", "⏺ It printed esc to interrupt", "✻ Building… (5m · esc to interrupt)"]));
        assert!(view(&["• Working (12s • esc to interrupt)"]));
        assert!(view(&["✻ Building… (5m 12s · ↑ 1.2k tokens · esc to", "interrupt)"]));
        for line in ["⏺ Update(src/main.rs)", "> Building… (2m · esc to interrupt)", "✻ building… (esc to interrupt)"]
        {
            assert!(!status_line(line), "{line}");
        }
    }

    #[test]
    fn send_now_keys_come_from_what_each_agent_shows_while_a_message_waits() {
        let claude = "> Write the parser\n  ⎿  Queued · ctrl+x ctrl+s to send now\n> ";
        assert_eq!(send_now_keys(claude, false), Some(Ok(vec![0x18, 0x13])));
        // A person may bind Claude's send-now to another chord; the hint names it.
        assert_eq!(send_now_keys("  ⎿  Queued · ctrl+g to send now", false), Some(Ok(vec![0x07])));
        assert_eq!(send_now_keys("  ⎿  Queued · click to send now", false), Some(Err("click".into())));
        assert_eq!(send_now_keys("  ⎿  Queued · meta+enter to send now", false), Some(Err("meta+enter".into())));
        let codex = "• Messages to be submitted after next tool call (esc to interrupt and send immediately)\n  ↳ go\n\
                     Working (esc to interrupt)\n› Ask Codex to do anything";
        assert_eq!(send_now_keys(codex, true), Some(Ok(vec![0x1b])));
        assert_eq!(send_now_keys("(press esc to interrupt and send immediately)", true), Some(Ok(vec![0x1b])));
        let wrapped =
            "• Messages to be submitted after next tool call (esc to interrupt and send immed\niately)\n  ↳ go";
        assert_eq!(send_now_keys(wrapped, true), Some(Ok(vec![0x1b])));
        assert_eq!(send_now_keys("call (esc to interrupt and send\nimmediately)", true), Some(Ok(vec![0x1b])));
        // Nothing waits: a turn under way is never interrupted.
        assert_eq!(send_now_keys("Working (esc to interrupt)\n› ", true), None);
        assert_eq!(send_now_keys("✻ Working… (esc to interrupt)\n> ", false), None);
    }

    /// `now` makes a busy agent take a message at once, mid-turn, by the key its own queue names: Claude's
    /// send-now chord and Codex's Escape. Without it, the message waits in the agent's queue.
    #[test]
    fn send_now_steers_busy_claude_and_codex_stand_ins_mid_turn() {
        with_app("app::sessions::tests::send_now_steers_busy_claude_and_codex_stand_ins_mid_turn", |app, _| {
            let claude_dir = crate::setup::agent_claude_dir();
            std::fs::create_dir_all(&claude_dir).unwrap();
            let folder = app.runtime.launch_dir.canonicalize().unwrap();
            std::fs::write(claude_dir.join("trusted-folders"), format!("{}\n", folder.display())).unwrap();
            let codex = crate::app::tests::fixture_agent("codex");
            app.runtime.config.agents.push(atomics_core::config::AgentConfig {
                name: "fake-codex".into(),
                command: vec![codex.to_string_lossy().into_owned()],
                kind: AgentKind::Generic,
                busy_text: None,
                isolation: None,
            });
            let claude = crate::app::tests::fixture_agent("claude");
            let env: HashMap<String, String> =
                [("CLAUDE_CONFIG_DIR".to_string(), claude_dir.to_string_lossy().into_owned())].into();
            let stand_ins = [
                (AgentKind::Claude, claude, vec!["--mode", "busy", "--session-id", "conversation"], "Working…"),
                (AgentKind::Generic, codex, vec!["--mode", "steer"], "tab to queue"),
            ];
            for (kind, program, args, ready) in stand_ins {
                let terminal = Terminal::spawn(
                    app.runtime.wake.clone(),
                    app.runtime.theme.terminal_colors(),
                    program.to_string_lossy().into_owned(),
                    args.into_iter().map(str::to_string).collect(),
                    &folder,
                    env.clone(),
                    100,
                )
                .unwrap();
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                while !terminal.screen_text(60).contains(ready) {
                    assert!(Instant::now() < deadline, "{kind:?} did not start: {}", terminal.screen_text(60));
                    std::thread::sleep(Duration::from_millis(10));
                }
                let mut busy = session("busy", kind, AgentStatus::Working, &folder);
                if kind == AgentKind::Generic {
                    busy.agent = "fake-codex".into();
                }
                busy.terminal = Some(terminal);
                app.runtime.sessions = vec![busy];
                let screen = |app: &Desktop<'_>| app.runtime.sessions[0].terminal.as_ref().unwrap().screen_text(60);
                let send = |app: &mut Desktop<'_>, text: &str, now: bool| {
                    let (tx, _rx) = channel();
                    let request = ControlRequest::Send { session: "busy".into(), text: text.into(), raw: false, now };
                    app.runtime.control(request, tx);
                };

                send(app, "Then run the whole suite", false);
                let deadline = Instant::now() + Duration::from_millis(1500);
                while Instant::now() < deadline {
                    app.runtime.runtime_tick();
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(!screen(app).contains("Sent now"), "{kind:?}: a plain send waits: {}", screen(app));

                // Codex holds it behind the one waiting in its queue: Escape sends that one now, and this follows.
                send(app, "Stop and check the failing test first", true);
                let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
                let sent_now = |screen: &str| {
                    screen.lines().any(|line| line.contains("Sent now:") && line.contains("Stop and check"))
                };
                while !sent_now(&screen(app)) {
                    assert!(Instant::now() < deadline, "{kind:?} was not steered: {}", screen(app));
                    app.runtime.runtime_tick();
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(screen(app).contains("Stop and check the failing test first"), "{}", screen(app));
                assert!(app.runtime.sessions[0].send_now.is_none());
            }
        });
    }
}
