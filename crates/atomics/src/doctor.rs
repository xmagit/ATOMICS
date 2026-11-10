use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use atomics_core::{
    config::Config,
    doctor::{
        approvals_check, cache_clone_check, check_line, codex_trust_lines, codex_version_check, plans_check,
        stash_lines, worktree_checks,
    },
    paths,
    selfconfig::{self, Layer, Source},
    tickets::Cleaner,
    workspace::Workspace,
};
use serde_json::{Value, json};

use crate::setup::{
    codex_supports_hooks, find_program, global_hooks_installed, legacy_hooks_installed, resolve_program,
};

/// What the doctor found, line by line in the order it prints, and whether the required
/// checks (git and a config that loads) passed.
struct Report {
    lines: Vec<String>,
    /// The lines of the required checks; every other failed check is a warning.
    required: [usize; 2],
    git_ok: bool,
    config_ok: bool,
}

pub fn run(workspace: Option<&str>) -> i32 {
    // Worktree checks run git in folders sandboxes have worked in.
    let report = report(workspace, false, &[]);
    for line in &report.lines {
        println!("{line}");
    }
    exit_code(report.git_ok, report.config_ok)
}

/// The doctor's checks for an agent, as JSON. Config errors name the file and line without
/// quoting it, and every line is cleaned of the secrets the configuration runs with.
pub fn json(workspace: Option<&str>, projects: &[PathBuf]) -> Value {
    let report = report(workspace, true, projects);
    let config_path = paths::config_file();
    let [main, host] = Config::source_paths(&config_path);
    let layers = [Layer::read(Source::Main, &main), Layer::read(Source::Host, &host)];
    let effective = Config::load(&config_path).unwrap_or_default();
    let cleaner = Cleaner::new(selfconfig::secrets(&effective, &layers, crate::setup::env_var));
    let (mut checks, mut facts) = (Vec::new(), Vec::new());
    for (index, line) in report.lines.iter().enumerate() {
        if let Some(check) = line.strip_prefix("✓ ") {
            checks.push(json!({ "ok": true, "check": check }));
        } else if let Some(failed) = line.strip_prefix("✗ ") {
            let (check, hint) = failed.rsplit_once(" — ").unwrap_or((failed, ""));
            let warning = !report.required.contains(&index);
            checks.push(json!({ "ok": false, "warning": warning, "check": check, "hint": hint }));
        } else {
            facts.push(json!(line));
        }
    }
    cleaner.setting(json!({
        "ok": report.git_ok && report.config_ok,
        "required": "git and a config that loads; every other failed check is a warning",
        "facts": facts,
        "checks": checks,
    }))
}

#[cfg(target_os = "macos")]
fn spotlight_check(repo: &std::path::Path) -> Option<String> {
    let mut child = Command::new("/usr/bin/mdutil")
        .arg("-s")
        .arg(repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    spotlight_line(repo, &String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "macos", test))]
fn spotlight_line(repo: &std::path::Path, output: &str) -> Option<String> {
    output.contains("Indexing enabled").then(|| {
        check_line(
            false,
            &format!("project folder is indexed by Spotlight (warning): {}", repo.display()),
            "add the project folder and its worktree parent to System Settings → Spotlight → Search Privacy",
        )
    })
}

/// Whether any agent would run sandboxed: `[isolation] mode`, with the workspace's merged in,
/// or an agent's own `isolation`.
fn sandboxes_wanted(config: &Config, workspace: Option<&str>) -> bool {
    use atomics_core::isolation::IsolationMode;
    effective_isolation(config, workspace).mode == IsolationMode::Sandbox
        || config.agents.iter().any(|a| a.isolation == Some(IsolationMode::Sandbox))
}

fn report(workspace: Option<&str>, for_agent: bool, projects: &[PathBuf]) -> Report {
    let saved = atomics_core::store::Store::load(&paths::store_file());
    let workspace = workspace.or_else(|| (!for_agent).then_some(saved.workspace.as_deref()).flatten());
    let mut lines = Vec::new();
    if let Err(error) = crate::sandbox::load_owned_pins(&paths::atomics_home()) {
        lines.push(check_line(false, "sandbox git pins", &format!("{error:#}")));
    }
    // What every check below runs with, and every program ATOMICS starts.
    let (path_ok, path) = crate::setup::login_path().describe();
    lines.push(check_line(
        path_ok,
        &path,
        "make your login shell print its PATH quickly when started with -l -i -c, or set SHELL to one that does",
    ));
    let git_version = Command::new(resolve_program("git"))
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|version| !version.is_empty());
    let git_ok = git_version.is_some();
    let git = git_version.map(|version| format!("git found ({version})")).unwrap_or_else(|| "git not found".into());
    let git_line = lines.len();
    lines.push(check_line(git_ok, &git, "install git and add it to your login-shell PATH"));

    let config_path = paths::config_file();
    let config = Config::load_with_sources(&config_path);
    let editor = config.as_ref().ok().and_then(|(config, _)| config.editor.command.first());
    for program in [Some("claude"), Some("codex"), editor.map(String::as_str)].into_iter().flatten() {
        let found = if Some(program) == editor.map(String::as_str) {
            crate::setup::editor_on_path(program, find_program)
        } else {
            find_program(program)
        };
        let label = match &found {
            Some(path) => format!("{program} found on login-shell PATH ({})", path.display()),
            None if Some(program) == editor.map(String::as_str) => format!(
                "{program} not found on login-shell PATH (warning): the editor panel cannot start it ([editor] command)"
            ),
            None => format!("{program} not found on login-shell PATH (warning)"),
        };
        lines.push(check_line(found.is_some(), &label, "install it or add it to your login-shell PATH"));
        if program == "codex"
            && let Some(path) = found
        {
            // Like agent launch, tolerate a cold CLI starting while other agents compile.
            let supported = codex_supports_hooks(&path.to_string_lossy(), None, std::time::Duration::from_secs(10));
            lines.push(codex_version_check(supported));
            // An older Codex gets no hooks to trust.
            if supported != Some(false) {
                lines.extend(codex_trust_lines_for(config.as_ref().ok().map(|(config, _)| config), workspace));
            }
        }
    }

    lines.push(format!("Host: {}", paths::host_name()));
    let label = match &config {
        Ok(_) => format!("config parses ({})", config_path.display()),
        // TOML errors quote the failing line, which may hold a secret: agents get file and line only.
        Err(_) if for_agent => {
            let [main, host] = Config::source_paths(&config_path);
            let layers = [Layer::read(Source::Main, &main), Layer::read(Source::Host, &host)];
            format!("config failed: {}", selfconfig::problems(&layers).join("; "))
        }
        Err(err) => format!("config failed: {err:#}"),
    };
    let config_line = lines.len();
    lines.push(check_line(config.is_ok(), &label, "create or fix this config; start ATOMICS to create a missing file"));
    if let Ok((_, sources)) = &config {
        for source in sources {
            lines.push(format!("Loaded config: {}", source.display()));
        }
    }

    lines.extend(plans_check(&paths::atomics_home()));
    let kept = crate::lifecycle::kept_leftovers(&paths::atomics_home());
    for note in &kept {
        lines.push(check_line(
            false,
            &format!("cleanup leftover (warning): {note}"),
            "review in ATOMICS's Clean up… dialog",
        ));
    }
    for note in
        crate::lifecycle::sweep_leftovers(&paths::atomics_home()).into_iter().filter(|note| !kept.contains(note))
    {
        lines.push(check_line(
            false,
            &format!("cleanup leftover (warning): {note}"),
            "ATOMICS looks again when it next starts and drops it once fixed; review in its Clean up… dialog",
        ));
    }
    let credential_keychain = if cfg!(test) {
        crate::relay::Keychain::at(
            std::path::Path::new("/nonexistent/atomics-test-keychain"),
            cfg!(target_os = "macos"),
        )
    } else {
        crate::relay::Keychain::current()
    };
    lines.extend(crate::credentials::doctor_lines(&crate::credentials::Store::new(credential_keychain.clone())));
    if let Ok((config, _)) = &config {
        let azure = crate::sandbox::checks::setup_config(config, workspace)
            .map(|sandbox| sandbox.azure.enabled)
            .unwrap_or(false);
        lines.push(crate::relay::azure::doctor_line(azure));
        lines.push(approvals_check(&config.approvals));
        if let crate::sandbox::checks::ClaudeRoute::BareRelay { subscription } =
            crate::sandbox::checks::claude_route(config, workspace, &credential_keychain)
        {
            lines.push(check_line(
                subscription,
                "bare Claude credential: OS keychain → ATOMICS relay",
                "run `atomics setup --credential claude`; input is stdin only",
            ));
        }
        if sandboxes_wanted(config, workspace) {
            // Tests never reach the sbx installed on the machine running them.
            let sbx = if cfg!(test) { "/nonexistent/atomics-test-sbx".into() } else { resolve_program("sbx") };
            match crate::sandbox::checks::configured(config, workspace) {
                Ok(sandbox) => {
                    // Nor this machine's keychain.
                    let keychain = if cfg!(test) {
                        crate::relay::Keychain::at(
                            std::path::Path::new("/nonexistent/atomics-test-keychain"),
                            cfg!(target_os = "macos"),
                        )
                    } else {
                        crate::relay::Keychain::current()
                    };
                    let claude = crate::sandbox::checks::claude_route(config, workspace, &keychain);
                    lines.extend(crate::sandbox::doctor_lines(&sbx, &sandbox, claude))
                }
                Err(_) => lines.push(check_line(
                    false,
                    "sandbox configuration unavailable",
                    "fix the workspace isolation and tickets sections",
                )),
            }
        }
        match ticket_config(config, workspace) {
            Ok(tickets) if !tickets.system.trim().is_empty() => {
                lines.extend(crate::ticket_signin::doctor_line(
                    &crate::mcp_client::OAuth::new(credential_keychain.clone(), crate::connections::Browser::current()),
                    &tickets,
                ));
                lines.push(ticket_check(&tickets, config.remote.rendezvous.enabled));
                lines.extend(http_operations_check(&tickets));
                lines.extend(remote_check(&tickets));
            }
            Ok(_) => {}
            Err(error) => {
                let error = match workspace.and_then(|name| Workspace::file(name).ok()).filter(|_| for_agent) {
                    Some(file) => selfconfig::problems(&[Layer::read(Source::Workspace, &file)]).join("; "),
                    None => format!("{error:#}"),
                };
                lines.push(check_line(
                    false,
                    &format!("ticket config failed: {error}"),
                    "fix the workspace [tickets] section",
                ))
            }
        }
        let mut repos: Vec<_> =
            config.repos.iter().map(|repo| paths::expand(repo)).chain(projects.iter().cloned()).collect();
        if !for_agent {
            repos.extend(saved.projects.iter().cloned());
        }
        if repos.is_empty()
            && let Ok(cwd) = std::env::current_dir()
        {
            repos.push(cwd);
        }
        if let Some(workspace) = workspace.and_then(|name| Workspace::load(name).ok()) {
            repos.extend(workspace.projects);
        }
        let mut repos: Vec<_> =
            repos.into_iter().map(|repo| atomics_core::workspace::project_root(&repo).unwrap_or(repo)).collect();
        repos.sort();
        repos.dedup();
        for repo in repos {
            lines.extend(identity_lines(config, workspace, &repo));
            #[cfg(target_os = "macos")]
            if let Some(line) = spotlight_check(&repo) {
                lines.push(line);
            }
            let workflow = atomics_core::repo_workflow::RepoWorkflow::read(&repo);
            for warning in &workflow.warnings {
                lines.push(check_line(
                    false,
                    &format!("repository workflow (warning): {warning}"),
                    "use only documented declarative keys in .atomics/config.toml",
                ));
            }
            let config = workflow.apply(config);
            lines.push(cache_clone_check(&repo, &config.worktree_path));
            lines.extend(worktree_checks(&repo, &config.base_ref));
            lines.extend(stash_lines(&repo));
        }
    }

    let socket_path = paths::socket_path();
    let running = UnixStream::connect(&socket_path).is_ok();
    let label = if running {
        format!("ATOMICS running ({})", socket_path.display())
    } else {
        format!("ATOMICS not running (warning): nothing answers at {}", socket_path.display())
    };
    lines.push(check_line(running, &label, "start ATOMICS or check ATOMICS_SOCKET / ATOMICS_HOME"));

    let (settings, installed) = (crate::setup::claude_settings_path(), global_hooks_installed());
    let label = if installed {
        format!("global Claude hooks installed ({})", settings.display())
    } else {
        format!(
            "global Claude hooks not installed (warning): Claude sessions started outside ATOMICS stay unseen ({})",
            settings.display()
        )
    };
    lines.push(check_line(installed, &label, "run `atomics install-hooks` if you want them seen"));
    if legacy_hooks_installed() {
        lines.push(check_line(
            false,
            "leftover cox hook handlers (warning)",
            "run `atomics install-hooks` to remove them",
        ));
    }

    Report { lines, required: [git_line, config_line], git_ok, config_ok: config.is_ok() }
}

fn identity_lines(config: &Config, workspace: Option<&str>, repo: &Path) -> Vec<String> {
    use atomics_core::git;
    let workspace = workspace.map(Workspace::load_identity).transpose().map(Option::flatten);
    let workspace = match workspace {
        Ok(identity) => identity,
        Err(_) => {
            return vec![check_line(
                false,
                "workspace identity does not load (warning)",
                "fix the workspace [identity] section",
            )];
        }
    };
    let identity = config.identity.resolve(&repo.to_string_lossy(), workspace.as_ref(), |path| {
        let path = paths::expand(path);
        std::fs::canonicalize(&path).unwrap_or(path).to_string_lossy().into_owned()
    });
    let fallback = |key: &str| git::git(repo, &["config", "--get", key]).ok().map(|v| v.trim().to_string());
    let name = identity.name.clone().or_else(|| fallback("user.name")).unwrap_or_else(|| "unset".into());
    let email = identity.email.clone().or_else(|| fallback("user.email")).unwrap_or_else(|| "unset".into());
    let git_fallback = identity.name.is_none() || identity.email.is_none();
    let mut lines = vec![format!(
        "Identity for {}: {name} <{email}>{}",
        paths::display(repo),
        if git_fallback { " (Git fallback)" } else { " (configured)" }
    )];
    let global =
        |key: &str| git::git(repo, &["config", "--show-scope", "--get", key]).is_ok_and(|v| v.starts_with("global\t"));
    if workspace.is_some()
        && (identity.name.is_none() && global("user.name") || identity.email.is_none() && global("user.email"))
    {
        lines.push(check_line(
            false,
            &format!(
                "{} falls back to a global identity while the workspace defines one (warning)",
                paths::display(repo)
            ),
            "set both name and email in [identity], or add a user-owned project identity",
        ));
    }
    lines
}

/// Whether Codex runs the hooks ATOMICS passes its bare agents, from the trust in the Codex home
/// they get, for the install that passes them, and how other installs found stand. Sandboxed Codex
/// agents get no ATOMICS hooks, so the check is moot while every Codex agent runs sandboxed.
fn codex_trust_lines_for(config: Option<&Config>, workspace: Option<&str>) -> Vec<String> {
    if let Some(config) = config
        && !bare_codex_agents(config, workspace)
    {
        return vec![check_line(
            true,
            "Codex hook trust not needed: Codex agents run sandboxed, where ATOMICS passes no hooks; a bare \
             Codex agent or task needs it",
            "",
        )];
    }
    // The command and socket agent launch passes, for each install.
    let socket = paths::socket_path();
    let installs: Vec<_> = atomics_installs(
        crate::setup::recorded_executable(&paths::atomics_home()),
        crate::setup::hook_executable().unwrap_or_else(|_| "atomics".into()),
        &paths::home(),
    )
    .into_iter()
    .map(|(executable, why)| atomics_core::doctor::InstallHooks {
        binary: format!("{} ({why})", executable.display()),
        hooks: atomics_core::hook::codex_hook_trust(&executable, &socket),
    })
    .collect();
    let file = crate::setup::agent_codex_home().join("config.toml");
    codex_trust_lines(&installs, &file, std::fs::read_to_string(&file))
}

/// The ATOMICS installs whose hooks Codex can be asked to trust, and why each is listed. First the
/// one whose hooks bare Codex agents get: the executable `recorded` by the desktop or host that last
/// served this home's socket, or with none recorded (or gone), this doctor's `own`. Then the others
/// found: this doctor, the app in `home`'s or the system's Applications folder, and what the Linux
/// installer and cargo install. A binary reached by several of these paths is listed once.
fn atomics_installs(recorded: Option<PathBuf>, own: PathBuf, home: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut installs = match recorded.filter(|executable| executable.is_file()) {
        Some(served) => vec![(served, "the desktop or host that last ran here"), (own, "this doctor")],
        None => vec![(own, "this doctor; no desktop or host has run here since recording its own")],
    };
    let app = "ATOMICS.app/Contents/MacOS/atomics";
    for found in [
        home.join("Applications").join(app),
        Path::new("/Applications").join(app),
        home.join(".local/bin/atomics"),
        home.join(".cargo/bin/atomics"),
    ] {
        if found.is_file() {
            installs.push((found, "installed"));
        }
    }
    let mut seen = Vec::new();
    installs.retain(|(executable, _)| {
        let real = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.clone());
        let first = !seen.contains(&real);
        seen.push(real);
        first
    });
    installs
}

/// Whether a configured Codex agent runs bare by default, with the workspace's `[isolation]`.
fn bare_codex_agents(config: &Config, workspace: Option<&str>) -> bool {
    use atomics_core::isolation::IsolationMode;
    let isolation = effective_isolation(config, workspace);
    config
        .agents
        .iter()
        .filter(|agent| agent.is_codex())
        .any(|agent| isolation.mode_for(agent, None) == IsolationMode::Bare)
}

/// `[isolation]` with the workspace's merged in.
fn effective_isolation(config: &Config, workspace: Option<&str>) -> atomics_core::isolation::IsolationConfig {
    let overrides = workspace.and_then(|name| Workspace::load(name).ok()).and_then(|w| w.isolation().ok().flatten());
    match overrides {
        Some(overrides) => config.isolation.with_overrides(&overrides).unwrap_or_else(|_| config.isolation.clone()),
        None => config.isolation.clone(),
    }
}

/// The effective `[tickets]` of the main config and the workspace, for commands outside the app.
pub(crate) fn ticket_config_for(workspace: Option<&str>) -> anyhow::Result<atomics_core::tickets::TicketConfig> {
    ticket_config(&Config::load(&paths::config_file())?, workspace)
}

fn ticket_config(config: &Config, workspace: Option<&str>) -> anyhow::Result<atomics_core::tickets::TicketConfig> {
    let Some(name) = workspace else { return Ok(config.tickets.clone()) };
    let tickets = Workspace::load(name)?.tickets()?;
    Ok(tickets.unwrap_or_else(|| config.tickets.clone()))
}

fn ticket_check(config: &atomics_core::tickets::TicketConfig, rendezvous: bool) -> String {
    let result = (|| {
        let mut client = crate::mcp_client::McpClient::connect(
            &config.server,
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
            crate::setup::env_var,
        )
        .map_err(|e| e.to_string())?;
        let tools = client.tool_names().map_err(|e| e.to_string())?;
        let missing: Vec<_> = [
            "get",
            "list",
            "update_state",
            "comment",
            "create",
            "whoami",
            "conversation_open",
            "conversation_post",
            "conversation_read",
            "conversation_ack",
            "conversation_steerers",
            "rendezvous_register",
            "rendezvous_challenge",
            "rendezvous_pass",
        ]
        .into_iter()
        .filter(|name| rendezvous || !name.starts_with("rendezvous_"))
        .filter_map(|name| {
            config
                .operation(name)
                .filter(|op| op.http.is_none() && !tools.contains(&op.tool))
                .map(|op| format!("{name} → {}", op.tool))
        })
        .collect();
        if !missing.is_empty() {
            return Err(format!("missing mapped tools: {}", missing.join(", ")));
        }
        Ok(["search", "semantic_search", "browse", "children", "related", "read", "read_comments"]
            .into_iter()
            .filter_map(|name| {
                config
                    .operation(name)
                    .filter(|op| op.http.is_none() && !tools.contains(&op.tool))
                    .map(|op| format!("{name} → {}", op.tool))
            })
            .collect::<Vec<_>>())
    })();
    // Doctor output is shared in bug reports: the server's words go through the same
    // cleaner as everything else ATOMICS keeps or shows from a ticket server.
    let cleaner = atomics_core::tickets::Cleaner::of(config, crate::setup::env_var);
    let label = match &result {
        Ok(notes) if notes.is_empty() => {
            format!("{} ticket MCP server starts and offers all mapped tools", config.system)
        }
        Ok(notes) => format!(
            "{} ticket MCP server starts; note: optional mapped tools unavailable: {}",
            config.system,
            cleaner.error(&notes.join(", "))
        ),
        Err(error) => format!("{} ticket MCP check failed (warning): {}", config.system, cleaner.error(error)),
    };
    check_line(
        result.is_ok(),
        &label,
        "check [tickets].server and operations; authenticate a stdio bridge in a terminal first",
    )
}

/// The operations mapped with `http`, which the tool check above skips: whether their address is
/// one ATOMICS will call, and whether they may. It sends nothing: their paths need ticket values.
fn http_operations_check(config: &atomics_core::tickets::TicketConfig) -> Option<String> {
    let names: Vec<&str> = atomics_core::tickets::HTTP_OPERATIONS
        .iter()
        .copied()
        .filter(|name| config.operation(name).is_some_and(|op| op.http.is_some()))
        .collect();
    if names.is_empty() {
        return None;
    }
    let names = names.join(", ");
    let checked = config.http_server().ok_or_else(|| "set [tickets.http] url".to_string()).and_then(|api| {
        let base = crate::mcp_client::http_base(api.url.as_deref().unwrap_or_default()).map_err(|e| e.to_string())?;
        let token = crate::mcp_client::shared_token(&config.server, &api, Some(String::new()));
        let left_out = config.server.bearer_env().is_some() && token.is_none();
        Ok((base.host_str().unwrap_or("the API").to_string(), left_out))
    });
    let cleaner = atomics_core::tickets::Cleaner::of(config, crate::setup::env_var);
    let label = match &checked {
        Ok((host, false)) => format!("{} ticket HTTP operations at {host}: {names}", config.system),
        Ok((host, true)) => format!(
            "{} ticket HTTP operations at {host}: {names}; note: token_env goes only to server.url's origin, so they send no token",
            config.system
        ),
        Err(error) => {
            format!("{} ticket HTTP operations ({names}) cannot run: {}", config.system, cleaner.error(error))
        }
    };
    Some(check_line(checked.is_ok(), &label, "check [tickets.http] url"))
}

/// `[tickets.remote]`, when enabled: reads whoami (a read without side effects) and applies
/// the steering-list rules. A connection that could steer itself is an error.
fn remote_check(config: &atomics_core::tickets::TicketConfig) -> Option<String> {
    use atomics_core::tickets::{Vars, Whoami};
    if !config.remote.enabled {
        return None;
    }
    let whoami = config.operation("whoami").map(|operation| -> Result<Whoami, String> {
        let mut client = crate::mcp_client::McpClient::connect(
            &config.server,
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
            crate::setup::env_var,
        )
        .map_err(|e| e.to_string())?;
        let result = client
            .call_tool(&operation.tool, operation.arguments(&Vars::new(&config.system))?)
            .map_err(|e| e.to_string())?;
        Whoami::read(operation, &atomics_core::tickets::tool_output(&result))
    });
    let cleaner = atomics_core::tickets::Cleaner::of(config, crate::setup::env_var);
    let status = match whoami.transpose() {
        Ok(whoami) => config.remote_status(whoami.map(|w| w.cleaned(&cleaner)).as_ref()),
        Err(error) => return Some(crate::ticket_signin::remote_failed_line(&cleaner.error(&error))),
    };
    Some(crate::ticket_signin::remote_line(&status))
}

fn exit_code(git_ok: bool, config_ok: bool) -> i32 {
    i32::from(!git_ok || !config_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotlight_warning_explains_folder_exclusion() {
        let repo = std::path::Path::new("/projects/app");
        let line = spotlight_line(repo, "/projects/app:\n Indexing enabled.").unwrap();
        assert!(line.contains("Spotlight") && line.contains("Search Privacy") && line.contains("/projects/app"));
        assert!(spotlight_line(repo, "Indexing disabled.").is_none());
        assert!(spotlight_line(repo, "Error: unknown indexing state").is_none());
    }

    #[test]
    fn identity_doctor_resolves_scopes_and_warns_on_workspace_global_fallback() {
        const CHILD: &str = "ATOMICS_IDENTITY_DOCTOR_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let global = dir.path().join("gitconfig");
            std::fs::write(&global, "[user]\nname = Global Example\nemail = global@example.com\n").unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "doctor::tests::identity_doctor_resolves_scopes_and_warns_on_workspace_global_fallback",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                    .env("GIT_CONFIG_GLOBAL", global)
                    .env("GIT_CONFIG_NOSYSTEM", "1"),
            );
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let repo = dir.path();
        atomics_core::git::git(repo, &["init", "-q"]).unwrap();
        let file = Workspace::file("identity-example").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut config = Config::default();
        let global = identity_lines(&config, None, repo).join("\n");
        assert!(global.contains("Global Example <global@example.com> (Git fallback)"), "{global}");
        std::fs::write(&file, "projects = []\n[identity]\nname = 'Workspace Example'\n").unwrap();
        let partial = identity_lines(&config, Some("identity-example"), repo).join("\n");
        assert!(partial.contains("Workspace Example <global@example.com>"), "{partial}");
        assert!(partial.contains("falls back to a global identity while the workspace defines one"), "{partial}");
        std::fs::write(
            &file,
            "projects = []\n[identity]\nname = 'Workspace Example'\nemail = 'workspace@example.com'\n",
        )
        .unwrap();
        let workspace = identity_lines(&config, Some("identity-example"), repo).join("\n");
        assert!(workspace.contains("workspace@example.com"));
        assert!(!workspace.contains("warning"));
        config.identity.projects.insert(
            repo.to_string_lossy().into_owned(),
            atomics_core::identity::Identity {
                name: Some("Project Example".into()),
                email: Some("project@example.com".into()),
                ..Default::default()
            },
        );
        assert!(
            identity_lines(&config, Some("identity-example"), repo)
                .join("\n")
                .contains("Project Example <project@example.com>")
        );
    }

    #[test]
    fn ticket_workspace_overrides_main_config_as_a_whole() {
        const CHILD: &str = "ATOMICS_TICKET_DOCTOR_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "doctor::tests::ticket_workspace_overrides_main_config_as_a_whole",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("ATOMICS_CONFIG", dir.path().join("config.toml"))
                    .env("ATOMICS_HOST", "fixture"),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        use atomics_core::{tickets::TicketConfig, workspace::Workspace};
        let main = Config {
            tickets: toml::from_str::<TicketConfig>(
                "system = 'main'\nserver = {command = ['main-mcp']}\n[operations.get]\ntool = 'main_get'",
            )
            .unwrap(),
            ..Config::default()
        };
        let file = Workspace::file("work").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        assert_eq!(ticket_config(&main, None).unwrap(), main.tickets);
        std::fs::write(&file, "projects = []").unwrap();
        assert_eq!(ticket_config(&main, Some("work")).unwrap(), main.tickets);
        std::fs::write(&file, "projects = []\n[tickets]\nsystem = 'team'\nserver = {command = ['team-mcp']}").unwrap();
        let team = ticket_config(&main, Some("work")).unwrap();
        assert_eq!(team.system, "team");
        assert!(team.operations.get.is_none(), "workspace does not inherit main operation maps");
        std::fs::write(&file, "projects = []\n[tickets]\nsystem = ''").unwrap();
        assert!(!ticket_config(&main, Some("work")).unwrap().is_configured());
        std::fs::write(&file, "projects = []\n[tickets]\nmode = 'invalid'").unwrap();
        assert!(ticket_config(&main, Some("work")).is_err());
        assert!(ticket_config(&main, Some("missing")).is_err());
    }

    #[test]
    fn ticket_check_requires_rendezvous_tools_only_when_enabled() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        let dir = crate::test_support::tempdir().unwrap();
        let state = fake_state(dir.path(), serde_json::json!([]));
        let mut config = example_profile("keyword-example", &state);
        config.operations.rendezvous_register =
            Some(atomics_core::tickets::Operation { tool: "missing_register".into(), ..Default::default() });
        assert!(ticket_check(&config, false).starts_with('✓'));
        let line = ticket_check(&config, true);
        assert!(line.starts_with('✗') && line.contains("rendezvous_register → missing_register"), "{line}");
    }

    #[test]
    fn ticket_check_notes_missing_optional_workspace_operations() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        let dir = crate::test_support::tempdir().unwrap();
        let state = fake_state(dir.path(), serde_json::json!([]));
        let mut config = example_profile("keyword-example", &state);
        for name in ["search", "semantic_search", "children", "related", "read"] {
            let operation = atomics_core::tickets::Operation { tool: format!("missing_{name}"), ..Default::default() };
            match name {
                "search" => config.operations.search = Some(operation),
                "semantic_search" => config.operations.semantic_search = Some(operation),
                "children" => config.operations.children = Some(operation),
                "related" => config.operations.related = Some(operation),
                _ => config.operations.read = Some(operation),
            }
            let line = ticket_check(&config, false);
            assert!(
                line.starts_with('✓') && line.contains("note:") && line.contains(&format!("{name} → missing_{name}")),
                "{line}"
            );
            assert!(!line.contains("warning"));
        }
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(state).unwrap()).unwrap();
        assert_eq!(value["calls"], serde_json::json!([]));
    }

    #[test]
    fn remote_doctor_errors_for_self_steering_and_human_connections_and_says_why_it_is_off() {
        use serde_json::{Value, json};
        let dir = crate::test_support::tempdir().unwrap();
        let state = crate::mcp_client::tests::fake_state(dir.path(), json!([]));
        let whoami = |identity: Value| {
            let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
            value["whoami"] = identity;
            std::fs::write(&state, value.to_string()).unwrap();
        };
        let config = crate::tickets::tests::conversation_profile("", &state);
        let line = remote_check(&config).unwrap();
        assert!(line.starts_with('✓') && line.contains("human-1"), "{line}");
        let mut own = config.clone();
        own.remote.owners = vec!["agent-1".into()];
        let line = remote_check(&own).unwrap();
        assert!(line.starts_with('✗') && line.contains("(error)") && line.contains("agent-1"), "{line}");
        let mut unmapped = config.clone();
        unmapped.operations.conversation_read.as_mut().unwrap().result.author_kind.clear();
        let line = remote_check(&unmapped).unwrap();
        assert!(line.starts_with('·') && line.contains("off") && line.contains("author_id and author_kind"), "{line}");
        whoami(json!({"userId":"agent-2", "kind":"agent", "displayName":"Product agent"}));
        let line = remote_check(&config).unwrap();
        assert!(
            line.starts_with('·') && line.contains("off") && line.contains("set [tickets.remote] owners"),
            "{line}"
        );
        let mut listed = config.clone();
        listed.remote.owners = vec!["human-9".into()];
        assert!(remote_check(&listed).unwrap().starts_with('✓'));
        whoami(json!({"userId":"human-1", "kind":"human", "displayName":"Example human"}));
        let line = remote_check(&listed).unwrap();
        assert!(line.starts_with('✗') && line.contains("(error)") && line.contains("connect as an agent"), "{line}");
        let mut off = config.clone();
        off.remote.enabled = false;
        assert!(remote_check(&off).is_none());
    }

    #[test]
    fn http_operations_are_reported_apart_from_the_tool_check() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        let dir = crate::test_support::tempdir().unwrap();
        let state = fake_state(dir.path(), serde_json::json!([]));
        let mut config = example_profile("meaning-example", &state);
        assert_eq!(http_operations_check(&config), None);
        // The tool check does not look for a tool behind an http operation.
        let call = atomics_core::tickets::HttpCall {
            method: atomics_core::tickets::HttpMethod::Get,
            path: "/items/{id}".into(),
            query: Vec::new(),
            response_headers: Default::default(),
        };
        let read = config.operations.read.as_mut().unwrap();
        read.tool.clear();
        read.http = Some(Box::new(call.clone()));
        let get = config.operations.get.as_mut().unwrap();
        get.tool.clear();
        get.http = Some(Box::new(call));
        let line = ticket_check(&config, false);
        assert!(line.starts_with('✓') && !line.contains("note:"), "{line}");

        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✗') && line.contains("read, get"), "{line}");
        assert!(line.contains("set [tickets.http] url"), "{line}");
        config.http.url = "http://tickets.example/v1".into();
        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✗') && line.contains("must be https"), "{line}");
        config.http.url = "https://me:password-9f@tickets.example/v1".into();
        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✗') && !line.contains("password-9f"), "{line}");
        config.http.url = "https://tickets.example/v1".into();
        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✓') && line.contains("at tickets.example:"), "{line}");
        assert!(!line.contains("token"), "{line}");

        // A token that stays behind with the MCP server is pointed out.
        config.server = atomics_core::tickets::McpServer {
            url: Some("https://mcp.example/mcp".into()),
            token_env: Some("TICKETS_TOKEN".into()),
            ..Default::default()
        };
        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✓') && line.contains("send no token"), "{line}");
        config.server.url = Some("https://tickets.example/mcp".into());
        let line = http_operations_check(&config).unwrap();
        assert!(line.starts_with('✓') && !line.contains("send no token"), "{line}");
    }

    #[test]
    fn ticket_check_never_prints_the_servers_credentials() {
        let mut config: atomics_core::tickets::TicketConfig = toml::from_str("system = \"echo\"").unwrap();
        config.server.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo \"bad key $API_KEY for https://me:pw@t.example\" >&2; exit 1".into(),
        ];
        config.server.env.insert("API_KEY".into(), "doctorkey-4e5f6a".into());
        let line = ticket_check(&config, false);
        assert!(line.starts_with('✗') && line.contains("bad key ***"), "{line}");
        assert!(!line.contains("doctorkey-4e5f6a") && !line.contains("pw@"), "{line}");
    }

    #[test]
    fn ticket_check_starts_profiles_lists_all_pages_and_reports_failures_without_writes() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        use serde_json::{Value, json};
        let dir = crate::test_support::tempdir().unwrap();
        let state = fake_state(dir.path(), json!([]));
        for name in ["meaning-example", "keyword-example"] {
            let mut config = example_profile(name, &state);
            let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
            value["page_size"] = json!(2);
            std::fs::write(&state, value.to_string()).unwrap();
            assert!(ticket_check(&config, false).starts_with('✓'));
            if name == "meaning-example" {
                assert!(config.escape_mentions);
                assert_eq!(config.operation("whoami").unwrap().tool, "whoami");
            }
            config.operations.get.as_mut().unwrap().tool = "unknown_get".into();
            let failed = ticket_check(&config, false);
            assert!(failed.starts_with('✗') && failed.contains("get → unknown_get"), "{failed}");
            config.server.command = vec![dir.path().join("absent-server").display().to_string()];
            let failed = ticket_check(&config, false);
            assert!(failed.starts_with('✗') && failed.contains("starting"), "{failed}");
        }
        let mut config = example_profile("meaning-example", &state);
        config.timeout_secs = 1;
        let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        value["list_hang"] = json!(true);
        std::fs::write(&state, value.to_string()).unwrap();
        let failed = ticket_check(&config, false);
        assert!(failed.starts_with('✗') && failed.contains("did not answer within 1 s"), "{failed}");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(value["calls"], json!([]), "doctor must never call mapped operations");
    }

    #[test]
    fn doctor_reports_new_checks_without_changing_warning_exit_codes() {
        const CHILD: &str = "ATOMICS_DOCTOR_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(run(None), 0);
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        let config = dir.path().join("config.toml");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir_all(state.join("plans")).unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        atomics_core::git::git(&repo, &["init", "-b", "main"]).unwrap();
        atomics_core::git::git(
            &repo,
            &["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "base"],
        )
        .unwrap();
        std::fs::write(repo.join("wip.txt"), "work in progress").unwrap();
        atomics_core::git::git(&repo, &["add", "wip.txt"]).unwrap();
        atomics_core::git::git(
            &repo,
            &["-c", "user.name=test", "-c", "user.email=test@example.com", "stash", "push", "-q", "-m", "ENG-123"],
        )
        .unwrap();
        let merged = repo.with_file_name("merged");
        let shell = bin.join("shell");
        crate::test_support::write_program(
            &shell,
            format!("#!/bin/sh\nprintf '__ATOMICS__{}:/usr/bin:/bin__ATOMICS__'\n", bin.display()),
        );
        let codex = bin.join("codex");
        for (probe, allow, bad_plan) in [
            ("printf 'codex-cli 0.165.0\\n'", "rg", false),
            ("printf 'codex-cli 0.164.0\\n'", "cargo test", true),
            ("exit 1", "cargo test", true),
            ("missing", "cargo test", true),
        ] {
            if probe == "missing" {
                std::fs::remove_file(&codex).unwrap();
            } else {
                crate::test_support::write_program(&codex, format!("#!/bin/sh\n{probe}\n"));
            }
            std::fs::write(&config, format!("base_ref = \"main\"\n[approvals]\nallow = [\"{allow}\"]\n")).unwrap();
            std::fs::write(state.join("plans/conversation.remote.json"), "{}").unwrap();
            if bad_plan {
                std::fs::write(state.join("plans/bad.json"), "bad JSON").unwrap();
                if !merged.exists() {
                    atomics_core::git::add_worktree(&repo, &merged, "merged", "main").unwrap();
                }
            }
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "doctor::tests::doctor_reports_new_checks_without_changing_warning_exit_codes",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("SHELL", &shell)
                    .env("ATOMICS_HOME", &state)
                    .env("ATOMICS_CONFIG", &config)
                    .env("ATOMICS_SOCKET", dir.path().join("absent.sock"))
                    .current_dir(&repo),
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
            let prefix = if bad_plan { '✗' } else { '✓' };
            // The default editor, hx, is on neither this PATH's bin nor (on a test machine) /usr/bin or /bin.
            if !["/usr/bin/hx", "/usr/bin/helix", "/bin/hx", "/bin/helix"].iter().any(|p| Path::new(p).exists()) {
                assert!(
                    stdout.contains(
                        "✗ hx not found on login-shell PATH (warning): the editor panel cannot start it ([editor] command)"
                    ),
                    "{stdout}"
                );
            }
            if probe == "missing" {
                assert!(stdout.contains("codex not found on login-shell PATH"), "{stdout}");
                assert!(!stdout.contains("Codex version") && !stdout.contains("Codex supports"), "{stdout}");
            } else if probe == "exit 1" {
                assert!(stdout.contains("✗ could not determine the Codex version"), "{stdout}");
                assert!(!stdout.contains("install Codex"), "{stdout}");
            } else {
                assert!(stdout.contains(&format!("{prefix} Codex supports ATOMICS hooks")), "{stdout}");
            }
            assert!(stdout.contains(&format!("{prefix} [approvals] allow list")), "{stdout}");
            // Codex agents run sandboxed by default, where ATOMICS passes no hooks; an older Codex gets none.
            let trust = !matches!(probe, "missing" | "printf 'codex-cli 0.164.0\\n'");
            assert_eq!(stdout.contains("✓ Codex hook trust not needed: Codex agents run sandboxed"), trust, "{stdout}");
            assert!(!stdout.contains("Codex does not run every ATOMICS hook"), "{stdout}");
            assert!(stdout.contains(if bad_plan { "unreadable plan" } else { "plan files readable" }));
            let worktree_lines: Vec<_> = stdout.lines().filter(|line| line.contains("merged worktree")).collect();
            let expected = if bad_plan {
                format!(
                    "✗ merged worktree can be cleaned up (warning): {} (merged) — review it in ATOMICS's Clean up… dialog before removing the worktree",
                    merged.display()
                )
            } else {
                format!("✓ no merged worktrees to clean up ({})", repo.display())
            };
            assert_eq!(worktree_lines, [expected], "{stdout}");
            let stash = format!(
                "Stash of {}, shared by its worktrees: 1 entry\n  stash@{{0}}: On main: ENG-123\n",
                repo.display()
            );
            assert!(stdout.contains(&stash), "{stdout}");
        }

        // Bare Codex agents get the hooks, and their trust lives in this instance's own Codex home.
        crate::test_support::write_program(&codex, "#!/bin/sh\nprintf 'codex-cli 0.165.0\\n'\n");
        std::fs::write(&config, "base_ref = \"main\"\n[isolation]\nmode = \"bare\"\n").unwrap();
        let codex_home = state.join("codex");
        let doctor = || {
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "doctor::tests::doctor_reports_new_checks_without_changing_warning_exit_codes",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env("SHELL", &shell)
                    .env("ATOMICS_HOME", &state)
                    .env("ATOMICS_CONFIG", &config)
                    .env("ATOMICS_SOCKET", dir.path().join("absent.sock"))
                    .env("CODEX_HOME", dir.path().join("inherited-codex"))
                    .current_dir(&repo),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            String::from_utf8(output.stdout).unwrap()
        };
        let stdout = doctor();
        let file = codex_home.join("config.toml").display().to_string();
        let line = stdout.lines().find(|line| line.contains("Codex does not run every ATOMICS hook")).unwrap();
        assert!(line.starts_with("✗") && line.contains("(warning): untrusted: ") && line.contains(&file), "{line}");
        let hooks = atomics_core::hook::codex_hook_trust(
            &crate::setup::hook_executable().unwrap(),
            &dir.path().join("absent.sock"),
        );
        assert_eq!(hooks.len(), 8);
        for hook in &hooks {
            assert!(
                line.contains(&format!("[hooks.state.\"{}\"] trusted_hash = \"{}\"", hook.key, hook.hash)),
                "{line}"
            );
        }
        std::fs::create_dir_all(&codex_home).unwrap();
        let trusted: String = hooks
            .iter()
            .map(|hook| format!("[hooks.state.\"{}\"]\ntrusted_hash = \"{}\"\n", hook.key, hook.hash))
            .collect();
        std::fs::write(codex_home.join("config.toml"), &trusted).unwrap();
        let stdout = doctor();
        let own = crate::setup::hook_executable().unwrap().display().to_string();
        assert!(
            stdout.contains(&format!(
                "✓ Codex trusts ATOMICS's hooks for all 8 events, as {own} (this doctor; no desktop or host has run \
                 here since recording its own) passes them ({file})"
            )),
            "{stdout}"
        );
        // This doctor is the install checked, not another (an app in /Applications may be).
        assert!(!stdout.contains(&format!("Other ATOMICS install {own}")), "{stdout}");
        assert!(!dir.path().join("inherited-codex").exists());

        // Two installs: the app, which serves this home and so passes the hooks Codex agents get, and
        // the doctor run from another (as a cargo-installed CLI in a terminal would be).
        let app = dir.path().join("Applications/ATOMICS.app/Contents/MacOS/atomics");
        std::fs::create_dir_all(app.parent().unwrap()).unwrap();
        crate::test_support::write_program(&app, "#!/bin/sh\nexit 0\n");
        std::fs::write(state.join("executable.json"), serde_json::to_vec(&app.to_string_lossy()).unwrap()).unwrap();
        assert_eq!(crate::setup::recorded_executable(&state), Some(app.clone()));
        let app_hooks = atomics_core::hook::codex_hook_trust(&app, &dir.path().join("absent.sock"));
        let entry = |hook: &atomics_core::hook::CodexHookTrust| format!("trusted_hash = \"{}\"", hook.hash);
        let checked = format!("{} (the desktop or host that last ran here)", app.display());
        // Codex trusts this doctor's hooks, not the app's: the check is the app's, and says so.
        let stdout = doctor();
        let line = stdout.lines().find(|line| line.contains("Codex does not run every ATOMICS hook")).unwrap();
        assert!(line.starts_with(&format!("✗ Codex does not run every ATOMICS hook as {checked} passes them")));
        assert!(line.contains("changed since trusted: "), "{line}");
        assert!(app_hooks.iter().all(|hook| line.contains(&entry(hook))), "{line}");
        assert!(hooks.iter().all(|hook| !line.contains(&entry(hook))), "{line}");
        assert!(
            stdout.contains(&format!(
                "Other ATOMICS install {own} (this doctor): Codex trusts its hooks, rather than those of the install \
                 checked above"
            )),
            "{stdout}"
        );
        // Trusted for the app, as its review writes it: a doctor from the other install agrees, and
        // suggests nothing that would trust its own hooks instead.
        let trusted: String = app_hooks
            .iter()
            .map(|hook| format!("[hooks.state.\"{}\"]\ntrusted_hash = \"{}\"\n", hook.key, hook.hash))
            .collect();
        std::fs::write(codex_home.join("config.toml"), trusted).unwrap();
        let stdout = doctor();
        assert!(
            stdout.contains(&format!(
                "✓ Codex trusts ATOMICS's hooks for all 8 events, as {checked} passes them ({file})"
            )),
            "{stdout}"
        );
        assert!(!stdout.contains("changed since trusted"), "{stdout}");
        assert!(hooks.iter().all(|hook| !stdout.contains(&entry(hook))), "{stdout}");
        assert!(
            stdout.contains(&format!("Other ATOMICS install {own} (this doctor): its hooks are not trusted.")),
            "{stdout}"
        );
        // The app found where it is installed is the recorded one, listed once.
        assert_eq!(stdout.matches(&*app.display().to_string()).count(), 1, "{stdout}");
    }

    #[test]
    fn json_lists_checks_with_hints_and_never_quotes_config_lines_or_secrets() {
        const CHILD: &str = "ATOMICS_DOCTOR_JSON_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let config = paths::config_file();
            let host = Config::source_paths(&config)[1].clone();
            std::fs::create_dir_all(host.parent().unwrap()).unwrap();
            std::fs::write(&config, "[voice]\napi_key = 'sk-voice-doctor-secret'\n").unwrap();
            std::fs::write(&host, "[layout]\nsidebar_width = pw-doctor-secret\n").unwrap();
            let broken = json(None, &[]);
            let text = broken.to_string();
            assert!(!text.contains("pw-doctor-secret") && !text.contains("sk-voice-doctor-secret"), "{text}");
            assert_eq!(broken["ok"], false);
            let config_check = broken["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["check"].as_str().unwrap().starts_with("config failed:"))
                .unwrap_or_else(|| panic!("{broken}"));
            assert!(config_check["check"].as_str().unwrap().contains("fixture.toml: line 2, column"), "{config_check}");
            assert_eq!((config_check["ok"].clone(), config_check["warning"].clone()), (json!(false), json!(false)));
            assert!(config_check["hint"].as_str().unwrap().contains("fix this config"));
            // The terminal's doctor still shows the user the failing line.
            assert!(report(None, false, &[]).lines.iter().any(|line| line.contains("pw-doctor-secret")));

            std::fs::write(&host, "[layout]\nsidebar_width = 300.0\n").unwrap();
            let healthy = json(None, &[]);
            let text = healthy.to_string();
            assert!(!text.contains("sk-voice-doctor-secret"), "{text}");
            assert!(!text.contains("doctor-password"), "{text}");
            assert_eq!(healthy["ok"], true, "{healthy}");
            assert!(healthy["facts"].as_array().unwrap().contains(&json!("Host: fixture")), "{healthy}");
            assert!(
                healthy["facts"].as_array().unwrap().iter().any(|fact| {
                    let fact = fact.as_str().unwrap();
                    fact.starts_with("Loaded config:") && fact.contains("https://***@doctor.example/config.toml")
                }),
                "{healthy}"
            );
            let checks = healthy["checks"].as_array().unwrap();
            assert!(checks.iter().any(|c| c["ok"] == true && c["check"].as_str().unwrap().starts_with("git found")));
            let socket =
                checks.iter().find(|c| c["check"].as_str().unwrap().starts_with("ATOMICS not running")).unwrap();
            assert_eq!((socket["ok"].clone(), socket["warning"].clone()), (json!(false), json!(true)));
            let hooks = checks.iter().find(|c| c["check"].as_str().unwrap().contains("global Claude hooks")).unwrap();
            assert_eq!((hooks["ok"].clone(), hooks["warning"].clone()), (json!(false), json!(true)));
            assert!(hooks["check"].as_str().unwrap().starts_with("global Claude hooks not installed"), "{hooks}");
            assert!(hooks["hint"].as_str().unwrap().contains("atomics install-hooks"), "{hooks}");
            // Every failed check is a warning except the two required ones, as the answer says.
            assert!(checks.iter().all(|c| c["ok"] == true || c["warning"] == true), "{healthy}");
            assert_eq!(socket["hint"], "start ATOMICS or check ATOMICS_SOCKET / ATOMICS_HOME");
            let codex = checks.iter().find(|c| c["check"].as_str().unwrap().starts_with("codex not found")).unwrap();
            assert_eq!(codex["warning"], true);
            crate::lifecycle::save_leftovers(
                &paths::atomics_home(),
                &["Warm worktree /repo/dirty: uncommitted changes".into()],
            )
            .unwrap();
            let with_leftovers = json(None, &[]);
            assert!(with_leftovers["checks"].as_array().unwrap().iter().any(|check| check["warning"] == true
                && check["check"].as_str().unwrap().contains("cleanup leftover")
                && check["hint"].as_str().unwrap().contains("Clean up…")));
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let shell = bin.join("shell");
        crate::test_support::write_program(
            &shell,
            format!("#!/bin/sh\nprintf '__ATOMICS__{}:/usr/bin:/bin__ATOMICS__'\n", bin.display()),
        );
        let output = crate::test_support::output(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "doctor::tests::json_lists_checks_with_hints_and_never_quotes_config_lines_or_secrets",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("HOME", dir.path())
                .env("SHELL", &shell)
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env(
                    "ATOMICS_CONFIG",
                    dir.path().join("https://doctor-user:doctor-password@doctor.example/config.toml"),
                )
                .env("ATOMICS_HOST", "fixture")
                .env("ATOMICS_SOCKET", dir.path().join("absent.sock"))
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
    fn successful_check_omits_hint() {
        assert_eq!(
            check_line(true, "git found (git version 2.50.0)", "install git"),
            "✓ git found (git version 2.50.0)"
        );
    }

    #[test]
    fn failed_check_includes_hint() {
        assert_eq!(
            check_line(false, "claude not found (warning)", "install claude"),
            "✗ claude not found (warning) — install claude"
        );
    }

    #[test]
    fn multiline_errors_stay_on_one_line() {
        assert_eq!(
            check_line(false, "config failed:\ninvalid TOML\r\nat line 2", "fix\nconfig"),
            "✗ config failed: invalid TOML at line 2 — fix config"
        );
    }

    #[test]
    fn only_git_and_config_are_required() {
        assert_eq!(exit_code(true, true), 0);
        assert_eq!(exit_code(false, true), 1);
        assert_eq!(exit_code(true, false), 1);
        assert_eq!(exit_code(false, false), 1);
    }
}

#[cfg(test)]
mod editor_tests {
    use crate::setup::editor_on_path;
    use std::path::PathBuf;

    #[test]
    fn helix_fallback_respects_the_configured_program_and_prefers_hx() {
        let helix = |name: &str| (name == "helix").then(|| PathBuf::from("/bin/helix"));
        assert_eq!(editor_on_path("hx", helix), Some(PathBuf::from("/bin/helix")));
        assert_eq!(editor_on_path("nvim", helix), None);
        assert_eq!(editor_on_path("/custom/hx", helix), None);
        assert_eq!(editor_on_path("hx", |_| None), None);
        assert_eq!(editor_on_path("hx", |name| Some(PathBuf::from(name))), Some(PathBuf::from("hx")));
        assert_eq!(editor_on_path("nvim", |name| Some(PathBuf::from(name))), Some(PathBuf::from("nvim")));
    }
}
