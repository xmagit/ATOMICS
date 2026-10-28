use std::os::unix::net::UnixStream;
use std::process::Command;

use imacs_core::{
    config::Config,
    doctor::{approvals_check, check_line, codex_checks, plans_check, worktree_checks},
    paths,
};

use crate::setup::{
    codex_supports_hooks, find_program, global_hooks_installed, legacy_hooks_installed, resolve_program,
};

pub fn run() -> i32 {
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
    }

    for line in codex_checks(
        codex_supports_hooks(&resolve_program("codex"), std::time::Duration::from_secs(2)).unwrap_or(false),
    ) {
        println!("{line}");
    }

    let config_path = paths::config_file();
    let config = Config::load(&config_path);
    let label = match &config {
        Ok(_) => format!("config parses ({})", config_path.display()),
        Err(err) => format!("config failed: {err:#}"),
    };
    println!(
        "{}",
        check_line(config.is_ok(), &label, "create or fix this config; start IMACS to create a missing file")
    );

    for line in plans_check(&paths::imacs_home()) {
        println!("{line}");
    }
    if let Ok(config) = &config {
        println!("{}", approvals_check(&config.approvals));
        let mut repos: Vec<_> = config.repos.iter().map(|repo| paths::expand(repo)).collect();
        if repos.is_empty()
            && let Ok(cwd) = std::env::current_dir()
        {
            repos.push(cwd);
        }
        repos.sort();
        repos.dedup();
        for repo in repos {
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

fn exit_code(git_ok: bool, config_ok: bool) -> i32 {
    i32::from(!git_ok || !config_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reports_new_checks_without_changing_warning_exit_codes() {
        use std::os::unix::fs::PermissionsExt;

        const CHILD: &str = "IMACS_DOCTOR_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(run(), 0);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        let state = dir.path().join("state");
        let config = dir.path().join("config.toml");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir_all(state.join("plans")).unwrap();
        let shell = bin.join("shell");
        std::fs::write(&shell, format!("#!/bin/sh\nprintf '__IMACS__{}:/usr/bin:/bin__IMACS__'\n", bin.display()))
            .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let codex = bin.join("codex");
        for (version, allow, bad_plan) in [("0.165.0", "rg", false), ("0.164.0", "cargo test", true)] {
            std::fs::write(&codex, format!("#!/bin/sh\nprintf 'codex-cli {version}\\n'\n")).unwrap();
            std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(&config, format!("[approvals]\nallow = [\"{allow}\"]\n")).unwrap();
            if bad_plan {
                std::fs::write(state.join("plans/bad.json"), "bad JSON").unwrap();
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
                    .env("IMACS_SOCKET", dir.path().join("absent.sock")),
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
            let prefix = if bad_plan { '✗' } else { '✓' };
            assert!(stdout.contains(&format!("{prefix} Codex supports IMACS hooks")), "{stdout}");
            assert!(stdout.contains(&format!("{prefix} [approvals] allow list")), "{stdout}");
            assert!(stdout.contains("IMACS Codex hook trust not verified"));
            assert!(stdout.contains("/hooks"));
            assert!(stdout.contains(if bad_plan { "unreadable plan" } else { "plan files readable" }));
            assert!(stdout.contains("worktree"));
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
