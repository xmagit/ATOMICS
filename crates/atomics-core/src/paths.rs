use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Machine identity shared by config overrides and workspace storage.
pub fn host_name() -> String {
    let override_name = std::env::var("ATOMICS_HOST").ok();
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
/// `ATOMICS_HOME` overrides it so several instances (or tests) can run side by side.
pub fn atomics_home() -> PathBuf {
    if let Some(dir) = env_override("HOME") {
        return PathBuf::from(dir);
    }
    state_home(&home(), xdg_dir("XDG_STATE_HOME"), cfg!(target_os = "linux"))
}

fn state_home(home: &Path, xdg: Option<PathBuf>, linux: bool) -> PathBuf {
    let legacy = home.join(".atomics");
    // Keep existing installs in place, including links to state on another volume.
    // Selecting a new empty directory must never hide a user's sessions or plans.
    if !linux || legacy.symlink_metadata().is_ok() {
        legacy
    } else {
        xdg.unwrap_or_else(|| home.join(".local/state")).join("atomics")
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
        private_runtime_dir(&xdg_dir("XDG_RUNTIME_DIR")?, home)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = home;
        None
    }
}

fn private_runtime_dir(base: &Path, home: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = base.symlink_metadata().ok()?;
    // SAFETY: getuid has no preconditions.
    if !base.is_absolute()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::getuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return None;
    }
    Some(base.join("atomics").join(runtime_home_hash(home)))
}

fn socket_runtime_dir(home: &Path) -> Option<PathBuf> {
    runtime_dir_for(home).or_else(|| {
        // On macOS a private per-user temporary directory can provide a short socket path.
        // Leave the jobserver's existing placement alone; only the socket needs this fallback.
        #[cfg(target_os = "macos")]
        {
            private_runtime_dir(&std::env::temp_dir(), home)
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    })
}

pub fn config_file() -> PathBuf {
    if let Some(file) = env_override("CONFIG") {
        return PathBuf::from(file);
    }
    let base = xdg_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"));
    base.join("atomics").join("config.toml")
}

/// Someone named the config file, so it is no person's own config in its usual place.
pub fn config_named() -> bool {
    env_override("CONFIG").is_some()
}

/// `ATOMICS_<name>`, or `IMACS_<name>` as set for builds from before the rename to ATOMICS.
pub fn env_override(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("ATOMICS_{name}")).or_else(|| std::env::var_os(format!("IMACS_{name}")))
}

/// Set by a host in each of its terminals, beside `ATOMICS_SOCKET` and to the same path: that socket is the
/// host's, given so its agents reach it, and not one chosen for an instance of their own. So an explicit
/// `ATOMICS_HOME` there (a separate test instance) still keeps its socket beside its state.
pub const HOST_SOCKET_ENV: &str = "ATOMICS_HOST_SOCKET";

pub fn socket_path() -> PathBuf {
    let socket = env_override("SOCKET");
    if let Some(path) = chosen_socket(socket, std::env::var_os(HOST_SOCKET_ENV), env_override("HOME").is_some()) {
        return path;
    }
    let state = atomics_home();
    let prefer_runtime = env_override("HOME").is_none() && state != home().join(".atomics");
    default_socket(&state, socket_runtime_dir(&state), prefer_runtime)
}

fn default_socket(state: &Path, runtime: Option<PathBuf>, prefer_runtime: bool) -> PathBuf {
    let persistent = state.join("atomics.sock");
    // Explicit homes and old installs retain their location when it fits. A long state path
    // can still use the private runtime directory, with a stable per-instance name.
    if (prefer_runtime || validate_socket_path(&persistent).is_err())
        && let Some(path) = runtime.map(|dir| dir.join("atomics.sock"))
        && validate_socket_path(&path).is_ok()
    {
        return path;
    }
    persistent
}

/// Validate before the desktop installs crash logging or opens a window, so startup failures
/// reach stderr. Unix socket limits count bytes, including the final NUL.
pub fn validate_socket_path(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: all-zero bytes are a valid sockaddr_un and it is used only for its array's length.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let limit = address.sun_path.len() - 1;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() > limit || bytes.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "control socket path {} is invalid ({} bytes; platform limit {limit} bytes). Set ATOMICS_SOCKET to a shorter socket path in a private directory, or ATOMICS_HOME to a shorter state directory",
                path.display(),
                bytes.len(),
            ),
        ));
    }
    Ok(())
}

/// `ATOMICS_SOCKET`, unless it is only the socket a host gave its terminal ([`HOST_SOCKET_ENV`]) and
/// `ATOMICS_HOME` names an instance of its own.
fn chosen_socket(socket: Option<OsString>, host_socket: Option<OsString>, own_home: bool) -> Option<PathBuf> {
    let socket = socket?;
    let inherited = host_socket.is_some_and(|host| host == socket);
    (!(inherited && own_home)).then(|| PathBuf::from(socket))
}

#[derive(Debug, Default)]
pub struct Migration {
    pub moved: Vec<String>,
    pub errors: Vec<String>,
}

/// Moves state and config from where they lived under earlier names, IMACS (`~/.imacs`,
/// `~/.local/state/imacs`, `~/.config/imacs`) and before it cox (`~/.cox`, `~/.config/cox`),
/// once, when the new places do not exist yet. The first old place found wins.
pub fn migrate_old_names() -> Migration {
    let config_dir = config_file().parent().map(Path::to_path_buf);
    let mut pairs = Vec::new();
    if env_override("HOME").is_none() {
        let state = atomics_home();
        let xdg = xdg_dir("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local/state"));
        for old in [xdg.join("imacs"), home().join(".imacs"), home().join(".cox")] {
            pairs.push((old, state.clone()));
        }
    }
    if env_override("CONFIG").is_none()
        && let Some(dir) = config_dir
        && let Some(base) = dir.parent()
    {
        pairs.push((base.join("imacs"), dir.clone()));
        pairs.push((base.join("cox"), dir.clone()));
    }
    let mut result = Migration::default();
    for (old, new) in pairs {
        if old.is_dir() && !new.exists() {
            match move_directory(&old, &new) {
                Ok(method) => {
                    result.moved.push(format!("{method} {} → {}", display(&old), display(&new)));
                    if let Err(err) = rewrite_moved_paths(&old, &new) {
                        result.errors.push(format!("Could not update paths in {}: {err}", display(&new)));
                    }
                }
                Err(err) => {
                    result.errors.push(format!("Could not migrate {} to {}: {err:#}", display(&old), display(&new)))
                }
            }
        }
    }
    result
}

/// State records (the sandbox ledger, sessions, plans, git pins) name files under the state folder by
/// absolute path; after a move, point those at the new place.
fn rewrite_moved_paths(old: &Path, new: &Path) -> std::io::Result<()> {
    let (Some(old), Some(new)) = (old.to_str(), new.to_str()) else { return Ok(()) };
    let root = Path::new(new);
    for dir in [root.to_path_buf(), root.join("plans"), root.join("git-pins")] {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_none_or(|ext| ext != "json") || !path.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            let updated = text
                .replace(&format!("\"{old}/"), &format!("\"{new}/"))
                .replace(&format!("\"{old}\""), &format!("\"{new}\""));
            if updated != text {
                crate::store::write_private(&path, updated.as_bytes())?;
            }
        }
    }
    Ok(())
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
    atomics_home().join("briefs")
}

/// Where git finds each sandbox's repository, recorded when the sandbox is made. No sandbox
/// mounts it.
pub fn git_pins_dir() -> PathBuf {
    atomics_home().join("git-pins")
}

/// Where sessions outside any project work, each in a folder of its own that goes when it closes.
pub fn scratch_dir() -> PathBuf {
    atomics_home().join("scratch")
}

/// The scratch folder `dir` is or lies in.
pub fn scratch_of(dir: &Path) -> Option<PathBuf> {
    scratch_in(&scratch_dir(), dir)
}

fn scratch_in(root: &Path, dir: &Path) -> Option<PathBuf> {
    match dir.strip_prefix(root).ok()?.components().next()? {
        std::path::Component::Normal(name) => Some(root.join(name)),
        _ => None,
    }
}

pub fn plan_file() -> PathBuf {
    atomics_home().join("plan.json")
}

pub fn store_file() -> PathBuf {
    atomics_home().join("sessions.json")
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
    fn audit_p7_long_socket_paths_use_a_short_runtime_or_explain_the_override() {
        use std::os::unix::fs::PermissionsExt;
        let state = PathBuf::from(format!("/{}", "long-state/".repeat(20)));
        let runtime = PathBuf::from("/run/private/instance");
        assert_eq!(default_socket(&state, Some(runtime.clone()), false), runtime.join("atomics.sock"));
        assert_eq!(default_socket(Path::new("/short"), Some(runtime), false), Path::new("/short/atomics.sock"));
        for runtime in [None, Some(state.clone())] {
            let path = default_socket(&state, runtime, false);
            let error = validate_socket_path(&path).unwrap_err().to_string();
            assert!(error.contains("platform limit"), "{error}");
            assert!(error.contains("ATOMICS_SOCKET") && error.contains("ATOMICS_HOME"), "{error}");
        }
        // Count UTF-8 bytes, rather than characters, just as bind does.
        assert!(validate_socket_path(Path::new(&format!("/{}", "é".repeat(60)))).is_err());
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(private_runtime_dir(dir.path(), &state).is_some());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_runtime_dir(dir.path(), &state).is_none());
    }

    #[test]
    fn runtime_home_folder_name_is_stable() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        assert_eq!(runtime_home_hash(Path::new("/Users/x/.atomics")), "c32c7b261409a862");
        assert_eq!(runtime_home_hash(Path::new(OsStr::from_bytes(b"/home/\xff/.atomics"))), "980d1996d7114d38");
    }

    #[test]
    fn a_hosts_socket_routes_its_terminals_but_not_an_instance_of_their_own() {
        let os = |text: &str| Some(OsString::from(text));
        // Nothing set: the default place.
        assert_eq!(chosen_socket(None, None, false), None);
        assert_eq!(chosen_socket(None, os("/host.sock"), true), None);
        // Set by hand, with or without a home of its own.
        assert_eq!(chosen_socket(os("/mine.sock"), None, true), Some(PathBuf::from("/mine.sock")));
        // A host's terminal: its host's socket.
        assert_eq!(chosen_socket(os("/host.sock"), os("/host.sock"), false), Some(PathBuf::from("/host.sock")));
        // A test instance started there with only ATOMICS_HOME keeps its socket beside its state ...
        assert_eq!(chosen_socket(os("/host.sock"), os("/host.sock"), true), None);
        // ... and one given a socket of its own uses that.
        assert_eq!(chosen_socket(os("/mine.sock"), os("/host.sock"), true), Some(PathBuf::from("/mine.sock")));
    }

    #[test]
    fn scratch_folders_are_the_folders_directly_in_scratch() {
        let root = Path::new("/state/scratch");
        assert_eq!(scratch_in(root, &root.join("a1b2")), Some(root.join("a1b2")));
        assert_eq!(scratch_in(root, &root.join("a1b2/notes")), Some(root.join("a1b2")));
        assert_eq!(scratch_in(root, root), None);
        assert_eq!(scratch_in(root, Path::new("/state/scratchpad/a1b2")), None);
        assert_eq!(scratch_in(root, Path::new("/repos/app")), None);
    }

    #[test]
    fn state_paths_keep_existing_installs_and_macos_in_place() {
        let dir = crate::test_support::tempdir().unwrap();
        let xdg = dir.path().join("state");
        assert_eq!(state_home(dir.path(), Some(xdg.clone()), false), dir.path().join(".atomics"));
        assert_eq!(state_home(dir.path(), Some(xdg.clone()), true), xdg.join("atomics"));
        assert_eq!(state_home(dir.path(), None, true), dir.path().join(".local/state/atomics"));
        let legacy = dir.path().join(".atomics");
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

        const CHILD: &str = "ATOMICS_PATHS_TEST_CASE";
        let Ok(case) = std::env::var(CHILD) else {
            for case in
                ["new", "legacy", "explicit", "socket", "relative", "empty", "missing-runtime", "public-runtime"]
                    .into_iter()
                    .chain(["host-socket", "host-socket-home"])
            {
                let dir = crate::test_support::tempdir().unwrap();
                let home = dir.path().join("home");
                let runtime = home.join("runtime");
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&runtime).unwrap();
                if case == "legacy" {
                    std::fs::create_dir(home.join(".atomics")).unwrap();
                    std::fs::write(home.join(".atomics/sessions.json"), "existing").unwrap();
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
                    .env_remove("ATOMICS_HOME")
                    .env_remove("ATOMICS_SOCKET")
                    .env_remove(HOST_SOCKET_ENV)
                    .env_remove("ATOMICS_CONFIG");
                match case {
                    "explicit" => {
                        command.env("ATOMICS_HOME", home.join("isolated"));
                    }
                    "socket" => {
                        command.env("ATOMICS_SOCKET", home.join("custom.sock"));
                    }
                    // A host's terminal, and an instance with a home of its own started there.
                    "host-socket" | "host-socket-home" => {
                        command
                            .env("ATOMICS_SOCKET", home.join("host.sock"))
                            .env(HOST_SOCKET_ENV, home.join("host.sock"));
                        if case == "host-socket-home" {
                            command.env("ATOMICS_HOME", home.join("isolated"));
                        }
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
            "legacy" => home.join(".atomics"),
            "explicit" | "host-socket-home" => home.join("isolated"),
            "relative" | "empty" => home.join(".local/state/atomics"),
            _ => home.join("state/atomics"),
        };
        assert_eq!(atomics_home(), state);
        assert_eq!(
            config_file(),
            home.join(if matches!(case.as_str(), "relative" | "empty") {
                ".config/atomics/config.toml"
            } else {
                "config/atomics/config.toml"
            })
        );
        let runtime = runtime_dir_for(&state);
        if matches!(case.as_str(), "relative" | "empty" | "missing-runtime" | "public-runtime") {
            assert!(runtime.is_none());
        } else {
            let runtime = runtime.as_ref().unwrap();
            assert!(runtime.starts_with(home.join("runtime/atomics")));
            assert_eq!(
                runtime_dir_for(Path::new("/Users/x/.atomics")).unwrap(),
                home.join("runtime/atomics/c32c7b261409a862")
            );
            assert_ne!(Some(runtime.clone()), runtime_dir_for(&home.join("other-instance")));
        }
        let expected = match case.as_str() {
            "socket" => home.join("custom.sock"),
            "host-socket" => home.join("host.sock"),
            "legacy" | "explicit" | "host-socket-home" => state.join("atomics.sock"),
            _ => runtime.unwrap_or_else(|| state.clone()).join("atomics.sock"),
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
        const CHILD: &str = "ATOMICS_MIGRATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let dir = crate::test_support::tempdir().unwrap();
            let output = crate::test_support::output(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "paths::tests::migration_with_temporary_home", "--nocapture"])
                    .env(CHILD, "1")
                    .env("HOME", dir.path())
                    .env_remove("ATOMICS_HOME")
                    .env_remove("ATOMICS_CONFIG")
                    .env_remove("IMACS_HOME")
                    .env_remove("IMACS_CONFIG")
                    .env_remove("XDG_STATE_HOME")
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
        // State under the IMACS name and config under the cox name both move.
        let legacy = home().join(".imacs");
        let config = home().join(".config/cox");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(legacy.join("plan.json"), "plan").unwrap();
        std::fs::write(config.join("config.toml"), "config").unwrap();
        let ledger =
            format!("{{\"pins\": \"{0}/git-pins\", \"home\": \"{0}\", \"other\": \"{0}x\"}}", legacy.display());
        std::fs::write(legacy.join("sandbox-ownership.json"), &ledger).unwrap();
        let result = migrate_old_names();
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        let moved = atomics_home().display().to_string();
        assert_eq!(
            std::fs::read_to_string(atomics_home().join("sandbox-ownership.json")).unwrap(),
            format!("{{\"pins\": \"{moved}/git-pins\", \"home\": \"{moved}\", \"other\": \"{}x\"}}", legacy.display())
        );
        assert_eq!(result.moved.len(), 2);
        assert_eq!(std::fs::read_to_string(plan_file()).unwrap(), "plan");
        assert_eq!(std::fs::read_to_string(config_file()).unwrap(), "config");
        assert!(!legacy.exists());
        assert!(!config.exists());
        assert!(migrate_old_names().moved.is_empty());

        // Existing ATOMICS state must never be overwritten.
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("plan.json"), "old").unwrap();
        assert!(migrate_old_names().moved.is_empty());
        assert_eq!(std::fs::read_to_string(plan_file()).unwrap(), "plan");
        assert!(legacy.exists());

        std::fs::remove_dir_all(atomics_home()).unwrap();
        // A dangling destination link makes both directory renames fail.
        std::os::unix::fs::symlink(home().join("missing"), atomics_home()).unwrap();
        let result = migrate_old_names();
        assert!(result.moved.is_empty());
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("rename failed"));
        assert!(result.errors[0].contains("copy failed"));
        assert!(!atomics_home().with_extension("migration").exists());
        assert_eq!(std::fs::read_to_string(legacy.join("plan.json")).unwrap(), "old");
    }

    #[test]
    fn stale_migration_staging_does_not_block_retry_or_get_removed() {
        let dir = crate::test_support::tempdir().unwrap();
        let old = dir.path().join("cox");
        let new = dir.path().join("missing-parent/.atomics");
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
        let new = dir.path().join("missing-parent/atomics");
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
