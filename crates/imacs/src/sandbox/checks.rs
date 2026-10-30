//! Read-only sandbox checks shared by doctor and Settings. CLI output is untrusted:
//! in particular, `secret ls` may contain a credential in its name column.
use super::*;
use imacs_core::doctor::check_line;
use serde_json::Value;

/// Checks never include sbx's raw output or errors, even when a command fails. `claude` says how
/// Claude agents get their credential ([`claude_route`]).
pub fn doctor_lines(program: &str, config: &SandboxConfig, claude: ClaudeRoute) -> Vec<String> {
    let mut lines = sandbox_lines(program, config, claude);
    lines.extend(host_lines());
    lines
}

/// How Claude agents get their credential.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClaudeRoute {
    /// They run bare, with this machine's Claude login.
    Bare,
    /// Bare Claude explicitly uses the same host relay, including for the chief.
    BareRelay { subscription: bool },
    /// They run sandboxed. With the Claude subscription token in the OS keychain they use it through
    /// IMACS's relay; without it, sbx's anthropic secret, which must be an API key.
    Sandboxed { subscription: bool },
}

/// The route Claude agents take. It reads the OS keychain when they run sandboxed: call it off the
/// UI thread.
pub(crate) fn claude_route(
    config: &imacs_core::config::Config,
    workspace: Option<&str>,
    keychain: &crate::relay::Keychain,
) -> ClaudeRoute {
    if claude_sandboxed(config, workspace) {
        ClaudeRoute::Sandboxed { subscription: keychain.claude_token().is_some() }
    } else {
        let isolation = workspace
            .and_then(|name| workspace::Workspace::load(name).ok())
            .and_then(|workspace| workspace.isolation().ok().flatten())
            .and_then(|overrides| config.isolation.with_overrides(&overrides).ok())
            .unwrap_or_else(|| config.isolation.clone());
        if isolation.bare_relay {
            ClaudeRoute::BareRelay { subscription: keychain.claude_token().is_some() }
        } else {
            ClaudeRoute::Bare
        }
    }
}

/// Whether a Claude agent runs sandboxed: its own `isolation`, else the workspace's or main `mode`.
/// A repository can still require sandboxes for its own agents.
pub(crate) fn claude_sandboxed(config: &imacs_core::config::Config, workspace: Option<&str>) -> bool {
    let overrides = workspace
        .and_then(|name| workspace::Workspace::load(name).ok())
        .and_then(|workspace| workspace.isolation().ok().flatten());
    let isolation = overrides
        .and_then(|overrides| config.isolation.with_overrides(&overrides).ok())
        .unwrap_or_else(|| config.isolation.clone());
    config
        .agents
        .iter()
        .filter(|agent| agent.kind == imacs_core::config::AgentKind::Claude)
        .any(|agent| isolation.mode_for(agent, None) == isolation::IsolationMode::Sandbox)
}

pub(super) fn sandbox_lines(program: &str, config: &SandboxConfig, claude: ClaudeRoute) -> Vec<String> {
    let mut lines = Vec::new();
    match preflight(program) {
        Ok(ready) => {
            lines.push(check_line(
                true,
                &format!("Docker Sandboxes ready (sbx {}, signed in)", isolation::version_text(ready.version)),
                "",
            ));
            let preset = query(program, &["policy", "ls", "--json"]).and_then(|v| policy_preset(&v));
            let initialized = ready.diagnosis.check("Global network policy").is_some_and(|c| c.status == "pass");
            lines.push(check_line(
                initialized && preset.is_some(),
                &format!(
                    "sandbox global network policy: {} (warning if unavailable)",
                    if initialized { preset.unwrap_or("initialized; preset unknown") } else { "not initialized" }
                ),
                &format!(
                    "initialize with `sbx policy init {}`; inspect changes with `sbx policy ls`",
                    config.network.name()
                ),
            ));
            lines.extend(secret_lines(secrets(program).as_ref(), claude));
            let mcp = query(program, &["mcp", "ls", "--json"]);
            match mcp.as_ref().and_then(Value::as_array) {
                None => lines.push(check_line(
                    false,
                    "sandbox MCP status unavailable (warning)",
                    "check `sbx mcp ls`; ticket integration runs on the host",
                )),
                Some(servers) => {
                    let mut names = config.mcp.clone();
                    names.extend(
                        servers
                            .iter()
                            .filter_map(|s| s["name"].as_str())
                            .filter(|n| !n.starts_with("imacs-"))
                            .map(String::from),
                    );
                    names.sort();
                    names.dedup();
                    if names.is_empty() {
                        lines.push(check_line(
                            true,
                            "sandbox MCP: no direct servers configured; ticket integration runs on the host",
                            "",
                        ));
                    }
                    for name in names.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                        let server = servers.iter().find(|s| s["name"].as_str() == Some(name));
                        let status = match server {
                            None => "not registered",
                            Some(s) if s["status"] == "ready" => "ready",
                            Some(s) if s["status"] == "needs-auth" => "needs authorization",
                            Some(_) => "unavailable",
                        };
                        lines.push(check_line(status == "ready", &format!("sandbox MCP {name}: {status} (warning if unavailable)"),
                            &format!("register with `sbx mcp add {name} --url <server URL>`, then `sbx mcp auth {name}`; check `sbx mcp ls`")));
                    }
                }
            }
            let forwarding = setting(program, "ssh.agentForwardingEnabled");
            let socket = setting(program, "ssh.agentSocketPath");
            let off = !config.ssh_agent && forwarding.as_deref() == Some("false") && socket.as_deref() == Some("");
            lines.push(check_line(off, "sandbox SSH agent forwarding off (warning if enabled or unreadable)",
                "set [isolation.sandbox] ssh_agent = false, `sbx settings set ssh.agentForwardingEnabled false` and `sbx settings unset ssh.agentSocketPath`; a forwarded agent can sign and push as you"));
        }
        Err(error) => {
            lines.push(check_line(false, "Docker Sandboxes not ready for sandboxed agents (warning)", &error))
        }
    }
    lines
}

/// Doctor's credential lines, from what sbx lists. Setup checks its own secret steps with these
/// same lines, so the two cannot disagree about what sbx stores.
pub(super) fn secret_lines(secrets: Option<&SecretNames>, claude: ClaudeRoute) -> Vec<String> {
    let mut lines = Vec::new();
    // sbx cannot pass a Claude subscription to a local sandbox; IMACS's relay does (docs/isolation.md).
    match claude {
        ClaudeRoute::Bare => lines.push(
            "sandboxed Claude: off; Claude agents run bare with this machine's Claude login (sandboxed, they use the \
             Claude subscription through IMACS's relay)"
                .into(),
        ),
        ClaudeRoute::BareRelay { subscription } => lines.push(check_line(
            subscription,
            "bare Claude credential: Claude subscription through IMACS's relay (OS keychain)",
            "run `imacs setup --credential claude` in a trusted host terminal; value is read from stdin",
        )),
        ClaudeRoute::Sandboxed { subscription: true } => lines.push(check_line(
            true,
            "sandbox Claude credential: Claude subscription through IMACS's relay (OS keychain)",
            "",
        )),
        ClaudeRoute::Sandboxed { subscription: false } => lines.push(check_line(
            secrets.is_some_and(|s| s.anthropic),
            "sandbox Claude credential: anthropic API key in sbx (warning if missing)",
            "Claude agents run sandboxed: run `claude setup-token` in a host terminal, then `imacs setup` to keep the token in the OS keychain for IMACS's relay, or store an Anthropic API key with `imacs setup`",
        )),
    }
    for (present, label, hint) in [
        (
            secrets.is_some_and(|s| s.openai),
            "sandbox OpenAI secret: openai",
            "run `sbx secret set openai` interactively",
        ),
        (
            secrets.is_some_and(|s| !s.github),
            "sandbox github secret absent (warning if stored or unreadable)",
            "a github secret lets agents push as you; review it and remove with `sbx secret rm github` if unintended",
        ),
    ] {
        lines.push(check_line(present, label, hint));
    }
    if secrets.is_some_and(|s| s.oauth) {
        lines.push(
            "sandbox CLAUDE_CODE_OAUTH_TOKEN route: registered but unused, since local sbx strips it; remove the custom secret with `sbx secret rm`"
                .into(),
        );
    }
    let misfiled = secrets.map(|s| s.misfiled.as_slice()).filter(|misfiled| !misfiled.is_empty());
    lines.push(check_line(
        secrets.is_some_and(|s| s.misfiled.is_empty()),
        &match misfiled {
            Some(misfiled) => format!("sandbox secrets: {} (warning)", misfiled_text(misfiled)),
            None => "sandbox secrets filed under known services and sandboxes (warning if not)".into(),
        },
        "run `imacs setup` to remove them; their names stay hidden, since they may hold a pasted credential",
    ));
    lines
}

pub(super) fn query_version(program: &str) -> Option<(u64, u64, u64)> {
    let (status, out, _) = sbx_output(program, &["version"], false, TIMEOUT).ok()?;
    status.success().then(|| isolation::parse_version(&out)).flatten()
}

pub(super) fn query(program: &str, args: &[&str]) -> Option<Value> {
    let (status, out, _) = sbx_output(program, args, false, TIMEOUT).ok()?;
    (status.success() || args.first() == Some(&"diagnose")).then(|| serde_json::from_str(&out).ok()).flatten()
}

pub(super) fn setting(program: &str, name: &str) -> Option<String> {
    let (status, out, _) = sbx_output(program, &["settings", "get", name], false, TIMEOUT).ok()?;
    status.success().then(|| out.trim().to_owned())
}

/// sbx exposes the current rules rather than an immutable preset. Report custom rules
/// honestly instead of claiming the configured IMACS default is the global policy.
fn policy_preset(value: &Value) -> Option<&'static str> {
    let rules = value["rules"].as_array()?;
    let network: Vec<_> = rules
        .iter()
        .filter(|r| r["scope"] == "global" && r["resource_type"] == "network" && r["status"] == "active")
        .collect();
    if network.is_empty() {
        return None;
    }
    if network.iter().any(|r| r["provenance"]["created_via"] != "default") {
        return Some("custom (inspect `sbx policy ls`)");
    }
    if network.iter().any(|r| r["id"] == "default-ai-services") {
        return Some("balanced (current default rules)");
    }
    if network.iter().any(|r| {
        r["decision"] == "allow" && r["resources"].as_array().is_some_and(|v| v.iter().any(|s| s == "**" || s == "*"))
    }) {
        return Some("allow-all");
    }
    if network.iter().all(|r| r["decision"] == "deny") {
        return Some("deny-all");
    }
    Some("custom (inspect `sbx policy ls`)")
}

/// The service secrets `sbx secret set` stores (sbx v0.50.1).
const SERVICES: [&str; 13] = [
    "anthropic",
    "copilot",
    "cursor",
    "devin",
    "droid",
    "github",
    "google",
    "groq",
    "mistral",
    "nebius",
    "openai",
    "openrouter",
    "xai",
];

#[derive(Default)]
pub(super) struct SecretNames {
    pub anthropic: bool,
    pub oauth: bool,
    pub openai: bool,
    pub github: bool,
    /// Service secrets under a name sbx has no service for, or in a scope no sandbox has: usually a
    /// credential pasted where the service or sandbox goes, as with `sbx secret set anthropic <token>`.
    pub misfiled: Vec<Misfiled>,
}

/// A misfiled secret. Its name may be a credential, so it only ever goes to `sbx secret rm`.
pub(super) struct Misfiled {
    pub scope: String,
    name: String,
}

impl Misfiled {
    /// The `sbx secret rm` arguments that remove exactly this entry, without a prompt.
    pub(super) fn remove_args(&self) -> Vec<&str> {
        let mut args = vec!["secret", "rm", self.name.as_str()];
        if self.scope != "global" {
            args.extend(["--sandbox", self.scope.as_str()]);
        }
        args.push("--force");
        args
    }
}

/// Misfiled secrets as doctor and setup show them: how many, and in which scopes, never a name.
pub(super) fn misfiled_text(misfiled: &[Misfiled]) -> String {
    // A scope is `global` or a sandbox's name, but a pasted credential can land there too.
    let shown = |scope: &str| {
        let plain = (1..=40).contains(&scope.len())
            && !scope.starts_with("sk-")
            && scope.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
        if plain { scope.to_string() } else { "(not shown)".to_string() }
    };
    let mut scopes: Vec<String> = misfiled.iter().map(|secret| shown(&secret.scope)).collect();
    scopes.sort();
    scopes.dedup();
    let plural = if misfiled.len() == 1 { "" } else { "s" };
    format!("{} misfiled secret{plural} in scope {}", misfiled.len(), scopes.join(", "))
}

/// `sandboxes` are the names `sbx ls` knows, or None when it could not say: a scope is then trusted.
fn secret_names(value: &Value, sandboxes: Option<&[String]>) -> Option<SecretNames> {
    let rows = value.get("secrets").unwrap_or(value).as_array()?;
    let mut known = SecretNames::default();
    for row in rows {
        // Every scope is listed, but only global secrets serve every sandbox.
        let scope = row["scope"].as_str().unwrap_or("global");
        let Some(name) = row["name"].as_str() else { continue };
        // Only exact, known identifiers are allowed out of this parser, and a misfiled entry's
        // name only to remove it: unknown names can be accidentally stored credential values.
        if scope == "global" {
            match name {
                "anthropic" => known.anthropic = true,
                "openai" => known.openai = true,
                "github" => known.github = true,
                "CLAUDE_CODE_OAUTH_TOKEN" => known.oauth = true,
                _ => {}
            }
        }
        let service = row.get("type").is_none_or(|kind| kind == "service");
        let unknown = !SERVICES.contains(&name) && name != "CLAUDE_CODE_OAUTH_TOKEN";
        let orphaned = scope != "global" && sandboxes.is_some_and(|names| !names.iter().any(|known| known == scope));
        if service && (unknown || orphaned) {
            known.misfiled.push(Misfiled { scope: scope.to_string(), name: name.to_string() });
        }
    }
    // sbx lists `secret set-custom` secrets apart. Read only the variable each fills and the hosts it
    // goes to: `source` and `placeholder` can hold a credential or the command that prints one.
    if let Some(custom) = value.get("custom_secrets") {
        let anthropic = |target: &Value| {
            let host = target.as_str().or_else(|| target["host"].as_str()).unwrap_or_default();
            let host = host.strip_prefix("https://").unwrap_or(host).trim_end_matches('/');
            host.strip_suffix(":443").unwrap_or(host).eq_ignore_ascii_case("api.anthropic.com")
        };
        for secret in custom.as_array()? {
            if secret["env"] == "CLAUDE_CODE_OAUTH_TOKEN"
                && secret["scope"].as_str().is_none_or(|scope| scope == "global")
                && secret["targets"].as_array().is_some_and(|targets| targets.iter().any(anthropic))
            {
                known.oauth = true;
            }
        }
    }
    Some(known)
}

pub(super) fn secrets(program: &str) -> Option<SecretNames> {
    // Every scope: `sbx secret set <service> <token>` files a token as a name in a sandbox's scope.
    let sandboxes = list(program).ok().map(|list| list.into_iter().map(|sandbox| sandbox.name).collect::<Vec<_>>());
    query(program, &["secret", "ls", "--json"]).and_then(|v| secret_names(&v, sandboxes.as_deref()))
}

/// A Linear ticket profile also needs the sandbox gateway registration/authentication.
/// Workspace ticket blocks replace the main block, as in doctor.
pub(crate) fn configured(config: &imacs_core::config::Config, workspace: Option<&str>) -> Result<SandboxConfig> {
    let mut sandbox = config.isolation.sandbox.clone();
    let workspace = workspace.map(workspace::Workspace::load).transpose()?;
    if let Some(overrides) = workspace.as_ref().map(workspace::Workspace::isolation).transpose()?.flatten() {
        sandbox = config.isolation.with_overrides(&overrides)?.sandbox;
    }
    let tickets = workspace.as_ref().map(workspace::Workspace::tickets).transpose()?.flatten();
    if tickets.as_ref().unwrap_or(&config.tickets).system.eq_ignore_ascii_case("linear")
        && !sandbox.mcp.iter().any(|name| name == "linear")
    {
        sandbox.mcp.push("linear".into());
    }
    Ok(sandbox)
}

/// What `imacs setup` prepares the machine for. It runs outside IMACS, so without `--workspace` it
/// cannot know the workspace IMACS will open: any saved workspace whose ticket profile uses Linear
/// adds the Linear MCP server, as the main config's does. Unreadable workspaces are left to doctor.
pub(crate) fn setup_config(config: &imacs_core::config::Config, workspace: Option<&str>) -> Result<SandboxConfig> {
    let mut sandbox = configured(config, workspace)?;
    let linear = |name: &String| {
        workspace::Workspace::load(name)
            .ok()
            .and_then(|workspace| workspace.tickets().ok().flatten())
            .is_some_and(|tickets| tickets.system.eq_ignore_ascii_case("linear"))
    };
    if workspace.is_none()
        && !sandbox.mcp.iter().any(|name| name == "linear")
        && workspace::Workspace::list().iter().any(linear)
    {
        sandbox.mcp.push("linear".into());
    }
    Ok(sandbox)
}

pub(super) fn host_lines() -> Vec<String> {
    host_lines_with(|name| crate::setup::find_program(name).is_some())
}

pub(super) fn host_lines_with(installed: impl Fn(&str) -> bool) -> Vec<String> {
    let mac = cfg!(target_os = "macos");
    let mut lines = prerequisite_lines(mac, installed);
    lines.extend(linux_checks());
    lines
}

fn prerequisite_lines(mac: bool, installed: impl Fn(&str) -> bool) -> Vec<String> {
    // Linux's keychain tool, through which IMACS's relay reads the Claude subscription; macOS has `security`.
    let keychain = (!mac).then_some("secret-tool");
    ["git", "claude", "codex"]
        .into_iter()
        .chain(keychain)
        .map(|name| check_line(installed(name), &format!("sandbox prerequisite: {name}"), install_hint(name, mac)))
        .collect()
}

pub(super) fn install_hint(name: &str, mac: bool) -> &'static str {
    match (name, mac) {
        ("git", true) => "install with `xcode-select --install` (or `brew install git`)",
        ("git", false) => "install with `sudo apt-get install git`",
        ("claude", _) => "install with `curl -fsSL https://claude.ai/install.sh | bash`",
        ("codex", true) => "install with `brew install --cask codex`",
        ("codex", false) => "install Node.js/npm, then `npm install -g @openai/codex`",
        ("secret-tool", _) => {
            "install with `sudo apt-get install libsecret-tools gnome-keyring` and unlock the desktop keyring: IMACS's \
             relay reads the Claude subscription with it"
        }
        (_, true) => {
            "install with `brew trust docker/tap` then `brew install docker/tap/sbx` (upgrade: `brew upgrade sbx`)"
        }
        (_, false) => {
            "install with `curl -fsSL https://get.docker.com | sudo REPO_ONLY=1 sh` then `sudo apt install docker-sbx`"
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_checks() -> Vec<String> {
    let os = std::fs::read_to_string("/etc/os-release").ok();
    let kvm = File::options().read(true).write(true).open("/dev/kvm").ok();
    let kvm = kvm.is_some_and(|file| {
        use std::os::fd::AsRawFd;
        // SAFETY: KVM_GET_API_VERSION takes no argument and does not write memory.
        unsafe { libc::ioctl(file.as_raw_fd(), 0xae00) == 12 }
    });
    let groups = Command::new("id").arg("-Gn").output().ok().filter(|o| o.status.success());
    linux_lines(os.as_deref(), kvm, groups.as_ref().and_then(|o| std::str::from_utf8(&o.stdout).ok()))
}

#[cfg(not(target_os = "linux"))]
fn linux_checks() -> Vec<String> {
    Vec::new()
}

#[cfg(any(target_os = "linux", test))]
fn linux_lines(os: Option<&str>, kvm: bool, groups: Option<&str>) -> Vec<String> {
    let field = |key: &str| {
        os.and_then(|text| text.lines().find_map(|line| line.strip_prefix(key))).map(|v| v.trim().trim_matches('"'))
    };
    let supported = field("ID=") == Some("ubuntu")
        && field("VERSION_ID=")
            .and_then(|v| v.split_once('.'))
            .and_then(|(major, minor)| Some((major.parse::<u32>().ok()?, minor.parse::<u32>().ok()?)))
            .is_some_and(|v| v >= (24, 4));
    vec![
        check_line(
            supported,
            "sandbox Linux: Ubuntu 24.04+ (warning if unsupported)",
            "use Ubuntu 24.04 or newer for Docker Sandboxes",
        ),
        check_line(
            kvm,
            "sandbox Linux: KVM available and accessible (warning if unavailable)",
            "enable hardware virtualization, load kvm_intel or kvm_amd and check /dev/kvm permissions",
        ),
        check_line(
            groups.is_some_and(|g| g.split_whitespace().any(|g| g == "kvm")),
            "sandbox Linux: current session in kvm group (warning if missing)",
            "run `sudo usermod -aG kvm \"$USER\"`, then log out and back in",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const API_KEY_ROUTE: ClaudeRoute = ClaudeRoute::Sandboxed { subscription: false };

    fn report(dir: &Path) -> String {
        sandbox_lines(dir.join("sbx").to_str().unwrap(), &SandboxConfig::default(), API_KEY_ROUTE).join("\n")
    }
    fn fixture() -> crate::test_support::TempDir {
        let dir = crate::test_support::tempdir().unwrap();
        fake::install(dir.path());
        dir
    }
    fn write(dir: &Path, file: &str, value: Value) {
        std::fs::write(dir.join(file), value.to_string()).unwrap();
    }
    fn line<'a>(report: &'a str, label: &str) -> &'a str {
        report.lines().find(|l| l.contains(label)).unwrap_or_else(|| panic!("missing {label}: {report}"))
    }

    #[test]
    fn policy_is_initialized_and_its_actual_preset_is_shown() {
        let dir = fixture();
        let dir = dir.path();
        assert!(line(&report(dir), "global network policy").starts_with("✓"));
        assert!(report(dir).contains("balanced"));
        for (decision, preset) in [("allow", "allow-all"), ("deny", "deny-all")] {
            write(
                dir,
                "policy.json",
                json!({"rules":[{"id":"default-network","scope":"global","resource_type":"network","status":"active","decision":decision,"resources":["**"],"provenance":{"created_via":"default"}}]}),
            );
            assert!(line(&report(dir), "global network policy").contains(preset));
        }
        write(
            dir,
            "policy.json",
            json!({"rules":[{"scope":"global","resource_type":"network","status":"active","provenance":{"created_via":"added"}}]}),
        );
        assert!(report(dir).contains("custom"));
        std::fs::write(dir.join("no-policy"), "").unwrap();
        assert!(line(&report(dir), "global network policy").starts_with('✗'));
        assert!(report(dir).contains("sbx policy init balanced"));
        // Every check is read-only, including when policy is uninitialized.
        assert!(fake::calls_to(dir, &["policy", "init"]).is_empty());
    }

    #[test]
    fn only_known_secret_names_escape_even_when_a_value_is_misfiled_as_a_name() {
        let dir = fixture();
        let dir = dir.path();
        for (secrets, claude, openai, github) in [
            (json!([{"name":"anthropic"},{"name":"openai"}]), true, true, false),
            (
                json!([{"name":"CLAUDE_CODE_OAUTH_TOKEN","env":"CLAUDE_CODE_OAUTH_TOKEN","host":"api.anthropic.com"},{"name":"openai"},{"name":"github"}]),
                false,
                true,
                true,
            ),
            (
                json!([{"name":"sk-ant-secret-misfiled","value":"never-display-this"},{"name":"anthropic-suffix"}]),
                false,
                false,
                false,
            ),
            (json!([]), false, false, false),
        ] {
            write(dir, "secrets.json", secrets);
            let text = report(dir);
            assert_eq!(line(&text, "Claude credential:").starts_with('✓'), claude, "{text}");
            assert_eq!(line(&text, "OpenAI secret:").starts_with('✓'), openai, "{text}");
            assert_eq!(line(&text, "github secret absent").starts_with('✗'), github, "{text}");
            assert!(!text.contains("sk-ant-secret-misfiled") && !text.contains("never-display-this"));
        }
        // A leftover subscription route is reported as unused, without a warning that blocks setup.
        write(
            dir,
            "secrets.json",
            json!({"secrets":[{"type":"service","name":"anthropic"},{"type":"custom","name":"CLAUDE_CODE_OAUTH_TOKEN"}]}),
        );
        let text = report(dir);
        assert!(line(&text, "Claude credential:").starts_with('✓'), "{text}");
        assert!(line(&text, "CLAUDE_CODE_OAUTH_TOKEN route: registered but unused").starts_with("sandbox"), "{text}");
    }

    #[test]
    fn claude_runs_sandboxed_by_default_on_the_subscription_through_the_relay_or_an_api_key() {
        let dir = fixture();
        let dir = dir.path();
        let program = dir.join("keychain");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &program).unwrap();
        let keychain = crate::relay::Keychain::at(&program, cfg!(target_os = "macos"));
        let mut config = imacs_core::config::Config::default();
        assert!(claude_sandboxed(&config, None), "the default claude agent follows the sandbox mode");
        assert_eq!(claude_route(&config, None, &keychain), API_KEY_ROUTE);
        std::fs::write(dir.join("keychain-value"), "sk-ant-oat01-PRIVATE-subscription").unwrap();
        let subscription = claude_route(&config, None, &keychain);
        assert_eq!(subscription, ClaudeRoute::Sandboxed { subscription: true });
        // A config that keeps Claude bare, or every agent, never asks the keychain.
        std::fs::remove_file(dir.join("keychain-argv")).unwrap();
        config.agents.iter_mut().find(|agent| agent.name == "claude").unwrap().isolation =
            Some(isolation::IsolationMode::Bare);
        assert_eq!(claude_route(&config, None, &keychain), ClaudeRoute::Bare);
        config.agents.iter_mut().for_each(|agent| agent.isolation = None);
        config.isolation.mode = isolation::IsolationMode::Bare;
        assert_eq!(claude_route(&config, None, &keychain), ClaudeRoute::Bare);
        assert!(!dir.join("keychain-argv").exists());

        std::fs::write(dir.join("removed-anthropic"), "").unwrap();
        let lines =
            |route| sandbox_lines(dir.join("sbx").to_str().unwrap(), &SandboxConfig::default(), route).join("\n");
        let bare = lines(ClaudeRoute::Bare);
        assert!(bare.lines().any(|l| l.starts_with("sandboxed Claude: off; Claude agents run bare")), "{bare}");
        assert!(!bare.contains("sandbox Claude credential"), "{bare}");
        let relayed = lines(subscription);
        let relay = line(&relayed, "sandbox Claude credential:");
        assert!(
            relay.starts_with("✓ sandbox Claude credential: Claude subscription through IMACS's relay"),
            "{relayed}"
        );
        assert!(!relayed.contains("PRIVATE"));
        let text = report(dir);
        let missing = line(&text, "sandbox Claude credential: anthropic API key");
        assert!(missing.starts_with('✗') && missing.contains("claude setup-token"), "{missing}");
    }

    #[test]
    fn mcp_checks_registration_and_authorization_without_starting_oauth() {
        let dir = fixture();
        let dir = dir.path();
        let program = dir.join("sbx");
        let config = SandboxConfig { mcp: vec!["linear".into()], ..Default::default() };
        for (servers, expected) in [
            (json!([]), "not registered"),
            (json!([{"name":"linear","status":"needs-auth"}]), "needs authorization"),
            (json!([{"name":"linear","status":"ready"}]), "ready"),
            (json!([{"name":"linear","status":"error","error":"private-credential"}]), "unavailable"),
        ] {
            write(dir, "mcp.json", servers);
            let text = doctor_lines(program.to_str().unwrap(), &config, API_KEY_ROUTE).join("\n");
            let mcp = line(&text, "MCP linear:");
            assert!(mcp.contains(expected), "{text}");
            assert_eq!(mcp.starts_with('✓'), expected == "ready");
            assert!(!text.contains("private-credential"));
        }
        assert!(fake::calls_to(dir, &["mcp", "auth"]).is_empty());
        assert!(fake::calls_to(dir, &["mcp", "add"]).is_empty());
    }

    #[test]
    fn ssh_warns_for_imacs_forwarding_global_forwarding_or_a_fixed_socket() {
        let dir = fixture();
        let dir = dir.path();
        assert!(line(&report(dir), "SSH agent forwarding").starts_with('✓'));
        for (file, value) in [("ssh-forwarding", "true"), ("ssh-socket", "/private/socket")] {
            std::fs::write(dir.join(file), value).unwrap();
            let text = report(dir);
            assert!(line(&text, "SSH agent forwarding").starts_with('✗'));
            assert!(!text.contains("/private/socket"));
            std::fs::remove_file(dir.join(file)).unwrap();
        }
        let config = SandboxConfig { ssh_agent: true, ..Default::default() };
        let lines = doctor_lines(dir.join("sbx").to_str().unwrap(), &config, API_KEY_ROUTE).join("\n");
        assert!(line(&lines, "SSH agent forwarding").starts_with('✗'));
    }

    #[test]
    fn failed_and_malformed_probes_warn_without_echoing_stdout_or_stderr() {
        let dir = fixture();
        let dir = dir.path();
        std::fs::write(dir.join("error-output"), "misfiled-secret-in-error").unwrap();
        for (verb, label) in [
            ("secret", "Claude credential:"),
            ("policy", "global network policy"),
            ("mcp", "MCP status unavailable"),
            ("settings", "SSH agent forwarding"),
        ] {
            std::fs::write(dir.join(format!("fail-{verb}")), "").unwrap();
            let text = report(dir);
            assert!(line(&text, label).starts_with('✗'), "{text}");
            assert!(!text.contains("misfiled-secret-in-error"));
            std::fs::remove_file(dir.join(format!("fail-{verb}"))).unwrap();
        }
        for (file, label) in [
            ("secrets.json", "Claude credential:"),
            ("policy.json", "global network policy"),
            ("mcp.json", "MCP status unavailable"),
        ] {
            std::fs::write(dir.join(file), "malformed-private-output").unwrap();
            let text = report(dir);
            assert!(line(&text, label).starts_with('✗'));
            assert!(!text.contains("malformed-private-output"));
            std::fs::remove_file(dir.join(file)).unwrap();
        }
    }

    #[test]
    fn linux_checks_version_kvm_access_and_effective_group_membership() {
        assert!(
            linux_lines(Some("ID=ubuntu\nVERSION_ID=\"24.04\""), true, Some("users kvm video"))
                .iter()
                .all(|l| l.starts_with('✓'))
        );
        for os in [
            None,
            Some("ID=ubuntu\nVERSION_ID=22.04"),
            Some("ID=ubuntu\nVERSION_ID=bad"),
            Some("ID=debian\nVERSION_ID=24.04"),
        ] {
            assert!(linux_lines(os, true, Some("kvm"))[0].starts_with('✗'));
        }
        assert!(
            linux_lines(Some("ID=ubuntu\nVERSION_ID=26.04"), false, Some("users not-kvm"))[1..]
                .iter()
                .all(|l| l.starts_with('✗'))
        );
        assert!(linux_lines(None, false, None).iter().all(|l| l.starts_with('✗')));
    }

    #[test]
    fn fresh_machine_prerequisites_have_os_specific_install_commands() {
        for mac in [true, false] {
            assert!(prerequisite_lines(mac, |_| true).iter().all(|line| line.starts_with('✓')));
            let missing = prerequisite_lines(mac, |_| false).join("\n");
            assert!(missing.lines().all(|line| line.starts_with('✗')));
            assert!(missing.contains("git") && missing.contains("claude") && missing.contains("codex"));
            assert!(missing.contains(if mac { "xcode-select --install" } else { "sudo apt-get install git" }));
            assert!(install_hint("sbx", mac).contains(if mac {
                "brew install docker/tap/sbx"
            } else {
                "sudo apt install docker-sbx"
            }));
            // Linux's keychain tool keeps the Claude subscription for the relay; macOS always has `security`.
            assert_eq!(prerequisite_lines(mac, |_| false).len(), if mac { 3 } else { 4 });
            assert_eq!(missing.contains("sandbox prerequisite: secret-tool"), !mac);
            assert_eq!(missing.contains("libsecret-tools"), !mac);
        }
    }

    #[test]
    fn real_secret_envelope_ignores_misfiled_names_and_unrelated_metadata() {
        let names = secret_names(
            &json!({"secrets":[
                {"type":"service", "scope":"global", "name":"sk-ant-MISFILED-private-value"},
                {"type":"service", "scope":"global", "name":"openai"},
                {"type":"custom", "scope":"global", "name":"CLAUDE_CODE_OAUTH_TOKEN"},
                {"name":"unknown", "env":"CLAUDE_CODE_OAUTH_TOKEN", "host":"api.anthropic.com"}
            ]}),
            None,
        )
        .unwrap();
        assert!(names.openai && names.oauth);
        assert!(!names.anthropic && !names.github);
        assert!(
            !secret_names(
                &json!({"secrets":[{"name":"unknown", "env":"CLAUDE_CODE_OAUTH_TOKEN", "host":"api.anthropic.com"}]}),
                None
            )
            .unwrap()
            .oauth
        );
        assert!(secret_names(&json!({"secrets":"private-malformed-output"}), None).is_none());
    }

    #[test]
    fn the_subscription_route_is_a_custom_secret_found_without_reading_its_source() {
        let route = |env: &str, targets: Value| {
            json!({"env": env, "kind": "command", "placeholder": "sk-ant-oat01-PRIVATE-placeholder", "refresh": null,
                   "scope": "global", "source": "PRIVATE-source-command", "targets": targets})
        };
        let names = |custom: Value| {
            secret_names(
                &json!({"secrets":[{"type":"service","name":"openai","scope":"global"}],"custom_secrets":custom}),
                None,
            )
        };
        let found = names(json!([route("CLAUDE_CODE_OAUTH_TOKEN", json!(["api.anthropic.com"]))])).unwrap();
        assert!(found.oauth && found.openai && !found.anthropic);
        for targets in [json!(["https://api.anthropic.com:443/"]), json!([{"host":"API.anthropic.com"}])] {
            assert!(names(json!([route("CLAUDE_CODE_OAUTH_TOKEN", targets.clone())])).unwrap().oauth, "{targets}");
        }
        for (env, targets) in [
            ("ANTHROPIC_API_KEY", json!(["api.anthropic.com"])),
            ("CLAUDE_CODE_OAUTH_TOKEN", json!(["api.anthropic.com.example.org"])),
            ("CLAUDE_CODE_OAUTH_TOKEN", json!("api.anthropic.com")),
            ("CLAUDE_CODE_OAUTH_TOKEN", Value::Null),
        ] {
            assert!(!names(json!([route(env, targets.clone())])).unwrap().oauth, "{env} {targets}");
        }
        assert!(names(json!("private-malformed-output")).is_none());

        // Doctor, against sbx's real envelope: the route and nothing private shows.
        let dir = fixture();
        let dir = dir.path();
        std::fs::write(dir.join("oauth"), "").unwrap();
        std::fs::write(dir.join("removed-anthropic"), "").unwrap();
        // Local sbx strips this route (docs/isolation.md): doctor names it unused, not a credential.
        let text = report(dir);
        assert!(line(&text, "Claude credential:").starts_with('✗'), "{text}");
        assert!(line(&text, "CLAUDE_CODE_OAUTH_TOKEN route: registered but unused").starts_with("sandbox"), "{text}");
        assert!(!text.contains("PRIVATE"), "{text}");
    }

    #[test]
    fn misfiled_secrets_show_as_a_count_and_scopes_and_only_rm_gets_their_names() {
        let sandboxes = ["imacs-live".to_string()];
        let names = secret_names(
            &json!({"secrets":[
                {"type":"service", "scope":"global", "name":"openai"},
                {"type":"service", "scope":"imacs-live", "name":"github"},
                {"type":"service", "scope":"anthropic", "name":"sk-ant-oat01-SCOPED-private-value"},
                {"type":"service", "scope":"imacs-gone", "name":"openai"},
                {"type":"service", "scope":"global", "name":"sk-ant-MISFILED-private-value"},
                {"type":"service", "scope":"sk-ant-api03-PRIVATE-scope", "name":"anthropic"},
                {"type":"registry", "scope":"global", "name":"ghcr.io"},
                {"type":"custom", "scope":"global", "name":"CLAUDE_CODE_OAUTH_TOKEN"}
            ]}),
            Some(&sandboxes),
        )
        .unwrap();
        // Only global secrets count; a sandbox's own github secret is its business.
        assert!(names.openai && names.oauth && !names.github && !names.anthropic);
        let text = misfiled_text(&names.misfiled);
        assert_eq!(text, "4 misfiled secrets in scope (not shown), anthropic, global, imacs-gone");
        assert!(!text.contains("PRIVATE"));
        let removals: Vec<Vec<&str>> = names.misfiled.iter().map(Misfiled::remove_args).collect();
        assert_eq!(
            removals[0],
            ["secret", "rm", "sk-ant-oat01-SCOPED-private-value", "--sandbox", "anthropic", "--force"]
        );
        assert_eq!(removals[2], ["secret", "rm", "sk-ant-MISFILED-private-value", "--force"]);
        // Without a sandbox list a scope is trusted; a name sbx has no service for never is.
        let unknown = secret_names(
            &json!({"secrets":[{"type":"service", "scope":"imacs-gone", "name":"openai"},
                               {"type":"service", "scope":"anthropic", "name":"sk-ant-oat01-SCOPED-private-value"}]}),
            None,
        )
        .unwrap();
        assert_eq!(misfiled_text(&unknown.misfiled), "1 misfiled secret in scope anthropic");

        // Doctor, against the fake sbx: Max's token-as-name in a scope named anthropic.
        let dir = fixture();
        let dir = dir.path();
        std::fs::write(dir.join("misfiled-scoped"), "").unwrap();
        let text = report(dir);
        let line = line(&text, "sandbox secrets");
        assert!(line.starts_with("✗ sandbox secrets: 1 misfiled secret in scope anthropic (warning)"), "{line}");
        assert!(!text.contains("SCOPED-private"), "{text}");
        std::fs::remove_file(dir.join("misfiled-scoped")).unwrap();
        assert!(line_ok(&report(dir), "sandbox secrets filed under known services"));
    }

    fn line_ok(report: &str, label: &str) -> bool {
        line(report, label).starts_with('✓')
    }

    #[test]
    fn linear_ticket_profile_is_required_even_without_explicit_static_mcp() {
        let mut config = imacs_core::config::Config::default();
        assert!(!configured(&config, None).unwrap().mcp.contains(&"linear".into()));
        config.tickets.system = "Linear".into();
        assert_eq!(configured(&config, None).unwrap().mcp, ["linear"]);
        config.isolation.sandbox.mcp.push("linear".into());
        assert_eq!(configured(&config, None).unwrap().mcp, ["linear"]);
    }
}
