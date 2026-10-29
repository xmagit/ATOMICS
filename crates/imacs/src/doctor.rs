use std::os::unix::net::UnixStream;
use std::process::Command;

use imacs_core::{
    config::Config,
    doctor::{approvals_check, cache_clone_check, check_line, codex_checks, plans_check, worktree_checks},
    paths,
};

use crate::setup::{
    codex_supports_hooks, find_program, global_hooks_installed, legacy_hooks_installed, resolve_program,
};

pub fn run(workspace: Option<&str>) -> i32 {
    let git_version = Command::new(resolve_program("git"))
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|version| !version.is_empty());
    let git_ok = git_version.is_some();
    let git = git_version.map(|version| format!("git found ({version})")).unwrap_or_else(|| "git not found".into());
    println!("{}", check_line(git_ok, &git, "install git and add it to your login-shell PATH"));

    for program in ["claude", "codex", "hx"] {
        let found = find_program(program);
        let label = match &found {
            Some(path) => format!("{program} found on login-shell PATH ({})", path.display()),
            None => format!("{program} not found on login-shell PATH (warning)"),
        };
        println!("{}", check_line(found.is_some(), &label, "install it or add it to your login-shell PATH"));
        if program == "codex"
            && let Some(path) = found
        {
            for line in codex_checks(codex_supports_hooks(&path.to_string_lossy(), std::time::Duration::from_secs(2))) {
                println!("{line}");
            }
        }
    }

    let config_path = paths::config_file();
    println!("Host: {}", paths::host_name());
    let config = Config::load_with_sources(&config_path);
    let label = match &config {
        Ok(_) => format!("config parses ({})", config_path.display()),
        Err(err) => format!("config failed: {err:#}"),
    };
    println!(
        "{}",
        check_line(config.is_ok(), &label, "create or fix this config; start IMACS to create a missing file")
    );
    if let Ok((_, sources)) = &config {
        for source in sources {
            println!("Loaded config: {}", source.display());
        }
    }

    for line in plans_check(&paths::imacs_home()) {
        println!("{line}");
    }
    if let Ok((config, _)) = &config {
        println!("{}", approvals_check(&config.approvals));
        match ticket_config(config, workspace) {
            Ok(tickets) if !tickets.system.trim().is_empty() => println!("{}", ticket_check(&tickets)),
            Ok(_) => {}
            Err(error) => println!(
                "{}",
                check_line(false, &format!("ticket config failed: {error}"), "fix the workspace [tickets] section")
            ),
        }
        let mut repos: Vec<_> = config.repos.iter().map(|repo| paths::expand(repo)).collect();
        if repos.is_empty()
            && let Ok(cwd) = std::env::current_dir()
        {
            repos.push(cwd);
        }
        repos.sort();
        repos.dedup();
        for repo in repos {
            println!("{}", cache_clone_check(&repo, &config.worktree_path));
            for line in worktree_checks(&repo, &config.base_ref) {
                println!("{line}");
            }
        }
    }

    let socket_path = paths::socket_path();
    let running = UnixStream::connect(&socket_path).is_ok();
    let label = format!("IMACS running ({})", socket_path.display());
    println!("{}", check_line(running, &label, "start IMACS or check IMACS_SOCKET / IMACS_HOME"));

    println!("{}", check_line(global_hooks_installed(), "global Claude hooks installed", "run `imacs install-hooks`"));
    if legacy_hooks_installed() {
        println!(
            "{}",
            check_line(false, "leftover cox hook handlers (warning)", "run `imacs install-hooks` to remove them")
        );
    }

    exit_code(git_ok, config.is_ok())
}

fn ticket_config(config: &Config, workspace: Option<&str>) -> anyhow::Result<imacs_core::tickets::TicketConfig> {
    use imacs_core::workspace::Workspace;
    let Some(name) = workspace else { return Ok(config.tickets.clone()) };
    let tickets = Workspace::load(name)?.tickets()?;
    Ok(tickets.unwrap_or_else(|| config.tickets.clone()))
}

fn ticket_check(config: &imacs_core::tickets::TicketConfig) -> String {
    let result = (|| {
        let mut client = crate::mcp_client::McpClient::connect(
            &config.server,
            std::time::Duration::from_secs(config.timeout_secs.max(1)),
            crate::setup::env_var,
        )
        .map_err(|e| e.to_string())?;
        let tools = client.tool_names().map_err(|e| e.to_string())?;
        let missing: Vec<_> = ["get", "list", "update_state", "comment", "create"]
            .into_iter()
            .filter_map(|name| {
                config.operation(name).filter(|op| !tools.contains(&op.tool)).map(|op| format!("{name} → {}", op.tool))
            })
            .collect();
        if missing.is_empty() { Ok(()) } else { Err(format!("missing mapped tools: {}", missing.join(", "))) }
    })();
    // Doctor output is shared in bug reports: the server's words go through the same
    // cleaner as everything else IMACS keeps or shows from a ticket server.
    let cleaner = imacs_core::tickets::Cleaner::of(config, crate::setup::env_var);
    let label = match &result {
        Ok(()) => format!("{} ticket MCP server starts and offers all mapped tools", config.system),
        Err(error) => format!("{} ticket MCP check failed (warning): {}", config.system, cleaner.error(error)),
    };
    check_line(
        result.is_ok(),
        &label,
        "check [tickets].server and operations; authenticate a stdio bridge in a terminal first",
    )
}

fn exit_code(git_ok: bool, config_ok: bool) -> i32 {
    i32::from(!git_ok || !config_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_workspace_overrides_main_config_as_a_whole() {
        const CHILD: &str = "IMACS_TICKET_DOCTOR_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = crate::test_support::output(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "doctor::tests::ticket_workspace_overrides_main_config_as_a_whole",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("IMACS_CONFIG", dir.path().join("config.toml"))
                    .env("IMACS_HOST", "fixture"),
            );
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
            return;
        }
        use imacs_core::{tickets::TicketConfig, workspace::Workspace};
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
    fn ticket_check_never_prints_the_servers_credentials() {
        let mut config: imacs_core::tickets::TicketConfig = toml::from_str("system = \"echo\"").unwrap();
        config.server.command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo \"bad key $API_KEY for https://me:pw@t.example\" >&2; exit 1".into(),
        ];
        config.server.env.insert("API_KEY".into(), "doctorkey-4e5f6a".into());
        let line = ticket_check(&config);
        assert!(line.starts_with('✗') && line.contains("bad key ***"), "{line}");
        assert!(!line.contains("doctorkey-4e5f6a") && !line.contains("pw@"), "{line}");
    }

    #[test]
    fn ticket_check_starts_profiles_lists_all_pages_and_reports_failures_without_writes() {
        use crate::mcp_client::tests::{example_profile, fake_state};
        use serde_json::{Value, json};
        let dir = tempfile::tempdir().unwrap();
        let state = fake_state(dir.path(), json!([]));
        for name in ["RELAY", "LINEAR"] {
            let mut config = example_profile(name, &state);
            let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
            value["page_size"] = json!(2);
            std::fs::write(&state, value.to_string()).unwrap();
            assert!(ticket_check(&config).starts_with('✓'));
            config.operations.get.as_mut().unwrap().tool = "unknown_get".into();
            let failed = ticket_check(&config);
            assert!(failed.starts_with('✗') && failed.contains("get → unknown_get"), "{failed}");
            config.server.command = vec![dir.path().join("absent-server").display().to_string()];
            let failed = ticket_check(&config);
            assert!(failed.starts_with('✗') && failed.contains("starting"), "{failed}");
        }
        let mut config = example_profile("RELAY", &state);
        config.timeout_secs = 1;
        let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        value["list_hang"] = json!(true);
        std::fs::write(&state, value.to_string()).unwrap();
        let failed = ticket_check(&config);
        assert!(failed.starts_with('✗') && failed.contains("did not answer within 1 s"), "{failed}");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(value["calls"], json!([]), "doctor must never call mapped operations");
    }

    #[test]
    fn doctor_reports_new_checks_without_changing_warning_exit_codes() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD: &str = "IMACS_DOCTOR_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(run(None), 0);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        let config = dir.path().join("config.toml");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir_all(state.join("plans")).unwrap();
        let repo = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&repo).unwrap();
        imacs_core::git::git(&repo, &["init", "-b", "main"]).unwrap();
        imacs_core::git::git(
            &repo,
            &["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "base"],
        )
        .unwrap();
        let merged = repo.with_file_name("merged");
        let shell = bin.join("shell");
        std::fs::write(&shell, format!("#!/bin/sh\nprintf '__IMACS__{}:/usr/bin:/bin__IMACS__'\n", bin.display()))
            .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
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
                std::fs::write(&codex, format!("#!/bin/sh\n{probe}\n")).unwrap();
                std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            std::fs::write(&config, format!("base_ref = \"main\"\n[approvals]\nallow = [\"{allow}\"]\n")).unwrap();
            if bad_plan {
                std::fs::write(state.join("plans/bad.json"), "bad JSON").unwrap();
                if !merged.exists() {
                    imacs_core::git::add_worktree(&repo, &merged, "merged", "main").unwrap();
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
                    .env("IMACS_HOME", &state)
                    .env("IMACS_CONFIG", &config)
                    .env("IMACS_SOCKET", dir.path().join("absent.sock"))
                    .current_dir(&repo),
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
            let prefix = if bad_plan { '✗' } else { '✓' };
            if probe == "missing" {
                assert!(stdout.contains("codex not found on login-shell PATH"), "{stdout}");
                assert!(!stdout.contains("Codex version") && !stdout.contains("Codex supports"), "{stdout}");
            } else if probe == "exit 1" {
                assert!(stdout.contains("✗ could not determine the Codex version"), "{stdout}");
                assert!(!stdout.contains("install Codex"), "{stdout}");
            } else {
                assert!(stdout.contains(&format!("{prefix} Codex supports IMACS hooks")), "{stdout}");
            }
            assert!(stdout.contains(&format!("{prefix} [approvals] allow list")), "{stdout}");
            assert_eq!(stdout.contains("IMACS Codex hook trust not verified"), probe != "missing", "{stdout}");
            assert_eq!(stdout.contains("/hooks"), probe != "missing", "{stdout}");
            assert!(stdout.contains(if bad_plan { "unreadable plan" } else { "plan files readable" }));
            let worktree_lines: Vec<_> = stdout.lines().filter(|line| line.contains("merged worktree")).collect();
            let expected = if bad_plan {
                format!(
                    "✗ merged worktree can be cleaned up (warning): {} (merged) — review it in IMACS's Clean up… dialog before removing the worktree",
                    merged.display()
                )
            } else {
                format!("✓ no merged worktrees to clean up ({})", repo.display())
            };
            assert_eq!(worktree_lines, [expected], "{stdout}");
        }
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
