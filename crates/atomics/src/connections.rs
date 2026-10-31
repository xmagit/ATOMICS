//! MCP connections: the servers sandboxed agents use through sbx's MCP gateway. sbx's registry is
//! the list. Every server registered there, other than ATOMICS's own `atomics-<session>` servers, is
//! loaded into every sandboxed session once it is ready. Connect registers a server and runs sbx's
//! OAuth flow, opening its authorization page in this machine's browser; Disconnect removes it.
//!
//! Import reads server definitions only, from Claude Code's and Codex's config files. ATOMICS keeps
//! what it imported or was given in `connections.json` in its state folder, never a value: a
//! server's environment goes to the OS keychain with the user's consent, and `atomics mcp-host`
//! hands it to the server's process when sbx starts it. sbx's output during authorization is never
//! shown, since it carries the OAuth state.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use atomics_core::isolation;
use atomics_core::mcp_import::{self, Definition, Source, Transport};
use serde_json::Value;

use crate::credentials::Store;
use crate::sandbox::sbx;
use crate::sandbox_ownership as ownership;

/// sbx answers within a second; this bounds a stuck daemon.
const TIMEOUT: Duration = Duration::from_secs(60);
/// How long sbx's OAuth flow may wait for the user in the browser.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Config files are small; `~/.claude.json` grows with history but stays far below this.
const MAX_CONFIG: u64 = 64 * 1024 * 1024;

// ── Import ────────────────────────────────────────────────────────────────

/// The config files Import reads on this platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Sources {
    pub files: Vec<(Source, PathBuf)>,
}

impl Sources {
    pub fn current() -> Self {
        Self::at(&atomics_core::paths::home(), cfg!(target_os = "macos"), |name| std::env::var(name).ok())
    }

    /// Claude Code's user config (`$CLAUDE_CONFIG_DIR/.claude.json`, else `~/.claude.json`), its
    /// managed servers (`/Library/Application Support/ClaudeCode` on macOS, `/etc/claude-code` on
    /// Linux) and Codex's config (`$CODEX_HOME/config.toml`, else `~/.codex/config.toml`).
    pub fn at(home: &Path, mac: bool, env: impl Fn(&str) -> Option<String>) -> Self {
        let dir = |name: &str, default: PathBuf| {
            env(name).filter(|value| Path::new(value).is_absolute()).map(PathBuf::from).unwrap_or(default)
        };
        let managed = if mac {
            "/Library/Application Support/ClaudeCode/managed-mcp.json"
        } else {
            "/etc/claude-code/managed-mcp.json"
        };
        Self {
            files: vec![
                (Source::ClaudeCode, dir("CLAUDE_CONFIG_DIR", home.to_path_buf()).join(".claude.json")),
                (Source::ClaudeManaged, PathBuf::from(managed)),
                (Source::Codex, dir("CODEX_HOME", home.join(".codex")).join("config.toml")),
            ],
        }
    }

    fn file(&self, source: Source) -> Option<&Path> {
        self.files.iter().find(|(kind, _)| *kind == source).map(|(_, file)| file.as_path())
    }
}

/// What Import found: definitions, and a note for each file it could not read.
#[derive(Debug, Default)]
pub struct Imported {
    pub definitions: Vec<Definition>,
    pub notes: Vec<String>,
}

/// Reads the server definitions in `sources`. A missing file is skipped; an unreadable one is
/// noted without quoting it.
pub fn import(sources: &Sources) -> Imported {
    let mut imported = Imported::default();
    for (source, file) in &sources.files {
        let text = match read_config(file) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(error) => {
                imported.notes.push(format!("{}: {error}", source.label()));
                continue;
            }
        };
        let found = match source {
            Source::Codex => mcp_import::codex(&text),
            _ => mcp_import::claude(&text, *source),
        };
        match found {
            Ok(found) => imported.definitions.extend(found),
            Err(error) => imported.notes.push(format!("{}: {} is {error}", source.label(), file.display())),
        }
    }
    imported.definitions = mcp_import::dedup(std::mem::take(&mut imported.definitions));
    imported
}

fn read_config(file: &Path) -> Result<Option<String>> {
    let opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot read {}", file.display()),
    };
    let mut text = String::new();
    opened
        .take(MAX_CONFIG + 1)
        .read_to_string(&mut text)
        .map_err(|_| anyhow::anyhow!("cannot read {}", file.display()))?;
    if text.len() as u64 > MAX_CONFIG {
        bail!("{} is too large to read", file.display());
    }
    Ok(Some(text))
}

/// The environment `definition` has in its source file, read again only to store it.
pub fn env_values(sources: &Sources, definition: &Definition) -> Result<BTreeMap<String, String>> {
    let file = sources.file(definition.source).context("this server has no source file to read")?;
    let text = read_config(file)?.with_context(|| format!("{} is gone", file.display()))?;
    mcp_import::env_values(&text, definition).map_err(anyhow::Error::msg)
}

/// The definitions ATOMICS imported or was given, from `connections.json` in `home`.
pub fn definitions(home: &Path) -> Vec<Definition> {
    std::fs::read(home.join("connections.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<Definition>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|definition| mcp_import::valid_name(&definition.name))
        .collect()
}

/// Adds `found` to the definitions in `home`, replacing those of the same name.
pub fn remember(home: &Path, found: &[Definition]) -> Result<Vec<Definition>> {
    let mut all = definitions(home);
    all.retain(|kept| !found.iter().any(|new| new.name == kept.name));
    all.extend(found.iter().cloned());
    all.sort_by(|a, b| a.name.cmp(&b.name));
    std::fs::create_dir_all(home).with_context(|| format!("creating {}", home.display()))?;
    let file = home.join("connections.json");
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&all)?)?;
    std::fs::rename(&temporary, &file)?;
    Ok(all)
}

/// A definition for a remote server added by name and URL, or `None` when either is unusable.
pub fn added(name: &str, url: &str) -> Option<Definition> {
    let url = url.trim();
    let name = mcp_import::sbx_name(name)?;
    let host = url.strip_prefix("https://")?.split(['/', '?', '#']).next()?;
    if host.is_empty() || host.contains('@') || url.chars().any(char::is_whitespace) || url.len() > 2048 {
        return None;
    }
    Some(Definition {
        name,
        source: Source::Added,
        key: String::new(),
        project: None,
        transport: Transport::Url { url: url.to_string() },
        env: Vec::new(),
        skipped: 0,
    })
}

// ── sbx ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connected,
    NeedsAuth,
    /// sbx's own word for another state, such as `unsupported`.
    Error(String),
}

impl Status {
    pub fn label(&self) -> String {
        match self {
            Status::Connected => "connected".into(),
            Status::NeedsAuth => "needs sign-in".into(),
            Status::Error(status) => format!("error: {status}"),
        }
    }
}

/// A server registered with sbx.
#[derive(Debug, Clone, PartialEq)]
pub struct Registered {
    pub name: String,
    pub status: Status,
    /// `remote http`, `local stdio` and the like.
    pub transport: String,
    /// Exact remote endpoint, used only to identify aliases, never displayed without redaction.
    pub url: Option<String>,
}

impl Registered {
    pub fn same_url(&self, other: &Self) -> bool {
        self.url.is_some() && self.url == other.url
    }
}

/// The servers registered with sbx, without ATOMICS's per-sandbox ones.
pub fn registered(program: &str) -> Result<Vec<Registered>> {
    let listed: Value = serde_json::from_str(&sbx(program, &["mcp", "ls", "--json"], false, TIMEOUT)?)
        .context("sbx gave no readable MCP server list")?;
    let word = |value: &Value| -> Option<String> {
        value.as_str().filter(|text| text.len() <= 40 && !text.contains(['\n', '\r'])).map(str::to_string)
    };
    Ok(listed
        .as_array()
        .context("sbx gave no readable MCP server list")?
        .iter()
        .filter_map(|server| {
            let name = server["name"].as_str()?;
            if name.starts_with(isolation::NAME_PREFIX) {
                return None;
            }
            let status = match server["status"].as_str() {
                Some("ready") => Status::Connected,
                Some("needs-auth") => Status::NeedsAuth,
                _ => Status::Error(word(&server["status"]).unwrap_or_else(|| "unavailable".into())),
            };
            Some(Registered {
                name: name.to_string(),
                status,
                transport: word(&server["transport"]).unwrap_or_default(),
                // sbx's list omits the endpoint. Inspect only remote registrations; ignore
                // every other field (including header and OAuth settings).
                url: server["transport"].as_str().filter(|kind| kind.starts_with("remote ")).and_then(|_| {
                    sbx(program, &["mcp", "inspect", name, "--json"], false, TIMEOUT)
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|details| details["url"].as_str().map(str::to_string))
                }),
            })
        })
        .collect())
}

/// The program that opens a page on Windows: `cmd /c start "" "<url>"`.
const WINDOWS_START: &str = "cmd";

/// The arguments cmd.exe gets to open `url`, quoted as one piece so `&` and `?` in the address stay in it.
/// An address with a quote or a control character is refused instead of escaped.
pub(crate) fn windows_start_args(url: &str) -> Option<String> {
    (!url.chars().any(|c| c == '"' || c.is_control()) && url.starts_with("http"))
        .then(|| format!("/c start \"\" \"{url}\""))
}

/// How ATOMICS opens an authorization page: `open` on macOS, `xdg-open` on Linux, `cmd /c start` on Windows.
#[derive(Debug, Clone, PartialEq)]
pub struct Browser {
    program: String,
}

impl Browser {
    pub fn current() -> Self {
        Self::at(if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(windows) {
            WINDOWS_START
        } else {
            "xdg-open"
        })
    }

    pub fn at(program: impl Into<String>) -> Self {
        Self { program: program.into() }
    }

    pub fn open(&self, url: &str) -> Result<()> {
        let mut command = Command::new(&self.program);
        if self.program == WINDOWS_START {
            let line = windows_start_args(url).context("not an address to open")?;
            #[cfg(windows)]
            std::os::windows::process::CommandExt::raw_arg(&mut command, line);
            #[cfg(not(windows))]
            command.arg(line);
        } else {
            command.arg(url);
        }
        command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        let mut child = command.spawn().with_context(|| format!("could not run {}", self.program))?;
        // `open` returns at once; some `xdg-open` handlers wait for the browser.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait()? {
                Some(status) if status.success() => return Ok(()),
                Some(_) => bail!("{} could not open the browser", self.program),
                None if Instant::now() >= deadline => {
                    std::thread::spawn(move || child.wait());
                    return Ok(());
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }
}

/// Runs `sbx mcp auth <name>` with sbx's own browser opening turned off, and opens the
/// authorization URL it prints with `browser`. Waits for the user to finish in the browser.
pub fn authorize(program: &str, name: &str, browser: &Browser) -> Result<()> {
    authorize_within(program, name, browser, AUTH_TIMEOUT)
}

fn authorize_within(program: &str, name: &str, browser: &Browser, timeout: Duration) -> Result<()> {
    anyhow::ensure!(mcp_import::valid_name(name), "not a connection name ATOMICS manages");
    let mut command = Command::new(program);
    command.args(["mcp", "auth", name]).env("SBX_NO_BROWSER_OPEN", "1").process_group(0);
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::sandbox::sbx_env(&mut command, false);
    let mut child = command.spawn().with_context(|| format!("running {program}"))?;
    // sbx asks for consent on a terminal in some versions; it reads this when it does.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"y\n");
    }
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [child.stdout.take().map(|out| Box::new(out) as Box<dyn Read + Send>)]
        .into_iter()
        .chain([child.stderr.take().map(|err| Box::new(err) as Box<dyn Read + Send>)])
        .flatten()
        .map(|stream| {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream.take(1024 * 1024)).lines() {
                    let Ok(line) = line else { break };
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    drop(tx);
    let deadline = Instant::now() + timeout;
    let kill = |child: &mut std::process::Child| {
        // SAFETY: process_group(0) gave sbx its own group, which holds its helpers.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    };
    let (mut asked, mut opened) = (false, false);
    loop {
        let wait = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait.min(Duration::from_millis(200))) {
            Ok(line) => {
                let line = line.trim();
                if line.contains("Open this URL") || line.contains("in your browser") {
                    asked = true;
                }
                let url =
                    line.split_whitespace().find(|word| word.starts_with("https://") || word.starts_with("http://"));
                if asked
                    && !opened
                    && let Some(url) = url
                {
                    opened = true;
                    if let Err(error) = browser.open(url) {
                        kill(&mut child);
                        return Err(error.context(format!("authorizing {name}")));
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            bail!("authorizing {name} took longer than {} minutes; try Connect again", timeout.as_secs() / 60);
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill(&mut child);
            bail!("authorizing {name} did not finish; try Connect again");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        bail!("authorizing {name} did not complete (cancelled or refused in the browser); try Connect again");
    }
    Ok(())
}

/// What connecting a server needs.
#[derive(Debug, Clone)]
pub struct Connect {
    pub home: PathBuf,
    pub sbx: String,
    pub name: String,
    /// How to register it when sbx does not have it yet.
    pub definition: Option<Definition>,
    /// Its environment, stored in the OS keychain first (the user agreed to that).
    pub store_env: bool,
    pub sources: Sources,
    /// ATOMICS's executable, which sbx runs for a command server (`atomics mcp-host`).
    pub exe: PathBuf,
    pub browser: Browser,
    /// The sandboxes of running sessions, which get the server as soon as it is connected.
    pub sandboxes: Vec<String>,
}

/// Connects a server: registers it with sbx unless sbx has it, runs its OAuth flow when it needs
/// one, and loads it into `sandboxes`. Returns the sandboxes it could not be loaded into.
pub fn connect(job: &Connect, store: &Store) -> Result<Vec<String>> {
    anyhow::ensure!(mcp_import::valid_name(&job.name), "not a connection name ATOMICS manages");
    let find = || -> Result<Option<Registered>> {
        Ok(registered(&job.sbx)?.into_iter().find(|server| server.name == job.name))
    };
    let mut server = find()?;
    if server.is_none() {
        let definition = job.definition.as_ref().with_context(|| {
            format!("sbx has no server {}; import it or add its URL in Settings → Connections", job.name)
        })?;
        let args = match &definition.transport {
            Transport::Url { url } => isolation::connection_url_args(&job.name, url),
            Transport::Command { command, args, cwd } => {
                let stored = job.store_env && !definition.env.is_empty();
                if stored {
                    let env = env_values(&job.sources, definition)?;
                    if !env.is_empty() {
                        store.put_mcp_env(&job.name, &env)?;
                    }
                }
                isolation::connection_command_args(&job.name, &job.exe, command, args, cwd.as_deref(), stored)
            }
        };
        sbx(&job.sbx, &args, false, TIMEOUT).map_err(|error| redacted(error, &format!("registering {}", job.name)))?;
        server = find()?;
    }
    match server.map(|server| server.status) {
        Some(Status::Connected) => {}
        Some(Status::NeedsAuth) => {
            authorize(&job.sbx, &job.name, &job.browser)?;
            match find()?.map(|server| server.status) {
                Some(Status::Connected) => {}
                Some(status) => bail!("{} is still {} after authorization", job.name, status.label()),
                None => bail!("{} is no longer registered with sbx", job.name),
            }
        }
        Some(Status::Error(status)) => bail!("sbx reports {} as {status}; check `sbx mcp ls`", job.name),
        None => bail!("sbx did not register {}", job.name),
    }
    let servers = registered(&job.sbx)?;
    let server =
        servers.iter().find(|server| server.name == job.name).context("server disappeared after connecting")?;
    Ok(job
        .sandboxes
        .iter()
        .filter(|sandbox| {
            ownership::with_connections(&job.home, sandbox, |loaded| {
                load_server(&job.sbx, sandbox, server, &servers, loaded, false)
            })
            .map_or(true, |loaded| !loaded)
        })
        .cloned()
        .collect())
}

/// A gateway cannot serve duplicate tool names. Keep the connection already loaded for this
/// exact URL, even when a newly connected alias sorts before it. Different URLs stay distinct.
fn load_server(
    program: &str,
    sandbox: &str,
    server: &Registered,
    servers: &[Registered],
    loaded: &mut Vec<String>,
    ssh: bool,
) -> bool {
    loaded.retain(|name| servers.iter().any(|known| known.name == *name));
    if loaded.iter().any(|name| {
        servers.iter().any(|other| {
            other.name == *name
                && other.name != server.name
                && other.status == Status::Connected
                && server.same_url(other)
        })
    }) {
        return true;
    }
    let serving = if sbx(program, &isolation::mcp_load_args(&server.name, sandbox), ssh, TIMEOUT).is_ok() {
        server
    } else {
        // A pre-upgrade sandbox (or a crash before saving the ledger) can already have an
        // alias loaded. sbx accepts loading that name again; discover it without unloading
        // or replacing the tools the agent is using.
        let Some(other) = servers.iter().find(|other| {
            other.name != server.name
                && other.status == Status::Connected
                && server.same_url(other)
                && sbx(program, &isolation::mcp_load_args(&other.name, sandbox), ssh, TIMEOUT).is_ok()
        }) else {
            return false;
        };
        other
    };
    if !loaded.contains(&serving.name) {
        loaded.push(serving.name.clone());
    }
    true
}

/// Disconnects server `name`: unloads it from `sandboxes`, removes its registration and OAuth
/// credential from sbx and its stored environment from the OS keychain.
pub fn disconnect(home: &Path, program: &str, name: &str, sandboxes: &[String], store: &Store) -> Result<()> {
    anyhow::ensure!(mcp_import::valid_name(name), "not a connection name ATOMICS manages");
    for sandbox in sandboxes {
        ownership::with_connections(home, sandbox, |loaded| {
            // A sandbox that never had it, or is gone, has nothing to unload.
            let _ = sbx(program, &["mcp", "unload", name, "--sandbox", sandbox], false, TIMEOUT);
            loaded.retain(|other| other != name);
        })?;
    }
    if registered(program)?.iter().any(|server| server.name == name) {
        sbx(program, &["mcp", "rm", "--force", name], false, TIMEOUT)
            .map_err(|error| redacted(error, &format!("removing {name} from sbx")))?;
    }
    store.remove_mcp_env(name)?;
    // If the removed registration served another alias's URL, let that alias take over.
    for sandbox in sandboxes {
        let _ = load_into(home, program, sandbox, &[], false);
    }
    Ok(())
}

/// Loads every connected server, and each server the configuration lists, into a session's
/// sandbox. Returns the servers it needs that are not connected: registered but waiting for
/// authorization, or configured but unknown to sbx. Nothing here stops the agent from starting.
pub fn load_into(home: &Path, program: &str, sandbox: &str, configured: &[String], ssh_agent: bool) -> Vec<String> {
    let configured: Vec<&str> = configured.iter().map(|name| name.trim()).filter(|name| !name.is_empty()).collect();
    let mut needed = Vec::new();
    match registered(program) {
        Ok(mut servers) => {
            servers.sort_by(|a, b| a.name.cmp(&b.name));
            let loaded = ownership::with_connections(home, sandbox, |loaded| {
                for server in &servers {
                    match server.status {
                        Status::Connected => {
                            if !load_server(program, sandbox, server, &servers, loaded, ssh_agent) {
                                needed.push(server.name.clone());
                            }
                        }
                        Status::NeedsAuth => needed.push(server.name.clone()),
                        Status::Error(_) if configured.contains(&server.name.as_str()) => {
                            needed.push(server.name.clone())
                        }
                        Status::Error(_) => {}
                    }
                }
            });
            if loaded.is_err() {
                needed.extend(
                    servers
                        .iter()
                        .filter(|server| server.status == Status::Connected)
                        .map(|server| server.name.clone()),
                );
            }
            needed.extend(
                configured
                    .iter()
                    .filter(|name| !servers.iter().any(|server| server.name == **name))
                    .map(|name| name.to_string()),
            );
        }
        // Without endpoint metadata, loading aliases could collide. Keep existing tools and
        // offer to retry the configured servers when sbx's registry is available again.
        Err(_) => needed.extend(configured.iter().map(|name| name.to_string())),
    }
    needed.sort();
    needed.dedup();
    needed
}

/// sbx's error with URLs shortened to scheme, host and path: a URL's query can hold a key.
fn redacted(error: anyhow::Error, what: &str) -> anyhow::Error {
    let text = format!("{error:#}");
    let text: Vec<String> = text
        .split_whitespace()
        .map(|word| match word.find("://") {
            Some(at) if word[..at].ends_with("http") || word[..at].ends_with("https") => {
                let start = word[..at].rfind(|c: char| !c.is_ascii_alphabetic()).map_or(0, |i| i + 1);
                format!("{}{}", &word[..start], mcp_import::display_url(&word[start..]))
            }
            _ => word.to_string(),
        })
        .collect();
    anyhow::anyhow!("{what}: {}", text.join(" "))
}

// ── atomics mcp-host ────────────────────────────────────────────────────────

/// The process sbx runs for a command server ATOMICS connected: its command with the environment
/// stored for it (`env`) and the login shell's PATH, so `npx` and the like resolve as they do in a
/// terminal. Stdout is the MCP channel: nothing else may write to it.
pub fn host_command(store: &Store, server: &str, env: bool, command: &[String], path: Option<&str>) -> Result<Command> {
    anyhow::ensure!(mcp_import::valid_name(server), "not a connection name ATOMICS manages");
    let (program, args) = command.split_first().context("no command to run")?;
    let mut host = Command::new(program);
    host.args(args);
    for name in ["ATOMICS_SOCKET", "ATOMICS_PANE", "ATOMICS_HOME", "ATOMICS_CONFIG"] {
        host.env_remove(name);
    }
    if let Some(path) = path {
        host.env("PATH", path);
    }
    if env {
        let values = store
            .mcp_env(server)?
            .with_context(|| format!("no environment is stored for {server}; connect it again in ATOMICS"))?;
        host.envs(values);
    }
    Ok(host)
}

/// `atomics mcp-host`: replaces this process with the server's.
pub fn run_host(server: &str, env: bool, command: &[String]) -> Result<std::convert::Infallible> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        // sbx's daemon may lack the desktop session's bus, which secret-tool needs.
        let bus = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
            .join("bus");
        if bus.exists() {
            // SAFETY: `atomics mcp-host` runs this before starting any thread.
            unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", bus.display())) };
        }
    }
    let store = Store::new(crate::relay::Keychain::current());
    let mut host = host_command(&store, server, env, command, crate::setup::SHELL_PATH.as_deref())?;
    Err(host.exec()).with_context(|| format!("running {}", command[0]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sandbox::fake;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn windows_opens_an_address_with_cmd_start_quoted_as_one_piece() {
        assert_eq!(
            windows_start_args("https://a.example/authorize?x=1&state=2").unwrap(),
            "/c start \"\" \"https://a.example/authorize?x=1&state=2\""
        );
        assert!(windows_start_args("https://a.example/\" & calc").is_none());
        assert!(windows_start_args("calc.exe").is_none());
        assert!(windows_start_args("https://a.example/\n").is_none());
    }

    /// A browser that records each URL it is asked to open in `opened` beside it.
    pub fn browser(dir: &Path) -> Browser {
        let program = dir.join("browser");
        std::fs::write(&program, "#!/bin/sh\nprintf '%s\\n' \"$1\" >> \"${0%/*}/opened\"\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Browser::at(program.display().to_string())
    }

    pub fn opened(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("opened")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn sources(home: &Path) -> Sources {
        let mut sources = Sources::at(home, cfg!(target_os = "macos"), |_| None);
        sources.files[1].1 = home.join("managed/managed-mcp.json");
        sources
    }

    const CLAUDE: &str = r#"{"oauthAccount": {"emailAddress": "PRIVATE-account"},
        "mcpServers": {"notion": {"type": "http", "url": "https://mcp.notion.com/mcp?key=PRIVATE-query"},
                       "fs": {"command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem"],
                              "env": {"FS_TOKEN": "PRIVATE-fs-token"}}}}"#;
    const CODEX: &str = "[mcp_servers.playwright]\ncommand = \"npx\"\nargs = [\"@playwright/mcp\"]\n\
                         [mcp_servers.fs]\ncommand = \"other\"\n";

    #[test]
    fn import_reads_definitions_at_each_platforms_paths_and_never_values() {
        for mac in [true, false] {
            let home = crate::test_support::tempdir().unwrap();
            let sources = Sources::at(home.path(), mac, |_| None);
            let managed = if mac {
                "/Library/Application Support/ClaudeCode/managed-mcp.json"
            } else {
                "/etc/claude-code/managed-mcp.json"
            };
            assert_eq!(
                sources.files,
                [
                    (Source::ClaudeCode, home.path().join(".claude.json")),
                    (Source::ClaudeManaged, PathBuf::from(managed)),
                    (Source::Codex, home.path().join(".codex/config.toml")),
                ]
            );
            assert_eq!(Sources::at(home.path(), mac, |_| Some("relative".into())), sources);
            // This test's own managed file, not the machine's.
            let mut sources = sources;
            sources.files[1].1 = home.path().join("managed/managed-mcp.json");
            // Nothing there yet: nothing found, nothing to complain about.
            assert!(import(&sources).definitions.is_empty() && import(&sources).notes.is_empty());
            write(&home.path().join(".claude.json"), CLAUDE);
            write(&home.path().join(".codex/config.toml"), CODEX);
            write(
                &sources.files[1].1,
                r#"{"mcpServers": {"wiki": {"type": "http", "url": "https://wiki.example.com/mcp"}}}"#,
            );
            let token_store = home.path().join(".claude/.credentials.json");
            write(&token_store, "{\"claudeAiOauth\": \"PRIVATE-token-store\"}");
            std::fs::set_permissions(&token_store, std::fs::Permissions::from_mode(0o000)).unwrap();
            let imported = import(&sources);
            let names: Vec<_> = imported.definitions.iter().map(|d| (d.name.as_str(), d.source)).collect();
            assert_eq!(
                names,
                [
                    ("fs", Source::ClaudeCode),
                    ("notion", Source::ClaudeCode),
                    ("wiki", Source::ClaudeManaged),
                    ("playwright", Source::Codex)
                ]
            );
            assert_eq!(imported.definitions[0].env, ["FS_TOKEN"]);
            let shown = format!("{imported:?}");
            assert!(!shown.contains("PRIVATE-fs-token") && !shown.contains("PRIVATE-account"), "{shown}");
            // The environment comes back only when asked for, to store it.
            let env = env_values(&sources, &imported.definitions[0]).unwrap();
            assert_eq!(env["FS_TOKEN"], "PRIVATE-fs-token");

            // Overridden folders, as Claude Code and Codex honour them.
            let other = crate::test_support::tempdir().unwrap();
            let dirs = other.path().display().to_string();
            let moved = Sources::at(home.path(), mac, |name| {
                matches!(name, "CLAUDE_CONFIG_DIR" | "CODEX_HOME").then(|| dirs.clone())
            });
            assert_eq!(moved.files[0].1, other.path().join(".claude.json"));
            assert_eq!(moved.files[2].1, other.path().join("config.toml"));

            write(&home.path().join(".codex/config.toml"), "[mcp_servers\nPRIVATE = 1");
            let imported = import(&sources);
            assert_eq!(imported.definitions.len(), 3);
            assert_eq!(imported.notes.len(), 1);
            assert!(imported.notes[0].starts_with("Codex: ") && !imported.notes[0].contains("PRIVATE"));
        }
    }

    #[test]
    fn remembered_definitions_merge_by_name_and_hold_no_values() {
        let home = crate::test_support::tempdir().unwrap();
        let sources = sources(home.path());
        write(&home.path().join(".claude.json"), CLAUDE);
        let state = home.path().join("state");
        remember(&state, &import(&sources).definitions).unwrap();
        let notion = added("Notion", "https://mcp.notion.com/v2").unwrap();
        let all = remember(&state, std::slice::from_ref(&notion)).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(definitions(&state), all);
        assert!(all.contains(&notion));
        let saved = std::fs::read_to_string(state.join("connections.json")).unwrap();
        assert!(!saved.contains("PRIVATE-fs-token"));
        for (name, url) in [
            ("x", "http://plain.example.com/mcp"),
            ("x", "https://user:pass@example.com/mcp"),
            ("x", "https:///mcp"),
            ("x", "https://example.com/a b"),
            ("--", "https://example.com/mcp"),
        ] {
            assert!(added(name, url).is_none(), "{name} {url}");
        }
    }

    #[test]
    fn connect_registers_authorizes_in_the_browser_and_loads_running_sandboxes() {
        let bin = crate::test_support::tempdir().unwrap();
        let program = fake::install(bin.path()).display().to_string();
        let (_dir, _, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let home = crate::test_support::tempdir().unwrap();
        write(&home.path().join(".claude.json"), CLAUDE);
        let sources = sources(home.path());
        let definitions = import(&sources).definitions;
        for sandbox in ["atomics-a", "atomics-b"] {
            sbx(&program, &["create", "--name", sandbox, "claude", "/w"], false, TIMEOUT).unwrap();
            ownership::created(home.path(), sandbox, ownership::Resource::Sandbox).unwrap();
        }
        let job = Connect {
            home: home.path().into(),
            sbx: program.clone(),
            name: "notion".into(),
            definition: definitions.iter().find(|d| d.name == "notion").cloned(),
            store_env: false,
            sources: sources.clone(),
            exe: "/Applications/ATOMICS.app/Contents/MacOS/atomics".into(),
            browser: browser(bin.path()),
            sandboxes: vec!["atomics-a".into(), "atomics-b".into()],
        };
        assert!(connect(&job, &store).unwrap().is_empty());
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "add"]),
            [isolation::connection_url_args("notion", "https://mcp.notion.com/mcp?key=PRIVATE-query")]
        );
        assert_eq!(fake::calls_to(bin.path(), &["mcp", "auth"]), [["mcp", "auth", "notion"]]);
        assert_eq!(std::fs::read_to_string(bin.path().join("auth-no-browser")).unwrap().trim(), "1");
        let opened = opened(bin.path());
        assert_eq!(opened.len(), 1);
        assert!(opened[0].starts_with("https://auth.example.com/authorize?"));
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]),
            [isolation::mcp_load_args("notion", "atomics-a"), isolation::mcp_load_args("notion", "atomics-b")]
        );
        let status = registered(&program).unwrap();
        assert_eq!(
            status,
            [Registered {
                name: "notion".into(),
                status: Status::Connected,
                transport: "remote http".into(),
                url: Some("https://mcp.notion.com/mcp?key=PRIVATE-query".into())
            }]
        );

        // Connecting again only loads: no second registration or browser page.
        connect(&job, &store).unwrap();
        assert_eq!(fake::calls_to(bin.path(), &["mcp", "add"]).len(), 1);
        assert_eq!(super::tests::opened(bin.path()).len(), 1);

        disconnect(home.path(), &program, "notion", &job.sandboxes, &store).unwrap();
        assert_eq!(fake::calls_to(bin.path(), &["mcp", "unload"]).len(), 2);
        assert_eq!(fake::calls_to(bin.path(), &["mcp", "rm"]), [["mcp", "rm", "--force", "notion"]]);
        assert!(registered(&program).unwrap().is_empty());
        assert!(
            disconnect(home.path(), &program, "atomics-a", &[], &store).is_err(),
            "ATOMICS's own servers are not connections"
        );
    }

    #[test]
    fn a_command_server_stores_its_environment_only_with_consent_and_runs_through_mcp_host() {
        let bin = crate::test_support::tempdir().unwrap();
        let program = fake::install(bin.path()).display().to_string();
        let (dir, _, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let home = crate::test_support::tempdir().unwrap();
        write(&home.path().join(".claude.json"), CLAUDE);
        let sources = sources(home.path());
        let fs = import(&sources).definitions.into_iter().find(|d| d.name == "fs").unwrap();
        let exe = PathBuf::from("/Apps/ATOMICS.app/atomics");
        let mut job = Connect {
            home: home.path().into(),
            sbx: program.clone(),
            name: "fs".into(),
            definition: Some(fs.clone()),
            store_env: false,
            sources,
            exe: exe.clone(),
            browser: browser(bin.path()),
            sandboxes: vec![],
        };
        connect(&job, &store).unwrap();
        assert_eq!(store.mcp_env("fs").unwrap(), None, "skipped without consent");
        let args = vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()];
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "add"]),
            [isolation::connection_command_args("fs", &exe, "npx", &args, None, false)]
        );
        assert!(opened(bin.path()).is_empty(), "a command server needs no sign-in");
        disconnect(home.path(), &program, "fs", &[], &store).unwrap();

        job.store_env = true;
        connect(&job, &store).unwrap();
        assert_eq!(store.mcp_env("fs").unwrap().unwrap()["FS_TOKEN"], "PRIVATE-fs-token");
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "add"])[1],
            isolation::connection_command_args("fs", &exe, "npx", &args, None, true)
        );
        let calls = std::fs::read_to_string(bin.path().join("calls.log")).unwrap();
        let argv = std::fs::read_to_string(dir.path().join("keychain-argv")).unwrap();
        assert!(!calls.contains("PRIVATE") && !argv.contains("PRIVATE"), "values never reach a command line");

        // What sbx runs gets the value from the keychain, and the login shell's PATH.
        let out = bin.path().join("env-out");
        let command = ["/bin/sh".to_string(), "-c".into(), format!("env > '{}'", out.display())];
        let status =
            host_command(&store, "fs", true, &command, Some("/login/bin:/usr/bin:/bin")).unwrap().status().unwrap();
        assert!(status.success());
        let env = std::fs::read_to_string(&out).unwrap();
        assert!(env.contains("FS_TOKEN=PRIVATE-fs-token") && env.contains("PATH=/login/bin:/usr/bin:/bin"), "{env}");

        disconnect(home.path(), &program, "fs", &[], &store).unwrap();
        assert_eq!(store.mcp_env("fs").unwrap(), None, "disconnecting forgets the environment");
        let error = host_command(&store, "fs", true, &command, None).unwrap_err().to_string();
        assert!(error.contains("connect it again"), "{error}");
    }

    #[test]
    fn authorization_failures_are_reported_without_sbx_output_and_never_hang() {
        let bin = crate::test_support::tempdir().unwrap();
        let program = fake::install(bin.path()).display().to_string();
        sbx(&program, &isolation::connection_url_args("notion", "https://mcp.notion.com/mcp"), false, TIMEOUT).unwrap();
        std::fs::write(bin.path().join("deny-auth"), "").unwrap();
        let error = authorize(&program, "notion", &browser(bin.path())).unwrap_err().to_string();
        assert!(error.contains("did not complete") && !error.contains("PRIVATE") && !error.contains("access_denied"));
        std::fs::remove_file(bin.path().join("deny-auth")).unwrap();

        std::fs::write(bin.path().join("wait-authorized"), "").unwrap();
        let error = authorize_within(&program, "notion", &browser(bin.path()), Duration::from_millis(500)).unwrap_err();
        assert!(error.to_string().contains("took longer"), "{error}");
        std::fs::remove_file(bin.path().join("wait-authorized")).unwrap();

        // sbx waits for the browser that never opens, and stops with it.
        std::fs::write(bin.path().join("wait-authorized"), "").unwrap();
        let broken = Browser::at(bin.path().join("missing-browser").display().to_string());
        let error = format!("{:#}", authorize(&program, "notion", &broken).unwrap_err());
        assert!(error.contains("missing-browser") && !error.contains("PRIVATE"), "{error}");
        assert_eq!(registered(&program).unwrap()[0].status, Status::NeedsAuth);
    }

    #[test]
    fn sessions_load_connected_servers_and_name_the_ones_they_need() {
        let bin = crate::test_support::tempdir().unwrap();
        let program = fake::install(bin.path()).display().to_string();
        std::fs::write(bin.path().join("auth-free-sentry"), "").unwrap();
        let home = crate::test_support::tempdir().unwrap();
        ownership::created(home.path(), "atomics-s1", ownership::Resource::Sandbox).unwrap();
        for (name, url) in [("sentry", "https://mcp.sentry.dev/mcp"), ("slack", "https://mcp.slack.com/mcp")] {
            sbx(&program, &isolation::connection_url_args(name, url), false, TIMEOUT).unwrap();
        }
        let exe = Path::new("/atomics");
        sbx(&program, &isolation::connection_command_args("fs", exe, "npx", &[], None, false), false, TIMEOUT).unwrap();
        sbx(&program, &isolation::mcp_add_args("atomics-other", exe, Path::new("/s")), false, TIMEOUT).unwrap();
        let needed = load_into(home.path(), &program, "atomics-s1", &["notion".into(), " ".into()], false);
        assert_eq!(needed, ["notion", "slack"]);
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]),
            [isolation::mcp_load_args("fs", "atomics-s1"), isolation::mcp_load_args("sentry", "atomics-s1")],
            "connected servers only, never another session's ATOMICS server"
        );
        // Without endpoint metadata, leave the gateway alone rather than risk collisions.
        std::fs::write(bin.path().join("fail-mcp-ls"), "").unwrap();
        assert_eq!(
            load_into(home.path(), &program, "atomics-s1", &["notion".into(), "fs".into()], false),
            ["fs", "notion"]
        );
    }

    #[test]
    fn same_url_aliases_preserve_loaded_tools_on_connect_reattach_and_disconnect() {
        let bin = crate::test_support::tempdir().unwrap();
        let home = crate::test_support::tempdir().unwrap();
        let program = fake::install(bin.path()).display().to_string();
        let (_dir, _, store) = crate::credentials::tests::machine(cfg!(target_os = "macos"));
        let url = "https://mcp.example.com/mcp";
        std::fs::write(bin.path().join("auth-free-service"), "").unwrap();
        sbx(&program, &isolation::connection_url_args("service", url), false, TIMEOUT).unwrap();
        for sandbox in ["atomics-running", "atomics-new"] {
            ownership::created(home.path(), sandbox, ownership::Resource::Sandbox).unwrap();
        }
        assert!(load_into(home.path(), &program, "atomics-running", &[], false).is_empty());
        let job = Connect {
            home: home.path().into(),
            sbx: program.clone(),
            // A new alias sorts earlier, but must not replace what is serving a running agent.
            name: "a-alias".into(),
            definition: added("a-alias", url),
            store_env: false,
            sources: sources(home.path()),
            exe: "/atomics".into(),
            browser: browser(bin.path()),
            sandboxes: vec!["atomics-running".into(), "atomics-new".into()],
        };
        assert!(connect(&job, &store).unwrap().is_empty());
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]),
            [
                isolation::mcp_load_args("service", "atomics-running"),
                isolation::mcp_load_args("a-alias", "atomics-new")
            ]
        );
        // The fixture reproduces sbx's real rejection if deduplication were bypassed.
        assert!(sbx(&program, &isolation::mcp_load_args("a-alias", "atomics-running"), false, TIMEOUT).is_err());
        for sandbox in &job.sandboxes {
            assert!(load_into(home.path(), &program, sandbox, &["a-alias".into(), "service".into()], false).is_empty());
        }
        let owned = ownership::owned(home.path()).unwrap();
        assert_eq!(owned["atomics-running"].connections, ["service"]);
        assert_eq!(owned["atomics-new"].connections, ["a-alias"]);
        assert!(
            fake::calls_to(bin.path(), &["mcp", "unload"]).is_empty(),
            "reattaching preserves the serving connection"
        );
        // Older sandboxes have no loaded-connection record. Recover the name already in
        // their gateway rather than removing its tools to impose a different alias.
        ownership::with_connections(home.path(), "atomics-running", Vec::clear).unwrap();
        assert!(load_into(home.path(), &program, "atomics-running", &[], false).is_empty());
        assert_eq!(ownership::owned(home.path()).unwrap()["atomics-running"].connections, ["service"]);
        assert!(fake::calls_to(bin.path(), &["mcp", "unload"]).is_empty());

        // Disconnecting the selected connection makes the remaining alias serve its URL.
        disconnect(home.path(), &program, "service", &job.sandboxes, &store).unwrap();
        let owned = ownership::owned(home.path()).unwrap();
        assert_eq!(owned["atomics-running"].connections, ["a-alias"]);
        assert_eq!(owned["atomics-new"].connections, ["a-alias"]);
        assert_eq!(
            registered(&program).unwrap().iter().map(|server| server.name.as_str()).collect::<Vec<_>>(),
            ["a-alias"]
        );

        // Different endpoints on the same host are separate services, never conflated.
        std::fs::write(bin.path().join("auth-free-other"), "").unwrap();
        sbx(&program, &isolation::connection_url_args("other", "https://mcp.example.com/other"), false, TIMEOUT)
            .unwrap();
        assert!(load_into(home.path(), &program, "atomics-running", &[], false).is_empty());
        assert_eq!(ownership::owned(home.path()).unwrap()["atomics-running"].connections, ["a-alias", "other"]);
    }

    #[test]
    fn sbx_errors_lose_url_queries() {
        let error =
            redacted(anyhow::anyhow!("sbx mcp failed: resolve (https://u:p@h.example/mcp?key=PRIVATE) 404"), "x");
        assert_eq!(error.to_string(), "x: sbx mcp failed: resolve (https://h.example/mcp 404");
    }
}
