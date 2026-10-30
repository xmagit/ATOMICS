use std::path::{Path, PathBuf};

/// Machine identity shared by config overrides and workspace storage.
pub fn host_name() -> String {
    let override_name = std::env::var("IMACS_HOST").ok();
    let mut buffer = [0u8; 256];
    // SAFETY: buffer is writable for the supplied length.
    let system_name = if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0 {
        let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(buffer.len());
        std::str::from_utf8(&buffer[..end]).ok()
    } else {
        None
    };
    select_host(override_name.as_deref(), local_host_name(), system_name).to_string()
}

fn local_host_name() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        // Cache the stable machine name; config reload checks run every second.
        static NAME: std::sync::LazyLock<Option<String>> = std::sync::LazyLock::new(|| {
            std::process::Command::new("/usr/sbin/scutil")
                .args(["--get", "LocalHostName"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
        });
        NAME.as_deref()
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

fn select_host<'a>(
    override_name: Option<&'a str>,
    local_name: Option<&'a str>,
    system_name: Option<&'a str>,
) -> &'a str {
    [override_name, local_name, system_name]
        .into_iter()
        .flatten()
        .map(str::trim)
        .map(|name| name.strip_suffix(".local").unwrap_or(name))
        .find(|name| {
            !name.is_empty()
                && *name != "."
                && *name != ".."
                && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || b".-_".contains(&byte))
        })
        .unwrap_or("localhost")
}

/// Root for persistent state: hook settings, plans and session store.
/// `IMACS_HOME` overrides it so several instances (or tests) can run side by side.
pub fn imacs_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("IMACS_HOME") {
        return PathBuf::from(dir);
    }
    state_home(&home(), xdg_dir("XDG_STATE_HOME"), cfg!(target_os = "linux"))
}

fn state_home(home: &Path, xdg: Option<PathBuf>, linux: bool) -> PathBuf {
    let legacy = home.join(".imacs");
    // Keep existing installs in place, including links to state on another volume.
    // Selecting a new empty directory must never hide a user's sessions or plans.
    if !linux || legacy.symlink_metadata().is_ok() {
        legacy
    } else {
        xdg.unwrap_or_else(|| home.join(".local/state")).join("imacs")
    }
}

fn xdg_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from).filter(|path| path.is_absolute())
}

/// Stable folder name shared by runtime sockets and jobserver pools.
/// Hash the raw path bytes with FNV-1a 64 so separately built clients agree.
pub fn runtime_home_hash(home: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut hash = 0xcbf29ce484222325u64;
    for &byte in home.as_os_str().as_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Per-instance runtime folder in a valid, private Linux XDG_RUNTIME_DIR.
/// Callers create the folder as needed; missing/invalid runtime roots fall back
/// to their established socket or temporary FIFO locations.
pub fn runtime_dir_for(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let base = xdg_dir("XDG_RUNTIME_DIR")?;
        let metadata = base.symlink_metadata().ok()?;
        // SAFETY: getuid has no preconditions.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return None;
        }
        Some(base.join("imacs").join(runtime_home_hash(home)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = home;
        None
    }
}

pub fn config_file() -> PathBuf {
    if let Some(file) = std::env::var_os("IMACS_CONFIG") {
        return PathBuf::from(file);
    }
    let base = xdg_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"));
    base.join("imacs").join("config.toml")
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("IMACS_SOCKET") {
        return PathBuf::from(path);
    }
    let state = imacs_home();
    // Explicit instance homes and existing installs retain their socket location.
    if std::env::var_os("IMACS_HOME").is_none()
        && state != home().join(".imacs")
        && let Some(runtime) = runtime_dir_for(&state)
    {
        return runtime.join("imacs.sock");
    }
    state.join("imacs.sock")
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
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).context(format!("rename failed ({rename_error}); creating copy parent"))?;
    }
    let mut attempt = 0;
    let staging = loop {
        let candidate = new.with_extension(format!("migration-{}-{attempt}", std::process::id()));
        match std::fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(err) => {
                return Err(err).context(format!("rename failed ({rename_error}); creating copy staging directory"));
            }
        }
    };
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

/// Where git finds each sandbox's repository, recorded when the sandbox is made. No sandbox
/// mounts it.
pub fn git_pins_dir() -> PathBuf {
    imacs_home().join("git-pins")
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
    fn runtime_home_folder_name_is_stable() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(runtime_home_hash(Path::new("/Users/x/.imacs")), "51d177b92c46bedb");
        assert_eq!(runtime_home_hash(Path::new(OsStr::from_bytes(b"/home/\xff/.imacs"))), "f3937a555619b1fd");
    }

    #[test]
    fn state_paths_keep_existing_installs_and_macos_in_place() {
        let dir = crate::test_support::tempdir().unwrap();
        let xdg = dir.path().join("state");
        assert_eq!(state_home(dir.path(), Some(xdg.clone()), false), dir.path().join(".imacs"));
        assert_eq!(state_home(dir.path(), Some(xdg.clone()), true), xdg.join("imacs"));
        assert_eq!(state_home(dir.path(), None, true), dir.path().join(".local/state/imacs"));
        let legacy = dir.path().join(".imacs");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("sessions.json"), "existing sessions").unwrap();
        assert_eq!(state_home(dir.path(), Some(xdg.clone()), true), legacy);
        std::fs::remove_dir_all(&legacy).unwrap();
        std::os::unix::fs::symlink("offline-volume", &legacy).unwrap();
        assert_eq!(state_home(dir.path(), Some(xdg), true), legacy, "even a dangling link must not hide state");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_xdg_paths_and_instance_overrides() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

        const CHILD: &str = "IMACS_PATHS_TEST_CASE";
        let Ok(case) = std::env::var(CHILD) else {
            for case in
                ["new", "legacy", "explicit", "socket", "relative", "empty", "missing-runtime", "public-runtime"]
            {
                let dir = crate::test_support::tempdir().unwrap();
                let home = dir.path().join("home");
                let runtime = home.join("runtime");
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&runtime).unwrap();
                if case == "legacy" {
                    std::fs::create_dir(home.join(".imacs")).unwrap();
                    std::fs::write(home.join(".imacs/sessions.json"), "existing").unwrap();
                }
                if case == "public-runtime" {
                    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "paths::tests::linux_xdg_paths_and_instance_overrides", "--nocapture"])
                    .env(CHILD, case)
                    .env("HOME", &home)
                    .env("XDG_STATE_HOME", home.join("state"))
                    .env("XDG_CONFIG_HOME", home.join("config"))
                    .env("XDG_RUNTIME_DIR", &runtime)
                    .env_remove("IMACS_HOME")
                    .env_remove("IMACS_SOCKET")
                    .env_remove("IMACS_CONFIG");
                match case {
                    "explicit" => {
                        command.env("IMACS_HOME", home.join("isolated"));
                    }
                    "socket" => {
                        command.env("IMACS_SOCKET", home.join("custom.sock"));
                    }
                    "relative" | "empty" => {
                        for name in ["XDG_STATE_HOME", "XDG_CONFIG_HOME", "XDG_RUNTIME_DIR"] {
                            command.env(name, if case == "empty" { "" } else { "relative" });
                        }
                    }
                    "missing-runtime" => {
                        command.env_remove("XDG_RUNTIME_DIR");
                    }
                    _ => {}
                }
                let output = crate::test_support::output(&mut command);
                assert!(
                    output.status.success(),
                    "{case}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        let home = home();
        let state = match case.as_str() {
            "legacy" => home.join(".imacs"),
            "explicit" => home.join("isolated"),
            "relative" | "empty" => home.join(".local/state/imacs"),
            _ => home.join("state/imacs"),
        };
        assert_eq!(imacs_home(), state);
        assert_eq!(
            config_file(),
            home.join(if matches!(case.as_str(), "relative" | "empty") {
                ".config/imacs/config.toml"
            } else {
                "config/imacs/config.toml"
            })
        );
        let runtime = runtime_dir_for(&state);
        if matches!(case.as_str(), "relative" | "empty" | "missing-runtime" | "public-runtime") {
            assert!(runtime.is_none());
        } else {
            let runtime = runtime.as_ref().unwrap();
            assert!(runtime.starts_with(home.join("runtime/imacs")));
            assert_eq!(
                runtime_dir_for(Path::new("/Users/x/.imacs")).unwrap(),
                home.join("runtime/imacs/51d177b92c46bedb")
            );
            assert_ne!(Some(runtime.clone()), runtime_dir_for(&home.join("other-instance")));
        }
        let expected = match case.as_str() {
            "socket" => home.join("custom.sock"),
            "legacy" | "explicit" => state.join("imacs.sock"),
            _ => runtime.unwrap_or_else(|| state.clone()).join("imacs.sock"),
        };
        assert_eq!(socket_path(), expected);
        if case == "legacy" {
            assert_eq!(std::fs::read_to_string(store_file()).unwrap(), "existing");
        }
    }

    #[test]
    fn host_override_falls_back_to_system_hostname_then_localhost() {
        assert_eq!(select_host(Some(" work-mac.local "), Some("stable-mac"), Some("Mac")), "work-mac");
        for value in [None, Some(""), Some("  "), Some("../other"), Some("/tmp/other"), Some(".."), Some("a\\b")] {
            assert_eq!(select_host(value, Some("stable-mac.local\n"), Some("Mac")), "stable-mac");
            assert_eq!(select_host(value, None, Some("desktop.local")), "desktop");
        }
        for value in [None, Some(""), Some("bad/host"), Some(".local")] {
            assert_eq!(select_host(None, value, Some("kernel.local")), "kernel");
            assert_eq!(select_host(None, None, value), "localhost");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_local_hostname_comes_from_system_configuration() {
        let output = std::process::Command::new("/usr/sbin/scutil").args(["--get", "LocalHostName"]).output().unwrap();
        if output.status.success() {
            let expected = String::from_utf8(output.stdout).unwrap();
            assert_eq!(local_host_name().unwrap().trim(), expected.trim());
            let expected = expected.trim();
            assert_eq!(
                select_host(None, local_host_name(), Some("Mac")),
                expected.strip_suffix(".local").unwrap_or(expected)
            );
        } else {
            assert!(local_host_name().is_none());
        }
    }

    #[test]
    fn home_paths_expand_without_changing_other_paths() {
        assert_eq!(expand("~"), home());
        assert_eq!(expand("~/repos/app"), home().join("repos/app"));
        assert_eq!(expand("relative/app"), Path::new("relative/app"));
        assert_eq!(expand("/srv/app"), Path::new("/srv/app"));
        assert_eq!(expand("~someone/app"), Path::new("~someone/app"));
        assert_eq!(display(&expand("~/repos/app")), "~/repos/app");
    }

    #[test]
    fn migration_with_temporary_home() {
        const CHILD: &str = "IMACS_MIGRATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "paths::tests::migration_with_temporary_home", "--nocapture"])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env_remove("IMACS_HOME")
                    .env_remove("IMACS_CONFIG")
                    .env_remove("XDG_CONFIG_HOME"),
            );
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
    fn stale_migration_staging_does_not_block_retry_or_get_removed() {
        let dir = crate::test_support::tempdir().unwrap();
        let old = dir.path().join("cox");
        let new = dir.path().join("missing-parent/.imacs");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("plan.json"), "current").unwrap();
        for staging in
            [new.with_extension("migration"), new.with_extension(format!("migration-{}-0", std::process::id()))]
        {
            std::fs::create_dir_all(&staging).unwrap();
            std::fs::write(staging.join("plan.json"), "stale").unwrap();
        }
        // Force the rename fallback while leaving both old staging directories in place.
        std::os::unix::fs::symlink(dir.path().join("missing"), &new).unwrap();
        for _ in 0..2 {
            let error = format!("{:#}", move_directory(&old, &new).unwrap_err());
            assert!(error.contains("copy failed"), "must get past stale staging: {error}");
            assert_eq!(std::fs::read_to_string(old.join("plan.json")).unwrap(), "current");
        }
        std::fs::remove_file(&new).unwrap();
        move_directory(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(new.join("plan.json")).unwrap(), "current");
        for staging in
            [new.with_extension("migration"), new.with_extension(format!("migration-{}-0", std::process::id()))]
        {
            assert_eq!(std::fs::read_to_string(staging.join("plan.json")).unwrap(), "stale");
        }
    }

    #[test]
    fn rename_failure_falls_back_to_copying() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::os::unix::net::UnixListener;

        let dir = crate::test_support::tempdir().unwrap();
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
