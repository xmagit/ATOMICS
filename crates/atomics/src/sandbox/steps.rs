//! What sandboxes need on this machine, as Settings shows it, and running one step of it on a click. The work itself is
//! what `atomics setup` runs ([`super::setup`]). What takes a terminal or sudo says its exact command instead: installing
//! system packages, joining the `kvm` group, firmware settings, and `sbx secret rm`, whose list keeps a credential filed
//! under the wrong name off the command line. Nothing here shows a credential or what a helper printed about one.
use super::*;
use atomics_core::projection::{SetupAction, SetupClick, SetupStep};
use std::sync::Arc;

/// How the machine stands for sandboxes.
pub(crate) struct Machine {
    pub(crate) sbx: String,
    pub(crate) config: SandboxConfig,
    /// A Claude agent would run in a sandbox if sandboxes were on.
    pub(crate) claude: bool,
    /// A Codex agent would.
    pub(crate) codex: bool,
    pub(crate) keychain: crate::relay::Keychain,
}

fn step(name: &str, done: bool, note: &str) -> SetupStep {
    SetupStep { name: name.into(), done, note: note.into(), ..SetupStep::default() }
}

/// What this machine still needs. Reads the keychain and asks sbx: call it off the window's thread.
pub(crate) fn status(machine: &Machine) -> Vec<SetupStep> {
    let mac = cfg!(target_os = "macos");
    let has = |program: &str| crate::setup::find_program(program).is_some();
    let command = |program: &str| checks::install_command(program, mac, &has);
    let mut steps = Vec::new();
    #[cfg(target_os = "linux")]
    {
        let (kvm, in_group) = checks::kvm_state();
        steps.push(step(
            "Hardware virtualization",
            kvm,
            if kvm {
                ""
            } else {
                "Every sandbox is a small VM. Turn on virtualization in your firmware settings and load kvm_intel or \
                 kvm_amd; in a VM, it takes nested virtualization. No command here can do that."
            },
        ));
        steps.push(SetupStep {
            command: (!in_group).then(|| "sudo usermod -aG kvm \"$USER\"".to_string()),
            ..step("The kvm group", in_group, if in_group { "" } else { "Takes sudo. Log out and back in after it." })
        });
    }
    if machine.claude && !mac {
        let found = has("secret-tool");
        steps.push(SetupStep {
            command: (!found).then(|| command("secret-tool")),
            ..step(
                "The OS keychain",
                found,
                if found {
                    ""
                } else {
                    "Takes sudo. Then unlock the desktop keyring. Sandboxes keep credentials there."
                },
            )
        });
    }
    let installed = checks::query_version(&machine.sbx).is_some_and(|version| version >= MIN_SBX_VERSION);
    let brew = mac && has("brew");
    let sbx = format!("sbx {} or newer.", isolation::version_text(MIN_SBX_VERSION));
    steps.push(SetupStep {
        clicks: (!installed && brew).then(|| SetupClick::new(SetupAction::InstallSbx, "Install")).into_iter().collect(),
        command: (!installed && !brew).then(|| command("sbx")),
        ..step("Docker Sandboxes", installed, if installed { "" } else { &sbx })
    });
    if !installed {
        steps.push(step("Docker account", false, "After Docker Sandboxes."));
        return steps;
    }
    let diagnosis: Option<Diagnosis> =
        checks::query(&machine.sbx, &["diagnose", "--json"]).and_then(|value| serde_json::from_value(value).ok());
    let signed_in = diagnosis.as_ref().is_some_and(Diagnosis::signed_in);
    steps.push(SetupStep {
        clicks: (!signed_in).then(|| SetupClick::new(SetupAction::DockerLogin, "Sign in")).into_iter().collect(),
        ..step("Docker account", signed_in, if signed_in { "" } else { "Opens your browser." })
    });
    let policy = diagnosis.as_ref().is_some_and(Diagnosis::policy_initialized);
    let network = machine.config.network.name();
    steps.push(SetupStep {
        clicks: (!policy)
            .then(|| SetupClick::new(SetupAction::NetworkPolicy, &format!("Use {network}")))
            .into_iter()
            .collect(),
        ..step("Network policy", policy, if policy { "" } else { "What sandboxes may reach." })
    });
    let secrets = checks::secrets(&machine.sbx);
    if machine.claude {
        let done = machine.keychain.claude_token().is_some() || secrets.as_ref().is_some_and(|s| s.anthropic);
        steps.push(SetupStep {
            clicks: (!done)
                .then(|| SetupClick {
                    paste: Some("Anthropic API key or token".into()),
                    ..SetupClick::new(SetupAction::ClaudeCredential, "Save")
                })
                .into_iter()
                .collect(),
            command: (!done).then(|| "claude setup-token".to_string()),
            ..step(
                "Claude in sandboxes",
                done,
                if done {
                    ""
                } else {
                    "An Anthropic API key goes to sbx. Or paste the token this command prints, which stays in the OS \
                     keychain."
                },
            )
        });
    }
    if machine.codex {
        let done = secrets.as_ref().is_some_and(|s| s.openai);
        let clicks = [
            SetupClick::new(SetupAction::OpenAiLogin, "Sign in with ChatGPT"),
            SetupClick { paste: Some("OpenAI API key".into()), ..SetupClick::new(SetupAction::OpenAiKey, "Save") },
        ];
        steps.push(SetupStep {
            clicks: if done { Vec::new() } else { clicks.into() },
            ..step("Codex in sandboxes", done, if done { "" } else { "Only if you run Codex in a sandbox." })
        });
    }
    if machine.config.mcp.iter().any(|name| name == "linear") {
        let servers = checks::query(&machine.sbx, &["mcp", "ls", "--json"]);
        let done = servers
            .as_ref()
            .and_then(serde_json::Value::as_array)
            .is_some_and(|list| list.iter().any(|server| server["name"] == "linear" && server["status"] == "ready"));
        steps.push(SetupStep {
            clicks: (!done).then(|| SetupClick::new(SetupAction::LinearConnect, "Connect")).into_iter().collect(),
            ..step(
                "Linear in sandboxes",
                done,
                if done { "" } else { "Your ticket profile uses it. Opens your browser." },
            )
        });
    }
    if secrets.as_ref().is_some_and(|s| s.github) {
        steps.push(SetupStep {
            clicks: vec![SetupClick {
                confirm: Some("Remove sbx's github secret? Sandboxed agents can then no longer push as you.".into()),
                ..SetupClick::new(SetupAction::RemoveGithubSecret, "Remove")
            }],
            ..step("A github secret in sbx", false, "It lets agents push as you.")
        });
    }
    if let Some(misfiled) = secrets.as_ref().map(|s| &s.misfiled).filter(|misfiled| !misfiled.is_empty()) {
        // Removing one by name would put what may be a pasted credential on a command line.
        steps.push(SetupStep {
            command: Some("sbx secret rm".to_string()),
            ..step(
                "Secrets filed wrongly in sbx",
                false,
                &format!("{}. Choose each from sbx's own list, in a terminal.", checks::misfiled_text(misfiled)),
            )
        });
    }
    let forwarding_off = checks::setting(&machine.sbx, "ssh.agentForwardingEnabled").as_deref() == Some("false")
        && checks::setting(&machine.sbx, "ssh.agentSocketPath").as_deref() == Some("");
    let ssh_off = forwarding_off && !machine.config.ssh_agent;
    let mut clicks = Vec::new();
    if machine.config.ssh_agent {
        clicks.push(SetupClick::new(SetupAction::SshAgentOff, "Turn it off in the config"));
    }
    if !forwarding_off {
        clicks.push(SetupClick::new(SetupAction::SshForwardingOff, "Turn it off in sbx"));
    }
    steps.push(SetupStep {
        clicks,
        ..step(
            "SSH agent out of sandboxes",
            ssh_off,
            if ssh_off { "" } else { "With it, agents can sign and push as you." },
        )
    });
    steps
}

/// What a step is doing, for the person watching it.
pub(crate) fn progress(action: SetupAction) -> &'static str {
    match action {
        SetupAction::Simple | SetupAction::Full => "Choosing setup…",
        SetupAction::CheckVmRequirements => "Checking Lima and Rosetta…",
        SetupAction::DownloadVm => "Downloading and verifying the hardened VM…",
        SetupAction::CreateVm => "Creating and checking the hardened VM…",
        SetupAction::VmLogin | SetupAction::CheckVmLogins => "Checking sign-ins inside the VM…",
        SetupAction::ConnectVm => "Checking the VM host connection…",
        SetupAction::SimpleMode => "Saving simple mode and connecting the window…",
        SetupAction::InstallSelf => "Installing ATOMICS…",
        SetupAction::InstallClaude => "Installing Claude Code…",
        SetupAction::InstallCodex => "Installing Codex…",
        SetupAction::InstallGit => "Asking macOS to install git…",
        SetupAction::InstallSbx => "Installing Docker Sandboxes…",
        SetupAction::AzureLogin | SetupAction::DockerLogin | SetupAction::OpenAiLogin | SetupAction::LinearConnect => {
            "Finish in your browser…"
        }
        SetupAction::NetworkPolicy => "Setting the network policy…",
        SetupAction::ClaudeCredential | SetupAction::OpenAiKey => "Saving…",
        SetupAction::RemoveGithubSecret => "Removing…",
        SetupAction::SshForwardingOff | SetupAction::SshAgentOff => "Turning it off…",
    }
}

/// A step to run off the window's thread.
pub(crate) struct Job {
    pub(crate) action: SetupAction,
    pub(crate) value: Option<String>,
    pub(crate) machine: Machine,
    pub(crate) stop: Arc<AtomicBool>,
}

impl Job {
    pub(crate) fn run(self) -> Result<()> {
        let program = self.machine.sbx.as_str();
        let stop = Some(self.stop.as_ref());
        let pasted = || self.value.as_deref().unwrap_or_default();
        match self.action {
            SetupAction::Simple
            | SetupAction::Full
            | SetupAction::CheckVmRequirements
            | SetupAction::DownloadVm
            | SetupAction::CreateVm
            | SetupAction::VmLogin
            | SetupAction::CheckVmLogins
            | SetupAction::ConnectVm
            | SetupAction::SimpleMode => bail!("VM onboarding is handled by the host"),
            SetupAction::InstallSelf => crate::install::install_self().map(drop),
            SetupAction::InstallClaude => install("claude", "claude", &self.stop),
            SetupAction::InstallCodex => install("codex", "codex", &self.stop),
            SetupAction::InstallGit => install("git", "git", &self.stop),
            SetupAction::InstallSbx => install("sbx", "sbx", &self.stop),
            SetupAction::AzureLogin => {
                let az = crate::setup::find_program("az").ok_or_else(|| anyhow::anyhow!("az is not installed"))?;
                setup::private_command_until(&az.to_string_lossy(), &["login"], "", stop)
            }
            SetupAction::DockerLogin => setup::docker_login(program, stop),
            SetupAction::NetworkPolicy => setup::network_policy(program, &self.machine.config),
            SetupAction::ClaudeCredential => setup::store_claude_credential(program, &self.machine.keychain, pasted()),
            SetupAction::OpenAiLogin => setup::openai_login(program, stop),
            SetupAction::OpenAiKey => {
                if pasted().is_empty() {
                    bail!("the key is empty")
                }
                crate::credentials::Store::new(self.machine.keychain.clone()).put("openai", pasted(), None)?;
                setup::store_openai_key(program, pasted())
            }
            SetupAction::LinearConnect => {
                let servers = checks::query(program, &["mcp", "ls", "--json"]);
                let registered = servers
                    .as_ref()
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|list| list.iter().any(|server| server["name"] == "linear"));
                if !registered {
                    setup::register_linear(program)?;
                }
                setup::authorize_linear(program, stop)
            }
            SetupAction::RemoveGithubSecret => setup::remove_github_secret(program),
            SetupAction::SshForwardingOff => setup::ssh_forwarding_off(program),
            SetupAction::SshAgentOff => bail!("this one changes the config, which the host does itself"),
        }
    }
}

/// Runs what gets `program`, with what the login shell finds now, and checks that it is there after: `curl | bash` says
/// nothing of a download that failed. An installer's own output holds no credential, so its last line says why.
fn install(program: &str, wanted: &str, stop: &Arc<AtomicBool>) -> Result<()> {
    let mac = cfg!(target_os = "macos");
    let path = crate::setup::refresh_login_path();
    let has = |name: &str| crate::setup::find_on_path(name, &path).is_some();
    let line = checks::install_command(program, mac, &has);
    let mut shell = Command::new("/bin/sh");
    shell.args(["-c", &line]).env("PATH", &path);
    let output = setup::private_output_until(&mut shell, CREATE_TIMEOUT, Some(stop.as_ref())).map_err(|error| {
        if matches!(error, setup::OutputError::Stopped) {
            anyhow::Error::from(setup::Stopped)
        } else {
            anyhow::anyhow!("`{line}` did not finish")
        }
    })?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let last = said.lines().rev().find(|text| !text.trim().is_empty()).unwrap_or("no output");
        bail!("`{line}` failed: {}", last.trim().chars().take(240).collect::<String>())
    }
    let path = crate::setup::refresh_login_path();
    // Apple's own dialog installs git after this has returned.
    if !mac && crate::setup::find_on_path(wanted, &path).is_none() {
        bail!("`{line}` finished, but {wanted} is not on your PATH")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_KEY: &str = "sk-ant-api03-PRIVATE-fixture";
    const OPENAI: &str = "sk-PRIVATE-openai-fixture";

    /// A fake sbx with these markers, and the keychain beside it.
    fn machine(markers: &[&str]) -> (crate::test_support::TempDir, Machine) {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).to_string_lossy().into_owned();
        crate::test_support::copy_program(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"),
            dir.path().join("keychain"),
        );
        for marker in markers {
            std::fs::write(dir.path().join(marker), "").unwrap();
        }
        let keychain = crate::relay::Keychain::at(&dir.path().join("keychain"), cfg!(target_os = "macos"));
        let machine = Machine { sbx, config: SandboxConfig::default(), claude: true, codex: true, keychain };
        (dir, machine)
    }

    fn find<'a>(steps: &'a [SetupStep], name: &str) -> &'a SetupStep {
        steps.iter().find(|step| step.name == name).unwrap_or_else(|| panic!("{name} in {steps:?}"))
    }

    fn run(machine: Machine, action: SetupAction, value: Option<&str>) -> (Result<()>, Machine) {
        let Machine { sbx, config, claude, codex, keychain } = machine;
        let again = Machine { sbx: sbx.clone(), config: config.clone(), claude, codex, keychain: keychain.clone() };
        let stop = Arc::new(AtomicBool::new(false));
        let job = Job {
            action,
            value: value.map(str::to_string),
            machine: Machine { sbx, config, claude, codex, keychain },
            stop,
        };
        (job.run(), again)
    }

    #[test]
    fn a_fresh_machine_reaches_every_step_with_a_click_or_says_its_command() {
        let (dir, mut machine) = machine(&["signed-out", "no-policy", "removed-anthropic", "no-openai"]);
        machine.config.mcp = vec!["linear".into()];
        let steps = status(&machine);
        for (name, action) in [
            ("Docker account", SetupAction::DockerLogin),
            ("Network policy", SetupAction::NetworkPolicy),
            ("Claude in sandboxes", SetupAction::ClaudeCredential),
            ("Codex in sandboxes", SetupAction::OpenAiLogin),
            ("Linear in sandboxes", SetupAction::LinearConnect),
        ] {
            let step = find(&steps, name);
            assert!(!step.done && step.clicks.iter().any(|click| click.action == action), "{step:?}");
        }
        assert!(find(&steps, "Claude in sandboxes").clicks[0].paste.is_some(), "it takes a paste");
        assert_eq!(find(&steps, "Claude in sandboxes").command.as_deref(), Some("claude setup-token"));
        assert!(find(&steps, "Docker Sandboxes").done);

        for (action, value) in [
            (SetupAction::DockerLogin, None),
            (SetupAction::NetworkPolicy, None),
            (SetupAction::ClaudeCredential, Some(API_KEY)),
            (SetupAction::OpenAiKey, Some(OPENAI)),
            (SetupAction::LinearConnect, None),
            (SetupAction::SshForwardingOff, None),
        ] {
            let (result, again) = run(machine, action, value);
            result.unwrap_or_else(|error| panic!("{action:?}: {error:#}"));
            machine = again;
        }
        let steps = status(&machine);
        #[cfg(target_os = "linux")]
        let steps = {
            // This host's own KVM and keychain tool are no part of what the fake says.
            let host = ["Hardware virtualization", "The kvm group", "The OS keychain"];
            steps.into_iter().filter(|step| !host.contains(&step.name.as_str())).collect::<Vec<_>>()
        };
        assert!(steps.iter().all(|step| step.done && step.clicks.is_empty()), "{steps:?}");
        // What was pasted went over stdin, to the right place, and into no argument and no row.
        let argv = format!("{:?}", fake::calls(dir.path()));
        assert!(!argv.contains("PRIVATE"), "{argv}");
        assert!(!format!("{steps:?}").contains("PRIVATE"));
        assert!(fake::calls_to(dir.path(), &["secret", "set"]).iter().any(|call| call[2] == "anthropic"));
        assert_eq!(fake::calls_to(dir.path(), &["mcp", "add"]).len(), 1);
    }

    #[test]
    fn without_sbx_only_its_install_is_offered_and_steps_that_need_it_wait() {
        let (_dir, mut machine) = machine(&[]);
        machine.sbx = "/nonexistent/sbx".into();
        let steps = status(&machine);
        let sbx = find(&steps, "Docker Sandboxes");
        assert!(!sbx.done && !sbx.note.is_empty());
        assert!(sbx.command.is_some() || !sbx.clicks.is_empty(), "there is a way forward: {sbx:?}");
        let account = find(&steps, "Docker account");
        assert!(!account.done && account.clicks.is_empty());
        assert!(!steps.iter().any(|step| step.name == "Network policy"));
    }

    #[test]
    fn what_a_click_may_not_do_says_its_command_and_what_cannot_be_undone_asks_first() {
        let (dir, machine) = machine(&[]);
        std::fs::write(
            dir.path().join("secrets.json"),
            r#"[{"name":"anthropic"},{"name":"openai"},{"name":"github"},{"name":"MISFILED-private"}]"#,
        )
        .unwrap();
        let steps = status(&machine);
        let github = find(&steps, "A github secret in sbx");
        assert!(github.clicks[0].confirm.is_some(), "removing it cannot be undone");
        let misfiled = find(&steps, "Secrets filed wrongly in sbx");
        assert_eq!(misfiled.command.as_deref(), Some("sbx secret rm"));
        assert!(misfiled.clicks.is_empty(), "a name could be a credential, and an argument is no place for one");
        assert!(!format!("{steps:?}").contains("MISFILED-private"));
        run(machine, SetupAction::RemoveGithubSecret, None).0.unwrap();
        assert!(fake::calls_to(dir.path(), &["secret", "rm"]).iter().any(|call| call[2] == "github"));
    }

    #[test]
    fn failing_steps_say_nothing_of_what_was_pasted() {
        let (dir, machine) = machine(&["removed-anthropic", "no-openai", "fail-secret-set"]);
        std::fs::write(dir.path().join("error-output"), API_KEY).unwrap();
        let (result, machine) = run(machine, SetupAction::ClaudeCredential, Some(API_KEY));
        assert!(!format!("{:#}", result.unwrap_err()).contains("PRIVATE"));
        let (result, machine) = run(machine, SetupAction::OpenAiKey, Some(OPENAI));
        assert!(!format!("{:#}", result.unwrap_err()).contains("PRIVATE"));
        let (result, _) = run(machine, SetupAction::ClaudeCredential, Some(""));
        assert!(result.is_err(), "nothing pasted is nothing to store");
    }

    #[test]
    fn stopping_a_sign_in_ends_it_and_is_no_failure() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("waits");
        crate::test_support::write_program(&program, "#!/bin/sh\nexec sleep 30\n");
        let stop = Arc::new(AtomicBool::new(false));
        let waiting = stop.clone();
        let started = std::time::Instant::now();
        let thread =
            std::thread::spawn(move || setup::docker_login(&program.to_string_lossy(), Some(waiting.as_ref())));
        std::thread::sleep(Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let error = thread.join().unwrap().unwrap_err();
        assert!(error.is::<setup::Stopped>(), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(15));
    }
}
