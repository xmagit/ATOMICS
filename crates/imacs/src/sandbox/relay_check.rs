//! `imacs sandbox relay-check`: sandboxed Claude answers on the Claude subscription through IMACS's
//! credential relay, and the token never enters the VM. It makes one throwaway sandbox and removes
//! it again whatever happens. Each step prints a PASS line; nothing it prints holds a value.

use super::*;

/// What the check asks Claude to say.
const ANSWER: &str = "IMACS relay OK";

pub fn run() -> Result<()> {
    let home = paths::imacs_home();
    let program = crate::setup::resolve_program("sbx");
    let relay = std::sync::Arc::new(crate::relay::Service::transient(home.clone(), crate::relay::Keychain::current()));
    let id = uuid::Uuid::new_v4().simple().to_string();
    let name = format!("imacs-relay-check-{}", &id[..12]);
    // An empty folder of its own, which the sandbox mounts as its workspace.
    let folder = home.join("relay-check").join(&name);
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let job = Prepare {
        home: home.clone(),
        sbx: program.clone(),
        name: name.clone(),
        agent: "claude",
        cwd: folder.clone(),
        config: SandboxConfig {
            memory: Some("4g".into()),
            cpus: Some(2),
            azure: imacs_core::isolation::AzureConfig { enabled: false, ..Default::default() },
            ..SandboxConfig::default()
        },
        warm: Vec::new(),
        exe: crate::setup::executable_path()?,
        socket: paths::socket_path(),
        briefs: None,
        pins: paths::git_pins_dir(),
        relay: Some(relay.clone()),
    };
    let checked = check(&job, &relay);
    let removed = remove(&home, &program, &name).and_then(|()| Ok(std::fs::remove_dir_all(&folder)?));
    if removed.is_ok() {
        println!("PASS cleanup: sandbox {name}, its relay rule and its folder removed");
    }
    match (checked, removed) {
        (checked, Ok(())) => checked,
        (Ok(()), Err(error)) => Err(error.context(format!("removing sandbox {name}"))),
        (Err(error), Err(cleanup)) => Err(error.context(format!("also failed to remove sandbox {name}: {cleanup:#}"))),
    }
}

fn check(job: &Prepare, relay: &crate::relay::Service) -> Result<()> {
    let granted = prepare(job)?.context(
        "the OS keychain holds no Claude subscription token: run `claude setup-token` in a host terminal, then `imacs setup`",
    )?;
    println!("PASS sandbox {} made; the relay on 127.0.0.1:{} is allowed for it alone", job.name, granted.port);
    let url = isolation::relay_url(granted.port, isolation::ANTHROPIC_API);
    // Without a placeholder the relay refuses: a 401 proves the request reached it.
    let status = |placeholder: Option<&str>| -> Result<String> {
        let header = placeholder.map(|p| format!(" -H 'x-api-key: {p}'")).unwrap_or_default();
        let probe = format!("curl -s -o /dev/null -w '%{{http_code}}' --max-time 20{header} {url}/v1/models");
        Ok(exec(job, &["sh", "-c", &probe], 60)?.trim().to_string())
    };
    let reached = status(None)?;
    if reached != "401" {
        bail!(
            "the sandbox did not reach the relay at {url} (curl status {reached:?}). Check `sbx policy ls {}` for \
             localhost:{} and that {} leads to this machine's loopback",
            job.name,
            granted.port,
            isolation::RELAY_HOST
        );
    }
    println!("PASS the sandbox reaches the relay at {url}");

    let settings = isolation::claude_relay_settings(granted.port, &granted.placeholder).to_string();
    let prompt = format!("Reply with exactly: {ANSWER}");
    let answer = exec(job, &["timeout", "240", "claude", "--settings", &settings, "-p", &prompt], 300)
        .context("running Claude in the sandbox")?;
    if !answer.contains(ANSWER) {
        bail!("sandboxed Claude did not answer through the relay; see {}", job.home.join("relay.log").display());
    }
    println!("PASS sandboxed Claude answered through the relay on the Claude subscription");
    let log = std::fs::read_to_string(job.home.join("relay.log")).unwrap_or_default();
    match log.lines().rev().find(|line| line.contains(&format!(" {} ", job.name))) {
        Some(line) => println!("PASS relay.log, which holds no values: {line}"),
        None => bail!("relay.log has no request from {}", job.name),
    }

    // By shape only: anything in the VM that looks like a Claude token must be an IMACS placeholder.
    let scan = r#"{ env; cat /etc/sandbox-persistent.sh ~/.claude.json 2>/dev/null; grep -rhaoE 'sk-ant-oat01-[A-Za-z0-9_-]+' ~/.claude /tmp 2>/dev/null; } | grep -aoE 'sk-ant-oat01-[A-Za-z0-9_-]+' | grep -vc '^sk-ant-oat01-imacs-' || true"#;
    let found = exec(job, &["sh", "-c", scan], 60)?;
    if found.trim() != "0" {
        bail!("the sandbox holds {} Claude token(s) that are not IMACS placeholders", found.trim());
    }
    println!("PASS no Claude token in the VM's environment, Claude's files or /tmp, only placeholders");

    relay.revoke(&granted);
    let revoked = status(Some(&granted.placeholder))?;
    if revoked != "401" {
        bail!("a revoked placeholder still reached the Anthropic API (status {revoked:?})");
    }
    println!("PASS a revoked placeholder is refused");
    Ok(())
}

/// Runs `command` in the check's sandbox within `secs` and returns its output.
fn exec(job: &Prepare, command: &[&str], secs: u64) -> Result<String> {
    let args: Vec<&str> = ["exec", job.name.as_str()].into_iter().chain(command.iter().copied()).collect();
    sbx(&job.sbx, &args, false, Duration::from_secs(secs))
}
