//! Docker Sandboxes runtime: a sandboxed agent runs in its own microVM, created when the agent
//! starts and named after its session, attached to the tile's terminal with `sbx run`, stopped
//! when the agent exits and removed with its worktree. The command lines come from
//! [`imacs_core::isolation`]; this module runs them.

use std::fs::File;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use imacs_core::isolation::{self, Diagnosis, Listed, MIN_SBX_VERSION, SandboxConfig, Spec};
use imacs_core::{git, paths, workspace};

/// Most sbx commands answer within a second; this bounds a stuck daemon.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The first sandbox of an agent pulls its image, which took 51 s on a fast connection.
const CREATE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

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
    pub sbx: String,
    pub name: String,
    pub agent: &'static str,
    pub cwd: PathBuf,
    pub config: SandboxConfig,
    /// IMACS's executable and socket, for the MCP server agents report through.
    pub exe: PathBuf,
    pub socket: PathBuf,
    /// A folder of this session's own, mounted read-only for its brief.
    pub briefs: Option<PathBuf>,
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
            "Docker Sandboxes (sbx) is not installed. Install it (brew install --cask docker/tap/sbx, \
             or see https://github.com/docker/sbx-releases) and sign in with `sbx login`, {BARE}."
        ));
    }
    let output = sbx(program, &["version"], false, TIMEOUT)
        .map_err(|err| format!("sbx did not report its version ({err:#}). Run `sbx version` to see why, {BARE}."))?;
    let version = isolation::parse_version(&output).ok_or_else(|| {
        format!("sbx reported no version IMACS can read ({:?}). Check `sbx version`, {BARE}.", output.trim())
    })?;
    if version < MIN_SBX_VERSION {
        return Err(format!(
            "sbx {} is older than {}, the oldest IMACS supports. Upgrade it (brew upgrade --cask sbx), {BARE}.",
            isolation::version_text(version),
            isolation::version_text(MIN_SBX_VERSION)
        ));
    }
    // diagnose exits non-zero when any check fails, such as an unrelated SSH setting; the report still holds.
    let (_, output, error) = sbx_output(program, &["diagnose", "--json"], false, TIMEOUT)
        .map_err(|err| format!("sbx diagnose did not run ({err:#}). Run `sbx diagnose` to see why, {BARE}."))?;
    let diagnosis: Diagnosis = serde_json::from_str(&output).map_err(|_| {
        let detail = [error.trim(), output.trim()].into_iter().find(|t| !t.is_empty()).unwrap_or("no output");
        format!("sbx diagnose gave no report ({detail}). Run `sbx diagnose` to see why, {BARE}.")
    })?;
    if !diagnosis.signed_in() {
        return Err(format!("sbx is not signed in to Docker. Run `sbx login` in a terminal, {BARE}."));
    }
    Ok(Ready { version, diagnosis })
}

/// The doctor's line about sandboxes: a warning, since bare agents need none of this.
pub fn doctor_line(program: &str) -> String {
    match preflight(program) {
        Ok(ready) => imacs_core::doctor::check_line(
            true,
            &format!("Docker Sandboxes ready (sbx {}, signed in)", isolation::version_text(ready.version)),
            "",
        ),
        Err(error) => {
            imacs_core::doctor::check_line(false, "Docker Sandboxes not ready for sandboxed agents (warning)", &error)
        }
    }
}

/// Makes the sandbox unless it exists, which is how a session reattaches after a restart,
/// once sbx passes [`preflight`]. A new sandbox mounts the worktree and git's write folders,
/// gets IMACS's MCP server, the configured ones and its repository's allowed hosts.
pub fn prepare(job: &Prepare) -> Result<()> {
    let ssh = job.config.ssh_agent;
    let ready = preflight(&job.sbx).map_err(anyhow::Error::msg)?;
    if list(&job.sbx)?.iter().any(|sandbox| sandbox.name == job.name) {
        return Ok(());
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
    let (workspace, git) = workspace(&job.cwd)?;
    if let Some(briefs) = &job.briefs {
        std::fs::create_dir_all(briefs).with_context(|| format!("creating {}", briefs.display()))?;
    }
    let mut read_only: Vec<PathBuf> = job.config.mounts.iter().map(|mount| paths::expand(mount)).collect();
    read_only.extend(job.briefs.iter().cloned());
    let spec = Spec {
        name: job.name.clone(),
        agent: job.agent,
        workspace,
        git,
        read_only,
        memory: job.config.memory.clone(),
        cpus: job.config.cpus,
        template: job.config.template.clone(),
        mcp: std::iter::once(job.name.clone()).chain(job.config.mcp.iter().cloned()).collect(),
    };
    // A registration left by an earlier attempt would make `mcp add` fail.
    let _ = sbx(&job.sbx, &["mcp", "rm", "--force", &job.name], ssh, TIMEOUT);
    sbx(&job.sbx, &isolation::mcp_add_args(&job.name, &job.exe, &job.socket), ssh, TIMEOUT)
        .context("registering IMACS's MCP server with sbx")?;
    sbx(&job.sbx, &isolation::create_args(&spec), ssh, CREATE_TIMEOUT).context("creating the sandbox")?;
    let root = real(&workspace::project_root(&job.cwd).unwrap_or_else(|_| job.cwd.clone()));
    let hosts = job.config.hosts_for(&root, |repo| real(&paths::expand(repo)));
    if !hosts.is_empty() {
        sbx(&job.sbx, &isolation::allow_hosts_args(&job.name, &hosts), ssh, TIMEOUT)
            .context("allowing the repository's hosts")?;
    }
    Ok(())
}

/// The folder a sandbox mounts read-write for `cwd`, its worktree (else `cwd` itself), and
/// git's folders beside it. A linked worktree gets the common dir's objects, refs and logs and
/// its own admin folder, so commits work while the repository's hooks and config stay on this
/// machine. A checkout whose git dir lies inside it keeps that dir's hooks and config read-only.
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
        if !git_dir.starts_with(&top) {
            mounts.push(git_dir.to_string_lossy().into_owned());
        }
        mounts
            .extend(["hooks", "config"].into_iter().filter_map(|path| existing(git_dir.join(path))).map(|p| p + ":ro"));
    }
    Ok((top, mounts))
}

/// Every sandbox sbx knows.
pub fn list(program: &str) -> Result<Vec<Listed>> {
    let output = sbx(program, &["ls", "--json"], false, TIMEOUT)?;
    isolation::parse_list(&output).context("reading `sbx ls --json`")
}

/// Stops a sandbox, keeping it for the next `sbx run`.
pub fn stop(program: &str, name: &str) -> Result<()> {
    sbx(program, &["stop", name], false, TIMEOUT).map(drop)
}

/// Removes a sandbox and IMACS's MCP registration for it.
pub fn remove(program: &str, name: &str) -> Result<()> {
    sbx(program, &["rm", "--force", name], false, TIMEOUT)?;
    let _ = sbx(program, &["mcp", "rm", "--force", name], false, TIMEOUT);
    Ok(())
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
pub fn for_repo(program: &str, repo: &Path) -> Result<RepoSandboxes> {
    let listed: Vec<Listed> = list(program)?.iter().map(real_paths).collect();
    let worktrees: Vec<PathBuf> = git::list_worktrees(repo)?.into_iter().map(|w| w.path).collect();
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
pub fn in_worktree(program: &str, worktree: &Path) -> Result<Vec<String>> {
    let worktree = real(worktree);
    Ok(list(program)?
        .iter()
        .map(real_paths)
        .filter(|sandbox| sandbox.is_imacs() && sandbox.workspace() == Some(worktree.as_path()))
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

/// Runs `sbx args` within `timeout` and returns its output, failing when sbx does.
fn sbx(program: &str, args: &[impl AsRef<str>], ssh_agent: bool, timeout: Duration) -> Result<String> {
    let (status, out, err) = sbx_output(program, args, ssh_agent, timeout)?;
    if !status.success() {
        let shown = args.first().map_or("", AsRef::as_ref);
        let detail = [err.trim(), out.trim()].into_iter().find(|text| !text.is_empty()).unwrap_or("no output");
        bail!("sbx {shown} failed ({status}): {detail}");
    }
    Ok(out)
}

/// Runs `sbx args` within `timeout`: its status, output and errors. Output goes to unlinked
/// files, so a daemon sbx starts and that inherits them cannot keep this waiting.
fn sbx_output(
    program: &str,
    args: &[impl AsRef<str>],
    ssh_agent: bool,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, String, String)> {
    let args: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    let shown = format!("sbx {}", args.first().copied().unwrap_or_default());
    let (mut stdout, mut stderr) = (capture()?, capture()?);
    let mut command = Command::new(program);
    command.args(&args).stdin(Stdio::null()).stdout(stdout.try_clone()?).stderr(stderr.try_clone()?);
    for name in ["IMACS_SOCKET", "IMACS_PANE", "IMACS_HOME", "IMACS_CONFIG"] {
        command.env_remove(name);
    }
    if !ssh_agent {
        command.env_remove("SSH_AUTH_SOCK");
    }
    if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
        command.env("PATH", path);
    }
    let mut child = command.spawn().with_context(|| format!("running {program}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{shown} did not finish within {} s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |file: &mut File| -> Result<String> {
        let mut text = String::new();
        file.rewind()?;
        file.read_to_string(&mut text)?;
        Ok(text)
    };
    Ok((status, read(&mut stdout)?, read(&mut stderr)?))
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

    fn repository() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
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
            sbx: sbx.display().to_string(),
            name: name.into(),
            agent: "codex",
            cwd: cwd.to_path_buf(),
            config,
            exe: "/Applications/IMACS.app/Contents/MacOS/imacs".into(),
            socket: "/run/user/501/imacs/imacs.sock".into(),
            briefs: None,
        }
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

        let (top, mounts) = workspace(&repo).unwrap();
        assert_eq!(top, repo.canonicalize().unwrap());
        let git_dir = top.join(".git");
        assert_eq!(mounts, [format!("{}/hooks:ro", git_dir.display()), format!("{}/config:ro", git_dir.display())]);

        let plain = tempfile::tempdir().unwrap();
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
        let bin = tempfile::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let docs = tempfile::tempdir().unwrap();
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
            ["version", "diagnose --json", "ls --json", "mcp rm", "mcp add", "create --name", "policy allow"]
        );
        let (top, git) = workspace(&worktree).unwrap();
        let expected = Spec {
            name: "imacs-s1".into(),
            agent: "codex",
            workspace: top.clone(),
            git,
            read_only: vec![docs.path().to_path_buf(), bin.path().join("briefs/s1")],
            memory: Some("8g".into()),
            cpus: Some(4),
            template: Some("registry.example.com/agent:1".into()),
            mcp: vec!["imacs-s1".into(), "notion".into()],
        };
        assert_eq!(fake::calls_to(bin.path(), &["create"]), [isolation::create_args(&expected)]);
        assert!(bin.path().join("briefs/s1").is_dir(), "the brief folder exists before it is mounted");
        let add = &fake::calls_to(bin.path(), &["mcp", "add"])[0];
        assert_eq!(*add, isolation::mcp_add_args("imacs-s1", &job.exe, &job.socket));
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
        stop(&job.sbx, "imacs-s1").unwrap();
        assert_eq!(fake::calls(bin.path()).last().unwrap(), &["stop", "imacs-s1"]);
    }

    #[test]
    fn a_failing_step_stops_preparation_with_sbxs_reason() {
        let (_dir, repo) = repository();
        let bin = tempfile::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let mut config = SandboxConfig::default();
        config.allow_hosts.insert(repo.display().to_string(), vec!["nuget.example.com".into()]);
        std::fs::write(bin.path().join("fail-create"), "").unwrap();
        let error = format!("{:#}", prepare(&job(&sbx, "imacs-s1", &repo, config)).unwrap_err());
        assert!(error.contains("creating the sandbox") && error.contains("fake create failure"), "{error}");
        assert!(fake::calls_to(bin.path(), &["policy"]).is_empty());
        assert!(list(&sbx.display().to_string()).unwrap().is_empty());
        let missing = job(&bin.path().join("missing-sbx"), "imacs-s1", &repo, SandboxConfig::default());
        assert!(prepare(&missing).is_err());
    }

    #[test]
    fn preflight_refuses_a_missing_old_or_signed_out_sbx_with_what_to_do() {
        let bin = tempfile::tempdir().unwrap();
        let sbx = fake::install(bin.path()).display().to_string();
        let ready = preflight(&sbx).unwrap();
        assert_eq!(ready.version, (0, 50, 1));
        assert!(ready.diagnosis.signed_in(), "an unrelated failing check does not matter");
        assert!(doctor_line(&sbx).starts_with("✓ Docker Sandboxes ready (sbx v0.50.1, signed in)"));
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
        refusal(&missing, &["not installed", "brew install --cask docker/tap/sbx"]);
        refusal("sbx-not-on-this-path", &["not installed"]);
        std::fs::write(bin.path().join("version"), "v0.49.9").unwrap();
        refusal(&sbx, &["sbx v0.49.9 is older than v0.50.1", "brew upgrade --cask sbx"]);
        std::fs::write(bin.path().join("version"), "dev").unwrap();
        refusal(&sbx, &["no version IMACS can read"]);
        std::fs::write(bin.path().join("version"), "v0.50.1").unwrap();
        std::fs::write(bin.path().join("signed-out"), "").unwrap();
        let error = refusal(&sbx, &["not signed in", "sbx login"]);
        let line = doctor_line(&sbx);
        assert!(line.starts_with("✗ Docker Sandboxes not ready for sandboxed agents (warning) — "), "{line}");
        assert!(line.ends_with(&error), "{line}");
        std::fs::write(bin.path().join("fail-diagnose"), "").unwrap();
        refusal(&sbx, &["sbx diagnose gave no report", "fake diagnose failure"]);

        // A refused sandbox is never made: preparation stops before sbx creates anything.
        std::fs::remove_file(bin.path().join("fail-diagnose")).unwrap();
        let plain = tempfile::tempdir().unwrap();
        let error =
            format!("{:#}", prepare(&job(Path::new(&sbx), "imacs-s1", plain.path(), Default::default())).unwrap_err());
        assert!(error.contains("not signed in"), "{error}");
        assert!(fake::calls_to(bin.path(), &["create"]).is_empty() && fake::calls_to(bin.path(), &["mcp"]).is_empty());
    }

    #[test]
    fn the_network_preset_sets_up_sbxs_policy_only_when_it_has_none() {
        let bin = tempfile::tempdir().unwrap();
        let sbx = fake::install(bin.path());
        let plain = tempfile::tempdir().unwrap();
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
        let bin = tempfile::tempdir().unwrap();
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

        let found = for_repo(&sbx, &repo).unwrap();
        let listed: Vec<PathBuf> = git::list_worktrees(&repo).unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(found.attached, [(listed[1].clone(), "imacs-a".into()), (listed[2].clone(), "imacs-b".into())]);
        assert_eq!(real(&listed[1]), real(&worktrees[0]), "paths as git lists them, for the dialog's rows");
        assert!(found.orphans.is_empty());
        assert_eq!(in_worktree(&sbx, &worktrees[0]).unwrap(), ["imacs-a"]);

        git::remove_worktree(&repo, &worktrees[1], None, false).unwrap();
        let found = for_repo(&sbx, &repo).unwrap();
        assert_eq!(found.attached.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>(), ["imacs-a"]);
        assert_eq!(found.orphans.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["imacs-b"]);

        remove(&sbx, "imacs-b").unwrap();
        let calls = fake::calls(bin.path());
        assert_eq!(
            calls[calls.len() - 2..],
            [vec!["rm", "--force", "imacs-b"], vec!["mcp", "rm", "--force", "imacs-b"]]
        );
        let names: Vec<String> = list(&sbx).unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["imacs-a", "poc-1"]);
        assert!(for_repo(&sbx, &repo).unwrap().orphans.is_empty());
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
