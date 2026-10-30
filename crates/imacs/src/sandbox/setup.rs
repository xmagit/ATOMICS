//! Guided, resumable host setup. Child output is private, and credentials travel over stdin.
use super::*;
use std::io::{BufRead, IsTerminal, Write};

pub fn run(workspace: Option<&str>) -> Result<()> {
    use imacs_core::config::Config;
    let main = Config::load_or_create(&paths::config_file())
        .map_err(|_| anyhow::anyhow!("cannot load IMACS config; fix it before running setup"))?;
    let claude = checks::claude_sandboxed(&main, workspace);
    let config = checks::setup_config(&main, workspace)
        .map_err(|_| anyhow::anyhow!("cannot load workspace isolation/ticket config; fix it before running setup"))?;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout();
    writeln!(
        output,
        "IMACS fresh-machine setup. Completed steps are reused; Ctrl-C or EOF stops safely. Rerun this command to resume."
    )?;
    // Installation, privilege elevation and group/session changes belong to the
    // user. Keep checking after they run the displayed OS-specific commands.
    let program = loop {
        // Installers often add ~/.local/bin or Homebrew to shell startup files.
        // Re-read that PATH instead of using the app's cached launch-time PATH.
        let path = crate::setup::login_shell_var("PATH").or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        let missing: Vec<_> = checks::host_lines_with(|name| crate::setup::find_on_path(name, &path).is_some())
            .into_iter()
            .filter(|line| line.starts_with('✗'))
            .collect();
        if missing.is_empty() {
            let program = crate::setup::find_on_path("sbx", &path).map(|p| p.to_string_lossy().into_owned());
            if let Some(program) = program
                && checks::query_version(&program).is_some_and(|v| v >= MIN_SBX_VERSION)
            {
                break program;
            }
            writeln!(
                output,
                "sbx {} or newer required: {}",
                isolation::version_text(MIN_SBX_VERSION),
                checks::install_hint("sbx", cfg!(target_os = "macos"))
            )?;
        }
        for line in missing {
            writeln!(output, "{line}")?;
        }
        retry(&mut input, &mut output)?;
    };
    guided(&program, claude, &config, &mut input, &mut output, stdin.is_terminal())?;
    writeln!(output, "Sandbox setup complete. Running imacs doctor (includes optional host/workspace diagnostics):")?;
    crate::doctor::run(workspace);
    Ok(())
}

fn retry(input: &mut impl BufRead, output: &mut impl Write) -> Result<()> {
    write!(
        output,
        "Run the suggested commands in another host terminal, then press Enter to check again (q to stop): "
    )?;
    output.flush()?;
    if answer(input)?.eq_ignore_ascii_case("q") {
        bail!("setup stopped; rerun `imacs setup` to resume")
    }
    Ok(())
}

fn consent(input: &mut impl BufRead, output: &mut impl Write, message: &str) -> Result<()> {
    write!(output, "{message} [y/N] ")?;
    output.flush()?;
    if !answer(input)?.eq_ignore_ascii_case("y") {
        bail!("setup stopped; rerun `imacs setup` to resume")
    }
    Ok(())
}

/// `claude` says whether Claude agents run sandboxed, and so need sbx's anthropic secret.
fn guided(
    program: &str,
    claude: bool,
    config: &SandboxConfig,
    input: &mut impl BufRead,
    output: &mut impl Write,
    terminal: bool,
) -> Result<()> {
    if checks::query_version(program).is_none_or(|v| v < MIN_SBX_VERSION) {
        bail!("sbx is missing or too old; install the required version and rerun `imacs setup`")
    }
    let diagnosis = || -> Result<Diagnosis> {
        checks::query(program, &["diagnose", "--json"])
            .and_then(|value| serde_json::from_value(value).ok())
            .ok_or_else(|| anyhow::anyhow!("cannot read sbx diagnosis; run `sbx diagnose` privately and retry setup"))
    };
    // diagnose exits nonzero for unrelated failures; its JSON is still valid.
    if !diagnosis()?.signed_in() {
        consent(input, output, "Sign in to Docker with sbx login? Complete the browser sign-in.")?;
        private_command(program, &["login"], "")?;
        if !diagnosis()?.signed_in() {
            bail!("Docker sign-in incomplete; run `sbx login` in a host terminal and retry setup")
        }
    }
    if !diagnosis()?.policy_initialized() {
        consent(input, output, &format!("Initialize sbx's global network policy to {}?", config.network.name()))?;
        safe_sbx(
            program,
            &["policy", "init", config.network.name()],
            "network policy initialization failed; retry setup",
        )?;
    }
    let secrets =
        checks::secrets(program).ok_or_else(|| anyhow::anyhow!("cannot check known secret services; retry setup"))?;
    // A Claude subscription cannot authenticate in a local sandbox: sbx v0.50.1 strips a custom
    // OAuth route, and sends its anthropic secret as an API key (docs/isolation.md).
    if !claude {
        writeln!(
            output,
            "Claude agents run bare by default; to sandbox Claude, store an Anthropic API key in sbx's built-in anthropic secret (stdin)."
        )?;
    } else if !secrets.anthropic {
        consent(
            input,
            output,
            "Claude agents run sandboxed: store an Anthropic API key in sbx's anthropic secret? The key is sent over stdin.",
        )?;
        let mut token = credential(input, output, "Anthropic API key: ", terminal)?;
        let result = if token.is_empty() {
            Err(anyhow::anyhow!("Anthropic API key is empty; rerun setup to resume"))
        } else if token.starts_with("sk-ant-oat") {
            Err(anyhow::anyhow!(
                "that is a Claude subscription token, which local sandboxes cannot use; enter an API key"
            ))
        } else {
            private_command(program, &["secret", "set", "anthropic"], &format!("{token}\n"))
        };
        token.clear();
        result?;
    }
    if !secrets.openai {
        consent(input, output, "Store an OpenAI API key in sbx? The key is sent over stdin.")?;
        let mut token = credential(input, output, "OpenAI API key: ", terminal)?;
        if token.is_empty() {
            bail!("OpenAI key is empty; rerun setup to resume")
        }
        let result = private_command(program, &["secret", "set", "openai"], &format!("{token}\n"));
        token.clear();
        result?;
    }
    let servers = checks::query(program, &["mcp", "ls", "--json"])
        .filter(|value| value.is_array())
        .ok_or_else(|| anyhow::anyhow!("cannot read sandbox MCP status; retry setup"))?;
    if config.mcp.iter().any(|name| name == "linear") {
        let linear = servers.as_array().unwrap().iter().find(|server| server["name"] == "linear");
        if linear.is_none() {
            consent(input, output, "Register the Linear MCP server with sbx?")?;
            safe_sbx(
                program,
                &["mcp", "add", "linear", "--url", "https://mcp.linear.app/mcp"],
                "Linear registration failed; retry setup",
            )?;
        }
        if linear.is_none_or(|server| server["status"] != "ready") {
            consent(input, output, "Authorize sbx's Linear MCP server? Complete the browser authorization.")?;
            private_command(program, &["mcp", "auth", "linear"], "y\n")?;
        }
    }
    if secrets.github {
        consent(input, output, "Remove sbx's github secret so sandbox agents cannot push as you?")?;
        safe_sbx(program, &["secret", "rm", "github", "--force"], "could not remove github secret; retry setup")?;
    }
    let misfiled = checks::secrets(program)
        .ok_or_else(|| anyhow::anyhow!("cannot check known secret services; retry setup"))?
        .misfiled;
    if !misfiled.is_empty() {
        consent(
            input,
            output,
            &format!(
                "Remove {} from sbx? Names stay hidden: they may hold a pasted credential.",
                checks::misfiled_text(&misfiled)
            ),
        )?;
        for secret in &misfiled {
            safe_sbx(program, &secret.remove_args(), "could not remove a misfiled secret; retry setup")?;
        }
    }
    // Read the store again through doctor's lines: setup is not done with a secret doctor reports missing.
    if let Some(failed) =
        checks::secret_lines(checks::secrets(program).as_ref(), claude).iter().find(|line| line.starts_with('✗'))
    {
        bail!("sbx does not list what setup stored: {failed}. Rerun `imacs setup`")
    }
    if config.ssh_agent {
        bail!("set [isolation.sandbox] ssh_agent = false, then rerun setup")
    }
    if checks::setting(program, "ssh.agentForwardingEnabled").as_deref() != Some("false")
        || checks::setting(program, "ssh.agentSocketPath").as_deref() != Some("")
    {
        consent(input, output, "Disable global sbx SSH agent forwarding?")?;
        safe_sbx(
            program,
            &["settings", "set", "ssh.agentForwardingEnabled", "false"],
            "could not disable SSH forwarding; retry setup",
        )?;
        safe_sbx(program, &["settings", "unset", "ssh.agentSocketPath"], "could not clear SSH socket; retry setup")?;
    }
    let lines = checks::sandbox_lines(program, config, claude);
    for line in &lines {
        writeln!(output, "{line}")?;
    }
    if lines.iter().any(|line| line.starts_with('✗')) {
        bail!("sandbox checks still need attention; follow the hints and rerun `imacs setup`")
    }
    Ok(())
}

fn credential(input: &mut impl BufRead, output: &mut impl Write, prompt: &str, terminal: bool) -> Result<String> {
    write!(output, "{prompt}")?;
    output.flush()?;
    let echo = Echo::hide(terminal)?;
    let value = answer(input);
    drop(echo);
    writeln!(output)?;
    value
}

fn answer(input: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    if input.take(8194).read_line(&mut line)? == 0 {
        bail!("setup input ended; rerun `imacs setup` to resume")
    }
    if line.len() > 8192 || !line.ends_with('\n') {
        bail!("setup requires a complete input line of at most 8192 bytes")
    }
    Ok(line.trim().to_string())
}

fn safe_sbx(program: &str, args: &[&str], message: &str) -> Result<()> {
    match sbx_output(program, args, false, TIMEOUT) {
        Ok((status, _, _)) if status.success() => Ok(()),
        _ => bail!("{message}"),
    }
}

/// Child output is discarded: a failed credential writer may echo its input.
/// Browser auth can take several minutes, but a stuck helper remains bounded.
fn private_command(program: &str, args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| anyhow::anyhow!("could not start setup helper; check installation and retry setup"))?;
    let result = child.stdin.take().expect("piped stdin").write_all(input.as_bytes());
    let deadline = Instant::now() + CREATE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() && result.is_ok() => return Ok(()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // SAFETY: the setup helper owns this process group, including its children.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                bail!("setup helper failed or timed out; complete browser sign-in and retry `imacs setup`")
            }
        }
    }
}

/// Restore terminal echo even when token input or validation fails.
struct Echo(Option<libc::termios>);
impl Echo {
    fn hide(terminal: bool) -> Result<Self> {
        if !terminal {
            return Ok(Self(None));
        }
        // SAFETY: termios is initialized by tcgetattr before being read; stdin is
        // a terminal here, and tcsetattr receives a valid termios pointer.
        unsafe {
            let mut previous = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut previous) != 0 {
                bail!("could not read terminal settings; token input cancelled")
            }
            let mut hidden = previous;
            hidden.c_lflag &= !libc::ECHO;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) != 0 {
                bail!("could not hide terminal input; token input cancelled")
            }
            Ok(Self(Some(previous)))
        }
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            // SAFETY: these are the settings read from this terminal by hide().
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &previous);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const API_KEY: &str = "sk-ant-api03-PRIVATE-fixture";
    const SUBSCRIPTION: &str = "sk-ant-oat01-PRIVATE-fixture";
    const OPENAI: &str = "sk-PRIVATE-openai-fixture";

    fn machine() -> (crate::test_support::TempDir, String) {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).to_string_lossy().into_owned();
        (dir, sbx)
    }

    fn touch(dir: &Path, markers: &[&str]) {
        for marker in markers {
            std::fs::write(dir.join(marker), "").unwrap();
        }
    }

    /// Guided setup with Claude agents sandboxed or bare (the default), and Linear or not.
    fn run_setup(sbx: &str, claude: bool, linear: bool, answers: &str) -> (Result<()>, String) {
        let config = SandboxConfig { mcp: if linear { vec!["linear".into()] } else { vec![] }, ..Default::default() };
        let mut output = Vec::new();
        let result = guided(sbx, claude, &config, &mut answers.as_bytes(), &mut output, false);
        let output = String::from_utf8(output).unwrap();
        for private in [API_KEY, SUBSCRIPTION, OPENAI, "MISFILED-private", "SCOPED-private"] {
            assert!(!output.contains(private));
            if let Err(error) = &result {
                assert!(!error.to_string().contains(private));
            }
        }
        (result, output)
    }

    #[test]
    fn fresh_setup_reaches_green_with_claude_bare_and_reruns_without_mutations() {
        let (dir, sbx) = machine();
        touch(
            dir.path(),
            &["signed-out", "no-policy", "removed-anthropic", "no-openai", "misfiled-secret", "misfiled-scoped"],
        );
        let (result, output) = run_setup(&sbx, false, true, &format!("y\ny\ny\n{OPENAI}\ny\ny\ny\n"));
        result.unwrap();
        assert!(
            output.lines().filter(|l| l.starts_with('✓') || l.starts_with('✗')).all(|l| l.starts_with('✓')),
            "{output}"
        );
        assert!(output.contains("Claude agents run bare by default; to sandbox Claude, store an Anthropic API key"));
        assert!(output.lines().any(|l| l.starts_with("sandboxed Claude: not configured")), "{output}");
        assert!(output.contains("Remove 2 misfiled secrets in scope anthropic, global from sbx?"), "{output}");
        let calls = fake::calls(dir.path());
        assert!(fake::calls_to(dir.path(), &["secret", "set-custom"]).is_empty());
        assert_eq!(fake::calls_to(dir.path(), &["secret", "set"]), [["secret", "set", "openai"]]);
        assert_eq!(std::fs::read_to_string(dir.path().join("secret-stdin")).unwrap().trim(), OPENAI);
        assert_eq!(
            fake::calls_to(dir.path(), &["mcp", "add"])[0],
            ["mcp", "add", "linear", "--url", "https://mcp.linear.app/mcp"]
        );
        // A misfiled name goes from the listing to `sbx secret rm` only, never to output.
        assert_eq!(
            fake::calls_to(dir.path(), &["secret", "rm"]),
            [
                vec!["secret", "rm", "sk-ant-MISFILED-private-value", "--force"],
                vec!["secret", "rm", "sk-ant-oat01-SCOPED-private-value", "--sandbox", "anthropic", "--force"],
            ]
        );
        let others: Vec<_> = calls.iter().filter(|c| !c.starts_with(&["secret".into(), "rm".into()])).collect();
        assert!(!format!("{others:?}").contains("PRIVATE"));
        let before = calls.len();
        run_setup(&sbx, false, true, "").0.unwrap();
        assert!(fake::calls(dir.path())[before..].iter().all(|c| matches!(
            c[0].as_str(),
            "version" | "diagnose" | "ls"
        ) || c[1] == "ls"
            || c[1] == "get"));
    }

    #[test]
    fn sandboxed_claude_stores_an_api_key_over_stdin_and_refuses_a_subscription_token() {
        let (dir, sbx) = machine();
        touch(dir.path(), &["removed-anthropic"]);
        for refused in ["n\n", "y\n\n", &format!("y\n{SUBSCRIPTION}\n")] {
            let error = run_setup(&sbx, true, false, refused).0.unwrap_err().to_string();
            assert!(fake::calls_to(dir.path(), &["secret", "set"]).is_empty(), "{refused:?}: {error}");
        }
        let error = run_setup(&sbx, true, false, &format!("y\n{SUBSCRIPTION}\n")).0.unwrap_err().to_string();
        assert!(error.contains("subscription token, which local sandboxes cannot use"), "{error}");
        let (result, output) = run_setup(&sbx, true, false, &format!("y\n{API_KEY}\n"));
        result.unwrap();
        assert_eq!(fake::calls_to(dir.path(), &["secret", "set"]), [["secret", "set", "anthropic"]]);
        assert_eq!(std::fs::read_to_string(dir.path().join("secret-stdin")).unwrap().trim(), API_KEY);
        assert!(output.contains("✓ sandbox Claude secret: anthropic API key"), "{output}");
        assert!(fake::calls_to(dir.path(), &["secret", "set-custom"]).is_empty());
    }

    #[test]
    fn setup_without_a_workspace_registers_linear_for_any_saved_workspace_that_uses_it() {
        const CHILD: &str = "IMACS_SETUP_WORKSPACE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            // Workspace files are found through IMACS_CONFIG, set for this child alone.
            let main = imacs_core::config::Config::default();
            let mcp = |workspace| checks::setup_config(&main, workspace).map(|sandbox| sandbox.mcp).ok();
            println!("none={:?} plain={:?} work={:?}", mcp(None), mcp(Some("plain")), mcp(Some("work")));
            let config = checks::setup_config(&main, None).unwrap();
            if config.mcp.is_empty() {
                return;
            }
            let (dir, sbx) = machine();
            let mut output = Vec::new();
            guided(&sbx, false, &config, &mut "y\ny\n".as_bytes(), &mut output, false).unwrap();
            println!(
                "linear calls={:?}",
                fake::calls_to(dir.path(), &["mcp"])
                    .iter()
                    .filter(|c| c.contains(&"linear".into()))
                    .collect::<Vec<_>>()
            );
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let workspaces = dir.path().join("config/workspaces");
        std::fs::create_dir_all(&workspaces).unwrap();
        std::fs::write(workspaces.join("plain.toml"), "projects = []\n").unwrap();
        std::fs::write(workspaces.join("broken.toml"), "projects = [\n").unwrap();
        let run = || {
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "sandbox::setup::tests::setup_without_a_workspace_registers_linear_for_any_saved_workspace_that_uses_it",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("IMACS_HOME", dir.path().join("state"))
                    .env("IMACS_CONFIG", dir.path().join("config/config.toml")),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            stdout
                .lines()
                .filter(|line| line.starts_with("none=") || line.starts_with("linear calls="))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(run(), "none=Some([]) plain=Some([]) work=None");
        // Max's daily workspace tracks its work in Linear; setup run without --workspace sees it.
        std::fs::write(workspaces.join("work.toml"), "projects = []\n[tickets]\nsystem = \"Linear\"\n").unwrap();
        let ran = run();
        assert!(ran.starts_with(r#"none=Some(["linear"]) plain=Some([]) work=Some(["linear"])"#), "{ran}");
        assert!(ran.contains(r#"["mcp", "add", "linear", "--url", "https://mcp.linear.app/mcp"]"#), "{ran}");
        assert!(ran.contains(r#"["mcp", "auth", "linear"]"#), "{ran}");
    }

    #[test]
    fn misfiled_secrets_are_removed_only_with_consent_and_never_shown() {
        let (dir, sbx) = machine();
        touch(dir.path(), &["misfiled-scoped"]);
        // Declining stops setup with the entry in place.
        let (result, output) = run_setup(&sbx, false, false, "n\n");
        assert!(result.is_err());
        assert!(output.contains("Remove 1 misfiled secret in scope anthropic from sbx?"), "{output}");
        assert!(dir.path().join("misfiled-scoped").exists());
        assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
        // Rerun and agree: exactly that entry goes, and setup reaches green.
        let (result, output) = run_setup(&sbx, false, false, "y\n");
        result.unwrap();
        assert!(!dir.path().join("misfiled-scoped").exists());
        assert!(output.contains("✓ sandbox secrets filed under known services and sandboxes"), "{output}");
    }

    #[test]
    fn setup_rechecks_its_secret_steps_with_doctors_parser() {
        // A stored key sbx does not list stops setup with doctor's own line, before later steps.
        for (claude, markers, answers, line) in [
            (false, ["no-openai", "skip-secret-write"], format!("y\n{OPENAI}\n"), "✗ sandbox OpenAI secret: openai"),
            (true, ["removed-anthropic", "skip-secret-write"], format!("y\n{API_KEY}\n"), "✗ sandbox Claude secret"),
        ] {
            let (dir, sbx) = machine();
            touch(dir.path(), &markers);
            let error = run_setup(&sbx, claude, false, &answers).0.unwrap_err().to_string();
            assert!(error.contains(&format!("sbx does not list what setup stored: {line}")), "{error}");
            assert!(fake::calls_to(dir.path(), &["settings"]).is_empty(), "SSH steps wait for the secrets");
        }
    }

    #[test]
    fn failures_and_invalid_input_never_show_values() {
        let (dir, sbx) = machine();
        touch(dir.path(), &["no-openai", "fail-secret-set"]);
        std::fs::write(dir.path().join("error-output"), OPENAI).unwrap();
        assert!(run_setup(&sbx, false, false, &format!("y\n{OPENAI}\n")).0.is_err());
        for input in ["n\n", "y\n\n", "y\n", ""] {
            let (dir, sbx) = machine();
            touch(dir.path(), &["no-openai"]);
            assert!(run_setup(&sbx, false, false, input).0.is_err(), "{input:?}");
            assert!(fake::calls_to(dir.path(), &["secret", "set"]).is_empty());
            assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
        }
    }

    #[test]
    fn version_signin_policy_openai_and_linear_failures_are_resumable() {
        for marker in ["fail-login", "fail-policy", "fail-secret-set", "fail-mcp-add", "fail-mcp-auth"] {
            let (dir, sbx) = machine();
            touch(dir.path(), &["signed-out", "no-policy", "no-openai", marker]);
            std::fs::write(dir.path().join("error-output"), OPENAI).unwrap();
            let answers = format!("y\ny\ny\n{OPENAI}\ny\ny\n");
            assert!(run_setup(&sbx, false, true, &answers).0.is_err(), "{marker}");
            std::fs::remove_file(dir.path().join(marker)).unwrap();
            // A fresh invocation re-probes the completed steps. Supply only the
            // answers needed by the state actually reached before the failure.
            let mut remaining = String::new();
            for (marker, answer) in [("signed-out", "y\n".to_string()), ("no-policy", "y\n".to_string())] {
                if dir.path().join(marker).exists() {
                    remaining.push_str(&answer);
                }
            }
            if !checks::secrets(&sbx).unwrap().openai {
                remaining.push_str(&format!("y\n{OPENAI}\n"));
            }
            if !dir.path().join("mcp.json").exists() {
                remaining.push_str("y\n");
            }
            remaining.push_str("y\n");
            run_setup(&sbx, false, true, &remaining).0.unwrap();
        }
        let (dir, sbx) = machine();
        for version in ["v0.1.0", "private-malformed-version"] {
            std::fs::write(dir.path().join("version"), version).unwrap();
            let error = run_setup(&sbx, false, false, "").0.unwrap_err();
            assert!(!error.to_string().contains(version));
            assert!(fake::calls_to(dir.path(), &["secret", "set"]).is_empty());
        }
    }
}
