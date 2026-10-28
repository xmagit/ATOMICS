use std::path::{Path, PathBuf};

/// Root for runtime state: socket, hook settings, session store.
/// `COX_HOME` overrides it so several instances (or tests) can run side by side.
pub fn cox_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("COX_HOME") {
        return PathBuf::from(dir);
    }
    home().join(".cox")
}

pub fn config_file() -> PathBuf {
    if let Some(file) = std::env::var_os("COX_CONFIG") {
        return PathBuf::from(file);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    base.join("cox").join("config.toml")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("COX_SOCKET") {
        return PathBuf::from(path);
    }
    cox_home().join("cox.sock")
}

pub fn hook_settings_file() -> PathBuf {
    cox_home().join("claude-hooks.json")
}

pub fn plan_file() -> PathBuf {
    cox_home().join("plan.json")
}

pub fn store_file() -> PathBuf {
    cox_home().join("sessions.json")
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Expands a leading `~/` so config values can use it.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// Shortens a path under the home directory to `~/…` for display.
pub fn display(path: &Path) -> String {
    match path.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
