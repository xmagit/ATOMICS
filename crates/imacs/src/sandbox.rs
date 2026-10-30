//! Docker Sandboxes runtime: a sandboxed agent runs in its own microVM, created when the agent
//! starts and named after its session, attached to the tile's terminal with `sbx run`, stopped
//! when the agent exits and removed with its worktree. The command lines come from
//! [`imacs_core::isolation`]; this module runs them.

pub mod template;

use std::fs::File;
use std::io::{Read, Seek};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::sandbox_ownership::{self as ownership, Resource};
use anyhow::{Context, Result, bail};
use imacs_core::isolation::{self, Diagnosis, Listed, MIN_SBX_VERSION, SandboxConfig, Spec};
use imacs_core::{git, paths, workspace};

pub mod azure_check;
pub(crate) mod checks;
pub mod exec;
pub mod relay_check;
pub mod setup;
pub use checks::doctor_lines;

/// Most sbx commands answer within a second; this bounds a stuck daemon.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The first sandbox of an agent pulls its image, which took 51 s on a fast connection.
pub(crate) const CREATE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Where an agent runs.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentRuntime {
    /// On this machine, in the tile's PTY, as agents always have.
    Bare,
    /// In its own sandbox, which the tile's PTY attaches to with `sbx run`.
    Sandbox(Sandboxed),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sandboxed {
    /// The `sbx` program.
    pub sbx: String,
    pub name: String,
    /// The sbx agent it runs, such as `claude`.
    pub agent: &'static str,
    pub ssh_agent: bool,
}

impl Sandboxed {
    /// What the tile's PTY runs: `sbx run` for `agent_args`. Without SSH agent forwarding, sbx
    /// runs without `SSH_AUTH_SOCK`, so there is no agent for it to forward.
    pub fn command(&self, agent_args: &[String]) -> (String, Vec<String>) {
        let run = isolation::run_args(&self.name, self.agent, agent_args);
        if self.ssh_agent {
            (self.sbx.clone(), run)
        } else {
            (
                "/usr/bin/env".into(),
                ["-u", "SSH_AUTH_SOCK", &self.sbx].map(String::from).into_iter().chain(run).collect(),
            )
        }
    }
}

/// Everything a background thread needs to make a session's sandbox.
#[derive(Debug, Clone)]
pub struct Prepare {
    pub home: PathBuf,
    pub sbx: String,
    pub name: String,
    pub agent: &'static str,
    pub cwd: PathBuf,
    pub config: SandboxConfig,
    pub warm: Vec<String>,
    /// IMACS's executable and socket, for the MCP server agents report through.
    pub exe: PathBuf,
    pub socket: PathBuf,
    /// A folder of this session's own, mounted read-only for its brief.
    pub briefs: Option<PathBuf>,
    /// IMACS's records of where git finds each sandbox's repository; see [`pin_git`].
    pub pins: PathBuf,
    /// For a Claude agent: the relay that gives it a placeholder for the Claude subscription, when
    /// the OS keychain holds one.
    pub relay: Option<std::sync::Arc<crate::relay::Service>>,
}

impl Prepare {
    /// The runtime this job's agent runs with once its sandbox is ready.
    pub fn sandboxed(&self) -> Sandboxed {
        Sandboxed {
            sbx: self.sbx.clone(),
            name: self.name.clone(),
            agent: self.agent,
            ssh_agent: self.config.ssh_agent,
        }
    }
}

/// What sbx said when it was checked.
#[derive(Debug, Clone)]
pub struct Ready {
    pub version: (u64, u64, u64),
    pub diagnosis: Diagnosis,
}

/// How to opt out, which every refusal says: IMACS never runs a sandboxed agent bare on its own.
const BARE: &str = "or set [isolation] mode = \"bare\" to run agents directly on this machine";

/// Checks that sbx can run sandboxes: installed, at least [`MIN_SBX_VERSION`], and signed in
/// to Docker. A failure says what to do about it.
pub fn preflight(program: &str) -> Result<Ready, String> {
    if !installed(program) {
        return Err(format!(
            "Docker Sandboxes (sbx) is not installed. {}; sign in with `sbx login`, {BARE}.",
            checks::install_hint("sbx", cfg!(target_os = "macos"))
        ));
    }
    let output = sbx(program, &["version"], false, TIMEOUT)
        .map_err(|_| format!("sbx did not report its version. Run `sbx version` to see why, {BARE}."))?;
    let version = isolation::parse_version(&output)
        .ok_or_else(|| format!("sbx reported no version IMACS can read. Check `sbx version`, {BARE}."))?;
    if version < MIN_SBX_VERSION {
        return Err(format!(
            "sbx {} is older than {}, the oldest IMACS supports. {}; {BARE}.",
            isolation::version_text(version),
            isolation::version_text(MIN_SBX_VERSION),
            checks::install_hint("sbx", cfg!(target_os = "macos"))
        ));
    }
    // diagnose exits non-zero when any check fails, such as an unrelated SSH setting; the report still holds.
    let (_, output, _) = sbx_output(program, &["diagnose", "--json"], false, TIMEOUT)
        .map_err(|_| format!("sbx diagnose did not run. Run `sbx diagnose` to see why, {BARE}."))?;
    let diagnosis: Diagnosis = serde_json::from_str(&output)
        .map_err(|_| format!("sbx diagnose gave no readable report. Run `sbx diagnose` to see why, {BARE}."))?;
    if !diagnosis.signed_in() {
        return Err(format!("sbx is not signed in to Docker. Run `sbx login` in a terminal, {BARE}."));
    }
    Ok(Ready { version, diagnosis })
}

/// Makes the sandbox unless it exists, which is how a session reattaches after a restart,
/// once sbx passes [`preflight`]. A new sandbox mounts the worktree and git's write folders,
/// gets IMACS's MCP server and its repository's allowed hosts. MCP connections are loaded
/// afterwards, for a new or reattached sandbox alike ([`crate::connections::load_into`]). A Claude agent
/// with the subscription in the OS keychain gets a new placeholder for it, revoked again if
/// preparation fails, and its sandbox the way to the relay.
pub fn prepare(job: &Prepare) -> Result<Option<crate::relay::Granted>> {
    let ready = preflight(&job.sbx).map_err(anyhow::Error::msg)?;
    // Fail closed before making anything if this home's ledger cannot be read.
    let owned = ownership::owned(&job.home)?;
    let existing = list(&job.sbx)?.into_iter().find(|sandbox| sandbox.name == job.name);
    if existing.is_some() {
        anyhow::ensure!(
            owned.get(&job.name).is_some_and(|entry| entry.sandbox),
            "Sandbox {} is not owned by this IMACS_HOME; refusing to attach",
            job.name
        );
    }
    if real(&paths::home()).starts_with(real(&job.cwd)) {
        bail!(
            "{} holds your whole home folder; start in a project folder or set isolation = \"bare\"",
            job.cwd.display()
        );
    }
    pin_git(&job.pins, &job.name, &job.cwd)?;
    ownership::record_pins(&job.home, &job.name, &job.pins)?;
    let (workspace, git) = workspace(&job.cwd)?;
    // First, so a new sandbox's environment can name the relay's port.
    let granted = match &job.relay {
        Some(relay) => relay.grant_session(&job.name, job.agent == "claude", &job.config.azure)?,
        None => None,
    };
    let made = make(job, &ready, existing, workspace, git, granted.as_ref());
    if made.is_err()
        && let (Some(relay), Some(granted)) = (&job.relay, &granted)
    {
        relay.revoke(granted);
    }
    made.map(|()| granted)
}

fn make(
    job: &Prepare,
    ready: &Ready,
    existing: Option<Listed>,
    workspace: PathBuf,
    git: Vec<String>,
    granted: Option<&crate::relay::Granted>,
) -> Result<()> {
    let ssh = job.config.ssh_agent;
    if let Some(existing) = existing {
        // A sandbox keeps its mounts for life: one made before a main checkout's .git was mounted
        // read-only whole lets its agent replace .git, config and all.
        let mounted = real_paths(&existing).workspaces;
        if let Some(missing) = git
            .iter()
            .filter_map(|mount| mount.strip_suffix(":ro"))
            .find(|protected| !mounted.contains(&format!("{}:ro", real(Path::new(protected)).display())))
        {
            bail!(
                "sandbox {} does not mount {missing} read-only, so its agent could replace it. Remove the \
                 sandbox (sbx rm {}) and start the agent again, or set isolation = \"bare\" on it.",
                job.name,
                job.name
            );
        }
        // Made before Claude used the relay, before the relay moved to another port, or used by a Claude
        // that no longer has a subscription to relay.
        return relay_rule(job, granted.map(|granted| granted.port));
    }
    // A machine's first sandbox needs sbx's global network policy; one already set up is left alone.
    if !ready.diagnosis.policy_initialized() {
        static SETUP: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _setup = SETUP.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another launch may have set it up while this one waited.
        if !preflight(&job.sbx).map_err(anyhow::Error::msg)?.diagnosis.policy_initialized() {
            sbx(&job.sbx, &["policy", "init", job.config.network.name()], ssh, TIMEOUT)
                .context("setting up sbx's network policy")?;
        }
    }
    if let Some(briefs) = &job.briefs {
        std::fs::create_dir_all(briefs).with_context(|| format!("creating {}", briefs.display()))?;
    }
    let mut read_only: Vec<PathBuf> = job.config.mounts.iter().map(|mount| paths::expand(mount)).collect();
    read_only.extend(job.briefs.iter().cloned());
    let spec = Spec {
        name: job.name.clone(),
        agent: job.agent,
        warm: warm_mounts(&workspace, &job.warm, cfg!(target_os = "linux")),
        workspace,
        git,
        read_only,
        memory: job.config.memory.clone(),
        cpus: job.config.cpus,
        template: job.config.template.clone(),
        env: granted
            .map(|granted| {
                let mut env = granted.azure_env();
                if !granted.placeholder.is_empty() {
                    env.extend(isolation::claude_relay_env(granted.port));
                }
                env
            })
            .unwrap_or_default(),
    };
    // A registration left by an earlier attempt would make `mcp add` fail.
    remove_mcp(&job.home, &job.sbx, &job.name)?;
    // Ownership is reserved before sbx makes anything, as for templates: a crash or a timeout part way
    // leaves resources this home owns, which removal or the next startup sweep takes away.
    ownership::created(&job.home, &job.name, Resource::Mcp)?;
    let registration = sbx(&job.sbx, &isolation::mcp_add_args(&job.name, &job.exe, &job.socket), ssh, TIMEOUT);
    ownership::removed(&job.home, &job.name, Resource::Creator)?;
    registration.context("registering IMACS's MCP server with sbx").inspect_err(|_| {
        let _ = remove(&job.home, &job.sbx, &job.name);
    })?;
    ownership::created(&job.home, &job.name, Resource::Sandbox)?;
    let creation = sbx_until(&job.sbx, &isolation::create_args(&spec), ssh, CREATE_TIMEOUT, Some(&QUITTING));
    ownership::removed(&job.home, &job.name, Resource::Creator)?;
    creation.context("creating the sandbox").inspect_err(|_| {
        let _ = remove(&job.home, &job.sbx, &job.name);
    })?;
    let finish = || -> Result<()> {
        sbx(&job.sbx, &isolation::mcp_load_args(&job.name, &job.name), ssh, TIMEOUT)
            .context("loading IMACS's MCP server")?;
        let root = real(&workspace::project_root(&job.cwd).unwrap_or_else(|_| job.cwd.clone()));
        let hosts = job.config.hosts_for(&root, |repo| real(&paths::expand(repo)));
        if !hosts.is_empty() {
            sbx(&job.sbx, &isolation::allow_hosts_args(&job.name, &hosts), ssh, TIMEOUT)
                .context("allowing the repository's hosts")?;
        }
        relay_rule(job, granted.map(|granted| granted.port))
    };
    // A half-made sandbox would be reattached to as it is: remove it so the next start makes it whole.
    finish().inspect_err(|_| {
        let _ = remove(&job.home, &job.sbx, &job.name);
    })
}

/// Lets the sandbox reach the relay at `port` on this machine's loopback, and nothing else there:
/// the ownership ledger records the rule so it goes with the sandbox, and a rule for a port the relay
/// left, or for a sandbox that no longer uses it (`None`), goes now, since another program may listen
/// there.
fn relay_rule(job: &Prepare, port: Option<u16>) -> Result<()> {
    let allowed = ownership::owned(&job.home)?.get(&job.name).and_then(|entry| entry.relay);
    if allowed == port {
        return Ok(());
    }
    if let Some(old) = allowed {
        // sbx fails when the rule is gone already; it also goes when the sandbox does.
        let _ = sbx(&job.sbx, &isolation::relay_remove_args(&job.name, old), false, TIMEOUT);
        ownership::removed(&job.home, &job.name, Resource::Relay)?;
    }
    let Some(port) = port else { return Ok(()) };
    sbx(&job.sbx, &isolation::relay_allow_args(&job.name, port), job.config.ssh_agent, TIMEOUT)
        .context("letting the sandbox reach IMACS's credential relay")?;
    ownership::record_relay(&job.home, &job.name, port)
}

/// Pins host git in `cwd` to IMACS's record for sandbox `name` in `dir`: the one saved when the
/// sandbox was made, another sandbox's for the same folder, or else what git says before this
/// sandbox has run. The record outlives restarts, and only cleanup removes it with the sandbox.
pub fn pin_git(dir: &Path, name: &str, cwd: &Path) -> Result<()> {
    let file = dir.join(format!("{name}.json"));
    let pins = match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", file.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => current_pins(cwd)?,
        Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
    };
    check_pins(&pins)?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temporary = file.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&pins)?)?;
    std::fs::rename(&temporary, &file)?;
    git::pin(pins);
    Ok(())
}

/// Where git finds `cwd`'s repository for a sandbox about to be made: the record of a sandbox that
/// has worked there, else what git says now, refused if git did not make that layout.
pub(crate) fn current_pins(cwd: &Path) -> Result<git::GitPins> {
    match git::pinned(cwd) {
        Some(pins) => Ok(pins),
        None => git::resolve_pins(cwd),
    }
}

/// Checked for every sandbox made or reattached to: the repository's config can change while its
/// sandbox exists.
pub(crate) fn check_pins(pins: &git::GitPins) -> Result<()> {
    match &pins.common_dir {
        Some(common) => git::check_worktree_config(common),
        None => Ok(()),
    }
}

/// Pins every folder IMACS recorded for a sandbox in `dir`, as each process that runs git in
/// them starts.
pub fn load_pins(dir: &Path) {
    for pins in read_pins(dir).into_values() {
        git::pin(pins);
    }
}

/// Drops sandbox `name`'s record once the sandbox is removed. Its folder stays pinned while
/// another sandbox's record covers it.
pub fn forget_pins(dir: &Path, name: &str) -> Result<()> {
    let mut records = read_pins(dir);
    let file = dir.join(format!("{name}.json"));
    let pins: git::GitPins = match std::fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    records.remove(name);
    // Drop the VM's named record, but keep poisoned metadata pinned across scans and restarts.
    let unsafe_folder = pins.work_tree.exists() && git::resolve_pins(&pins.work_tree).ok().as_ref() != Some(&pins);
    if unsafe_folder {
        let retained = dir.join(format!("retained-{}.json", paths::runtime_home_hash(&pins.work_tree)));
        let temporary = retained.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&pins)?)?;
        std::fs::rename(temporary, retained)?;
    }
    std::fs::remove_file(file)?;
    if !unsafe_folder && !records.values().any(|other| other.work_tree == pins.work_tree) {
        git::unpin(&pins.work_tree);
    }
    Ok(())
}

/// Drops the records of sandboxes sbx no longer has, as IMACS starts: a record outliving its
/// sandbox would keep git out of a folder for good. A folder whose git files no longer match its
/// record stays pinned, since the sandbox may have changed them and git would follow. Without an
/// sbx that answers, which sandboxes exist is unknown, and every record stays.
pub fn prune_pins(home: &Path, dir: &Path, program: &str) {
    if !installed(program) {
        return;
    }
    let Ok(sandboxes) = list(program) else { return };
    let Ok(owned) = ownership::owned(home) else { return };
    let names: std::collections::HashSet<String> = sandboxes.into_iter().map(|sandbox| sandbox.name).collect();
    for (name, pins) in read_pins(dir) {
        if names.contains(&name) || owned.get(&name).is_some_and(ownership::building) {
            continue;
        }
        if pins.work_tree.exists() && git::resolve_pins(&pins.work_tree).ok().as_ref() != Some(&pins) {
            eprintln!(
                "IMACS: keeping removed sandbox {name}'s git record: {} no longer looks as git made it",
                pins.work_tree.display()
            );
            continue;
        }
        if let Err(error) = forget_pins(dir, &name) {
            eprintln!("IMACS: removing git record {name}: {error:#}");
        }
    }
}

fn read_pins(dir: &Path) -> std::collections::HashMap<String, git::GitPins> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Default::default() };
    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.strip_suffix(".json")?.to_string();
            match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
                Ok(pins) => Some((name, pins)),
                Err(error) => {
                    eprintln!("IMACS: reading {} failed: {error:#}", path.display());
                    None
                }
            }
        })
        .collect()
}

/// Only real directories inside this worktree may be exposed as writable caches.
/// Explicit mounts keep sbx from filtering ignored build folders out of the workspace.
fn warm_mounts(root: &Path, folders: &[String], linux: bool) -> Vec<PathBuf> {
    if !linux {
        return Vec::new();
    }
    let root = real(root);
    let mut mounts = Vec::new();
    for folder in folders {
        let relative = Path::new(folder);
        if relative.as_os_str().is_empty()
            || !relative.components().all(|part| matches!(part, std::path::Component::Normal(name) if name != ".git"))
        {
            continue;
        }
        let path = root.join(relative);
        let mut current = root.clone();
        let no_links = relative.components().all(|part| {
            current.push(part);
            std::fs::symlink_metadata(&current).is_ok_and(|m| !m.file_type().is_symlink())
        });
        if no_links
            && path.is_dir()
            && path.canonicalize().is_ok_and(|resolved| resolved.starts_with(&root))
            && !mounts.contains(&path)
        {
            mounts.push(path);
        }
    }
    mounts
}

/// The folder a sandbox mounts read-write for `cwd`, its worktree (else `cwd` itself), and
/// git's folders beside it. A linked worktree gets the common dir's objects, refs and logs and
/// its own admin folder, so commits work while the repository's hooks and config stay on this
/// machine. A main checkout's git dir is mounted whole and read-only: a read-write checkout
/// around anything less lets the agent replace `.git` itself, config and all, and a mount point
/// cannot be moved. Agents there edit files; tasks commit in linked worktrees.
pub fn workspace(cwd: &Path) -> Result<(PathBuf, Vec<String>)> {
    let args = ["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-dir", "--git-common-dir"];
    let output = git::git(cwd, &args).ok();
    let top = output.as_deref().and_then(|output| output.lines().next()).map_or(cwd, Path::new);
    if real(&paths::home()).starts_with(real(top)) {
        bail!(
            "{} holds your whole home folder, which a sandbox would mount read-write. Start the agent in a \
             project folder, or set isolation = \"bare\" on it.",
            top.display()
        );
    }
    let Some(output) = output else { return Ok((cwd.to_path_buf(), Vec::new())) };
    let lines: Vec<&str> = output.lines().collect();
    let [top, git_dir, common] = lines[..] else { bail!("unexpected git rev-parse output: {output}") };
    let (top, git_dir, common) = (PathBuf::from(top), PathBuf::from(git_dir), PathBuf::from(common));
    let mut mounts = Vec::new();
    let existing = |path: PathBuf| path.exists().then(|| path.to_string_lossy().into_owned());
    if git_dir != common {
        mounts.extend(["objects", "refs", "logs"].into_iter().filter_map(|folder| existing(common.join(folder))));
        mounts.push(git_dir.to_string_lossy().into_owned());
    } else {
        mounts.push(format!("{}:ro", git_dir.display()));
    }
    Ok((top, mounts))
}

/// Every sandbox sbx knows.
pub fn list(program: &str) -> Result<Vec<Listed>> {
    let output = sbx(program, &["ls", "--json"], false, TIMEOUT)?;
    isolation::parse_list(&output).context("reading `sbx ls --json`")
}

/// Stops a sandbox, keeping it for the next `sbx run`.
pub fn stop(home: &Path, program: &str, name: &str) -> Result<()> {
    if !ownership::owned(home)?.get(name).is_some_and(|entry| entry.sandbox) {
        return Ok(());
    }
    sbx(program, &["stop", name], false, TIMEOUT).map(drop)
}

/// Removes a sandbox, its way to the relay and IMACS's MCP registration for it. The sandbox and the
/// registration may be only reserved: an unlisted recent or live create keeps its reservation
/// until a later cleanup can establish that creation has ended.
pub fn remove(home: &Path, program: &str, name: &str) -> Result<()> {
    let owned = ownership::owned(home)?.get(name).cloned().unwrap_or_default();
    if owned.sandbox {
        if ownership::may_be_creating(&owned) && !list(program)?.iter().any(|sandbox| sandbox.name == name) {
            // A canceled CLI can leave a create running in the daemon. Keep its VM
            // reservation and pins, but remove an already listed MCP registration.
            remove_mcp(home, program, name)?;
            bail!("Sandbox {name} may still be being created; keeping its reservation for cleanup");
        }
        if let Some(port) = owned.relay {
            // Taken away first: a sandbox sbx fails to remove keeps no way to this machine's loopback.
            let _ = sbx(program, &isolation::relay_remove_args(name, port), false, TIMEOUT);
        }
        if let Err(error) = sbx(program, &["rm", "--force", name], false, TIMEOUT)
            && (list(program)?.iter().any(|sandbox| sandbox.name == name) || ownership::may_be_creating(&owned))
        {
            return Err(error);
        }
        ownership::removed(home, name, Resource::Sandbox)?;
    }
    if owned.relay.is_some() {
        ownership::removed(home, name, Resource::Relay)?;
    }
    remove_mcp(home, program, name)?;
    // The ref guard's baseline goes with its sandbox; one made again under this name takes a new one.
    match std::fs::remove_file(home.join("ref-guards").join(format!("{name}.json"))) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(entry) = ownership::owned(home)?.get(name) {
        if let Some(dir) = &entry.pins {
            forget_pins(dir, name)?;
            ownership::removed(home, name, Resource::Pins)?;
        }
        if let Some(scratch) = &entry.scratch {
            match std::fs::remove_dir_all(scratch) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            ownership::removed(home, name, Resource::Scratch)?;
        }
        if entry.builder.is_some() && !ownership::building(entry) {
            ownership::removed(home, name, Resource::Builder)?;
        }
    }
    ownership::removed(home, name, Resource::Creator)
}

/// Fail closed on an unreadable owned record rather than following checkout metadata.
pub fn load_owned_pins(home: &Path) -> Result<()> {
    load_pins(&home.join("git-pins"));
    for (name, entry) in ownership::owned(home)? {
        if let Some(dir) = entry.pins {
            let file = dir.join(format!("{name}.json"));
            let pins =
                serde_json::from_slice(&std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?)?;
            git::pin(pins);
        }
    }
    Ok(())
}

pub fn remove_mcp(home: &Path, program: &str, name: &str) -> Result<()> {
    let owned = ownership::owned(home)?.get(name).cloned().unwrap_or_default();
    if !owned.mcp {
        return Ok(());
    }
    if ownership::may_be_creating(&owned) && !mcp_names(program)?.iter().any(|other| other == name) {
        bail!("MCP {name} may still be being created; keeping its reservation for cleanup");
    }
    if let Err(error) = sbx(program, &["mcp", "rm", "--force", name], false, TIMEOUT)
        && (mcp_names(program)?.iter().any(|other| other == name) || ownership::may_be_creating(&owned))
    {
        return Err(error.context("removing the sandbox's IMACS MCP registration"));
    }
    ownership::removed(home, name, Resource::Mcp)
}

pub fn mcp_names(program: &str) -> Result<Vec<String>> {
    Ok(sbx(program, &["mcp", "ls", "--quiet"], false, TIMEOUT)?
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect())
}

/// IMACS's sandboxes of one repository, as the cleanup dialog shows them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoSandboxes {
    /// Each sandbox whose worktree is still there: the worktree as git lists it, and its name.
    pub attached: Vec<(PathBuf, String)>,
    /// Sandboxes whose worktree is gone.
    pub orphans: Vec<Listed>,
}

/// IMACS's sandboxes of `repo`, split into those whose worktree is still there and orphans.
pub fn for_repo(home: &Path, program: &str, repo: &Path) -> Result<RepoSandboxes> {
    load_owned_pins(home)?;
    let owned = ownership::owned(home)?;
    let listed: Vec<Listed> = list(program)?
        .iter()
        .filter(|s| owned.get(&s.name).is_some_and(|entry| entry.sandbox))
        .map(real_paths)
        .collect();
    let worktrees: Vec<PathBuf> =
        git::list_worktrees(repo)?.into_iter().filter(|w| w.trusted).map(|w| w.path).collect();
    let real_worktrees: Vec<PathBuf> = worktrees.iter().map(|path| real(path)).collect();
    let common = git::common_dir(repo)?;
    let (attached, orphans) = isolation::partition(&listed, &real_worktrees, &common, Path::exists);
    let attached = attached
        .into_iter()
        .filter_map(|sandbox| {
            let index = real_worktrees.iter().position(|path| Some(path.as_path()) == sandbox.workspace())?;
            Some((worktrees[index].clone(), sandbox.name.clone()))
        })
        .collect();
    Ok(RepoSandboxes { attached, orphans: orphans.into_iter().cloned().collect() })
}

/// The names of IMACS's sandboxes working in `worktree`. Ask before removing the worktree:
/// paths compare by where they lead, which needs the folder.
pub fn in_worktree(home: &Path, program: &str, worktree: &Path) -> Result<Vec<String>> {
    let owned = ownership::owned(home)?;
    let worktree = real(worktree);
    Ok(list(program)?
        .iter()
        .map(real_paths)
        .filter(|sandbox| {
            owned.get(&sandbox.name).is_some_and(|entry| entry.sandbox)
                && sandbox.workspace() == Some(worktree.as_path())
        })
        .map(|sandbox| sandbox.name)
        .collect())
}

/// A sandbox's mounts with links resolved, as git and sbx may each name a folder differently.
fn real_paths(sandbox: &Listed) -> Listed {
    let workspaces = sandbox
        .workspaces
        .iter()
        .map(|mount| match mount.strip_suffix(":ro") {
            Some(path) => format!("{}:ro", real(Path::new(path)).display()),
            None => real(Path::new(mount)).display().to_string(),
        })
        .collect();
    Listed { workspaces, ..sandbox.clone() }
}

fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether `program` names an sbx this machine has: anything else means sandboxes cannot exist.
pub fn installed(program: &str) -> bool {
    Path::new(program).is_absolute() && Path::new(program).is_file()
}

/// The account usage a running sandbox's agents last saw ([`isolation::usage_args`]). Its agent
/// controls the VM, so only the first 128 KiB are read, and callers treat the rest as its claims.
pub fn read_usage(program: &str, name: &str) -> Result<String> {
    const LIMIT: u64 = 128 * 1024;
    let (mut stdout, stderr) = (capture()?, capture()?);
    let mut child = sbx_command(program, &isolation::usage_args(name), false, &stdout, &stderr)?
        .spawn()
        .with_context(|| format!("running {program}"))?;
    match wait(&mut child, Instant::now() + Duration::from_secs(20), None)? {
        Some(status) if status.success() => {}
        Some(status) => bail!("sbx exec failed ({status})"),
        None => bail!("sbx exec did not finish within 20 s"),
    }
    let mut bytes = Vec::new();
    stdout.rewind()?;
    stdout.take(LIMIT).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Runs `sbx args` within `timeout` and returns its output, failing when sbx does.
pub(crate) fn sbx(program: &str, args: &[impl AsRef<str>], ssh_agent: bool, timeout: Duration) -> Result<String> {
    sbx_until(program, args, ssh_agent, timeout, None)
}

/// Set as IMACS quits: see [`stop_creating`].
static QUITTING: AtomicBool = AtomicBool::new(false);

/// Stops making sandboxes, which can take [`CREATE_TIMEOUT`], so that a quit does not wait for it.
/// Each preparation then fails and removes what sbx made of its sandbox.
pub fn stop_creating() {
    QUITTING.store(true, Ordering::Relaxed);
}

/// [`sbx`], stopped early once `stop` is set.
fn sbx_until(
    program: &str,
    args: &[impl AsRef<str>],
    ssh_agent: bool,
    timeout: Duration,
    stop: Option<&AtomicBool>,
) -> Result<String> {
    let (status, out, err) = run(program, args, ssh_agent, timeout, stop)?;
    if !status.success() {
        let shown = args.first().map_or("", AsRef::as_ref);
        let detail = [err.trim(), out.trim()].into_iter().find(|text| !text.is_empty()).unwrap_or("no output");
        bail!("sbx {shown} failed ({status}): {detail}");
    }
    Ok(out)
}

/// Runs `sbx args` within `timeout`: its status, output and errors. Output goes to unlinked
/// files, so a daemon sbx starts and that inherits them cannot keep this waiting.
pub(crate) fn sbx_output(
    program: &str,
    args: &[impl AsRef<str>],
    ssh_agent: bool,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String, String)> {
    run(program, args, ssh_agent, timeout, None)
}

/// [`sbx_output`], stopped early once `stop` is set.
fn run(
    program: &str,
    args: &[impl AsRef<str>],
    ssh_agent: bool,
    timeout: Duration,
    stop: Option<&AtomicBool>,
) -> Result<(std::process::ExitStatus, String, String)> {
    let shown = format!("sbx {}", args.first().map_or("", AsRef::as_ref));
    let (mut stdout, mut stderr) = (capture()?, capture()?);
    let mut child = sbx_command(program, args, ssh_agent, &stdout, &stderr)?
        .spawn()
        .with_context(|| format!("running {program}"))?;
    let Some(status) = wait(&mut child, Instant::now() + timeout, stop)? else {
        if stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            bail!("{shown} stopped: IMACS is quitting");
        }
        bail!("{shown} did not finish within {} s", timeout.as_secs());
    };
    let read = |file: &mut File| -> Result<String> {
        let mut text = String::new();
        file.rewind()?;
        file.read_to_string(&mut text)?;
        Ok(text)
    };
    Ok((status, read(&mut stdout)?, read(&mut stderr)?))
}

/// `sbx args` writing to `stdout` and `stderr`, without IMACS's own environment, the SSH agent unless
/// it is forwarded, and with the login shell's PATH.
fn sbx_command(
    program: &str,
    args: &[impl AsRef<str>],
    ssh_agent: bool,
    stdout: &File,
    stderr: &File,
) -> Result<Command> {
    let mut command = Command::new(program);
    command.args(args.iter().map(AsRef::as_ref)).stdin(Stdio::null());
    command.stdout(stdout.try_clone()?).stderr(stderr.try_clone()?).process_group(0);
    sbx_env(&mut command, ssh_agent);
    Ok(command)
}

/// The environment every sbx command gets: none of IMACS's own, no SSH agent unless it is
/// forwarded, and the login shell's PATH.
pub(crate) fn sbx_env(command: &mut Command, ssh_agent: bool) {
    for name in ["IMACS_SOCKET", "IMACS_PANE", "IMACS_HOME", "IMACS_CONFIG"] {
        command.env_remove(name);
    }
    if !ssh_agent {
        command.env_remove("SSH_AUTH_SOCK");
    }
    if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
        command.env("PATH", path);
    }
}

/// The child's status, or None once it ran past `deadline`, or `stop` was set, and was killed.
fn wait(
    child: &mut std::process::Child,
    deadline: Instant,
    stop: Option<&AtomicBool>,
) -> Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline || stop.is_some_and(|stop| stop.load(Ordering::Relaxed)) {
            // Kill helpers too (SSH/keychain commands can outlive the sbx client).
            // SAFETY: process_group(0) gave this child its own process group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn capture() -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path =
        std::env::temp_dir().join(format!("imacs-sbx-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let file = File::options().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

/// The fake `sbx` in `tests/fixtures/sbx`, for tests here and in the app.
#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Copies the fake into `dir`, where it keeps its state and call log, and returns its path.
    pub fn install(dir: &Path) -> PathBuf {
        let program = dir.join("sbx");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sbx"), &program).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// Every call so far, each as its arguments.
    pub fn calls(dir: &Path) -> Vec<Vec<String>> {
        let log = std::fs::read_to_string(dir.join("calls.log")).unwrap_or_default();
        log.lines().map(|line| line.split('\t').map(String::from).collect()).collect()
    }

    /// The calls whose first arguments are `verb`, such as `["mcp", "add"]`.
    pub fn calls_to(dir: &Path, verb: &[&str]) -> Vec<Vec<String>> {
        calls(dir).into_iter().filter(|call| call.len() >= verb.len() && call[..verb.len()] == *verb).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use imacs_core::isolation::IsolationMode;

    #[test]
    fn timed_out_sbx_kills_its_helper_process_group() {
        let dir = crate::test_support::tempdir().unwrap();
        let marker = dir.path().join("leaked-helper");
        let script = format!("(sleep 0.3; touch '{}') & wait", marker.display());
        let error = sbx_output("/bin/sh", &["-c", &script], false, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("did not finish"));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!marker.exists(), "a timed-out sbx helper survived and kept working");
    }

    fn repository() -> (crate::test_support::TempDir, PathBuf) {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.org"],
            &["commit", "--allow-empty", "-qm", "initial"],
        ] {
            git::git(&repo, args).unwrap();
        }
        (dir, repo)
    }

    fn job(sbx: &Path, name: &str, cwd: &Path, config: SandboxConfig) -> Prepare {
        Prepare {
            home: sbx.parent().unwrap().join("home"),
            sbx: sbx.display().to_string(),
            name: name.into(),
            agent: "codex",
            cwd: cwd.to_path_buf(),
            config,
            warm: Vec::new(),
            exe: "/Applications/IMACS.app/Contents/MacOS/imacs".into(),
            socket: "/run/user/501/imacs/imacs.sock".into(),
            briefs: None,
            pins: sbx.with_file_name("git-pins"),
            relay: None,
        }
    }

    #[test]
    fn linux_mounts_only_configured_warm_directories_inside_the_worktree() {
        use std::os::unix::fs::symlink;
        let root = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(root.path().join("target")).unwrap();
        std::fs::create_dir_all(root.path().join("build/cache")).unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        symlink(root.path().join(".git"), root.path().join("git-link")).unwrap();
        let folders = [
            "target",
            "build/cache",
            "target",
            "missing",
            "../escape",
            "/tmp",
            ".git/objects",
            "escape",
            "git-link",
            "",
        ]
        .map(String::from);
        assert!(warm_mounts(root.path(), &folders, false).is_empty());
        let expected = vec![real(root.path()).join("target"), real(root.path()).join("build/cache")];
        assert_eq!(warm_mounts(root.path(), &folders, true), expected);
        let spec = Spec {
            name: "imacs-linux".into(),
            agent: "codex",
            workspace: real(root.path()),
            git: Vec::new(),
            warm: expected.clone(),
            read_only: Vec::new(),
            memory: None,
            cpus: None,
            template: Some("local:cache".into()),
            env: Vec::new(),
        };
        let args = isolation::create_args(&spec);
        for path in expected {
            assert!(args.contains(&path.display().to_string()));
        }
        assert!(args.contains(&format!("CARGO_TARGET_DIR={}/target", real(root.path()).display())));
    }

    #[test]
    fn a_linked_worktree_mounts_only_gits_write_folders() {
        let (dir, repo) = repository();
        let worktree = dir.path().join("app-feature");
        git::add_worktree(&repo, &worktree, "feature", "main").unwrap();
        std::fs::create_dir(worktree.join("src")).unwrap();

        let (top, mounts) = workspace(&worktree.join("src")).unwrap();
        assert_eq!(top, worktree.canonicalize().unwrap(), "the whole worktree, from any folder in it");
        let admin = std::fs::read_to_string(worktree.join(".git")).unwrap();
        let admin = admin.trim().strip_prefix("gitdir: ").unwrap();
        let common = Path::new(admin).parent().unwrap().parent().unwrap();
        let expected: Vec<String> = ["objects", "refs", "logs"]
            .iter()
            .map(|folder| common.join(folder).display().to_string())
            .chain([admin.to_string()])
            .collect();
        // The admin folder is mounted at the very path the worktree's .git file names.
        assert_eq!(mounts, expected);
        assert!(mounts.iter().all(|m| !m.contains("hooks") && !m.ends_with("config")), "{mounts:?}");

        // A main checkout's whole git folder is one read-only mount point, which the agent cannot move.
        let (top, mounts) = workspace(&repo).unwrap();
        assert_eq!(top, repo.canonicalize().unwrap());
        assert_eq!(mounts, [format!("{}:ro", top.join(".git").display())]);

        let plain = crate::test_support::tempdir().unwrap();
        assert_eq!(workspace(plain.path()).unwrap(), (plain.path().to_path_buf(), Vec::new()));
        // A folder holding the home folder is never mounted whole, repository or not.
        for folder in [paths::home(), PathBuf::from("/")] {
            let error = workspace(&folder).unwrap_err().to_string();
            assert!(error.contains("whole home folder"), "{error}");
        }
    }

    #[test]
    fn a_session_gets_its_sandbox_once_and_reattaches_to_it() {
        let (dir, repo) = repository();
        let worktree = dir.path().join("app-task");
        git::add_worktree(&repo, &worktree, "task", "main").unwrap();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let docs = crate::test_support::tempdir().unwrap();
        let mut config = SandboxConfig {
            memory: Some("8g".into()),
            cpus: Some(4),
            template: Some("registry.example.com/agent:1".into()),
            mounts: vec![docs.path().display().to_string()],
            mcp: vec!["notion".into()],
            ..SandboxConfig::default()
        };
        config.allow_hosts.insert(repo.display().to_string(), vec!["nuget.example.com".into()]);
        config.allow_hosts.insert("/elsewhere".into(), vec!["other.example.com".into()]);
        let mut job = job(&sbx, "imacs-s1", &worktree, config);
        job.briefs = Some(bin.path().join("briefs/s1"));

        prepare(&job).unwrap();
        let verbs: Vec<String> = fake::calls(bin.path())
            .iter()
            .map(|call| call.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(
            verbs,
            ["version", "diagnose --json", "ls --json", "mcp add", "create --name", "mcp load", "policy allow"]
        );
        let (top, git) = workspace(&worktree).unwrap();
        let expected = Spec {
            name: "imacs-s1".into(),
            agent: "codex",
            workspace: top.clone(),
            git,
            warm: Vec::new(),
            read_only: vec![docs.path().to_path_buf(), bin.path().join("briefs/s1")],
            memory: Some("8g".into()),
            cpus: Some(4),
            template: Some("registry.example.com/agent:1".into()),
            env: Vec::new(),
        };
        assert_eq!(fake::calls_to(bin.path(), &["create"]), [isolation::create_args(&expected)]);
        assert!(bin.path().join("briefs/s1").is_dir(), "the brief folder exists before it is mounted");
        let add = &fake::calls_to(bin.path(), &["mcp", "add"])[0];
        assert_eq!(*add, isolation::mcp_add_args("imacs-s1", &job.exe, &job.socket));
        let owned = ownership::owned(&job.home).unwrap();
        assert!(owned["imacs-s1"].sandbox && owned["imacs-s1"].mcp);
        assert_eq!(
            fake::calls_to(bin.path(), &["mcp", "load"]),
            [isolation::mcp_load_args("imacs-s1", "imacs-s1")],
            "connections load after preparation, which never fails on them"
        );
        assert_eq!(
            fake::calls_to(bin.path(), &["policy"]),
            [["policy", "allow", "network", "--sandbox", "imacs-s1", "nuget.example.com"]]
        );
        assert_eq!(list(&job.sbx).unwrap()[0].workspace(), Some(top.as_path()));

        // After a restart the same session finds its sandbox and only attaches.
        let before = fake::calls(bin.path()).len();
        prepare(&job).unwrap();
        assert_eq!(
            fake::calls(bin.path())[before..],
            [vec!["version"], vec!["diagnose", "--json"], vec!["ls", "--json"]]
        );
        stop(&job.home, &job.sbx, "imacs-s1").unwrap();
        assert_eq!(fake::calls(bin.path()).last().unwrap(), &["stop", "imacs-s1"]);
    }

    #[test]
    fn a_reattached_sandbox_follows_the_relay_to_a_new_port_and_loses_its_rule_without_it() {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let keychain = bin.path().join("keychain");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/keychain"), &keychain).unwrap();
        std::fs::write(bin.path().join("keychain-value"), "sk-ant-oat01-PRIVATE-token").unwrap();
        let mut job = job(&sbx, "imacs-c1", &repo, SandboxConfig::default());
        job.config.azure.enabled = false;
        job.agent = "claude";
        let relay = |job: &mut Prepare| {
            let service = crate::relay::Service::new(job.home.clone(), crate::relay::Keychain::at(&keychain, true));
            job.relay = Some(std::sync::Arc::new(service));
        };
        relay(&mut job);
        let first = prepare(&job).unwrap().unwrap();
        let rules = || fake::calls_to(bin.path(), &["policy"]);
        assert_eq!(rules(), [isolation::relay_allow_args("imacs-c1", first.port)]);
        // The port is taken while IMACS is away: the restarted relay listens elsewhere, and the
        // sandbox's rule follows it.
        let taken = std::net::TcpListener::bind(("127.0.0.1", first.port)).ok();
        drop(job.relay.take());
        relay(&mut job);
        let second = prepare(&job).unwrap().unwrap();
        if taken.is_some() {
            assert_ne!(second.port, first.port);
            assert_eq!(
                rules()[1..],
                [
                    isolation::relay_remove_args("imacs-c1", first.port),
                    isolation::relay_allow_args("imacs-c1", second.port)
                ]
            );
        }
        assert_eq!(ownership::owned(&job.home).unwrap()["imacs-c1"].relay, Some(second.port));
        // Started again with the same relay, nothing changes.
        let before = rules().len();
        prepare(&job).unwrap().unwrap();
        assert_eq!(rules().len(), before);
        // Without a token, Claude gets no placeholder, and its sandbox no way to this machine's loopback.
        std::fs::remove_file(bin.path().join("keychain-value")).unwrap();
        assert_eq!(prepare(&job).unwrap(), None);
        assert_eq!(rules().last().unwrap(), &isolation::relay_remove_args("imacs-c1", second.port));
        assert_eq!(ownership::owned(&job.home).unwrap()["imacs-c1"].relay, None);
        drop(taken);
    }

    #[test]
    fn a_failing_step_stops_preparation_with_sbxs_reason() {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let mut config = SandboxConfig::default();
        config.allow_hosts.insert(repo.display().to_string(), vec!["nuget.example.com".into()]);
        std::fs::write(bin.path().join("fail-create"), "").unwrap();
        // Ownership was reserved before create; `sbx rm` of a sandbox it never made may fail.
        std::fs::write(bin.path().join("fail-rm"), "").unwrap();
        let attempt = job(&sbx, "imacs-s1", &repo, config.clone());
        let error = format!("{:#}", prepare(&attempt).unwrap_err());
        assert!(error.contains("creating the sandbox") && error.contains("fake create failure"), "{error}");
        let mut owned = ownership::owned(&attempt.home).unwrap();
        assert!(owned["imacs-s1"].sandbox, "an unlisted recent create stays reserved");
        assert!(owned["imacs-s1"].creator.is_none(), "the creating process finished");
        std::fs::remove_file(bin.path().join("fail-rm")).unwrap();
        // Once the create window expires, absence from inventory is conclusive.
        owned.get_mut("imacs-s1").unwrap().created = 0;
        std::fs::write(attempt.home.join("sandbox-ownership.json"), serde_json::to_vec(&owned).unwrap()).unwrap();
        remove(&attempt.home, &sbx.display().to_string(), "imacs-s1").unwrap();
        assert!(ownership::owned(&attempt.home).unwrap().is_empty());
        assert!(fake::calls_to(bin.path(), &["policy"]).is_empty());
        assert!(list(&sbx.display().to_string()).unwrap().is_empty());
        // No sandbox, no git record of it.
        let record = bin.path().join("git-pins/imacs-s1.json");
        assert!(!record.exists());

        // A step after create fails: the half-made sandbox goes, so the next start makes it whole.
        std::fs::remove_file(bin.path().join("fail-create")).unwrap();
        std::fs::write(bin.path().join("fail-policy"), "").unwrap();
        let error = format!("{:#}", prepare(&job(&sbx, "imacs-s1", &repo, config.clone())).unwrap_err());
        assert!(error.contains("allowing the repository's hosts"), "{error}");
        assert_eq!(fake::calls(bin.path()).last().unwrap(), &["mcp", "rm", "--force", "imacs-s1"]);
        assert!(list(&sbx.display().to_string()).unwrap().is_empty());
        assert!(!record.exists());
        std::fs::remove_file(bin.path().join("fail-policy")).unwrap();
        prepare(&job(&sbx, "imacs-s1", &repo, config)).unwrap();
        assert!(record.exists());
        assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 3, "the failed, the removed and the whole one");
        assert_eq!(list(&sbx.display().to_string()).unwrap().len(), 1);
        let missing = job(&bin.path().join("missing-sbx"), "imacs-s1", &repo, SandboxConfig::default());
        assert!(prepare(&missing).is_err());
    }

    #[test]
    fn a_crash_while_sbx_creates_leaves_owned_resources_the_next_sweep_removes() {
        // The IMACS that crashes is a child process of its own.
        if let Some(dir) = std::env::var_os("IMACS_CREATE_CRASH_TEST") {
            let dir = PathBuf::from(dir);
            let _ = prepare(&job(&dir.join("sbx"), "imacs-crashed", &dir.join("app"), SandboxConfig::default()));
            return;
        }
        let (dir, _repo) = repository();
        let sbx = fake::install(dir.path());
        std::fs::write(dir.path().join("wait-create"), "").unwrap();
        let mut child = crate::test_support::TestChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sandbox::tests::a_crash_while_sbx_creates_leaves_owned_resources_the_next_sweep_removes",
                    "--nocapture",
                ])
                .env("IMACS_CREATE_CRASH_TEST", dir.path()),
            crate::test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        while !dir.path().join("waiting-create").exists() {
            child.check_timeout().unwrap();
            assert!(child.child.try_wait().unwrap().is_none(), "preparation ended before sbx create");
            std::thread::sleep(Duration::from_millis(10));
        }
        // IMACS dies while sbx creates, and sbx, in a process group of its own, finishes the sandbox.
        drop(child);
        std::fs::remove_file(dir.path().join("wait-create")).unwrap();
        let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(30));
        while !dir.path().join("sandboxes/imacs-crashed").exists() {
            assert!(Instant::now() < deadline, "sbx never finished the sandbox");
            std::thread::sleep(Duration::from_millis(10));
        }
        let home = dir.path().join("home");
        let owned = ownership::owned(&home).unwrap();
        assert!(owned["imacs-crashed"].sandbox && owned["imacs-crashed"].mcp, "{owned:?}");

        // The next start's sweep knows both as this home's, and removes them.
        let program = sbx.display().to_string();
        let notes = crate::lifecycle::sweep_before(
            &home,
            &Default::default(),
            &program,
            crate::lifecycle::Cutoff::mark(&home).unwrap(),
        )
        .unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert!(!dir.path().join("sandboxes/imacs-crashed").exists());
        assert!(!dir.path().join("mcp/imacs-crashed").exists());
        assert!(ownership::owned(&home).unwrap().is_empty());
    }

    #[test]
    fn a_main_checkouts_git_folder_is_one_read_only_mount_and_older_sandboxes_are_refused() {
        let (_dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let top = repo.canonicalize().unwrap();
        let git_dir = top.join(".git");
        // Linux warm cache mounts sit beside the read-only .git, never inside it.
        std::fs::create_dir(repo.join("target")).unwrap();
        let mut main = job(&sbx, "imacs-main", &repo, SandboxConfig::default());
        main.warm = ["target", ".git", ".git/objects"].map(String::from).into();
        prepare(&main).unwrap();
        let create = fake::calls_to(bin.path(), &["create"]).pop().unwrap();
        let agent = create.iter().position(|arg| arg == "codex").unwrap();
        assert_eq!(warm_mounts(&top, &main.warm, true), [top.join("target")]);
        let mut expected = vec![top.display().to_string(), format!("{}:ro", git_dir.display())];
        if cfg!(target_os = "linux") {
            expected.push(top.join("target").display().to_string());
        }
        assert_eq!(create[agent + 1..], expected);
        stop(&main.home, &main.sbx, &main.name).unwrap();
        prepare(&main).unwrap();
        assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1, "it reattaches");

        // A sandbox made when only hooks and config were read-only cannot be reattached to.
        let old = format!(
            r#"{{"name":"imacs-old","agent":"shell","status":"stopped","workspaces":["{}","{}/hooks:ro","{}/config:ro"]}}"#,
            repo.display(),
            git_dir.display(),
            git_dir.display()
        );
        std::fs::write(bin.path().join("sandboxes/imacs-old"), old).unwrap();
        ownership::created(&main.home, "imacs-old", Resource::Sandbox).unwrap();
        let error = format!("{:#}", prepare(&job(&sbx, "imacs-old", &repo, SandboxConfig::default())).unwrap_err());
        assert!(error.contains(&format!("does not mount {}", git_dir.display())), "{error}");
        assert!(error.contains("sbx rm imacs-old"), "{error}");
        assert_eq!(fake::calls_to(bin.path(), &["create"]).len(), 1);
    }

    #[test]
    fn records_of_removed_sandboxes_go_unless_their_folder_changed() {
        let (dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let pins = bin.path().join("git-pins");
        let worktree = |name: &str| {
            let path = dir.path().join(name);
            git::add_worktree(&repo, &path, name, "main").unwrap();
            path
        };
        let live = worktree("live");
        prepare(&job(&sbx, "imacs-live", &live, SandboxConfig::default())).unwrap();
        // Sandboxes that are gone: a worktree as git made it, one whose .git file now names another
        // folder, a folder outside any repository, and a folder deleted since.
        let (clean, tampered) = (worktree("clean"), worktree("tampered"));
        let (plain, deleted) = (dir.path().join("plain"), dir.path().join("deleted"));
        for (name, folder) in [("clean", &clean), ("tampered", &tampered), ("plain", &plain), ("deleted", &deleted)] {
            std::fs::create_dir_all(folder).unwrap();
            pin_git(&pins, &format!("imacs-{name}"), folder).unwrap();
        }
        std::fs::write(tampered.join(".git"), format!("gitdir: {}\n", dir.path().join("elsewhere").display())).unwrap();
        std::fs::remove_dir(&deleted).unwrap();
        assert!(git::git(&plain, &["status"]).is_err(), "git stays out of a folder outside any repository");
        let records = || {
            let mut names: Vec<String> = read_pins(&pins).into_keys().collect();
            names.sort();
            names
        };

        // Without an sbx to ask, nothing is known to be gone.
        prune_pins(&bin.path().join("home"), &pins, &bin.path().join("missing-sbx").display().to_string());
        assert_eq!(records().len(), 5);
        prune_pins(&bin.path().join("home"), &pins, &sbx.display().to_string());
        assert_eq!(records(), ["imacs-live", "imacs-tampered"]);
        assert!(git::git(&plain, &["status"]).is_err_and(|e| format!("{e:#}").contains("not a git repository")));
        assert!(git::pinned(&clean).is_none() && git::pinned(&plain).is_none());
        assert!(git::pinned(&live).is_some() && git::pinned(&tampered).is_some());
        git::unpin(&git::pinned(&live).unwrap().work_tree);
        git::unpin(&git::pinned(&tampered).unwrap().work_tree);
    }

    #[test]
    fn preflight_refuses_a_missing_old_or_signed_out_sbx_with_what_to_do() {
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        let ready = preflight(&sbx).unwrap();
        assert_eq!(ready.version, (0, 50, 1));
        assert!(ready.diagnosis.signed_in(), "an unrelated failing check does not matter");
        assert!(
            doctor_lines(&sbx, &SandboxConfig::default(), checks::ClaudeRoute::Bare)[0]
                .starts_with("✓ Docker Sandboxes ready (sbx v0.50.1, signed in)")
        );
        std::fs::write(bin.path().join("version"), "v0.51.2").unwrap();
        assert_eq!(preflight(&sbx).unwrap().version, (0, 51, 2));

        let refusal = |program: &str, needles: &[&str]| {
            let error = preflight(program).unwrap_err();
            for needle in needles.iter().chain(&["[isolation] mode = \"bare\""]) {
                assert!(error.contains(needle), "{needle:?} missing from: {error}");
            }
            error
        };
        let missing = bin.path().join("missing").display().to_string();
        refusal(&missing, &["not installed", checks::install_hint("sbx", cfg!(target_os = "macos"))]);
        refusal("sbx-not-on-this-path", &["not installed"]);
        std::fs::write(bin.path().join("version"), "v0.49.9").unwrap();
        refusal(&sbx, &["sbx v0.49.9 is older than v0.50.1", checks::install_hint("sbx", cfg!(target_os = "macos"))]);
        std::fs::write(bin.path().join("version"), "dev").unwrap();
        refusal(&sbx, &["no version IMACS can read"]);
        std::fs::write(bin.path().join("version"), "v0.50.1").unwrap();
        std::fs::write(bin.path().join("signed-out"), "").unwrap();
        let error = refusal(&sbx, &["not signed in", "sbx login"]);
        let line = &doctor_lines(&sbx, &SandboxConfig::default(), checks::ClaudeRoute::Bare)[0];
        assert!(line.starts_with("✗ Docker Sandboxes not ready for sandboxed agents (warning) — "), "{line}");
        assert!(line.ends_with(&error), "{line}");
        std::fs::write(bin.path().join("fail-diagnose"), "").unwrap();
        let error = refusal(&sbx, &["sbx diagnose gave no readable report"]);
        assert!(!error.contains("fake diagnose failure"));

        // A refused sandbox is never made: preparation stops before sbx creates anything.
        std::fs::remove_file(bin.path().join("fail-diagnose")).unwrap();
        let plain = crate::test_support::tempdir().unwrap();
        let error =
            format!("{:#}", prepare(&job(Path::new(&sbx), "imacs-s1", plain.path(), Default::default())).unwrap_err());
        assert!(error.contains("not signed in"), "{error}");
        assert!(fake::calls_to(bin.path(), &["create"]).is_empty());
        assert!(fake::calls_to(bin.path(), &["mcp", "add"]).is_empty());
        assert!(fake::calls_to(bin.path(), &["mcp", "auth"]).is_empty());
    }

    #[test]
    fn the_network_preset_sets_up_sbxs_policy_only_when_it_has_none() {
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let plain = crate::test_support::tempdir().unwrap();
        let config = SandboxConfig { network: isolation::NetworkPreset::DenyAll, ..Default::default() };
        prepare(&job(&sbx, "imacs-s1", plain.path(), config.clone())).unwrap();
        assert!(fake::calls_to(bin.path(), &["policy", "init"]).is_empty(), "an existing policy is left alone");
        std::fs::write(bin.path().join("no-policy"), "").unwrap();
        prepare(&job(&sbx, "imacs-s2", plain.path(), config)).unwrap();
        assert_eq!(fake::calls_to(bin.path(), &["policy", "init"]), [["policy", "init", "deny-all"]]);
        let calls: Vec<String> = fake::calls(bin.path()).iter().map(|call| call[0].clone()).collect();
        let init = calls.iter().rposition(|verb| verb == "policy").unwrap();
        assert!(init < calls.iter().rposition(|verb| verb == "create").unwrap(), "{calls:?}");
    }

    #[test]
    fn cleanup_finds_sandboxes_by_worktree_and_lists_orphans() {
        let (dir, repo) = repository();
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        let mut worktrees = Vec::new();
        for name in ["a", "b"] {
            let worktree = dir.path().join(format!("app-{name}"));
            git::add_worktree(&repo, &worktree, name, "main").unwrap();
            prepare(&job(Path::new(&sbx), &format!("imacs-{name}"), &worktree, SandboxConfig::default())).unwrap();
            worktrees.push(worktree);
        }
        // Someone else's sandbox on the same worktree is never IMACS's to remove.
        std::fs::write(
            bin.path().join("sandboxes/poc-1"),
            format!(
                r#"{{"name":"poc-1","agent":"claude","status":"stopped","workspaces":["{}"]}}"#,
                worktrees[0].display()
            ),
        )
        .unwrap();

        let home = bin.path().join("home");
        let found = for_repo(&home, &sbx, &repo).unwrap();
        let listed: Vec<PathBuf> = git::list_worktrees(&repo).unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(found.attached, [(listed[1].clone(), "imacs-a".into()), (listed[2].clone(), "imacs-b".into())]);
        assert_eq!(real(&listed[1]), real(&worktrees[0]), "paths as git lists them, for the dialog's rows");
        assert!(found.orphans.is_empty());
        assert_eq!(in_worktree(&home, &sbx, &worktrees[0]).unwrap(), ["imacs-a"]);

        git::remove_worktree(&repo, &worktrees[1], None, false).unwrap();
        let found = for_repo(&home, &sbx, &repo).unwrap();
        assert_eq!(found.attached.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>(), ["imacs-a"]);
        assert_eq!(found.orphans.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["imacs-b"]);

        let guards = home.join("ref-guards");
        std::fs::create_dir_all(&guards).unwrap();
        for name in ["imacs-a", "imacs-b"] {
            std::fs::write(guards.join(format!("{name}.json")), "{}").unwrap();
        }
        remove(&home, &sbx, "imacs-b").unwrap();
        assert!(!guards.join("imacs-b.json").exists(), "the ref guard's baseline goes with its sandbox");
        assert!(guards.join("imacs-a.json").exists());
        let calls: Vec<_> =
            fake::calls(bin.path()).into_iter().filter(|args| args.iter().any(|arg| arg == "rm")).collect();
        assert_eq!(
            calls[calls.len() - 2..],
            [vec!["rm", "--force", "imacs-b"], vec!["mcp", "rm", "--force", "imacs-b"]]
        );
        let names: Vec<String> = list(&sbx).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["imacs-a", "poc-1"]);
        assert!(for_repo(&home, &sbx, &repo).unwrap().orphans.is_empty());
    }

    #[test]
    fn usage_comes_back_from_the_end_of_the_newest_rollout_in_the_sandbox() {
        let bin = crate::test_support::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        std::fs::write(bin.path().join("exec-runs"), "").unwrap();
        std::fs::create_dir_all(bin.path().join("sandboxes")).unwrap();
        std::fs::write(bin.path().join("sandboxes/imacs-u"), r#"{"name":"imacs-u","status":"running"}"#).unwrap();
        // The fake runs `exec` with HOME at its folder, as the VM's agent home.
        let day = bin.path().join(".codex/sessions/2026/10/30");
        std::fs::create_dir_all(&day).unwrap();
        let event = |used: u32| {
            format!(
                r#"{{"timestamp":"2026-10-30T11:32:20Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"primary":{{"used_percent":{used},"window_minutes":10080,"resets_at":1793864523}}}}}}}}"#
            )
        };
        let old = day.join("rollout-old.jsonl");
        std::fs::write(&old, event(50) + "\n").unwrap();
        File::options().write(true).open(&old).unwrap().set_modified(std::time::UNIX_EPOCH).unwrap();
        std::fs::write(day.join("rollout-new.jsonl"), format!("{}\n{}\n", "x".repeat(40_000), event(12))).unwrap();

        let output = read_usage(&sbx, "imacs-u").unwrap();
        assert!(output.len() < 20_000, "the VM keeps only the end of the rollout: {} bytes", output.len());
        let reports = crate::usage::sandbox_reports(&output, i64::MAX);
        let [crate::usage::Report::Codex(usage)] = reports.as_slice() else { panic!("one Codex report: {output}") };
        assert_eq!(usage.weekly.as_ref().unwrap().remaining, 88.0, "from the newest rollout");
        assert!(read_usage(&sbx, "imacs-gone").is_err());
    }

    #[test]
    fn the_terminal_runs_sbx_run_and_hides_the_ssh_agent_unless_forwarded() {
        let mut sandboxed = Sandboxed {
            sbx: "/opt/homebrew/bin/sbx".into(),
            name: "imacs-s1".into(),
            agent: "claude",
            ssh_agent: false,
        };
        let args = ["--resume".to_string(), "abc".to_string()];
        assert_eq!(
            sandboxed.command(&args),
            (
                "/usr/bin/env".to_string(),
                [
                    "-u",
                    "SSH_AUTH_SOCK",
                    "/opt/homebrew/bin/sbx",
                    "run",
                    "--name",
                    "imacs-s1",
                    "claude",
                    "--",
                    "--resume",
                    "abc"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        sandboxed.ssh_agent = true;
        assert_eq!(sandboxed.command(&[]).0, "/opt/homebrew/bin/sbx");
        assert_eq!(IsolationMode::default(), IsolationMode::Sandbox);
    }
}
