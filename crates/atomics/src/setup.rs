use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;

use atomics_core::paths;

/// The PATH of an interactive login shell, with the user's own programs folder ([`with_user_programs`]). Desktop
/// launchers can supply a bare PATH, hiding tools installed by the user's shell startup files.
pub static SHELL_PATH: LazyLock<Option<String>> =
    LazyLock::new(|| login_path().path.clone().ok().map(|path| with_user_programs(&path, &paths::home())));

/// The login shell's PATH when someone last asked ATOMICS to look again ([`refresh_login_path`]).
static REFRESHED_PATH: Mutex<Option<String>> = Mutex::new(None);

/// How long ATOMICS waits for the login shell to print a variable. A shell that takes longer
/// is stopped, and ATOMICS goes on with what it has.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// The login shell's PATH, read once per process: by [`adopt_login_path`] at startup, or else
/// when something first needs it (for a window, a thread [`start_login_path`] starts).
static LOGIN_PATH: OnceLock<LoginPath> = OnceLock::new();

/// What a window took on at startup, for the read of the login shell that follows it.
static WINDOW: OnceLock<Window> = OnceLock::new();

/// A window's start without waiting for the login shell ([`start_login_path`]).
struct Window {
    /// Folders of the recorded login PATH added to the PATH the window was started with.
    added: usize,
    /// Where the login shell's PATH is recorded for the next start.
    record: PathBuf,
    timeout: Duration,
}

/// The login shell's PATH, and what ATOMICS made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginPath {
    pub shell: String,
    /// The login shell's PATH, or why it gave none.
    pub path: Result<String, String>,
    /// How many of its folders ATOMICS added to the PATH it was started with; `None` when this
    /// process did not take it on (only [`adopt_login_path`] and [`start_login_path`] do).
    pub added: Option<usize>,
    /// How many folders of the shell's PATH this process's own still lacks: a window took on the
    /// PATH recorded at an earlier start, which the shell has changed since, or none was recorded.
    pub late: usize,
}

impl LoginPath {
    /// Reads `shell`'s PATH, and the PATH taking it on makes of `current`, if that changes it.
    fn read(shell: String, current: &str, timeout: Duration) -> (LoginPath, Option<String>) {
        let path = login_shell_path(&shell, timeout);
        let missing = path.as_ref().map_or(0, |login| missing_folders(current, login).len());
        let merged = path.as_ref().ok().filter(|_| missing > 0).map(|login| with_login_path(current, login));
        (LoginPath { shell, path, added: Some(missing), late: 0 }, merged)
    }

    /// Where the PATH of ATOMICS and of what it runs came from, and whether the login shell gave
    /// its own, for the doctor.
    pub fn describe(&self) -> (bool, String) {
        let shell = &self.shell;
        let folders = |count: usize| format!("{count} folder{}", if count == 1 { "" } else { "s" });
        match (&self.path, self.added) {
            (Ok(_), Some(0)) if self.late > 0 => (
                true,
                format!(
                    "PATH as ATOMICS was started; {} of login shell {shell}'s reach agents and servers now, and what \
                     ATOMICS runs itself at its next start",
                    folders(self.late)
                ),
            ),
            (Ok(_), Some(added)) if self.late > 0 => (
                true,
                format!(
                    "PATH of login shell {shell} as recorded at an earlier start: {} added to the PATH ATOMICS was \
                     started with; {} the shell has now reach agents and servers, and what ATOMICS runs itself at \
                     its next start",
                    folders(added),
                    folders(self.late)
                ),
            ),
            (Ok(_), Some(0)) => {
                (true, format!("PATH as ATOMICS was started, which has all of login shell {shell}'s folders"))
            }
            (Ok(_), Some(added)) => (
                true,
                format!("PATH of login shell {shell}: {} added to the PATH ATOMICS was started with", folders(added)),
            ),
            (Ok(_), None) => (true, format!("PATH as ATOMICS was started; login shell {shell}'s is used where needed")),
            (Err(reason), Some(added)) if added > 0 => (
                false,
                format!(
                    "PATH of login shell {shell} as recorded at an earlier start: {} added to the PATH ATOMICS was \
                     started with; the shell gave none now, {reason} (warning)",
                    folders(added)
                ),
            ),
            (Err(reason), _) => {
                (false, format!("PATH as ATOMICS was started: login shell {shell} gave none, {reason} (warning)"))
            }
        }
    }
}

/// Takes on the login shell's PATH for this process, and so for everything it runs: git and its
/// filters and hooks, version probes, MCP and ticket servers. A desktop launcher starts ATOMICS
/// with a bare PATH that lacks what the user's shell startup files add. A PATH that already has
/// all the login shell's folders, as one from a terminal does, is left as it is; when the shell
/// gives none in time, ATOMICS keeps the PATH it was started with. What the shell gives is
/// recorded for a window's next start ([`start_login_path`]).
///
/// Waits for the shell, up to five seconds: for `atomics host` and the CLI, not for a window.
///
/// # Safety
///
/// Changes the process environment: no other thread may exist yet.
pub unsafe fn adopt_login_path() -> &'static LoginPath {
    let current = std::env::var("PATH").unwrap_or_default();
    let (found, merged) = LoginPath::read(shell_program(), &current, LOGIN_SHELL_TIMEOUT);
    if let Some(merged) = merged {
        // SAFETY: the caller guarantees this process has no other thread.
        unsafe { std::env::set_var("PATH", merged) };
    }
    if let Ok(login) = &found.path {
        let _ = record(&record_file(), &found.shell, login);
    }
    LOGIN_PATH.get_or_init(|| found)
}

/// Takes on the login shell's PATH for a window without waiting for the shell, which can take
/// seconds. The PATH the shell gave at an earlier start (see [`adopt_login_path`]) is merged in
/// now, as the shell's own would be, before any thread or child exists, so git and its filters
/// and hooks find what they found then; a PATH that already has those folders, as a terminal's
/// does, is left as it is. The shell is then read on a thread of its own, which records what it
/// gives for the next start. Agents, MCP and ticket servers and the Codex version probe wait for
/// that ([`SHELL_PATH`], [`child_path`]), off the window's thread. With nothing recorded yet,
/// what ATOMICS runs itself has the PATH it was started with until the next start.
///
/// # Safety
///
/// Changes the process environment: no other thread may exist yet.
pub unsafe fn start_login_path() {
    // SAFETY: passed on from the caller.
    unsafe { start_login_path_with(record_file(), LOGIN_SHELL_TIMEOUT) }
}

/// [`start_login_path`] with the record kept in `record`, and a shell stopped after `timeout`.
///
/// # Safety
///
/// Changes the process environment: no other thread may exist yet.
unsafe fn start_login_path_with(record: PathBuf, timeout: Duration) {
    let current = std::env::var("PATH").unwrap_or_default();
    let recorded = recorded(&record, &shell_program());
    let added = recorded.as_deref().map_or(0, |login| missing_folders(&current, login).len());
    if let Some(login) = recorded.filter(|_| added > 0) {
        // SAFETY: the caller guarantees this process has no other thread.
        unsafe { std::env::set_var("PATH", with_login_path(&current, &login)) };
    }
    if WINDOW.set(Window { added, record, timeout }).is_ok() {
        let _ = std::thread::Builder::new().name("login-path".into()).spawn(login_path);
    }
}

/// The login shell's PATH: as [`adopt_login_path`] found it, or read now without changing
/// this process's own. Blocks while the shell starts the first time.
pub fn login_path() -> &'static LoginPath {
    LOGIN_PATH.get_or_init(|| {
        let shell = shell_program();
        let Some(window) = WINDOW.get() else {
            let path = login_shell_path(&shell, LOGIN_SHELL_TIMEOUT);
            return LoginPath { shell, path, added: None, late: 0 };
        };
        let path = login_shell_path(&shell, window.timeout);
        let current = std::env::var("PATH").unwrap_or_default();
        let late = path.as_ref().map_or(0, |login| missing_folders(&current, login).len());
        if let Ok(login) = &path {
            let _ = record(&window.record, &shell, login);
        }
        LoginPath { shell, path, added: Some(window.added), late }
    })
}

/// The PATH for a program ATOMICS starts that must find what the login shell's PATH has, such
/// as the Codex version probe, whose `codex` can run `env node`: this process's PATH, with the
/// login shell's folders it lacks. In a window that started with a recorded PATH, or none, this
/// is the shell's current one. Blocks while the shell starts the first time, so call it off the
/// window's thread.
pub fn child_path() -> Option<String> {
    let login = login_path_now()?;
    let current = std::env::var("PATH").unwrap_or_default();
    (!missing_folders(&current, &login).is_empty()).then(|| with_login_path(&current, &login))
}

/// Where ATOMICS keeps the login shell's PATH from its last read.
fn record_file() -> PathBuf {
    paths::atomics_home().join("login-path.json")
}

/// The login shell's PATH as an earlier ATOMICS recorded it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Recorded {
    shell: String,
    path: String,
}

/// The PATH recorded for `shell`. One recorded for another shell is not this one's.
fn recorded(file: &std::path::Path, shell: &str) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(file).ok()?.take(64 * 1024).read_to_string(&mut text).ok()?;
    let recorded: Recorded = serde_json::from_str(&text).ok()?;
    (recorded.shell == shell).then_some(recorded.path)
}

/// Records `shell`'s PATH whole, so a window starting meanwhile reads the old one or the new.
fn record(file: &std::path::Path, shell: &str, path: &str) -> std::io::Result<()> {
    write_whole(file, &serde_json::to_vec(&Recorded { shell: shell.into(), path: path.into() })?)
}

/// Replaces `file` with `bytes` at once: a reader finds the old file or the new one, never part.
fn write_whole(file: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = file.parent().ok_or_else(|| std::io::Error::other("no folder to write in"))?;
    std::fs::create_dir_all(dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary.write_all(bytes)?;
    temporary.persist(file).map_err(|error| error.error)?;
    Ok(())
}

/// Where the ATOMICS that serves a home's socket, a desktop or `atomics host`, records the
/// executable its hooks run ([`hook_executable`]). The hooks it passes bare Codex agents name that
/// path, so Codex's trust in them is for it, which the doctor checks from whichever install it runs.
fn executable_record(home: &std::path::Path) -> PathBuf {
    home.join("executable.json")
}

/// Records the path this process's hooks run it by as the one that serves `home`'s socket.
pub(crate) fn record_executable(home: &std::path::Path) -> std::io::Result<()> {
    let executable = hook_executable()?;
    write_whole(&executable_record(home), &serde_json::to_vec(&executable.to_string_lossy())?)
}

/// The executable of the desktop or host that last served `home`'s socket.
pub(crate) fn recorded_executable(home: &std::path::Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(executable_record(home)).ok()?;
    serde_json::from_str::<String>(&text).ok().map(PathBuf::from)
}

fn login_shell_path(shell: &str, timeout: Duration) -> Result<String, String> {
    login_shell_output(shell, "PATH", timeout)
        .and_then(|path| if path.trim().is_empty() { Err("it printed an empty PATH".into()) } else { Ok(path) })
}

/// `path`, with the folder Claude Code's installer and ATOMICS's own install to. A login may add it only at the next
/// login or not at all, and what was installed there has to run now.
fn with_user_programs(path: &str, home: &Path) -> String {
    let folder = home.join(".local/bin");
    if !folder.is_dir() || std::env::split_paths(path).any(|known| known == folder) {
        return path.to_string();
    }
    format!("{path}:{}", folder.display())
}

/// The folders of `login`, in its order and followed by those only `current` has. Entries that are
/// not absolute are dropped (see [`absolute_folders`]).
fn with_login_path(current: &str, login: &str) -> String {
    let mut folders: Vec<&str> = Vec::new();
    for folder in absolute_folders(login).chain(absolute_folders(current)) {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders.join(":")
}

/// The folders of `login` that `current` lacks, each once, leaving out those that are not absolute.
fn missing_folders<'a>(current: &str, login: &'a str) -> Vec<&'a str> {
    let have: Vec<&str> = current.split(':').collect();
    let mut missing: Vec<&str> = Vec::new();
    for folder in absolute_folders(login).filter(|folder| !have.contains(folder)) {
        if !missing.contains(&folder) {
            missing.push(folder);
        }
    }
    missing
}

/// The absolute folders of a PATH. An empty entry, `.` or `node_modules/.bin` from a shell's startup
/// files names a folder relative to wherever a program runs: on the PATH of ATOMICS, git, probes and
/// servers would pick up programs from a repository or the folder ATOMICS was started in.
fn absolute_folders(path: &str) -> impl Iterator<Item = &str> {
    path.split(':').filter(|folder| folder.starts_with('/'))
}

pub fn shell_program() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| if cfg!(target_os = "macos") { "/bin/zsh" } else { "/bin/sh" }.into())
}

/// The installed path, including after an atomic update of a running Linux app.
pub fn executable_path() -> std::io::Result<PathBuf> {
    if let Some(executable) = crate::build_info::executable() {
        return Ok(executable.to_path_buf());
    }
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

/// The path the hooks ATOMICS passes its agents run it by: one that survives updates where there is
/// one, else [`executable_path`]. Codex trusts a hook by its command, and a Nix store path changes with
/// every build, so a hook naming one needs review again after each update. A Nix package runs as
/// `<package>/bin/.atomics-wrapped` behind the wrapper `<package>/bin/atomics`, which NixOS links into
/// /run/current-system/sw/bin. A profile's link is used only while it leads into this very package,
/// so a hook never runs another build than the ATOMICS that passed it.
pub fn hook_executable() -> std::io::Result<PathBuf> {
    let executable = executable_path()?;
    let user = std::env::var("USER").unwrap_or_default();
    let home = paths::home();
    let links = [
        PathBuf::from("/run/current-system/sw/bin/atomics"),
        PathBuf::from(format!("/etc/profiles/per-user/{user}/bin/atomics")),
        home.join(".nix-profile/bin/atomics"),
        home.join(".local/state/nix/profile/bin/atomics"),
    ];
    Ok(profile_link(&executable, &links, Path::new("/nix/store")).unwrap_or(executable))
}

/// The first of `links` that leads into the same package of `store` as `executable`.
fn profile_link(executable: &Path, links: &[PathBuf], store: &Path) -> Option<PathBuf> {
    let package =
        |path: &Path| path.strip_prefix(store).ok()?.components().next().map(|name| name.as_os_str().to_owned());
    let own = package(executable)?;
    links
        .iter()
        .find(|link| std::fs::canonicalize(link).ok().and_then(|real| package(&real)).as_ref() == Some(&own))
        .cloned()
}

/// A variable as an interactive login shell sets it. `name` must be a plain identifier.
pub(crate) fn login_shell_var(name: &str) -> Option<String> {
    login_shell_output(&shell_program(), name, LOGIN_SHELL_TIMEOUT).ok()
}

/// The variable `name` as `shell`, started as an interactive login shell, prints it, or why it
/// did not. After `timeout` the shell's process group is stopped.
fn login_shell_output(shell: &str, name: &str, timeout: Duration) -> Result<String, String> {
    let (child, stdout) = start_login_shell(shell, name)?;
    finish_login_shell(child, stdout, name, timeout)
}

/// `shell` started as an interactive login shell that prints the variable `name`, and the file
/// its output goes to. The shell runs in a session of its own, without a terminal, so job control
/// and prompts in its startup files cannot stop it. Output goes to an unlinked file: nothing left
/// running can keep a reader waiting.
fn start_login_shell(shell: &str, name: &str) -> Result<(std::process::Child, std::fs::File), String> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let stdout = tempfile::tempfile().map_err(|err| format!("no file for its output: {err}"))?;
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &format!("printf '{MARKER}%s{MARKER}' \"${name}\"")])
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().map_err(|err| format!("no file for its output: {err}"))?)
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|err| format!("it did not start: {err}"))?;
    Ok((child, stdout))
}

/// The variable a shell from [`start_login_shell`] printed, or why it did not. A shell still
/// running `timeout` from now is stopped with its whole process group.
fn finish_login_shell(
    mut child: std::process::Child,
    mut stdout: std::fs::File,
    name: &str,
    timeout: Duration,
) -> Result<String, String> {
    use std::io::{Read, Seek};
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                // SAFETY: setsid made the shell the leader of a process group of its own.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(match result {
                    Err(err) => format!("waiting for it failed: {err}"),
                    _ => format!("it did not finish within {} s, and was stopped", timeout.as_secs()),
                });
            }
        }
    };
    let mut bytes = Vec::new();
    let _ = stdout.rewind().and_then(|()| stdout.take(1024 * 1024).read_to_end(&mut bytes));
    between_markers(&String::from_utf8_lossy(&bytes)).ok_or_else(|| format!("it printed no {name} ({status})"))
}

/// Brackets the value the login shell prints, so shell startup noise is ignored.
const MARKER: &str = "__ATOMICS__";

fn between_markers(text: &str) -> Option<String> {
    let start = text.find(MARKER)? + MARKER.len();
    let end = text[start..].find(MARKER)? + start;
    Some(text[start..end].to_string())
}

/// An environment variable of ATOMICS, or of the login shell when ATOMICS lacks it (as apps
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

/// What `env_var` knows without starting a shell: ATOMICS's own environment, or a value
/// the login shell already gave. Safe on the runtime thread.
pub fn known_env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty()).or_else(|| FOUND.lock().unwrap().get(name).cloned())
}

pub fn find_program(program: &str) -> Option<PathBuf> {
    let path = login_path_now().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    find_on_path(program, &path)
}

/// The login shell's PATH for what ATOMICS starts now: [`SHELL_PATH`], or what [`refresh_login_path`] read since.
pub fn login_path_now() -> Option<String> {
    REFRESHED_PATH.lock().unwrap().clone().or_else(|| SHELL_PATH.clone())
}

/// Reads the login shell's PATH again, so that what was installed while ATOMICS ran can be found and started without
/// a restart. Waits for the shell, up to five seconds: call it off the window's thread. Returns the PATH to look on.
pub fn refresh_login_path() -> String {
    let login = login_shell_path(&shell_program(), LOGIN_SHELL_TIMEOUT)
        .ok()
        .map(|path| with_user_programs(&path, &paths::home()));
    if let Some(login) = &login {
        *REFRESHED_PATH.lock().unwrap() = Some(login.clone());
    }
    let login = login.or_else(login_path_now).unwrap_or_default();
    with_login_path(&std::env::var("PATH").unwrap_or_default(), &login)
}

pub(crate) fn find_on_path(program: &str, path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Finds the configured editor, including distributions that name Helix `helix`.
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
/// `path`, when given, is the probe's PATH ([`child_path`]); else it has this process's own.
pub fn codex_supports_hooks(program: &str, path: Option<&str>, timeout: std::time::Duration) -> Option<bool> {
    use std::io::Read;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let (mut stdout, writer) = UnixStream::pair().ok()?;
    stdout.set_nonblocking(true).ok()?;
    let mut command = Command::new(program);
    command
        .arg("--version")
        // Not wherever ATOMICS was started: a wrapper that runs the agent elsewhere, such as in a
        // VM that has only some of this machine's folders, needs a folder it is sure to have.
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(writer)))
        .stderr(Stdio::null());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut bytes = Vec::new();
                let _ = (&mut stdout).take(1024).read_to_end(&mut bytes);
                return atomics_core::hook::codex_supports_hooks(&String::from_utf8_lossy(&bytes));
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
pub(crate) fn claude_settings_path() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| paths::home().join(".claude"))
        .join("settings.json")
}

/// An explicit instance home also isolates the mutable provider trust and settings of the bare
/// agents it starts.
pub(crate) fn instance_agent_env() -> Vec<(String, String)> {
    let Some(home) = paths::env_override("HOME").map(PathBuf::from) else { return Vec::new() };
    [("CODEX_HOME", "codex"), ("CLAUDE_CONFIG_DIR", "claude")]
        .into_iter()
        .map(|(key, dir)| (key.into(), home.join(dir).to_string_lossy().into_owned()))
        .collect()
}

/// The Codex home of a bare agent ATOMICS starts, where Codex keeps its hook trust: the instance's
/// own ([`instance_agent_env`]), else the `$CODEX_HOME` it inherits, else `~/.codex`.
pub(crate) fn agent_codex_home() -> PathBuf {
    instance_agent_env()
        .into_iter()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, home)| PathBuf::from(home))
        .or_else(|| std::env::var_os("CODEX_HOME").filter(|home| !home.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| paths::home().join(".codex"))
}

pub fn global_hooks_installed() -> bool {
    let Ok(exe) = hook_executable() else { return false };
    std::fs::read_to_string(claude_settings_path()).is_ok_and(|text| text.contains(&format!("{} hook", exe.display())))
}

pub fn legacy_hooks_installed() -> bool {
    std::fs::read_to_string(claude_settings_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .is_some_and(|settings| atomics_core::hook::has_cox_hooks(&settings))
}

pub fn install_global_hooks() -> anyhow::Result<PathBuf> {
    let path = claude_settings_path();
    let exe = hook_executable()?;
    let mut settings: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => {
            std::fs::write(path.with_extension("json.atomics-backup"), &text)?;
            serde_json::from_str(&text)?
        }
        Err(_) => serde_json::json!({}),
    };
    atomics_core::hook::merge_into_settings(&mut settings, &exe);
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
    fn the_users_own_programs_folder_counts_where_it_exists_and_only_once() {
        let home = crate::test_support::tempdir().unwrap();
        assert_eq!(with_user_programs("/usr/bin", home.path()), "/usr/bin", "no such folder yet");
        let local = home.path().join(".local/bin");
        std::fs::create_dir_all(&local).unwrap();
        let shown = local.display().to_string();
        assert_eq!(
            with_user_programs("/usr/bin", home.path()),
            format!("/usr/bin:{shown}"),
            "after what the login lists"
        );
        let listed = format!("{shown}:/usr/bin");
        assert_eq!(with_user_programs(&listed, home.path()), listed, "already there");
    }

    #[test]
    fn hooks_run_a_nix_package_by_the_profile_link_that_leads_to_it() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let store = root.join("nix/store");
        let package = |name: &str| {
            let bin = store.join(name).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            crate::test_support::write_program(bin.join("atomics"), "#!/bin/sh\nexec .atomics-wrapped \"$@\"\n");
            crate::test_support::write_program(bin.join(".atomics-wrapped"), "#!/bin/sh\nexit 0\n");
            bin
        };
        let running = package("aaaa-atomics-0.1.13");
        let other = package("bbbb-atomics-0.1.12");
        // A system profile, as NixOS links /run/current-system/sw/bin/atomics, and a user's.
        let link = |profile: &str, to: &Path| {
            let bin = root.join(profile).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::os::unix::fs::symlink(to.join("atomics"), bin.join("atomics")).unwrap();
            bin.join("atomics")
        };
        let system = link("system", &running);
        let user = link("user", &running);
        let stale = link("stale", &other);
        let absent = root.join("absent/bin/atomics");
        let executable = running.join(".atomics-wrapped");

        assert_eq!(profile_link(&executable, &[absent.clone(), system.clone(), user], &store), Some(system.clone()));
        // A link to another build is passed over: the hook would run another ATOMICS than this one.
        assert_eq!(profile_link(&executable, &[stale.clone(), system.clone()], &store), Some(system));
        assert_eq!(profile_link(&executable, &[stale.clone(), absent], &store), None, "no link to this build");
        // Outside the store the program's own path stays, as a cargo build's or the app's does.
        let built = root.join("target/debug/atomics");
        assert_eq!(profile_link(&built, &[stale], &store), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn replaced_linux_executable_still_resolves_the_installation() {
        use crate::test_support::{CHILD_TIMEOUT, TestChild};
        use std::time::{Duration, Instant};

        const CHILD: &str = "ATOMICS_REPLACED_EXE_TEST";
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
            let target = crate::restart::Target::current(None).unwrap();
            assert_eq!(target.executable, executable);
            target.validate().unwrap();
            return;
        }
        let dir = crate::test_support::tempdir().unwrap();
        let executable = dir.path().join("atomics with spaces");
        crate::test_support::copy_program(std::env::current_exe().unwrap(), &executable);
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
        crate::test_support::write_program(&replacement, "#!/bin/sh\ntest \"$1\" = --version\n");
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
        use std::time::{Duration, Instant};
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("codex");
        crate::test_support::write_program(&program, "#!/bin/sh\nprintf 'codex-cli 0.165.0\\n'\nexec sleep 30\n");
        let started = Instant::now();
        assert_eq!(codex_supports_hooks(program.to_str().unwrap(), None, Duration::from_millis(100)), None);
        assert!(started.elapsed() < Duration::from_secs(5));
        for script in ["#!/bin/sh\nexit 1\n", "#!/bin/sh\nprintf 'not a version\\n'\n"] {
            crate::test_support::write_program(&program, script);
            assert_eq!(codex_supports_hooks(program.to_str().unwrap(), None, Duration::from_secs(10)), None);
        }
        assert_eq!(codex_supports_hooks("/no/such/codex", None, Duration::from_secs(1)), None);
    }

    #[test]
    fn version_probe_runs_in_the_root_folder() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("codex");
        // Like the VM wrapper, it answers only from a folder the other side is sure to have.
        crate::test_support::write_program(
            &program,
            "#!/bin/sh\n[ \"$(pwd)\" = / ] || exit 66\nprintf 'codex-cli 0.166.0\\n'\n",
        );
        assert_eq!(
            codex_supports_hooks(program.to_str().unwrap(), None, std::time::Duration::from_secs(10)),
            Some(true)
        );
    }

    #[test]
    fn successful_version_probe_tolerates_slow_startup() {
        let dir = crate::test_support::tempdir().unwrap();
        let program = dir.path().join("codex");
        crate::test_support::write_program(&program, "#!/bin/sh\nsleep 1.1\nprintf 'codex-cli 0.165.0\\n'\n");
        assert_eq!(
            codex_supports_hooks(program.to_str().unwrap(), None, std::time::Duration::from_secs(10)),
            Some(true)
        );
    }

    #[test]
    fn login_shell_output_is_read_between_the_markers() {
        let noise = "Last login: today\n__ATOMICS__/opt/homebrew/bin:/usr/bin__ATOMICS__";
        assert_eq!(between_markers(noise).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(between_markers("__ATOMICS____ATOMICS__").as_deref(), Some(""));
        assert_eq!(between_markers("no markers"), None);
    }

    #[test]
    fn the_login_path_comes_first_and_a_path_that_has_it_all_is_left_alone() {
        let login = "/opt/tools/bin:/usr/bin:/bin";
        assert_eq!(missing_folders("/usr/bin:/bin", login), ["/opt/tools/bin"]);
        assert_eq!(with_login_path("/usr/bin:/bin:/usr/sbin", login), "/opt/tools/bin:/usr/bin:/bin:/usr/sbin");
        // A terminal's PATH, in its own order and with more: nothing is missing.
        assert!(missing_folders("/bin:/home/user/venv/bin:/usr/bin:/opt/tools/bin", login).is_empty());
        // Empty entries, which mean the working folder, are dropped; a repeated folder counts once.
        assert_eq!(missing_folders("/bin", ":/opt/a::/opt/a:/bin"), ["/opt/a"]);
        assert_eq!(with_login_path("/bin::", ":/opt/a::/opt/a:/bin"), "/opt/a:/bin");
    }

    #[test]
    fn relative_folders_of_a_login_shell_never_reach_the_path_of_atomics() {
        // As an rc file that adds the working folder and a project's tools would print it.
        let login = ".:node_modules/.bin:/opt/tools/bin:bin:./vendor/bin:/usr/bin:~/bin";
        assert_eq!(missing_folders("/usr/bin:/bin", login), ["/opt/tools/bin"]);
        assert_eq!(with_login_path("/usr/bin:/bin", login), "/opt/tools/bin:/usr/bin:/bin");
        // Nor do those of the PATH it was started with, once it changes.
        assert_eq!(with_login_path(".:/usr/bin:node_modules/.bin", login), "/opt/tools/bin:/usr/bin");
        // A login PATH whose only extra folders are relative changes nothing.
        assert!(missing_folders("/usr/bin", ".:node_modules/.bin:/usr/bin").is_empty());

        let dir = crate::test_support::tempdir().unwrap();
        let shell = dir.path().join("shell");
        crate::test_support::write_program(&shell, format!("#!/bin/sh\nprintf '__ATOMICS__{login}__ATOMICS__'\n"));
        let (found, merged) = LoginPath::read(shell.to_string_lossy().into(), "/usr/bin:/bin", Duration::from_secs(10));
        assert_eq!(found.added, Some(1));
        assert_eq!(merged.as_deref(), Some("/opt/tools/bin:/usr/bin:/bin"));
        let (_, merged) =
            LoginPath::read(shell.to_string_lossy().into(), "/opt/tools/bin:/usr/bin", Duration::from_secs(10));
        assert_eq!(merged, None, "relative folders alone are no reason to change PATH");
    }

    #[test]
    fn the_login_shell_is_bounded_and_without_its_path_atomics_keeps_its_own() {
        use std::time::Instant;
        let dir = crate::test_support::tempdir().unwrap();
        let shell = |name: &str, script: &str| {
            let path = dir.path().join(name);
            crate::test_support::write_program(&path, format!("#!/bin/sh\n{script}\n"));
            path.to_string_lossy().into_owned()
        };
        let good = shell("good", "echo 'startup noise'; printf '__ATOMICS__/opt/tools/bin:/usr/bin__ATOMICS__'");
        let (found, merged) = LoginPath::read(good.clone(), "/usr/bin:/bin", Duration::from_secs(10));
        assert_eq!(found.path.as_deref(), Ok("/opt/tools/bin:/usr/bin"));
        assert_eq!(merged.as_deref(), Some("/opt/tools/bin:/usr/bin:/bin"));
        let expected = format!("PATH of login shell {good}: 1 folder added to the PATH ATOMICS was started with");
        assert_eq!(found.describe(), (true, expected));
        let (found, merged) = LoginPath::read(good.clone(), "/usr/bin:/opt/tools/bin", Duration::from_secs(10));
        assert_eq!((found.added, merged), (Some(0), None));
        assert!(found.describe().1.contains("which has all of login shell"));

        // One that hangs is stopped in time, however far it got.
        let hangs = shell("hangs", "exec sleep 30");
        let (started, timeout) = (Instant::now(), crate::test_support::scaled_timeout(Duration::from_secs(1)));
        let (found, merged) = LoginPath::read(hangs, "/usr/bin:/bin", timeout);
        assert!(started.elapsed() < timeout + Duration::from_secs(10));
        assert!(found.path.as_ref().unwrap_err().contains("did not finish within"), "{found:?}");
        assert_eq!(merged, None);
        let (ok, text) = found.describe();
        assert!(!ok && text.contains("(warning)"), "{text}");

        // What a stopped shell started stops with it. Its deadline starts only once it has started
        // its child, however long a loaded machine takes to get there.
        let starts = shell(
            "starts",
            "sleep 30 &\necho $! > \"${0%/*}/started.new\"\nmv \"${0%/*}/started.new\" \"${0%/*}/started\"\nwait",
        );
        let (child, stdout) = start_login_shell(&starts, "PATH").unwrap();
        let guard = Instant::now() + crate::test_support::CHILD_TIMEOUT;
        let sleeper: i32 = loop {
            if let Ok(pid) = std::fs::read_to_string(dir.path().join("started")) {
                break pid.trim().parse().unwrap();
            }
            assert!(Instant::now() < guard, "the shell never started its child");
            std::thread::sleep(Duration::from_millis(10));
        };
        let stopped = finish_login_shell(child, stdout, "PATH", Duration::ZERO).unwrap_err();
        assert!(stopped.contains("did not finish within"), "{stopped}");
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(5));
        // SAFETY: signal 0 only checks that the process exists.
        while unsafe { libc::kill(sleeper, 0) } == 0 {
            assert!(Instant::now() < deadline, "what the shell started must stop with it");
            std::thread::sleep(Duration::from_millis(10));
        }

        for (script, reason) in [
            ("exit 0", "printed no PATH"),
            ("printf '__ATOMICS__ __ATOMICS__'", "empty PATH"),
            ("printf '__ATOMICS__/opt/tools/bin__ATOMICS__'; exit 3", ""),
        ] {
            let (found, merged) = LoginPath::read(shell("other", script), "/usr/bin", Duration::from_secs(10));
            match reason {
                // Its exit status does not matter once it printed the PATH: interactive startup files often fail.
                "" => assert_eq!(merged.as_deref(), Some("/opt/tools/bin:/usr/bin")),
                reason => assert!(found.path.unwrap_err().contains(reason) && merged.is_none()),
            }
        }
        let (found, merged) = LoginPath::read("/nonexistent/shell".into(), "/usr/bin", Duration::from_secs(10));
        assert!(found.path.unwrap_err().contains("did not start") && merged.is_none());
    }

    #[test]
    fn a_window_takes_on_the_recorded_path_at_once_and_reads_the_shell_in_the_background() {
        use crate::test_support::{CHILD_TIMEOUT, TestChild};
        const CHILD: &str = "ATOMICS_WINDOW_LOGIN_PATH_TEST";
        const RECORDED: &str = "/opt/recorded/bin:/usr/bin";
        const FRESH: &str = "/opt/fresh/bin:/opt/recorded/bin:/usr/bin";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = PathBuf::from(dir);
            let record = dir.join("login-path.json");
            let started_with = std::env::var("PATH").unwrap();
            // SAFETY: the test harness's own threads only wait for this test; nothing reads the
            // environment while it changes.
            unsafe {
                start_login_path_with(record.clone(), crate::test_support::scaled_timeout(Duration::from_secs(30)))
            };
            // The shell cannot finish before `go` exists: a window that waited for it would have
            // read it (as failed, at its timeout) before getting here.
            assert!(LOGIN_PATH.get().is_none(), "the window waited for its login shell");
            let path = std::env::var("PATH").unwrap();
            std::fs::write(dir.join("go"), "").unwrap();

            let found = login_path();
            assert_eq!(found.path.as_deref(), Ok(FRESH));
            let (ok, text) = found.describe();
            assert!(ok, "{text}");
            // The next start takes on what the shell gives now.
            assert_eq!(recorded(&record, &shell_program()).as_deref(), Some(FRESH));
            match std::fs::read_to_string(dir.join("scenario")).unwrap().as_str() {
                "dock" => {
                    assert_eq!(path, "/opt/recorded/bin:/usr/bin:/bin");
                    assert_eq!((found.added, found.late), (Some(1), 1));
                    assert!(
                        text.contains("as recorded at an earlier start") && text.contains("1 folder the shell has now")
                    );
                    assert_eq!(child_path().as_deref(), Some("/opt/fresh/bin:/opt/recorded/bin:/usr/bin:/bin"));
                }
                "terminal" => {
                    assert_eq!(path, started_with);
                    assert_eq!((found.added, found.late), (Some(0), 0));
                    assert!(text.contains("which has all of login shell"), "{text}");
                    assert_eq!(child_path(), None);
                }
                "first" => {
                    assert_eq!(path, started_with);
                    assert_eq!((found.added, found.late), (Some(0), 2));
                    assert!(text.contains("2 folders of login shell"), "{text}");
                    assert_eq!(child_path().as_deref(), Some("/opt/fresh/bin:/opt/recorded/bin:/usr/bin:/bin"));
                }
                scenario => panic!("{scenario}"),
            }
            return;
        }

        for (scenario, started_with) in [
            // Started from the Dock or a launcher, with a PATH recorded at an earlier start.
            ("dock", "/usr/bin:/bin"),
            // Started from a terminal, whose PATH has all the login shell's folders.
            ("terminal", "/bin:/opt/fresh/bin:/usr/bin:/opt/recorded/bin"),
            // Started with nothing recorded.
            ("first", "/usr/bin:/bin"),
        ] {
            let dir = crate::test_support::tempdir().unwrap();
            // A slow login shell: it gives its PATH only once the test has seen the window start.
            let shell = dir.path().join("login-shell");
            crate::test_support::write_program(
                &shell,
                format!(
                    "#!/bin/sh\n[ \"$1 $2 $3\" = '-l -i -c' ] || exit 64\nwhile [ ! -e \"${{0%/*}}/go\" ]; do '{}' \
                     0.05; done\nPATH='{FRESH}'\neval \"$4\"\n",
                    crate::test_support::program("sleep").replace('\'', "'\\''")
                ),
            );
            std::fs::write(dir.path().join("scenario"), scenario).unwrap();
            if scenario != "first" {
                record(&dir.path().join("login-path.json"), &shell.to_string_lossy(), RECORDED).unwrap();
            }
            let mut child = TestChild::spawn(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "setup::tests::a_window_takes_on_the_recorded_path_at_once_and_reads_the_shell_in_the_background",
                        "--nocapture",
                    ])
                    .env(CHILD, dir.path())
                    .env("HOME", dir.path())
                    .env("SHELL", &shell)
                    .env("PATH", started_with)
                    .env("ATOMICS_HOME", dir.path().join("state")),
                CHILD_TIMEOUT,
            )
            .unwrap();
            let output = child.wait_output().unwrap();
            assert!(
                output.status.success(),
                "{scenario}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn the_login_path_is_recorded_whole_and_for_its_shell_only() {
        let dir = crate::test_support::tempdir().unwrap();
        let file = dir.path().join("state").join("login-path.json");
        assert_eq!(recorded(&file, "/bin/zsh"), None);
        record(&file, "/bin/zsh", "/opt/a:/usr/bin").unwrap();
        assert_eq!(recorded(&file, "/bin/zsh").as_deref(), Some("/opt/a:/usr/bin"));
        // SHELL changed since: that shell's PATH is not this one's.
        assert_eq!(recorded(&file, "/bin/bash"), None);
        record(&file, "/bin/zsh", "/opt/b:/usr/bin").unwrap();
        assert_eq!(recorded(&file, "/bin/zsh").as_deref(), Some("/opt/b:/usr/bin"));
        assert_eq!(std::fs::read_dir(file.parent().unwrap()).unwrap().count(), 1, "no temporary file is left");
        std::fs::write(&file, "not json").unwrap();
        assert_eq!(recorded(&file, "/bin/zsh"), None);
    }

    #[test]
    fn env_var_reads_atomics_first_and_passes_only_plain_names_to_the_shell() {
        assert_eq!(env_var("HOME"), std::env::var("HOME").ok());
        assert!(env_var("ATOMICS_UNSET; exit 1").is_none());
        assert!(env_var("1ABC").is_none());
    }
}
