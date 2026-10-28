use std::os::unix::net::UnixStream;
use std::process::Command;

use imacs_core::{config::Config, paths};

use crate::setup::{find_program, global_hooks_installed, resolve_program};

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

    let socket_path = paths::socket_path();
    let running = UnixStream::connect(&socket_path).is_ok();
    let label = format!("IMACS running ({})", socket_path.display());
    println!("{}", check_line(running, &label, "start IMACS or check IMACS_SOCKET / IMACS_HOME"));

    println!("{}", check_line(global_hooks_installed(), "global Claude hooks installed", "run `imacs install-hooks`"));

    exit_code(git_ok, config.is_ok())
}

fn check_line(passed: bool, label: &str, hint: &str) -> String {
    let line = if passed { format!("✓ {label}") } else { format!("✗ {label} — {hint}") };
    line.lines().collect::<Vec<_>>().join(" ")
}

fn exit_code(git_ok: bool, config_ok: bool) -> i32 {
    i32::from(!git_ok || !config_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

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
