//! Guided, resumable host setup. Child output is private, and credentials travel over stdin. The
//! Claude subscription token goes to the OS keychain, where IMACS's credential relay reads it.
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
    writeln!(output, "{}", crate::relay::azure::doctor_line(config.azure.enabled))?;
    // Installation, privilege elevation and group/session changes belong to the
    // user. Keep checking after they run the displayed OS-specific commands.
    let program = loop {
        // Installers often add ~/.local/bin or Homebrew to shell startup files.
        // Re-read that PATH instead of using the app's cached launch-time PATH.
        let path = crate::setup::login_shell_var("PATH").or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        let missing: Vec<_> = checks::host_lines_with(|name| crate::setup::find_on_path(name, &path).is_some())
            .into_iter()
            .filter(|line| checks::blocks_setup(line))
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
    let keychain = crate::relay::Keychain::current();
    guided(&program, claude, &keychain, &config, &mut input, &mut output, stdin.is_terminal())?;
    writeln!(output, "Sandbox setup complete. Running imacs doctor (includes optional host/workspace diagnostics):")?;
    crate::doctor::run(workspace);
    Ok(())
}

/// Provisioning a named credential does not need Docker. There is deliberately no value argument.
pub fn credential_setup(kind: &str, host: Option<&str>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut output = std::io::stdout();
    let store = crate::credentials::Store::new(crate::relay::Keychain::current());
    provision_credential(&store, kind, host, &mut stdin.lock(), &mut output, stdin.is_terminal())?;
    for line in crate::credentials::doctor_lines(&store) {
        writeln!(output, "{line}")?;
    }
    Ok(())
}

fn provision_credential(
    store: &crate::credentials::Store,
    kind: &str,
    host: Option<&str>,
    input: &mut impl BufRead,
    output: &mut impl Write,
    terminal: bool,
) -> Result<()> {
    let value = credential(input, output, "Credential value (stdin only): ", terminal)?;
    store.put(kind, &value, host)
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

/// `claude` says whether Claude agents run sandboxed, and so need the Claude subscription in
/// `keychain` or an API key in sbx's anthropic secret.
fn guided(
    program: &str,
    claude: bool,
    keychain: &crate::relay::Keychain,
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
    // sbx cannot pass a Claude subscription to a local sandbox; IMACS's relay does, reading it from
    // the OS keychain (docs/isolation.md). An API key still goes to sbx's anthropic secret.
    if !claude {
        writeln!(
            output,
            "Claude agents run bare; sandboxed, they use your Claude subscription through IMACS's relay."
        )?;
    } else if keychain.claude_token().is_none() && !secrets.anthropic {
        consent(
            input,
            output,
            "Claude agents run sandboxed: keep your Claude subscription token in the OS keychain for IMACS's credential relay?",
        )?;
        writeln!(
            output,
            "Run `claude setup-token` in a trusted host terminal and paste the token below; an Anthropic API key goes to sbx's anthropic secret instead. Input is hidden on a terminal."
        )?;
        let mut token = credential(input, output, "Claude setup-token or Anthropic API key: ", terminal)?;
        let result = if token.is_empty() {
            Err(anyhow::anyhow!("the token is empty; rerun setup to resume"))
        } else if token.starts_with("sk-ant-oat") {
            store_subscription(keychain, &token)
        } else {
            private_command(program, &["secret", "set", "anthropic"], &format!("{token}\n"))
        };
        token.clear();
        result?;
    }
    if !secrets.openai {
        let store = crate::credentials::Store::new(keychain.clone());
        let token = match store.token("openai")? {
            Some(token) => token,
            None => {
                consent(
                    input,
                    output,
                    "Store an OpenAI API key in the OS keychain and sbx? The key is sent over stdin.",
                )?;
                let token = credential(input, output, "OpenAI API key: ", terminal)?;
                store.put("openai", &token, None)?;
                token
            }
        };
        private_command(program, &["secret", "set", "openai"], &format!("{token}\n"))?;
        writeln!(output, "OpenAI: OS keychain for IMACS relay; sandbox Codex uses sbx's OpenAI credential proxy.")?;
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
    // Setup never removes these itself: `sbx secret rm <name>` would put a pasted credential in argv.
    loop {
        let misfiled = checks::secrets(program)
            .ok_or_else(|| anyhow::anyhow!("cannot check known secret services; retry setup"))?
            .misfiled;
        if misfiled.is_empty() {
            break;
        }
        writeln!(output, "sbx holds {}: {}.", checks::misfiled_text(&misfiled), checks::MISFILED_HINT)?;
        retry(input, output)?;
    }
    let route = match claude {
        true => checks::ClaudeRoute::Sandboxed { subscription: keychain.claude_token().is_some() },
        false => checks::ClaudeRoute::Bare,
    };
    // Read the store again through doctor's lines: setup is not done with a secret doctor reports missing.
    if let Some(failed) =
        checks::secret_lines(checks::secrets(program).as_ref(), route).iter().find(|line| line.starts_with('✗'))
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
    let lines = checks::sandbox_lines(program, config, route);
    for line in &lines {
        writeln!(output, "{line}")?;
    }
    if lines.iter().any(|line| line.starts_with('✗')) {
        bail!("sandbox checks still need attention; follow the hints and rerun `imacs setup`")
    }
    Ok(())
}

/// Keeps the Claude subscription token in the OS keychain, where IMACS's relay reads it.
fn store_subscription(keychain: &crate::relay::Keychain, token: &str) -> Result<()> {
    if !crate::relay::valid_claude_token(token) {
        bail!(
            "expected a sk-ant-oat01- token of letters, digits, hyphens and underscores, as `claude setup-token` prints it"
        )
    }
    crate::credentials::Store::new(keychain.clone()).put("claude", token, None)?;
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

/// Failures are deliberately value-free: callers may safely map these to their own diagnostics.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OutputError {
    Start,
    Read,
    Timeout,
    TooLarge,
}

/// Read credential helpers through bounded, nonblocking pipes only. Neither stream ever goes
/// through a capture file. A helper (or descendant keeping a pipe open) is killed on timeout.
pub(crate) fn private_output(
    command: &mut Command,
    timeout: Duration,
) -> std::result::Result<std::process::Output, OutputError> {
    use std::io::{ErrorKind, Read};
    use std::os::fd::AsRawFd;
    fn drain(reader: &mut impl Read, output: &mut Vec<u8>) -> std::result::Result<bool, OutputError> {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    output.extend_from_slice(&buffer[..n]);
                    if output.len() > 1024 * 1024 {
                        return Err(OutputError::TooLarge);
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(_) => return Err(OutputError::Read),
            }
        }
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|_| OutputError::Start)?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // SAFETY: both descriptors are live pipes owned by this function.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
                return Err(OutputError::Read);
            }
        }
        let deadline = Instant::now() + timeout;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (mut stdout_done, mut stderr_done, mut status) = (false, false, None);
        loop {
            if !stdout_done {
                stdout_done = drain(&mut stdout, &mut out)?;
            }
            if !stderr_done {
                stderr_done = drain(&mut stderr, &mut err)?;
            }
            if status.is_none() {
                status = child.try_wait().map_err(|_| OutputError::Read)?;
            }
            if stdout_done
                && stderr_done
                && let Some(status) = status
            {
                return Ok(std::process::Output { status, stdout: out, stderr: err });
            }
            if Instant::now() >= deadline {
                return Err(OutputError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        // SAFETY: process_group(0) gave this helper and its descendants their own group.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }
    result
}

/// Child output is discarded: a failed credential writer may echo its input.
/// Browser auth can take several minutes, but a stuck helper remains bounded.
pub(crate) fn private_command(program: &str, args: &[&str], input: &str) -> Result<()> {
    use std::os::fd::OwnedFd;
    let (mut writer, reader) =
        std::os::unix::net::UnixStream::pair().map_err(|_| anyhow::anyhow!("could not open setup input"))?;
    writer.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::from(OwnedFd::from(reader)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| anyhow::anyhow!("could not start setup helper; check installation and retry setup"))?;
    let result = writer.write_all(input.as_bytes());
    drop(writer);
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

    #[test]
    fn private_output_keeps_both_streams_in_memory_and_bounds_large_or_inherited_output() {
        let output = private_output(
            Command::new("/bin/sh").args(["-c", "printf PRIVATE-out; printf PRIVATE-error >&2; exit 7"]),
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"PRIVATE-out");
        assert_eq!(output.stderr, b"PRIVATE-error");
        let error =
            private_output(Command::new("/bin/sh").args(["-c", "yes PRIVATE-error >&2"]), Duration::from_secs(5))
                .unwrap_err();
        assert!(matches!(error, OutputError::TooLarge));
        assert!(!format!("{error:?}").contains("PRIVATE"));
        let dir = crate::test_support::tempdir().unwrap();
        let marker = dir.path().join("helper-leaked");
        let command = format!("(sleep 0.4; touch '{}') & exit 0", marker.display());
        assert!(matches!(
            private_output(Command::new("/bin/sh").args(["-c", &command]), Duration::from_millis(100)),
            Err(OutputError::Timeout)
        ));
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists());
    }

    #[test]
    fn named_credentials_setup_reads_stdin_and_never_prints_values() {
        for mac in [true, false] {
            let (dir, _, store) = crate::credentials::tests::machine(mac);
            for (kind, value, host) in [
                ("claude", SUBSCRIPTION, None),
                ("openai", OPENAI, None),
                ("linear", "lin_api_PRIVATE-setup", None),
                ("github", "ghp_PRIVATE-setup", None),
                ("registry", "PRIVATE-registry", Some("registry.example.com")),
            ] {
                let mut output = Vec::new();
                provision_credential(&store, kind, host, &mut format!("{value}\n").as_bytes(), &mut output, false)
                    .unwrap();
                let output = format!(
                    "{}\n{}",
                    String::from_utf8(output).unwrap(),
                    crate::credentials::doctor_lines(&store).join("\n")
                );
                let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
                assert!(!output.contains(value) && !argv.contains(value));
            }
            let mut output = Vec::new();
            assert!(
                provision_credential(
                    &store,
                    "openai",
                    None,
                    &mut "sk-PRIVATE-truncated".as_bytes(),
                    &mut output,
                    false
                )
                .is_err()
            );
            assert!(!String::from_utf8(output).unwrap().contains("PRIVATE"));
        }
    }

    /// A fake sbx, with a fake keychain beside it whose entry is `keychain-value`.
    fn machine() -> (crate::test_support::TempDir, String) {
        let dir = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(dir.path()).to_string_lossy().into_owned();
        let keychain = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain");
        std::fs::copy(keychain, dir.path().join("keychain")).unwrap();
        (dir, sbx)
    }

    fn keychain(sbx: &str, mac: bool) -> crate::relay::Keychain {
        crate::relay::Keychain::at(&Path::new(sbx).with_file_name("keychain"), mac)
    }

    fn touch(dir: &Path, markers: &[&str]) {
        for marker in markers {
            std::fs::write(dir.join(marker), "").unwrap();
        }
    }

    /// Guided setup with Claude agents sandboxed or bare, and Linear or not.
    fn run_setup(sbx: &str, claude: bool, linear: bool, answers: &str) -> (Result<()>, String) {
        setup_on(sbx, cfg!(target_os = "macos"), claude, linear, answers)
    }

    /// [`run_setup`] with macOS's keychain or Linux's.
    fn setup_on(sbx: &str, mac: bool, claude: bool, linear: bool, answers: &str) -> (Result<()>, String) {
        let config = SandboxConfig { mcp: if linear { vec!["linear".into()] } else { vec![] }, ..Default::default() };
        let mut output = Vec::new();
        let result = guided(sbx, claude, &keychain(sbx, mac), &config, &mut answers.as_bytes(), &mut output, false);
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
        touch(dir.path(), &["signed-out", "no-policy", "removed-anthropic", "no-openai"]);
        let (result, output) = run_setup(&sbx, false, true, &format!("y\ny\ny\n{OPENAI}\ny\ny\n"));
        result.unwrap();
        assert!(
            output.lines().filter(|l| l.starts_with('✓') || l.starts_with('✗')).all(|l| l.starts_with('✓')),
            "{output}"
        );
        assert!(output.contains("Claude agents run bare; sandboxed, they use your Claude subscription"), "{output}");
        assert!(output.lines().any(|l| l.starts_with("sandboxed Claude: off")), "{output}");
        assert!(
            keychain(&sbx, cfg!(target_os = "macos")).claude_token().is_none(),
            "bare Claude needs no subscription"
        );
        let calls = fake::calls(dir.path());
        assert!(fake::calls_to(dir.path(), &["secret", "set-custom"]).is_empty());
        assert_eq!(fake::calls_to(dir.path(), &["secret", "set"]), [["secret", "set", "openai"]]);
        assert_eq!(std::fs::read_to_string(dir.path().join("secret-stdin")).unwrap().trim(), OPENAI);
        assert_eq!(
            fake::calls_to(dir.path(), &["mcp", "add"])[0],
            ["mcp", "add", "linear", "--url", "https://mcp.linear.app/mcp"]
        );
        assert!(!format!("{calls:?}").contains("PRIVATE"));
        let before = calls.len();
        run_setup(&sbx, false, true, "").0.unwrap();
        assert!(fake::calls(dir.path())[before..].iter().all(|c| matches!(
            c[0].as_str(),
            "version" | "diagnose" | "ls"
        ) || c[1] == "ls"
            || c[1] == "get"));
    }

    #[test]
    fn sandboxed_claude_keeps_a_subscription_token_in_the_keychain_or_an_api_key_in_sbx() {
        for mac in [true, false] {
            let (dir, sbx) = machine();
            touch(dir.path(), &["removed-anthropic"]);
            for refused in ["n\n", "y\n\n", "y\nsk-ant-oat01-not valid\n"] {
                let error = setup_on(&sbx, mac, true, false, refused).0.unwrap_err().to_string();
                assert!(!dir.path().join("keychain-value").exists(), "{refused:?}: {error}");
                assert!(fake::calls_to(dir.path(), &["secret", "set"]).is_empty(), "{refused:?}: {error}");
            }
            let (result, output) = setup_on(&sbx, mac, true, false, &format!("y\n{SUBSCRIPTION}\n"));
            result.unwrap();
            assert!(output.contains("Run `claude setup-token` in a trusted host terminal"), "{output}");
            assert_eq!(keychain(&sbx, mac).claude_token().as_deref(), Some(SUBSCRIPTION));
            assert!(fake::calls_to(dir.path(), &["secret"]).iter().all(|call| call[1] == "ls"), "sbx gets no token");
            let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
            assert!(!argv.contains(SUBSCRIPTION), "the token never is an argument: {argv}");
            assert!(
                output.contains("✓ sandbox Claude credential: Claude subscription through IMACS's relay"),
                "{output}"
            );
            // Stored once, a rerun asks nothing.
            setup_on(&sbx, mac, true, false, "").0.unwrap();
        }
        // An API key goes to sbx's anthropic secret, as before the relay.
        let (dir, sbx) = machine();
        touch(dir.path(), &["removed-anthropic"]);
        let (result, output) = run_setup(&sbx, true, false, &format!("y\n{API_KEY}\n"));
        result.unwrap();
        assert_eq!(fake::calls_to(dir.path(), &["secret", "set"]), [["secret", "set", "anthropic"]]);
        assert_eq!(std::fs::read_to_string(dir.path().join("secret-stdin")).unwrap().trim(), API_KEY);
        assert!(output.contains("✓ sandbox Claude credential: anthropic API key"), "{output}");
        assert!(!dir.path().join("keychain-value").exists());
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
            guided(&sbx, false, &keychain(&sbx, false), &config, &mut "y\ny\n".as_bytes(), &mut output, false).unwrap();
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
    fn misfiled_secrets_are_left_to_sbxs_own_list_and_never_reach_argv_or_output() {
        let (dir, sbx) = machine();
        touch(dir.path(), &["misfiled-secret", "misfiled-scoped"]);
        // Setup waits for the user's own removal, rechecking on Enter; q stops it with the entries in place.
        let (result, output) = run_setup(&sbx, false, false, "\nq\n");
        assert!(result.is_err());
        let notice = "sbx holds 2 misfiled secrets in scope anthropic, global: run `sbx secret rm` in a host terminal";
        assert_eq!(output.matches(notice).count(), 2, "{output}");
        assert!(dir.path().join("misfiled-scoped").exists() && dir.path().join("misfiled-secret").exists());
        assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
        assert!(!format!("{:?}", fake::calls(dir.path())).contains("PRIVATE"));
        // The user removed them in sbx's list: the rerun goes on to green.
        std::fs::remove_file(dir.path().join("misfiled-secret")).unwrap();
        std::fs::remove_file(dir.path().join("misfiled-scoped")).unwrap();
        let (result, output) = run_setup(&sbx, false, false, "");
        result.unwrap();
        assert!(output.contains("✓ sandbox secrets filed under known services and sandboxes"), "{output}");
        assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
    }

    #[test]
    fn setup_rechecks_its_secret_steps_with_doctors_parser() {
        // A stored key sbx does not list stops setup with doctor's own line, before later steps.
        for (claude, markers, answers, line) in [
            (false, ["no-openai", "skip-secret-write"], format!("y\n{OPENAI}\n"), "✗ sandbox OpenAI secret: openai"),
            (
                true,
                ["removed-anthropic", "skip-secret-write"],
                format!("y\n{API_KEY}\n"),
                "✗ sandbox Claude credential",
            ),
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
            if !checks::secrets(&sbx).unwrap().openai
                && crate::credentials::Store::new(keychain(&sbx, cfg!(target_os = "macos")))
                    .token("openai")
                    .unwrap()
                    .is_none()
            {
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
