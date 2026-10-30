//! Guided, resumable host setup. Child output is private; credentials travel over
//! stdin, and Claude's subscription token is kept in the OS keychain.
use super::*;
use std::io::{BufRead, IsTerminal, Write};

const SERVICE: &str = "imacs-claude-setup-token";
const ACCOUNT: &str = "imacs";

#[derive(Clone, Copy)]
enum Platform {
    Mac,
    Linux,
}

pub(super) struct Keychain {
    platform: Platform,
    program: String,
}

impl Keychain {
    pub(super) fn current() -> Self {
        let platform = if cfg!(target_os = "macos") { Platform::Mac } else { Platform::Linux };
        Self {
            platform,
            program: if cfg!(target_os = "macos") { "/usr/bin/security" } else { "/usr/bin/secret-tool" }.into(),
        }
    }

    fn lookup_args(&self) -> Vec<&'static str> {
        match self.platform {
            Platform::Mac => vec!["find-generic-password", "-a", ACCOUNT, "-s", SERVICE, "-w"],
            Platform::Linux => vec!["lookup", "service", SERVICE, "account", ACCOUNT],
        }
    }

    fn lookup(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.lookup_args())
            .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(super) fn available(&self) -> bool {
        // Never show a keychain response, including errors from a locked keyring.
        sbx_output(&self.program, &self.lookup_args(), false, TIMEOUT)
            .is_ok_and(|(status, value, _)| status.success() && valid_token(value.trim()))
    }

    fn store_command(&self, token: &str) -> (Vec<&'static str>, String) {
        match self.platform {
            // security's interactive mode takes the whole command over stdin.
            // Token validation prevents adding another security command.
            Platform::Mac => (vec!["-i"], format!("add-generic-password -U -a {ACCOUNT} -s {SERVICE} -w {token}\n")),
            Platform::Linux => (
                vec!["store", "--label=IMACS Claude subscription", "service", SERVICE, "account", ACCOUNT],
                format!("{token}\n"),
            ),
        }
    }

    fn store(&self, token: &str) -> Result<()> {
        let (args, input) = self.store_command(token);
        private_command(&self.program, &args, &input)?;
        if !self.available() {
            bail!("OS keychain entry could not be read; unlock the keychain and retry `imacs setup`")
        }
        Ok(())
    }
}

pub fn run(workspace: Option<&str>) -> Result<()> {
    use imacs_core::config::Config;
    let config = Config::load_or_create(&paths::config_file())
        .map_err(|_| anyhow::anyhow!("cannot load IMACS config; fix it before running setup"))?;
    let config = checks::setup_config(&config, workspace)
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
    guided(&program, &Keychain::current(), &config, &mut input, &mut output, stdin.is_terminal())?;
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

fn guided(
    program: &str,
    keychain: &Keychain,
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
    if !secrets.oauth || secrets.anthropic || !keychain.available() {
        consent(
            input,
            output,
            "Set up Claude subscription OAuth in the OS keychain and remove the built-in anthropic secret? This changes credentials for new sandboxes.",
        )?;
        if !keychain.available() {
            writeln!(
                output,
                "Run `claude setup-token` in a trusted host terminal and paste the token below. Input is hidden on a terminal."
            )?;
            let mut token = credential(input, output, "Claude setup-token: ", terminal)?;
            let result = if valid_token(&token) {
                keychain.store(&token)
            } else {
                Err(anyhow::anyhow!(
                    "expected a sk-ant-oat01- token containing only letters, digits, hyphens and underscores"
                ))
            };
            token.clear();
            result?;
        }
        register(program, keychain)?;
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
    let mut lines = checks::sandbox_lines(program, config);
    lines.push(checks::subscription_line(keychain.available()));
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

fn valid_token(token: &str) -> bool {
    token.strip_prefix("sk-ant-oat01-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    })
}

fn register(program: &str, keychain: &Keychain) -> Result<()> {
    let lookup = keychain.lookup();
    safe_sbx(
        program,
        &[
            "secret",
            "set-custom",
            "--host",
            "api.anthropic.com",
            "--env",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "--placeholder",
            "sk-ant-oat01-{rand}",
            "--command",
            &lookup,
        ],
        "could not register the keychain lookup; unlock the keychain and retry setup",
    )?;
    let secrets = checks::secrets(program)
        .ok_or_else(|| anyhow::anyhow!("could not check known secret services; retry setup"))?;
    if secrets.anthropic {
        safe_sbx(
            program,
            &["secret", "rm", "anthropic", "--force"],
            "OAuth lookup registered, but removing anthropic failed; retry setup",
        )?;
    }
    Ok(())
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
                bail!(
                    "setup helper failed or timed out; unlock the keychain or complete browser sign-in and retry `imacs setup`"
                )
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
    use std::os::unix::fs::PermissionsExt;

    const CLAUDE: &str = "sk-ant-oat01-PRIVATE-fixture";
    const OPENAI: &str = "sk-PRIVATE-openai-fixture";

    fn machine(platform: Platform) -> (crate::test_support::TempDir, String, Keychain) {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).to_string_lossy().into_owned();
        let helper = dir.path().join("keychain with spaces");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &helper).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let keychain = Keychain { platform, program: helper.to_string_lossy().into_owned() };
        (dir, sbx, keychain)
    }

    fn run_setup(sbx: &str, keychain: &Keychain, linear: bool, answers: &str) -> (Result<()>, String) {
        let config = SandboxConfig { mcp: if linear { vec!["linear".into()] } else { vec![] }, ..Default::default() };
        let mut output = Vec::new();
        let result = guided(sbx, keychain, &config, &mut answers.as_bytes(), &mut output, false);
        let output = String::from_utf8(output).unwrap();
        for private in [CLAUDE, OPENAI, "sk-ant-MISFILED-private-value", "PRIVATE-keychain-error"] {
            assert!(!output.contains(private));
            if let Err(error) = &result {
                assert!(!error.to_string().contains(private));
            }
        }
        (result, output)
    }

    #[test]
    fn fresh_mac_and_linux_setup_reaches_green_and_reruns_without_mutations() {
        for platform in [Platform::Mac, Platform::Linux] {
            let (dir, sbx, keychain) = machine(platform);
            for marker in ["signed-out", "no-policy", "no-openai", "misfiled-secret"] {
                std::fs::write(dir.path().join(marker), "").unwrap();
            }
            let answers = format!("y\ny\ny\n{CLAUDE}\ny\n{OPENAI}\ny\ny\n");
            let (result, output) = run_setup(&sbx, &keychain, true, &answers);
            result.unwrap();
            assert!(
                output.lines().filter(|l| l.starts_with('✓') || l.starts_with('✗')).all(|l| l.starts_with('✓')),
                "{output}"
            );
            assert!(keychain.available());
            let calls = fake::calls(dir.path());
            let set = calls.iter().position(|c| c.starts_with(&["secret".into(), "set-custom".into()])).unwrap();
            let rm =
                calls.iter().position(|c| c.starts_with(&["secret".into(), "rm".into(), "anthropic".into()])).unwrap();
            assert!(set < rm);
            assert_eq!(
                calls[set],
                [
                    "secret",
                    "set-custom",
                    "--host",
                    "api.anthropic.com",
                    "--env",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    "--placeholder",
                    "sk-ant-oat01-{rand}",
                    "--command",
                    &keychain.lookup()
                ]
            );
            assert_eq!(
                fake::calls_to(dir.path(), &["mcp", "add"])[0],
                ["mcp", "add", "linear", "--url", "https://mcp.linear.app/mcp"]
            );
            assert_eq!(std::fs::read_to_string(dir.path().join("secret-stdin")).unwrap().trim(), OPENAI);
            let logs = format!("{calls:?}{}", std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap());
            assert!(!logs.contains(CLAUDE) && !logs.contains(OPENAI) && !logs.contains("MISFILED"));
            let before = calls.len();
            run_setup(&sbx, &keychain, true, "").0.unwrap();
            assert!(
                fake::calls(dir.path())[before..]
                    .iter()
                    .all(|c| matches!(c[0].as_str(), "version" | "diagnose") || c[1] == "ls" || c[1] == "get")
            );
        }
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
            let (dir, sbx, keychain) = machine(Platform::Mac);
            keychain.store(CLAUDE).unwrap();
            let mut output = Vec::new();
            guided(&sbx, &keychain, &config, &mut "y\ny\ny\n".as_bytes(), &mut output, false).unwrap();
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
    fn interrupted_setup_reuses_keychain_and_subscription() {
        for platform in [Platform::Mac, Platform::Linux] {
            let (dir, sbx, keychain) = machine(platform);
            std::fs::write(dir.path().join("no-openai"), "").unwrap();
            assert!(run_setup(&sbx, &keychain, false, &format!("y\n{CLAUDE}\n")).0.is_err());
            assert!(keychain.available());
            assert!(checks::secrets(&sbx).unwrap().oauth);
            assert!(!checks::secrets(&sbx).unwrap().anthropic);
            run_setup(&sbx, &keychain, false, &format!("y\n{OPENAI}\n")).0.unwrap();
            assert_eq!(fake::calls_to(dir.path(), &["secret", "set-custom"]).len(), 1);
            assert!(fake::calls_to(dir.path(), &["mcp", "add"]).is_empty());
        }
    }

    #[test]
    fn failures_and_invalid_input_never_remove_existing_anthropic_or_show_values() {
        for platform in [Platform::Mac, Platform::Linux] {
            for marker in ["fail-keychain", "fail-secret-set-custom"] {
                let (dir, sbx, keychain) = machine(platform);
                std::fs::write(dir.path().join(marker), "").unwrap();
                std::fs::write(dir.path().join("error-output"), CLAUDE).unwrap();
                assert!(run_setup(&sbx, &keychain, false, &format!("y\n{CLAUDE}\n")).0.is_err());
                assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
            }
            for input in ["n\n", "y\ninvalid-private-token\n", "y\nsk-ant-oat01-token;evil\n", "y\n", ""] {
                let (dir, sbx, keychain) = machine(platform);
                let (result, output) = run_setup(&sbx, &keychain, false, input);
                assert!(result.is_err());
                assert!(!output.contains("invalid-private-token"));
                assert!(fake::calls_to(dir.path(), &["secret", "set-custom"]).is_empty());
                assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
            }
        }
    }

    #[test]
    fn version_signin_policy_openai_and_linear_failures_are_resumable() {
        for marker in
            ["fail-login", "fail-policy", "fail-secret-set", "fail-mcp-add", "fail-mcp-auth", "skip-secret-write"]
        {
            let (dir, sbx, keychain) = machine(Platform::Linux);
            keychain.store(CLAUDE).unwrap();
            for name in ["signed-out", "no-policy", "no-openai", marker] {
                std::fs::write(dir.path().join(name), "").unwrap();
            }
            std::fs::write(dir.path().join("error-output"), OPENAI).unwrap();
            let answers = format!("y\ny\ny\ny\n{OPENAI}\ny\ny\n");
            assert!(run_setup(&sbx, &keychain, true, &answers).0.is_err(), "{marker}");
            std::fs::remove_file(dir.path().join(marker)).unwrap();
            // A fresh invocation re-probes the completed steps. Supply only the
            // answers needed by the state actually reached before the failure.
            let secrets = checks::secrets(&sbx).unwrap();
            let mut remaining = String::new();
            if dir.path().join("signed-out").exists() {
                remaining.push_str("y\n");
            }
            if dir.path().join("no-policy").exists() {
                remaining.push_str("y\n");
            }
            if !secrets.oauth || secrets.anthropic {
                remaining.push_str("y\n");
            }
            if !secrets.openai {
                remaining.push_str(&format!("y\n{OPENAI}\n"));
            }
            if !dir.path().join("mcp.json").exists() {
                remaining.push_str("y\n");
            }
            remaining.push_str("y\n");
            run_setup(&sbx, &keychain, true, &remaining).0.unwrap();
        }
        let (dir, sbx, keychain) = machine(Platform::Mac);
        for version in ["v0.1.0", "private-malformed-version"] {
            std::fs::write(dir.path().join("version"), version).unwrap();
            let error = run_setup(&sbx, &keychain, false, "").0.unwrap_err();
            assert!(!error.to_string().contains(version));
            assert!(fake::calls_to(dir.path(), &["secret", "set-custom"]).is_empty());
        }
    }
}
