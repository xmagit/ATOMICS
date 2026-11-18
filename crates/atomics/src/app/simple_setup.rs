//! Host actions for the optional VM wizard; Full setup uses its existing actions unchanged.
use super::*;
use SetupAction::*;
use atomics_core::projection::{SetupAction, SetupFailure};

impl State {
    pub(super) fn run_simple_setup(&mut self, action: SetupAction) -> Result<bool, String> {
        if !matches!(
            action,
            Simple
                | Full
                | CheckVmRequirements
                | DownloadVm
                | CreateVm
                | VmLogin
                | CheckVmLogins
                | ConnectVm
                | SimpleMode
        ) {
            return Ok(false);
        }
        let result = self.start_simple_step(action);
        if let Err(why) = result {
            self.setup_failure = Some(SetupFailure { action, why });
        }
        Ok(true)
    }

    fn start_simple_step(&mut self, action: SetupAction) -> Result<(), String> {
        let error = |err: anyhow::Error| format!("{err:#}");
        if matches!(action, Simple | Full) {
            return self.simple_setup.choose(action == Simple).map_err(error);
        }
        self.simple_setup.ready(action).map_err(error)?;
        if action == VmLogin {
            return self.simple_login();
        }
        if action == SimpleMode
            && self.sessions.iter().any(|session| session.terminal.as_ref().is_some_and(|terminal| !terminal.exited()))
        {
            return Err("Close the sign-in terminal and any local sessions before connecting this window to the VM. You can finish later.".into());
        }
        if action == DownloadVm {
            self.simple_setup.discard_download().map_err(error)?;
            self.simple_setup.download =
                Some(paths::atomics_home().join("setup-downloads").join(uuid::Uuid::new_v4().to_string()));
            self.simple_setup.done.retain(|done| *done != DownloadVm);
            self.simple_setup.save().map_err(error)?;
        }
        let program = crate::setup::executable_path().map_err(|err| err.to_string())?;
        #[cfg(test)]
        let program = self.simple_setup.program.clone().unwrap_or(program);
        let command = self.simple_setup.command(action, &program).map_err(error)?;
        let stop = Arc::new(AtomicBool::new(false));
        let output = Arc::new(std::sync::Mutex::new(crate::sandbox::steps::progress(action).to_string()));
        self.setup_job = Some(setup_screen::RunningSetup { action, stop: stop.clone(), output: Some(output.clone()) });
        let actions = self.action_tx.clone();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let result = crate::vm::simple::run(command, &stop, &|text| {
                *output.lock().unwrap() = text;
                wake.notify();
            });
            let _ = actions.send(Box::new(move |state| state.simple_finished(action, result)));
        });
        Ok(())
    }

    fn simple_finished(&mut self, action: SetupAction, result: anyhow::Result<()>) {
        self.setup_job = None;
        if let Err(error) = result {
            self.setup_failure = Some(SetupFailure { action, why: format!("{error:#}") });
            return;
        }
        if action == SimpleMode {
            if let Err(error) = self.simple_setup.discard_download() {
                self.setup_failure = Some(SetupFailure {
                    action,
                    why: format!("Could not remove the staged download: {error:#}. Retry this step."),
                });
                return;
            }
            // Keep retries usable even if the window cannot restart after deleting its staging.
            if let Err(error) = self.simple_setup.save() {
                self.setup_failure =
                    Some(SetupFailure { action, why: format!("Could not save setup progress: {error:#}") });
                return;
            }
            self.reload_config();
            let reply = self.restart_using(false, Some("Simple setup complete".into()), || {
                crate::restart::Target::with_args(&["--connect".into(), "vm".into()])
            });
            if let Some(why) = reply.error {
                self.setup_failure = Some(SetupFailure { action, why });
                return;
            }
            self.simple_setup.pending = false;
        }
        if let Err(error) = self.simple_setup.complete(action) {
            self.setup_failure =
                Some(SetupFailure { action, why: format!("Could not save setup progress: {error:#}") });
        }
    }

    fn simple_login(&mut self) -> Result<(), String> {
        let info = crate::vm::simple::owned_bundle().map_err(|err| format!("{err:#}"))?;
        if info.flavor != crate::vm::Flavor::Hardened {
            return Err("Simple sign-in requires the hardened VM bundle. Set up the hardened VM first.".into());
        }
        let command = crate::vm::guest_shell(&info).map_err(|err| format!("{err:#}"))?;
        let mut args: Vec<String> = command.get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        args.push("atomics-onboard".into());
        let agent = atomics_core::config::AgentConfig {
            name: "VM sign-in".into(),
            command: std::iter::once(command.get_program().to_string_lossy().into_owned()).chain(args).collect(),
            kind: atomics_core::config::AgentKind::Shell,
            busy_text: None,
            isolation: Some(atomics_core::isolation::IsolationMode::Bare),
        };
        let cwd = paths::atomics_home().join("setup-terminal");
        std::fs::create_dir_all(&cwd).map_err(|err| err.to_string())?;
        let id = self.launch_agent(
            AgentLaunch {
                id: uuid::Uuid::new_v4().to_string(),
                agent,
                cwd,
                resume: None,
                extra_args: vec![],
                prompt: None,
                chief: false,
                runtime: AgentRuntime::Bare,
                ref_guard: None,
                relay: None,
                interactive: true,
            },
            None,
        )?;
        self.active = Some(id);
        Ok(())
    }
}
