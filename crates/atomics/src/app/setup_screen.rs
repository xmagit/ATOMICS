//! The host's side of the setup screen (`atomics_ui::management::setup`): what it shows, and what its clicks do. Also
//! the question a person's start asks when no sandbox can be had for it: ATOMICS never runs a sandboxed agent bare on
//! its own, so it asks instead of failing (docs/isolation.md).
use super::*;
use crate::sandbox::steps;
use atomics_core::config::AgentKind;
use atomics_core::isolation::IsolationMode;
use atomics_core::projection::{
    SetupAction, SetupClick, SetupConnection, SetupFailure, SetupInfo, SetupJob, SetupProgram, StartChoice,
};
use std::sync::atomic::AtomicBool;

/// A person's start waiting for their answer.
pub(super) struct PendingStart {
    pub(super) agent: String,
    pub(super) cwd: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) prompt: Option<String>,
    pub(super) why: String,
    pub(super) sandbox_required: bool,
}

/// What looking for the programs found.
pub(super) struct ProgramsChecked {
    programs: Vec<SetupProgram>,
    azure: crate::relay::azure::Status,
    /// What gets the Azure CLI on this machine.
    azure_install: String,
}

/// The setup step under way.
pub(super) struct RunningSetup {
    pub(super) action: SetupAction,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) output: Option<Arc<std::sync::Mutex<String>>>,
}

impl RunningSetup {
    pub(super) fn progress(&self) -> String {
        self.output
            .as_ref()
            .map(|output| output.lock().unwrap().clone())
            .unwrap_or_else(|| steps::progress(self.action).into())
    }
}

/// The click that gets `program`, where it takes no terminal and no sudo: Claude Code's installer, npm or Homebrew where
/// they are here, and Apple's own dialog for git.
fn install_click(program: &str, mac: bool, has: &dyn Fn(&str) -> bool) -> Option<SetupClick> {
    let action = match (program, mac) {
        ("claude", _) if has("curl") && has("bash") => SetupAction::InstallClaude,
        ("codex", false) if has("npm") => SetupAction::InstallCodex,
        ("codex", true) if has("brew") => SetupAction::InstallCodex,
        ("git", true) => SetupAction::InstallGit,
        _ => return None,
    };
    Some(SetupClick::new(action, "Install"))
}

/// The programs a fresh machine needs: git, then the agents the config runs. A program whose name a person would not
/// recognize keeps it, so a custom agent shows as what it runs.
fn wanted_programs(config: &Config) -> Vec<(String, Option<String>, String)> {
    let mut wanted = vec![("git".to_string(), None, "git".to_string())];
    for agent in &config.agents {
        let Some(program) = agent.command.first() else { continue };
        let name = match agent.kind {
            AgentKind::Shell => continue,
            AgentKind::Claude => "Claude Code".to_string(),
            AgentKind::Generic if agent.is_codex() => "Codex".to_string(),
            AgentKind::Generic => agent.name.clone(),
        };
        wanted.push((name, Some(agent.name.clone()), program.clone()));
    }
    wanted
}

impl State {
    pub(super) fn setup_info(&self) -> SetupInfo {
        SetupInfo {
            simple: (cfg!(all(target_os = "macos", target_arch = "aarch64"))
                && (self.store.setup_pending || self.simple_setup.selected))
                .then(|| self.simple_setup.info()),
            first_run: self.store.setup_pending || self.simple_setup.pending,
            checking: self.programs_check.is_some(),
            programs: self.setup_programs.clone(),
            project: self.open_projects.first().map(|project| paths::display(project)),
            connections: self.setup_connections(),
            sandbox_steps: self.sandbox_steps.clone(),
            job: self.setup_job.as_ref().map(|job| SetupJob { action: job.action, progress: job.progress() }),
            failure: self.setup_failure.clone(),
            later: self.store.setup_later.clone(),
            install: self.install_offer.as_ref().map(|(from, to)| atomics_core::projection::SetupInstall {
                from: paths::display(from),
                to: paths::display(to),
            }),
            start_choice: self.start_choice.as_ref().map(|pending| StartChoice {
                agent: pending.agent.clone(),
                why: pending.why.clone(),
                sandbox_required: pending.sandbox_required,
            }),
        }
    }

    fn setup_connections(&self) -> Vec<SetupConnection> {
        use atomics_core::github::AZURE_HOST;
        let row =
            |id: &str, name: &str, connected: Option<String>, note: &str, command: Option<&str>| SetupConnection {
                id: id.into(),
                name: name.into(),
                connected,
                note: note.into(),
                command: command.map(str::to_string),
                click: None,
            };
        let github = self.github.signed_in_hosts().into_iter().find(|(host, _)| host != AZURE_HOST);
        let azure = self.github.viewer(AZURE_HOST).map(str::to_string).or_else(|| {
            let signed_in = self.setup_azure.found && self.setup_azure.logged_in;
            signed_in.then(|| "Signed in".to_string())
        });
        let tickets = self.ticket_config();
        let tickets_connected =
            tickets.as_ref().filter(|_| !self.tickets.panel.signin).map(|config| config.system.clone());
        let tickets_note = match &tickets {
            Some(_) => "Sign in to read tickets and plan from them.",
            None => "Follows a ticket system once the config has a [tickets] profile for it.",
        };
        let devices = self.remote_devices().map_or(0, |devices| devices.list().len());
        vec![
            row("github", "GitHub", github.map(|(_, login)| login), "See, review and merge pull requests here.", None),
            if self.setup_azure.found {
                SetupConnection {
                    click: Some(SetupClick::new(SetupAction::AzureLogin, "Sign in")),
                    ..row(
                        "azure",
                        "Azure DevOps",
                        azure,
                        "Uses the Azure CLI's sign-in, in your browser. Nothing is stored, and no agent gets it.",
                        None,
                    )
                }
            } else {
                row(
                    "azure",
                    "Azure DevOps",
                    azure,
                    "Takes the Azure CLI, which ATOMICS signs in with once it is here.",
                    self.setup_azure_install.as_deref(),
                )
            },
            row("tickets", "Tickets", tickets_connected, tickets_note, None),
            row(
                "remote",
                "Remote devices",
                (devices > 0).then(|| format!("{devices} paired")),
                "Follow and steer agents from a phone or another computer.",
                None,
            ),
        ]
    }

    /// Looks for the programs off the window's thread: reading the login shell can take seconds.
    pub(super) fn check_programs(&mut self) {
        if self.programs_check.is_some() {
            return;
        }
        let wanted = wanted_programs(&self.config);
        let (tx, rx) = std::sync::mpsc::channel();
        self.programs_check = Some(rx);
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            // What was installed since ATOMICS started counts, and agents started from now on find it too.
            let path = crate::setup::refresh_login_path();
            let mac = cfg!(target_os = "macos");
            let programs = wanted
                .into_iter()
                .map(|(name, agent, program)| {
                    let found = if program.contains('/') {
                        Path::new(&program).is_file().then(|| PathBuf::from(&program))
                    } else {
                        crate::setup::find_on_path(&program, &path)
                    };
                    let has = |name: &str| crate::setup::find_on_path(name, &path).is_some();
                    let install = crate::sandbox::checks::install_command(&program, mac, &has);
                    let click = install_click(&program, mac, &has);
                    SetupProgram { name, agent, found: found.as_deref().map(paths::display), install, click }
                })
                .collect();
            let has = |name: &str| crate::setup::find_on_path(name, &path).is_some();
            let azure_install = crate::sandbox::checks::install_command("az", mac, &has);
            let azure = crate::relay::azure::status(crate::setup::find_on_path("az", &path));
            let _ = tx.send(ProgramsChecked { programs, azure, azure_install });
            wake.notify();
        });
        // Azure DevOps counts as signed in once a poll has seen its login.
        self.github_poll_now();
    }

    pub(super) fn poll_programs_check(&mut self) -> bool {
        let Some(rx) = &self.programs_check else { return false };
        match rx.try_recv() {
            Ok(checked) => {
                self.setup_programs = checked.programs;
                self.setup_azure = checked.azure;
                self.setup_azure_install = Some(checked.azure_install);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        self.programs_check = None;
        true
    }

    /// What sandboxes would need of this machine, with `config` for them. Whether they are on yet makes no difference,
    /// so that someone can get ready before switching.
    pub(super) fn sandbox_machine(&self, config: atomics_core::isolation::SandboxConfig) -> steps::Machine {
        let would_run = |agent: &&atomics_core::config::AgentConfig| agent.isolation != Some(IsolationMode::Bare);
        steps::Machine {
            sbx: resolve_program(&self.sbx),
            config,
            claude: self.config.agents.iter().filter(would_run).any(|agent| agent.kind == AgentKind::Claude),
            codex: self.config.agents.iter().filter(would_run).any(|agent| agent.is_codex()),
            keychain: self.relay.keychain().clone(),
        }
    }

    /// Runs one step off the window's thread. What it finds goes back through [`Self::setup_finished`].
    pub(super) fn run_setup(&mut self, action: SetupAction, value: Option<String>) -> Result<(), String> {
        if self.setup_job.is_some() {
            return Err("Another setup step is under way.".into());
        }
        self.setup_failure = None;
        if self.run_simple_setup(action)? {
            return Ok(());
        }
        if action == SetupAction::SshAgentOff {
            let saved = self.write_machine_setting("[isolation.sandbox]\nssh_agent = false");
            self.setup_failure = saved.err().map(|why| SetupFailure { action, why });
            self.setup_look_again = true;
            return Ok(());
        }
        let config = crate::sandbox::checks::configured(&self.config, self.workspace.as_deref())
            .map_err(|_| "Cannot read sandbox/ticket configuration; fix it and try again".to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let job = steps::Job { action, value, machine: self.sandbox_machine(config), stop: stop.clone() };
        self.setup_job = Some(RunningSetup { action, stop, output: None });
        let actions = self.action_tx.clone();
        std::thread::spawn(move || {
            let result = job.run();
            let _ = actions.send(Box::new(move |state| state.setup_finished(action, result)));
        });
        Ok(())
    }

    pub(super) fn cancel_setup(&mut self) {
        if let Some(job) = &self.setup_job {
            job.stop.store(true, Ordering::Relaxed);
        }
    }

    /// A connection can wait, or no longer has to. It is saved at once: nobody should be asked again after a restart.
    pub(super) fn connect_later(&mut self, id: String, later: bool) -> Result<(), String> {
        let list = &mut self.store.setup_later;
        let known = list.contains(&id);
        match later {
            true if !known => list.push(id),
            false if known => list.retain(|other| *other != id),
            _ => return Ok(()),
        }
        self.store.save(&paths::store_file()).map_err(|err| format!("Could not save that: {err:#}"))
    }

    fn setup_finished(&mut self, action: SetupAction, result: anyhow::Result<()>) {
        self.setup_job = None;
        if action == SetupAction::InstallSelf && result.is_ok() {
            self.installed_self();
        }
        self.setup_failure = match result {
            Ok(()) => None,
            // Whoever stopped it knows.
            Err(error) if error.is::<crate::sandbox::setup::Stopped>() => None,
            Err(error) => Some(SetupFailure { action, why: format!("{error:#}") }),
        };
        self.setup_look_again = true;
    }

    /// It is in place. With nothing running, this window reopens from there, since what starts agents and what updates
    /// should be the installed copy; over running agents it stays as it is, and the next start uses that copy.
    fn installed_self(&mut self) {
        let Some((_, to)) = self.install_offer.take() else { return };
        let shown = paths::display(&to);
        if !self.sessions.is_empty() {
            self.toast(format!("Installed at {shown}. It opens from there next time."));
            return;
        }
        let workspace = self.workspace.clone();
        let answer = self.restart_using(false, Some("installed".into()), move || {
            let program = crate::install::program_at(&to, cfg!(target_os = "macos"));
            crate::restart::Target::at(program, workspace.as_deref())
        });
        if let Some(error) = answer.error {
            self.toast(format!("Installed at {shown}. Open it from there; it could not reopen by itself: {error}"));
        }
    }

    #[cfg(test)]
    pub(super) fn installed_self_for_test(&mut self) {
        self.installed_self();
    }

    /// Looks again once nothing that may be older than the last step is under way.
    pub(super) fn poll_setup_look_again(&mut self) -> bool {
        if !self.setup_look_again || self.sandbox_check.is_some() || self.programs_check.is_some() {
            return false;
        }
        self.setup_look_again = false;
        self.check_sandbox();
        self.check_programs();
        true
    }

    pub(super) fn finish_setup(&mut self) -> Result<(), String> {
        if self.simple_setup.pending {
            self.simple_setup.pending = false;
            self.simple_setup.save().map_err(|err| format!("Could not save that: {err:#}"))?;
        }
        if std::mem::take(&mut self.store.setup_pending) {
            self.store.save(&paths::store_file()).map_err(|err| format!("Could not save that: {err:#}"))?;
        }
        Ok(())
    }

    /// In this machine's own file: whether sandboxes can run here says nothing of another machine sharing the main one.
    pub(super) fn set_isolation(&mut self, mode: IsolationMode) -> Result<(), String> {
        self.write_machine_setting(&format!("[isolation]\nmode = \"{}\"", mode.name()))
    }

    /// Merges `fragment` into this machine's own config file, keeping what else it says.
    fn write_machine_setting(&mut self, fragment: &str) -> Result<(), String> {
        let [_, file] = Config::source_paths(&self.config_path);
        let unsaved = |why: String| format!("Could not save that: {why}");
        let before = match std::fs::read_to_string(&file) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(unsaved(err.to_string())),
        };
        let after = atomics_core::selfconfig::edit(before.as_deref().unwrap_or(""), fragment, &[]).map_err(unsaved)?;
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder).map_err(|err| unsaved(err.to_string()))?;
        }
        atomics_core::selfconfig::write(&file, before.as_deref(), &after, atomics_core::store::now())
            .map_err(|err| unsaved(format!("{err:#}")))?;
        self.reload_config();
        Ok(())
    }

    /// A folder nobody has made a repository yet, so that a first project needs no terminal.
    pub(super) fn init_project(&mut self, path: &str) -> Result<(), String> {
        let folder = paths::expand(path);
        if !folder.is_dir() {
            return Err(format!("{} is not a folder", folder.display()));
        }
        if workspace::project_root(&folder).is_err() {
            git::git(&folder, &["init", "-q"]).map_err(|err| format!("Could not start a repository: {err:#}"))?;
        }
        let root = workspace::project_root(&folder).map_err(|err| format!("{err:#}"))?;
        self.add_project(&root);
        Ok(())
    }

    /// Holds a person's start whose sandbox could not be had until they answer, and takes its tile away.
    pub(super) fn ask_about_start(&mut self, launch: AgentLaunch, why: String) {
        self.sessions.retain(|session| session.id != launch.id);
        if self.active.as_deref() == Some(launch.id.as_str()) {
            self.active = None;
        }
        // A start nobody answered stands in for this one; its scratch folder goes the way it would have.
        if let Some(earlier) = self.start_choice.take() {
            remove_scratch(&earlier.cwd);
        }
        let sandbox_required = self.current_project_config(&launch.cwd).isolation.required;
        self.start_choice = Some(PendingStart {
            agent: launch.agent.name,
            cwd: launch.cwd,
            args: launch.extra_args,
            prompt: launch.prompt,
            why: crate::sandbox::without_opt_out(&why),
            sandbox_required,
        });
    }

    /// `run_here` runs it on this machine, which this machine then keeps doing; anything else leaves it.
    pub(super) fn answer_start(&mut self, run_here: bool) -> Result<(), String> {
        let Some(pending) = self.start_choice.take() else { return Ok(()) };
        if !run_here || pending.sandbox_required {
            remove_scratch(&pending.cwd);
            return Ok(());
        }
        self.set_isolation(IsolationMode::Bare)?;
        let selection = self.active.clone();
        let id = self.start_as(
            &pending.agent,
            pending.cwd,
            None,
            pending.args,
            pending.prompt,
            Role::Agent(Some(IsolationMode::Bare)),
        )?;
        // Whoever else was on screen stays there.
        if selection.is_some() {
            self.active = selection;
        } else {
            self.active = Some(id);
        }
        Ok(())
    }
}

fn remove_scratch(folder: &Path) {
    if paths::scratch_of(folder).is_some() {
        let _ = std::fs::remove_dir_all(folder);
    }
}
