//! Interactive subscription setup. Token values travel only over stdin to the OS
//! keychain; sbx stores a lookup command, never the token itself.
use super::*;
use std::io::{BufRead, IsTerminal, Write};

const SERVICE: &str = "imacs-claude-setup-token";
const ACCOUNT: &str = "imacs";

#[derive(Clone, Copy)]
enum Keychain {
    Mac,
    Linux,
}

impl Keychain {
    fn lookup(self) -> String {
        match self {
            Self::Mac => format!("/usr/bin/security find-generic-password -a {ACCOUNT} -s {SERVICE} -w"),
            Self::Linux => format!("/usr/bin/secret-tool lookup service {SERVICE} account {ACCOUNT}"),
        }
    }

    fn store(self, token: &str) -> Result<()> {
        let (program, args, input) = self.store_command(token);
        private_command(program, &args, &input)
    }

    fn store_command(self, token: &str) -> (&'static str, Vec<&'static str>, String) {
        match self {
            // `security -i` reads commands from stdin. The token is restricted to
            // OAuth token characters before interpolation, so it cannot add commands.
            Self::Mac => (
                "/usr/bin/security",
                vec!["-i"],
                format!("add-generic-password -U -a {ACCOUNT} -s {SERVICE} -w {token}\n"),
            ),
            Self::Linux => (
                "/usr/bin/secret-tool",
                vec!["store", "--label=IMACS Claude subscription", "service", SERVICE, "account", ACCOUNT],
                format!("{token}\n"),
            ),
        }
    }
}

pub fn run() -> Result<()> {
    let program = crate::setup::resolve_program("sbx");
    preflight(&program).map_err(anyhow::Error::msg)?;
    let keychain = if cfg!(target_os = "macos") {
        Keychain::Mac
    } else if cfg!(target_os = "linux") {
        Keychain::Linux
    } else {
        bail!("subscription setup requires macOS security or Linux secret-tool")
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout();
    interactive(&program, keychain, &mut input, &mut output, stdin.is_terminal())
}

fn interactive(
    program: &str,
    keychain: Keychain,
    input: &mut impl BufRead,
    output: &mut impl Write,
    terminal: bool,
) -> Result<()> {
    writeln!(
        output,
        "Claude subscription setup stores your setup-token in the OS keychain and registers a keychain lookup with sbx."
    )?;
    writeln!(
        output,
        "It removes sbx's built-in anthropic secret, whose apiKeyHelper overrides subscription OAuth. Other agents using that secret will need this OAuth route too."
    )?;
    write!(output, "Continue? [y/N] ")?;
    output.flush()?;
    if !answer(input)?.eq_ignore_ascii_case("y") {
        writeln!(output, "Setup cancelled.")?;
        return Ok(());
    }
    write!(output, "Reuse the existing {SERVICE} keychain entry? [Y/n] ")?;
    output.flush()?;
    if answer(input)?.eq_ignore_ascii_case("n") {
        writeln!(
            output,
            "Run `claude setup-token` in a trusted host terminal, then paste its token here. Input is hidden on a terminal; do not pass the token as an argument."
        )?;
        write!(output, "Setup-token: ")?;
        output.flush()?;
        let echo = Echo::hide(terminal)?;
        let mut token = answer(input)?;
        drop(echo);
        writeln!(output)?;
        let result = if valid_token(&token) {
            keychain.store(&token)
        } else {
            Err(anyhow::anyhow!(
                "expected a sk-ant-oat01- setup-token containing only letters, digits, hyphens and underscores"
            ))
        };
        // Avoid keeping a second, long-lived token copy in this process.
        token.clear();
        result?;
    }
    register(program, keychain)?;
    writeln!(
        output,
        "Claude subscription registered. Run `imacs doctor` or Settings → Sandbox to check the remaining setup. Start a new sandbox to use the new credential configuration."
    )?;
    Ok(())
}

fn answer(input: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!("setup input ended; no further changes made")
    }
    if line.len() > 8192 {
        bail!("setup input is too long")
    }
    Ok(line.trim().to_string())
}

fn valid_token(token: &str) -> bool {
    token.strip_prefix("sk-ant-oat01-").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    })
}

fn register(program: &str, keychain: Keychain) -> Result<()> {
    // First verify and register the keychain lookup. A missing/locked keychain
    // leaves the existing Anthropic credential alone. No resolver stderr is shown.
    let lookup = keychain.lookup();
    let args = [
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
    ];
    safe_sbx(
        program,
        &args,
        "could not register the keychain lookup; check that the keychain entry exists and is unlocked",
    )?;
    // Query only to decide whether removal is needed; never display names or errors.
    let secrets = super::checks::anthropic_present(program)
        .ok_or_else(|| anyhow::anyhow!("could not check anthropic secret names; check sbx and retry setup"))?;
    if secrets {
        safe_sbx(
            program,
            &["secret", "rm", "anthropic", "--force"],
            "OAuth lookup registered, but removing the built-in anthropic secret failed; run `sbx secret rm anthropic` before using subscription OAuth",
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

/// All child output is discarded: keychain tools may quote input on failure.
fn private_command(program: &str, args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow::anyhow!("could not start OS keychain tool; install secret-tool on Linux"))?;
    let result = child.stdin.take().expect("piped stdin").write_all(input.as_bytes());
    let status = child.wait();
    if result.is_err() || !status.is_ok_and(|s| s.success()) {
        bail!("OS keychain write failed; unlock your keychain and retry")
    }
    Ok(())
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

    #[test]
    fn both_keychains_take_values_over_stdin_and_keep_only_lookups_in_argv() {
        let token = "sk-ant-oat01-FIXTURE-not-a-real-token";
        for keychain in [Keychain::Mac, Keychain::Linux] {
            let (_, args, input) = keychain.store_command(token);
            assert!(input.contains(token));
            assert!(args.iter().all(|arg| !arg.contains(token)));
            assert!(!keychain.lookup().contains(token));
            let dir = tempfile::tempdir().unwrap();
            let helper = dir.path().join("keychain");
            std::fs::write(
                &helper,
                "#!/bin/sh\ndir=${0%/*}\nprintf '%s\\n' \"$@\" > \"$dir/argv\"\ncat > \"$dir/stdin\"\n",
            )
            .unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
            private_command(helper.to_str().unwrap(), &args, &input).unwrap();
            assert!(!std::fs::read_to_string(dir.path().join("argv")).unwrap().contains(token));
            assert!(std::fs::read_to_string(dir.path().join("stdin")).unwrap().contains(token));
        }
        for token in [
            "",
            "sk-ant-oat01-",
            "sk-ant-oat01-token\nquit",
            "sk-ant-oat01-token;evil",
            "sk-ant-oat01-\"",
            "wrong-token",
        ] {
            assert!(!valid_token(token));
        }
        assert!(valid_token(token));
    }

    #[test]
    fn subscription_setup_verifies_lookup_before_removing_anthropic() {
        let dir = tempfile::tempdir().unwrap();
        let sbx = fake::install(dir.path());
        for keychain in [Keychain::Mac, Keychain::Linux] {
            let mut output = Vec::new();
            interactive(sbx.to_str().unwrap(), keychain, &mut &b"y\n\n"[..], &mut output, false).unwrap();
            let calls = fake::calls(dir.path());
            let set = calls.iter().rposition(|c| c.starts_with(&["secret".into(), "set-custom".into()])).unwrap();
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
            let remove = calls.iter().rposition(|c| c.starts_with(&["secret".into(), "rm".into()])).unwrap();
            assert!(remove > set);
            assert_eq!(calls[remove], ["secret", "rm", "anthropic", "--force"]);
            assert!(String::from_utf8(output).unwrap().contains("subscription registered"));
        }
    }

    #[test]
    fn cancellation_and_invalid_tokens_make_no_sbx_changes() {
        let dir = tempfile::tempdir().unwrap();
        let sbx = fake::install(dir.path());
        let mut output = Vec::new();
        interactive(sbx.to_str().unwrap(), Keychain::Mac, &mut &b"n\n"[..], &mut output, false).unwrap();
        assert!(fake::calls(dir.path()).is_empty());
        let err = interactive(
            sbx.to_str().unwrap(),
            Keychain::Linux,
            &mut &b"y\nn\nprivate-invalid-token\n"[..],
            &mut output,
            false,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("private-invalid-token"));
        assert!(!String::from_utf8(output).unwrap().contains("private-invalid-token"));
        assert!(fake::calls(dir.path()).is_empty());
    }

    #[test]
    fn resolver_failure_keeps_anthropic_and_does_not_echo_private_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sbx = fake::install(dir.path());
        std::fs::write(dir.path().join("fail-secret-set-custom"), "").unwrap();
        std::fs::write(dir.path().join("error-output"), "private-keychain-token").unwrap();
        let error = register(sbx.to_str().unwrap(), Keychain::Mac).unwrap_err();
        assert!(!error.to_string().contains("private-keychain-token"));
        assert!(fake::calls_to(dir.path(), &["secret", "rm"]).is_empty());
    }
}
