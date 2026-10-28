use std::path::{Path, PathBuf};

/// Root for runtime state: socket, hook settings, session store.
/// `IMACS_HOME` overrides it so several instances (or tests) can run side by side.
pub fn imacs_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("IMACS_HOME") {
        return PathBuf::from(dir);
    }
    home().join(".imacs")
}

pub fn config_file() -> PathBuf {
    if let Some(file) = std::env::var_os("IMACS_CONFIG") {
        return PathBuf::from(file);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"));
    base.join("imacs").join("config.toml")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("IMACS_SOCKET") {
        return PathBuf::from(path);
    }
    imacs_home().join("imacs.sock")
}

/// Moves state and config from where they lived before the rename to IMACS
/// (`~/.cox`, `~/.config/cox`), once, when the new places do not exist yet.
/// Returns what was moved.
pub fn migrate_from_cox() -> Vec<String> {
    let config_dir = config_file().parent().map(Path::to_path_buf);
    let mut pairs = Vec::new();
    if std::env::var_os("IMACS_HOME").is_none() {
        pairs.push((home().join(".cox"), imacs_home()));
    }
    if std::env::var_os("IMACS_CONFIG").is_none()
        && let Some(dir) = config_dir
        && let Some(base) = dir.parent()
    {
        pairs.push((base.join("cox"), dir.clone()));
    }
    let mut moved = Vec::new();
    for (old, new) in pairs {
        if old.is_dir() && !new.exists() && std::fs::rename(&old, &new).is_ok() {
            moved.push(format!("{} → {}", display(&old), display(&new)));
        }
    }
    moved
}

pub fn hook_settings_file() -> PathBuf {
    imacs_home().join("claude-hooks.json")
}

/// Task briefs handed to agents as files (Codex is killed when its first prompt
/// argument is about a kilobyte or longer).
pub fn briefs_dir() -> PathBuf {
    imacs_home().join("briefs")
}

pub fn plan_file() -> PathBuf {
    imacs_home().join("plan.json")
}

pub fn store_file() -> PathBuf {
    imacs_home().join("sessions.json")
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
