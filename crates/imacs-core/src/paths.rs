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

#[derive(Debug, Default)]
pub struct Migration {
    pub moved: Vec<String>,
    pub errors: Vec<String>,
}

/// Moves state and config from where they lived before the rename to IMACS
/// (`~/.cox`, `~/.config/cox`), once, when the new places do not exist yet.
pub fn migrate_from_cox() -> Migration {
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
    let mut result = Migration::default();
    for (old, new) in pairs {
        if old.is_dir() && !new.exists() {
            match move_directory(&old, &new) {
                Ok(method) => result.moved.push(format!("{method} {} → {}", display(&old), display(&new))),
                Err(err) => {
                    result.errors.push(format!("Could not migrate {} to {}: {err:#}", display(&old), display(&new)))
                }
            }
        }
    }
    result
}

fn move_directory(old: &Path, new: &Path) -> anyhow::Result<&'static str> {
    use anyhow::Context;

    let rename_error = match std::fs::rename(old, new) {
        Ok(()) => return Ok("moved"),
        Err(err) => err,
    };
    // Copy beside the destination, so a failed copy never hides the old state on retry.
    let staging = new.with_extension("migration");
    if let Some(parent) = staging.parent() {
        std::fs::create_dir_all(parent).context(format!("rename failed ({rename_error}); creating copy parent"))?;
    }
    std::fs::create_dir(&staging)
        .context(format!("rename failed ({rename_error}); creating copy staging directory"))?;
    let copied = copy_directory(old, &staging).and_then(|()| std::fs::rename(&staging, new));
    if let Err(err) = copied {
        let cleanup = std::fs::remove_dir_all(&staging);
        let cleanup_error = cleanup.err().map(|e| format!("; staging cleanup failed: {e}")).unwrap_or_default();
        anyhow::bail!("rename failed ({rename_error}); copy failed ({err}){cleanup_error}");
    }
    std::fs::remove_dir_all(old).context("copied successfully, but could not remove the old directory")?;
    Ok("copied (rename unavailable)")
}

fn copy_directory(old: &Path, new: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, symlink};

    for entry in std::fs::read_dir(old)? {
        let entry = entry?;
        let target = new.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir(&target)?;
            copy_directory(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            symlink(std::fs::read_link(entry.path())?, target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else if !kind.is_socket() {
            return Err(std::io::Error::other(format!("cannot copy {}", entry.path().display())));
        }
    }
    std::fs::set_permissions(new, std::fs::metadata(old)?.permissions())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_with_temporary_home() {
        const CHILD: &str = "IMACS_MIGRATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "paths::tests::migration_with_temporary_home", "--nocapture"])
                .env(CHILD, "1")
                .env("HOME", dir.path())
                .env_remove("IMACS_HOME")
                .env_remove("IMACS_CONFIG")
                .env_remove("XDG_CONFIG_HOME")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let legacy = home().join(".cox");
        let config = home().join(".config/cox");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(legacy.join("plan.json"), "plan").unwrap();
        std::fs::write(config.join("config.toml"), "config").unwrap();
        let result = migrate_from_cox();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(result.moved.len(), 2);
        assert_eq!(std::fs::read_to_string(plan_file()).unwrap(), "plan");
        assert_eq!(std::fs::read_to_string(config_file()).unwrap(), "config");
        assert!(!legacy.exists());
        assert!(!config.exists());
        assert!(migrate_from_cox().moved.is_empty());

        // Existing IMACS state must never be overwritten.
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("plan.json"), "old").unwrap();
        assert!(migrate_from_cox().moved.is_empty());
        assert_eq!(std::fs::read_to_string(plan_file()).unwrap(), "plan");
        assert!(legacy.exists());

        std::fs::remove_dir_all(imacs_home()).unwrap();
        // A dangling destination link makes both directory renames fail.
        std::os::unix::fs::symlink(home().join("missing"), imacs_home()).unwrap();
        let result = migrate_from_cox();
        assert!(result.moved.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("rename failed"));
        assert!(result.errors[0].contains("copy failed"));
        assert!(!imacs_home().with_extension("migration").exists());
        assert_eq!(std::fs::read_to_string(legacy.join("plan.json")).unwrap(), "old");
    }

    #[test]
    fn rename_failure_falls_back_to_copying() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("cox");
        let new = dir.path().join("missing-parent/imacs");
        std::fs::create_dir_all(old.join("briefs")).unwrap();
        std::fs::write(old.join("briefs/task.md"), "brief").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink("briefs/task.md", old.join("link")).unwrap();
        let _socket = UnixListener::bind(old.join("cox.sock")).unwrap();
        assert!(move_directory(&old, &new).unwrap().starts_with("copied"));
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(new.join("briefs/task.md")).unwrap(), "brief");
        assert_eq!(std::fs::read_link(new.join("link")).unwrap(), Path::new("briefs/task.md"));
        assert_eq!(std::fs::metadata(&new).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(!new.join("cox.sock").exists());
    }
}
