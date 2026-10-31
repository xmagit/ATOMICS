use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use imacs_core::paths;

/// The PATH of an interactive login shell. Desktop launchers can supply a bare
/// PATH, hiding tools installed by the user's shell startup files.
pub static SHELL_PATH: LazyLock<Option<String>> = LazyLock::new(|| login_shell_var("PATH"));

pub fn shell_program() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| if cfg!(target_os = "macos") { "/bin/zsh" } else { "/bin/sh" }.into())
}

/// The installed path, including after an atomic update of a running Linux app.
pub fn executable_path() -> std::io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        // /proc/self/exe describes the old, unlinked inode after an installer
        // renames its replacement into place. Hooks and restart need the new file.
        if !executable.exists()
            && let Some(installed) = executable.as_os_str().as_bytes().strip_suffix(b" (deleted)")
        {
            return Ok(PathBuf::from(std::ffi::OsStr::from_bytes(installed)));
        }
    }
    Ok(executable)
}

/// A variable as an interactive login shell sets it. `name` must be a plain identifier.
pub(crate) fn login_shell_var(name: &str) -> Option<String> {
    let shell = shell_program();
    let out = std::process::Command::new(shell)
        .args(["-l", "-i", "-c", &format!("printf '{MARKER}%s{MARKER}' \"${name}\"")])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    between_markers(&String::from_utf8_lossy(&out.stdout))
}

/// Brackets the value the login shell prints, so shell startup noise is ignored.
const MARKER: &str = "__IMACS__";

fn between_markers(text: &str) -> Option<String> {
    let start = text.find(MARKER)? + MARKER.len();
    let end = text[start..].find(MARKER)? + start;
    Some(text[start..end].to_string())
}

/// An environment variable of IMACS, or of the login shell when IMACS lacks it (as apps
/// started from the Dock do). Blocks while the shell starts, so call it off the UI thread.
pub fn env_var(name: &str) -> Option<String> {
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

/// Login-shell variables `env_var` has looked up.
static FOUND: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

/// What `env_var` knows without starting a shell: IMACS's own environment, or a value
/// the login shell already gave. Safe on the runtime thread.
pub fn known_env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty()).or_else(|| FOUND.lock().unwrap().get(name).cloned())
}

pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = SHELL_PATH.clone().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    find_on_path(program, &path)
}

pub(crate) fn find_on_path(program: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Resolve the configured editor, including distributions that name Helix `helix`.
pub fn editor_program(program: &str) -> String {
    editor_on_path(program, find_program)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.into())
}

pub(crate) fn editor_on_path(program: &str, find: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    find(program).or_else(|| if program == "hx" { find("helix") } else { None })
}

pub fn resolve_program(program: &str) -> String {
    if program.contains('/') {
        return program.to_string();
    }
    find_program(program).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| program.to_string())
}

/// Keep the version probe bounded even when a wrapper hangs or another child inherits stdout.
pub fn codex_supports_hooks(program: &str, timeout: std::time::Duration) -> Option<bool> {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let (mut stdout, writer) = UnixStream::pair().ok()?;
    stdout.set_nonblocking(true).ok()?;
    let mut child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut bytes = Vec::new();
                let _ = (&mut stdout).take(1024).read_to_end(&mut bytes);
                return imacs_core::hook::codex_supports_hooks(&String::from_utf8_lossy(&bytes));
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Claude Code's user settings, in `$CLAUDE_CONFIG_DIR` when it is set, as Claude Code reads them.
fn claude_settings_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| paths::home().join(".claude"))
        .join("settings.json")
}

pub fn global_hooks_installed() -> bool {
    let Ok(exe) = executable_path() else { return false };
    std::fs::read_to_string(claude_settings_path()).is_ok_and(|text| text.contains(&format!("{} hook", exe.display())))
}

pub fn legacy_hooks_installed() -> bool {
    std::fs::read_to_string(claude_settings_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .is_some_and(|settings| imacs_core::hook::has_cox_hooks(&settings))
}

pub fn install_global_hooks() -> anyhow::Result<PathBuf> {
    let path = claude_settings_path();
    let exe = executable_path()?;
    let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => {
            std::fs::write(path.with_extension("json.imacs-backup"), &text)?;
            serde_json::from_str(&text)?
        }
        Err(_) => serde_json::json!({}),
    };
    imacs_core::hook::merge_into_settings(&mut settings, &exe);
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
    #[cfg(target_os = "linux")]
    fn replaced_linux_executable_still_resolves_the_installation() {
        use crate::test_support::{CHILD_TIMEOUT, TestChild};
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        const CHILD: &str = "IMACS_REPLACED_EXE_TEST";
        if let Some(executable) = std::env::var_os(CHILD) {
            let executable = PathBuf::from(executable);
            let ready = executable.with_extension("ready");
            std::fs::write(&ready, "ready").unwrap();
            let deadline = Instant::now() + CHILD_TIMEOUT;
            while !executable.with_extension("updated").exists() {
                assert!(Instant::now() < deadline, "installer never replaced the running executable");
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_ne!(std::env::current_exe().unwrap(), executable, "exercise /proc/self/exe's deleted suffix");
            assert_eq!(executable_path().unwrap(), executable);
            let target = crate::restart::Target::current().unwrap();
            assert_eq!(target.executable, executable);
            target.validate().unwrap();
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let executable = dir.path().join("imacs with spaces");
        std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
        let mut child = TestChild::spawn(
            std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "setup::tests::replaced_linux_executable_still_resolves_the_installation",
                    "--nocapture",
                ])
                .env(CHILD, &executable),
            CHILD_TIMEOUT,
        )
        .unwrap();
        while !executable.with_extension("ready").exists() {
            child.check_timeout().unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, "#!/bin/sh\ntest \"$1\" = --version\n").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(replacement, &executable).unwrap();
        std::fs::write(executable.with_extension("updated"), "updated").unwrap();
        let output = child.wait_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn version_probe_times_out_even_after_printing_a_valid_version() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("codex");
        std::fs::write(&program, "#!/bin/sh\nprintf 'codex-cli 0.165.0\\n'\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        assert_eq!(codex_supports_hooks(program.to_str().unwrap(), Duration::from_millis(100)), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        for script in ["#!/bin/sh\nexit 1\n", "#!/bin/sh\nprintf 'not a version\\n'\n"] {
            std::fs::write(&program, script).unwrap();
            assert_eq!(codex_supports_hooks(program.to_str().unwrap(), Duration::from_secs(10)), None);
        }
        assert_eq!(codex_supports_hooks("/no/such/codex", Duration::from_secs(1)), None);
    }

    #[test]
    fn successful_version_probe_tolerates_slow_startup() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("codex");
        std::fs::write(&program, "#!/bin/sh\nsleep 1.1\nprintf 'codex-cli 0.165.0\\n'\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(codex_supports_hooks(program.to_str().unwrap(), std::time::Duration::from_secs(10)), Some(true));
    }

    #[test]
    fn login_shell_output_is_read_between_the_markers() {
        let noise = "Last login: today\n__IMACS__/opt/homebrew/bin:/usr/bin__IMACS__";
        assert_eq!(between_markers(noise).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(between_markers("__IMACS____IMACS__").as_deref(), Some(""));
        assert_eq!(between_markers("no markers"), None);
    }

    #[test]
    fn env_var_reads_imacs_first_and_passes_only_plain_names_to_the_shell() {
        assert_eq!(env_var("HOME"), std::env::var("HOME").ok());
        assert!(env_var("IMACS_UNSET; exit 1").is_none());
        assert!(env_var("1ABC").is_none());
    }
}
