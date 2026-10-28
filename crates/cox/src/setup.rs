use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::LazyLock;

use cox_core::paths;

/// The PATH of an interactive login shell. Apps started from the Dock get a bare
/// PATH, which would hide `claude`, `codex` and everything installed by Homebrew.
pub static SHELL_PATH: LazyLock<Option<String>> = LazyLock::new(|| {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let out = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", "printf '__COX__%s__COX__' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find("__COX__")? + 7;
    let end = text[start..].find("__COX__")? + start;
    Some(text[start..end].to_string())
});

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
