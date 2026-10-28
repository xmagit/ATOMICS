use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use cox_core::paths;

/// The PATH of an interactive login shell. Apps started from the Dock get a bare
/// PATH, which would hide `claude`, `codex` and everything installed by Homebrew.
pub static SHELL_PATH: LazyLock<Option<String>> = LazyLock::new(|| login_shell_var("PATH"));

/// A variable as an interactive login shell sets it. `name` must be a plain identifier.
fn login_shell_var(name: &str) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", &format!("printf '__COX__%s__COX__' \"${name}\"")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find("__COX__")? + 7;
    let end = text[start..].find("__COX__")? + start;
    Some(text[start..end].to_string())
}

/// An environment variable of cox, or of the login shell when cox lacks it (as apps
/// started from the Dock do). Blocks while the shell starts, so call it off the UI thread.
pub fn env_var(name: &str) -> Option<String> {
    static FOUND: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);
    if let Some(value) = std::env::var(name).ok().filter(|v| !v.trim().is_empty()) {
        return Some(value);
    }
    let identifier = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !identifier {
        return None;
    }
    if let Some(value) = FOUND.lock().unwrap().get(name) {
        return Some(value.clone());
    }
    let value = login_shell_var(name).filter(|v| !v.trim().is_empty())?;
    FOUND.lock().unwrap().insert(name.to_string(), value.clone());
    Some(value)
}

pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = SHELL_PATH.clone().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

pub fn resolve_program(program: &str) -> String {
    if program.contains('/') {
        return program.to_string();
    }
    find_program(program).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| program.to_string())
}

fn claude_settings_path() -> PathBuf {
    paths::home().join(".claude").join("settings.json")
}

pub fn global_hooks_installed() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    std::fs::read_to_string(claude_settings_path()).is_ok_and(|text| text.contains(&format!("{} hook", exe.display())))
}

pub fn install_global_hooks() -> anyhow::Result<PathBuf> {
    let path = claude_settings_path();
    let exe = std::env::current_exe()?;
    let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => {
            std::fs::write(path.with_extension("json.cox-backup"), &text)?;
            serde_json::from_str(&text)?
        }
        Err(_) => serde_json::json!({}),
    };
    cox_core::hook::merge_into_settings(&mut settings, &exe);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_reads_cox_first_and_passes_only_plain_names_to_the_shell() {
        assert_eq!(env_var("HOME"), std::env::var("HOME").ok());
        assert!(env_var("COX_UNSET; exit 1").is_none());
        assert!(env_var("1ABC").is_none());
    }
}
