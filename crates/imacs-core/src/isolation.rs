//! Where agents run. `sandbox` gives each agent its own Docker Sandboxes microVM (`sbx`),
//! which sees only the agent's worktree and git's write folders; `bare` runs agents on this
//! machine as before. This module holds the settings and the pure parts of driving `sbx`:
//! naming, agent mapping, command lines and parsing its output. Running it is the app's job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{AgentConfig, AgentKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IsolationMode {
    #[default]
    Sandbox,
    Bare,
}

impl IsolationMode {
    pub fn name(self) -> &'static str {
        match self {
            IsolationMode::Sandbox => "sandbox",
            IsolationMode::Bare => "bare",
        }
    }
}

/// `[isolation]`: the mode every agent runs in unless a plan task or its `[[agents]]` entry says otherwise.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IsolationConfig {
    pub mode: IsolationMode,
    pub sandbox: SandboxConfig,
    /// Set by a repository's `mode = "sandbox"`: nothing runs bare there. Never read from a file.
    #[serde(skip)]
    pub required: bool,
}

/// The global network policy IMACS sets up when `sbx` has none yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkPreset {
    AllowAll,
    #[default]
    Balanced,
    DenyAll,
}

impl NetworkPreset {
    pub fn name(self) -> &'static str {
        match self {
            NetworkPreset::AllowAll => "allow-all",
            NetworkPreset::Balanced => "balanced",
            NetworkPreset::DenyAll => "deny-all",
        }
    }
}

/// `[isolation.sandbox]`: how each sandbox is made. Unset limits follow sbx's defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    /// Memory limit in sbx's units, such as `8g`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u32>,
    /// Container image for every sandbox instead of the agent's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Extra host folders, always mounted read-only.
    pub mounts: Vec<String>,
    pub network: NetworkPreset,
    /// Hosts a repository's sandboxes may reach beyond the network policy, keyed by its main checkout.
    pub allow_hosts: BTreeMap<String, Vec<String>>,
    /// Lets sandboxes use this machine's SSH agent to sign; off unless set.
    pub ssh_agent: bool,
    /// MCP servers registered with `sbx mcp add`, loaded into every sandbox.
    pub mcp: Vec<String>,
}

impl IsolationConfig {
    /// The mode `agent` runs in for a plan task with its own `task` isolation, if any: sandboxed
    /// where a repository requires it, else the task's, the agent's or `mode`, in that order.
    pub fn mode_for(&self, agent: &AgentConfig, task: Option<IsolationMode>) -> IsolationMode {
        if self.required {
            return IsolationMode::Sandbox;
        }
        task.or(agent.isolation).unwrap_or(self.mode)
    }

    /// These settings with a workspace's `[isolation]` merged in key by key, as a host file's are.
    pub fn with_overrides(&self, overrides: &IsolationOverrides) -> Result<IsolationConfig, toml::de::Error> {
        let base = toml::Table::try_from(self).expect("isolation settings serialize to a table");
        crate::config::Config::merged([base, overrides.0.clone()]).try_into()
    }
}

/// A workspace's `[isolation]` as written, merged over the main and host config's whenever
/// either changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IsolationOverrides(pub toml::Table);

/// Memory each sandbox gets unless `memory` says otherwise.
pub const DEFAULT_MEMORY: &str = "8g";

impl SandboxConfig {
    /// CPUs for each sandbox: `cpus` when set (0 lets sbx give it every core), else one test
    /// run's share of the compiler job pool, `jobs ÷ test_slots`, and at least 2. Sandboxes cannot
    /// use IMACS's job pool or queue, and sbx's own default, every core for each sandbox, would
    /// oversubscribe the machine with several agents.
    pub fn cpus_or_default(&self, resources: &crate::governor::ResourceConfig, cores: usize) -> u32 {
        self.cpus.unwrap_or_else(|| {
            let share = resources.jobs(cores) / resources.slots(crate::governor::SlotClass::Test, cores);
            u32::try_from(share).unwrap_or(u32::MAX).max(2)
        })
    }

    /// Memory for each sandbox: `memory` when set (`""` lets sbx give it half of this machine's),
    /// else [`DEFAULT_MEMORY`].
    pub fn memory_or_default(&self) -> String {
        self.memory.clone().unwrap_or_else(|| DEFAULT_MEMORY.into())
    }

    /// These settings with the CPU and memory defaults filled in, as a new sandbox gets them.
    pub fn with_defaults(&self, resources: &crate::governor::ResourceConfig, cores: usize) -> SandboxConfig {
        let (cpus, memory) = (self.cpus_or_default(resources, cores), self.memory_or_default());
        SandboxConfig { cpus: Some(cpus), memory: Some(memory), ..self.clone() }
    }

    /// The hosts allowed for the repository whose main checkout is `root`; keys are expanded
    /// with `expand` (`~/` paths), and every matching key adds its hosts.
    pub fn hosts_for(&self, root: &Path, expand: impl Fn(&str) -> PathBuf) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .allow_hosts
            .iter()
            .filter(|(repo, _)| expand(repo) == root)
            .flat_map(|(_, hosts)| hosts.iter().map(|host| host.trim().to_string()))
            .filter(|host| !host.is_empty())
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }
}

/// The oldest `sbx` IMACS drives: the one its mounts, MCP loading and credential proxy were checked with.
pub const MIN_SBX_VERSION: (u64, u64, u64) = (0, 50, 1);

/// Every sandbox IMACS creates is named with this prefix, so cleanup never touches anyone else's.
pub const NAME_PREFIX: &str = "imacs-";

/// The sandbox for the agent session `session`.
pub fn sandbox_name(session: &str) -> String {
    format!("{NAME_PREFIX}{session}")
}

/// Agents `sbx` knows how to run.
const SBX_AGENTS: [&str; 11] =
    ["claude", "codex", "copilot", "cursor", "devin", "docker-agent", "droid", "gemini", "kiro", "opencode", "shell"];

/// The `sbx` agent a configured agent runs as: Claude agents as claude, shells as shell, and
/// other agents by their program's name when sbx has an agent of that name.
pub fn sbx_agent(agent: &AgentConfig) -> Option<&'static str> {
    match agent.kind {
        AgentKind::Claude => Some("claude"),
        AgentKind::Shell => Some("shell"),
        AgentKind::Generic => {
            let program = agent.command.first()?;
            let name = Path::new(program).file_name()?.to_str()?;
            SBX_AGENTS.iter().copied().find(|known| *known == name)
        }
    }
}

/// What a new sandbox is made from. `workspace` is mounted read-write, `git` are git's write
/// folders (also read-write) or protected paths ending in `:ro`, and `read_only` are mounted read-only.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub name: String,
    pub agent: &'static str,
    pub workspace: PathBuf,
    pub git: Vec<String>,
    pub read_only: Vec<PathBuf>,
    pub memory: Option<String>,
    pub cpus: Option<u32>,
    pub template: Option<String>,
}

/// `sbx create` for `spec`: flags, the agent, then every workspace path.
pub fn create_args(spec: &Spec) -> Vec<String> {
    let mut args: Vec<String> = ["create", "--name", &spec.name, "--no-progress"].map(String::from).into();
    if let Some(memory) = spec.memory.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        args.extend(["--memory".into(), memory.into()]);
    }
    if let Some(cpus) = spec.cpus.filter(|&cpus| cpus > 0) {
        args.extend(["--cpus".into(), cpus.to_string()]);
    }
    if let Some(template) = spec.template.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        args.extend(["--template".into(), template.into()]);
    }
    args.push(spec.agent.into());
    args.push(spec.workspace.to_string_lossy().into_owned());
    args.extend(spec.git.iter().cloned());
    args.extend(spec.read_only.iter().map(|path| format!("{}:ro", path.display())));
    args
}

/// `sbx run`: attach the tile's terminal to the agent in sandbox `name`, starting the sandbox when stopped.
pub fn run_args(name: &str, agent: &str, agent_args: &[String]) -> Vec<String> {
    ["run", "--name", name, agent, "--"].into_iter().map(String::from).chain(agent_args.iter().cloned()).collect()
}

/// How many values a dropped flag takes.
#[derive(Clone, Copy)]
enum Takes {
    Nothing,
    One,
    /// Every following argument up to the next flag, like Claude's `--add-dir a b`.
    Several,
}

/// An agent's own arguments, as its sandbox runs it. sbx runs Codex with `sandbox_mode =
/// "danger-full-access"` and `approval_policy = "never"`, and Claude without permission prompts:
/// the VM is the boundary. The host's sandbox, approval and permission flags, and folders
/// outside the sandbox, would conflict with that, so they are dropped. Everything else, such as
/// the model and reasoning effort, is kept.
pub fn sandboxed_args(agent: &str, args: &[String]) -> Vec<String> {
    use Takes::*;
    let (flags, config_keys): (&[(&str, Takes)], &[&str]) = match agent {
        "codex" => (
            &[
                ("-s", One),
                ("--sandbox", One),
                ("-a", One),
                ("--ask-for-approval", One),
                ("--add-dir", One),
                ("--full-auto", Nothing),
                ("--dangerously-bypass-approvals-and-sandbox", Nothing),
                ("--yolo", Nothing),
            ],
            &["sandbox_mode", "approval_policy", "sandbox_workspace_write"],
        ),
        "claude" => (
            &[
                ("--permission-mode", One),
                ("--add-dir", Several),
                ("--dangerously-skip-permissions", Nothing),
                ("--allow-dangerously-skip-permissions", Nothing),
            ],
            &[],
        ),
        _ => return args.to_vec(),
    };
    // A `-c key=value` override of a setting sbx makes, by key or a key below it.
    let host_setting = |value: &str| {
        let key = value.split('=').next().unwrap_or_default().trim();
        config_keys.iter().any(|k| key == *k || key.strip_prefix(k).is_some_and(|rest| rest.starts_with('.')))
    };
    let mut kept = Vec::new();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value)),
            _ => (arg.as_str(), None),
        };
        if let Some((_, takes)) = flags.iter().find(|(flag, _)| *flag == name) {
            match (takes, inline) {
                (One, None) => drop(args.next()),
                (Several, None) => while args.next_if(|value| !value.starts_with('-')).is_some() {},
                _ => {}
            }
            continue;
        }
        if matches!(name, "-c" | "--config") {
            let value = inline.or_else(|| args.peek().map(|value| value.as_str()));
            if value.is_some_and(host_setting) {
                if inline.is_none() {
                    args.next();
                }
                continue;
            }
        }
        kept.push(arg.clone());
    }
    kept
}

/// `sbx mcp add`: register IMACS's MCP server for sandbox `name`. It runs on this machine as
/// `exe mcp --agent`, knowing its session as the sandbox's name, which outlives IMACS restarts.
pub fn mcp_add_args(name: &str, exe: &Path, socket: &Path) -> Vec<String> {
    let mut args: Vec<String> = ["mcp", "add", name, "--command", "/usr/bin/env"].map(String::from).into();
    let command = [
        format!("IMACS_PANE={name}"),
        format!("IMACS_SOCKET={}", socket.display()),
        exe.display().to_string(),
        "mcp".into(),
        "--agent".into(),
    ];
    // `--args` is a comma-separated list; quoting keeps a comma in a path inside one argument.
    args.extend(command.iter().map(|arg| format!("--args={}", csv_field(arg))));
    args
}

/// `sbx mcp load`: give sandbox `name` a server registered with `sbx mcp add`. The sandbox
/// keeps it, and a stopped sandbox gets it when it starts again.
pub fn mcp_load_args(server: &str, name: &str) -> Vec<String> {
    ["mcp", "load", server.trim(), "--sandbox", name].map(String::from).into()
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n']) { format!("\"{}\"", value.replace('"', "\"\"")) } else { value.to_string() }
}

/// `sbx policy allow network`: let sandbox `name` reach `hosts` beyond the global policy.
pub fn allow_hosts_args(name: &str, hosts: &[String]) -> Vec<String> {
    ["policy", "allow", "network", "--sandbox", name].into_iter().map(String::from).chain([hosts.join(",")]).collect()
}

/// The version `sbx version` prints, such as `sbx version v0.50.1 ddb0268`.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split_whitespace().find_map(|word| {
        let mut parts = word.strip_prefix('v')?.split(['.', '-', '+']);
        let mut number = || parts.next()?.parse::<u64>().ok();
        Some((number()?, number()?, number()?))
    })
}

pub fn version_text((major, minor, patch): (u64, u64, u64)) -> String {
    format!("v{major}.{minor}.{patch}")
}

/// One check of `sbx diagnose --json`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Check {
    pub name: String,
    pub status: String,
    pub message: String,
    pub hint: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Diagnosis {
    pub checks: Vec<Check>,
}

impl Diagnosis {
    pub fn check(&self, name: &str) -> Option<&Check> {
        self.checks.iter().find(|check| check.name.eq_ignore_ascii_case(name))
    }

    /// Whether sbx is signed in to Docker: its Authentication check passed.
    pub fn signed_in(&self) -> bool {
        self.check("Authentication").is_some_and(|check| check.status == "pass")
    }

    /// Whether sbx's global network policy has been set up.
    pub fn policy_initialized(&self) -> bool {
        self.check("Global network policy").is_none_or(|check| check.status == "pass")
    }
}

/// One sandbox as `sbx ls --json` lists it. The first workspace is the one mounted read-write.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listed {
    pub name: String,
    pub agent: String,
    pub status: String,
    pub workspaces: Vec<String>,
}

impl Listed {
    /// The sandbox's main workspace, without a `:ro` suffix.
    pub fn workspace(&self) -> Option<&Path> {
        self.workspaces.first().map(|w| Path::new(w.strip_suffix(":ro").unwrap_or(w)))
    }

    pub fn is_imacs(&self) -> bool {
        self.name.starts_with(NAME_PREFIX)
    }

    /// Whether any of its mounts lies in `dir`, such as a repository's git common dir.
    pub fn mounts_within(&self, dir: &Path) -> bool {
        self.workspaces.iter().any(|w| Path::new(w.strip_suffix(":ro").unwrap_or(w)).starts_with(dir))
    }
}

pub fn parse_list(json: &str) -> Result<Vec<Listed>, serde_json::Error> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(json)
}

/// IMACS's sandboxes for one repository, split into those whose worktree is still there and
/// orphans: sandboxes whose worktree folder is gone or no longer one of `worktrees`. A sandbox
/// belongs to the repository when it works in one of `worktrees` or mounts from `common`.
pub fn partition<'a>(
    listed: &'a [Listed],
    worktrees: &[PathBuf],
    common: &Path,
    exists: impl Fn(&Path) -> bool,
) -> (Vec<&'a Listed>, Vec<&'a Listed>) {
    let ours = listed.iter().filter(|sandbox| {
        sandbox.is_imacs()
            && (sandbox.workspace().is_some_and(|w| worktrees.iter().any(|t| t == w)) || sandbox.mounts_within(common))
    });
    ours.partition(|sandbox| {
        sandbox.workspace().is_some_and(|workspace| exists(workspace) && worktrees.iter().any(|t| t == workspace))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn agent(name: &str, command: &[&str], kind: AgentKind, isolation: Option<IsolationMode>) -> AgentConfig {
        AgentConfig {
            name: name.into(),
            command: command.iter().map(|c| c.to_string()).collect(),
            kind,
            busy_text: None,
            isolation,
        }
    }

    #[test]
    fn sandbox_is_the_default_and_agents_can_override_the_mode() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.isolation.mode, IsolationMode::Sandbox);
        assert!(!config.isolation.sandbox.ssh_agent, "SSH agent forwarding is off by default");
        assert_eq!(config.isolation.sandbox.network, NetworkPreset::Balanced);
        let config: Config = toml::from_str(
            r#"
[isolation]
mode = "bare"
[isolation.sandbox]
memory = "8g"
cpus = 4
template = "registry.example.com/agent:1"
mounts = ["~/docs"]
network = "deny-all"
ssh_agent = true
mcp = ["notion"]
allow_hosts = { "~/repos/app" = ["nuget.example.com"] }

[[agents]]
name = "claude"
command = ["claude"]
kind = "claude"
isolation = "sandbox"

[[agents]]
name = "codex"
command = ["codex"]
"#,
        )
        .unwrap();
        let isolation = &config.isolation;
        assert_eq!(isolation.mode, IsolationMode::Bare);
        assert_eq!(isolation.mode_for(config.agent("claude").unwrap(), None), IsolationMode::Sandbox);
        assert_eq!(isolation.mode_for(config.agent("codex").unwrap(), None), IsolationMode::Bare);
        // A plan task's own isolation wins over the agent's and the mode.
        let claude = config.agent("claude").unwrap();
        assert_eq!(isolation.mode_for(claude, Some(IsolationMode::Bare)), IsolationMode::Bare);
        assert_eq!(
            isolation.mode_for(config.agent("codex").unwrap(), Some(IsolationMode::Sandbox)),
            IsolationMode::Sandbox
        );
        let mut required = isolation.clone();
        required.required = true;
        for task in [None, Some(IsolationMode::Bare), Some(IsolationMode::Sandbox)] {
            assert_eq!(required.mode_for(config.agent("codex").unwrap(), task), IsolationMode::Sandbox);
        }
        let sandbox = &isolation.sandbox;
        assert_eq!((sandbox.memory.as_deref(), sandbox.cpus), (Some("8g"), Some(4)));
        assert_eq!(sandbox.template.as_deref(), Some("registry.example.com/agent:1"));
        assert_eq!(sandbox.network, NetworkPreset::DenyAll);
        assert!(sandbox.ssh_agent);
        assert_eq!(sandbox.mcp, ["notion"]);
        let expand = |path: &str| PathBuf::from(path.replace("~", "/home/me"));
        assert_eq!(sandbox.hosts_for(Path::new("/home/me/repos/app"), expand), ["nuget.example.com"]);
        assert!(sandbox.hosts_for(Path::new("/home/me/repos/other"), expand).is_empty());
        let saved: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(saved.isolation, config.isolation);
        assert_eq!(saved.agent("claude").unwrap().isolation, Some(IsolationMode::Sandbox));
        for invalid in [
            "[isolation]\nmode = 'docker'",
            "[isolation.sandbox]\nnetwork = 'open'",
            "[[agents]]\nname = 'a'\nisolation = 'vm'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn documented_examples_load_as_described() {
        let docs = include_str!("../../../docs/isolation.md");
        let blocks: Vec<&str> = docs.split("```toml\n").skip(1).map(|b| b.split("```").next().unwrap()).collect();
        let config: Config = toml::from_str(blocks[0]).unwrap();
        assert_eq!(config.isolation.mode, IsolationMode::Sandbox);
        assert_eq!(config.isolation.sandbox.allow_hosts["~/repos/app"], ["nuget.example.com"]);
        assert_eq!(config.isolation.mode_for(config.agent("aider").unwrap(), None), IsolationMode::Bare);
        let (repo, warnings) = crate::repo_workflow::declarative(&toml::from_str(blocks[1]).unwrap());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(repo["isolation"]["mode"].as_str(), Some("sandbox"));
    }

    #[test]
    fn sandboxes_default_to_a_test_runs_share_of_the_cores_and_8g() {
        use crate::governor::ResourceConfig;
        let resources = ResourceConfig::default();
        let unset = SandboxConfig::default();
        // jobs default to cores - 2 and test slots to one per 6 cores, at least one.
        for (cores, cpus) in [(16, 7), (12, 5), (64, 6), (8, 6), (4, 2), (1, 2)] {
            assert_eq!(unset.cpus_or_default(&resources, cores), cpus, "{cores} cores");
        }
        let custom = ResourceConfig { jobs: Some(16), test_slots: Some(3), ..ResourceConfig::default() };
        assert_eq!(unset.cpus_or_default(&custom, 32), 5);
        let no_pool = ResourceConfig { jobs: Some(0), ..ResourceConfig::default() };
        assert_eq!(unset.cpus_or_default(&no_pool, 32), 2);
        assert_eq!(unset.memory_or_default(), "8g");
        let filled = unset.with_defaults(&resources, 16);
        assert_eq!((filled.cpus, filled.memory.as_deref()), (Some(7), Some("8g")));

        // Set values win, and 0 or "" leave the choice to sbx: create passes no flag.
        let set = SandboxConfig { cpus: Some(4), memory: Some("16g".into()), ..SandboxConfig::default() };
        let filled = set.with_defaults(&resources, 16);
        assert_eq!((filled.cpus, filled.memory.as_deref()), (Some(4), Some("16g")));
        let sbx_decides = SandboxConfig { cpus: Some(0), memory: Some(String::new()), ..SandboxConfig::default() }
            .with_defaults(&resources, 16);
        let spec = Spec {
            name: "imacs-s1".into(),
            agent: "claude",
            workspace: "/repos/app".into(),
            git: Vec::new(),
            read_only: Vec::new(),
            memory: sbx_decides.memory,
            cpus: sbx_decides.cpus,
            template: None,
        };
        let args = create_args(&spec);
        assert!(!args.iter().any(|a| a == "--cpus" || a == "--memory"), "{args:?}");
    }

    #[test]
    fn workspace_overrides_merge_key_by_key() {
        let mut base = IsolationConfig::default();
        base.sandbox.memory = Some("4g".into());
        base.sandbox.mounts = vec!["~/docs".into()];
        let table = toml::from_str("mode = 'bare'\n[sandbox]\ncpus = 2\nmounts = []").unwrap();
        let merged = base.with_overrides(&IsolationOverrides(table)).unwrap();
        assert_eq!(merged.mode, IsolationMode::Bare);
        assert_eq!(merged.sandbox.memory.as_deref(), Some("4g"));
        assert_eq!(merged.sandbox.cpus, Some(2));
        assert!(merged.sandbox.mounts.is_empty());
        assert!(base.with_overrides(&IsolationOverrides(toml::from_str("mode = 'open'").unwrap())).is_err());
    }

    #[test]
    fn agents_map_to_sbx_agents_by_kind_and_program() {
        assert_eq!(sbx_agent(&agent("claude", &["claude"], AgentKind::Claude, None)), Some("claude"));
        assert_eq!(sbx_agent(&agent("wrapped", &["/opt/bin/claude-wrapper"], AgentKind::Claude, None)), Some("claude"));
        assert_eq!(sbx_agent(&agent("codex", &["codex"], AgentKind::Generic, None)), Some("codex"));
        assert_eq!(
            sbx_agent(&agent("codex", &["/usr/local/bin/codex", "-m", "o3"], AgentKind::Generic, None)),
            Some("codex")
        );
        assert_eq!(sbx_agent(&agent("shell", &[], AgentKind::Shell, None)), Some("shell"));
        assert_eq!(sbx_agent(&agent("gemini", &["gemini"], AgentKind::Generic, None)), Some("gemini"));
        assert_eq!(sbx_agent(&agent("aider", &["aider"], AgentKind::Generic, None)), None);
        assert_eq!(sbx_agent(&agent("none", &[], AgentKind::Generic, None)), None);
    }

    #[test]
    fn sandboxes_are_named_after_their_session() {
        let name = sandbox_name("0d9c3a52-7d1e-4bd6-9a51-2f3c6a1b8e10");
        assert_eq!(name, "imacs-0d9c3a52-7d1e-4bd6-9a51-2f3c6a1b8e10");
        // sbx names: letters, numbers, hyphens and periods, starting with a letter or number.
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'));
    }

    #[test]
    fn create_mounts_the_workspace_git_folders_and_read_only_extras() {
        let mut spec = Spec {
            name: "imacs-s1".into(),
            agent: "codex",
            workspace: "/repos/app-feature".into(),
            git: vec!["/repos/app/.git/objects".into(), "/repos/app/.git/worktrees/app-feature".into()],
            read_only: vec!["/home/me/docs".into()],
            memory: None,
            cpus: None,
            template: None,
        };
        assert_eq!(
            create_args(&spec),
            [
                "create",
                "--name",
                "imacs-s1",
                "--no-progress",
                "codex",
                "/repos/app-feature",
                "/repos/app/.git/objects",
                "/repos/app/.git/worktrees/app-feature",
                "/home/me/docs:ro",
            ]
        );
        spec.memory = Some("8g".into());
        spec.cpus = Some(4);
        spec.template = Some("registry.example.com/agent:1".into());
        let args = create_args(&spec);
        assert_eq!(
            args[..11],
            [
                "create",
                "--name",
                "imacs-s1",
                "--no-progress",
                "--memory",
                "8g",
                "--cpus",
                "4",
                "--template",
                "registry.example.com/agent:1",
                "codex",
            ]
        );
    }

    #[test]
    fn run_mcp_and_policy_command_lines() {
        let args = ["--resume".to_string(), "abc".to_string()];
        assert_eq!(
            run_args("imacs-s1", "claude", &args),
            ["run", "--name", "imacs-s1", "claude", "--", "--resume", "abc"]
        );
        assert_eq!(run_args("imacs-s1", "shell", &[]), ["run", "--name", "imacs-s1", "shell", "--"]);
        assert_eq!(
            mcp_add_args("imacs-s1", Path::new("/Apps/IMACS, beta.app/imacs"), Path::new("/run/imacs.sock")),
            [
                "mcp",
                "add",
                "imacs-s1",
                "--command",
                "/usr/bin/env",
                "--args=IMACS_PANE=imacs-s1",
                "--args=IMACS_SOCKET=/run/imacs.sock",
                "--args=\"/Apps/IMACS, beta.app/imacs\"",
                "--args=mcp",
                "--args=--agent",
            ]
        );
        assert_eq!(mcp_load_args(" notion", "imacs-s1"), ["mcp", "load", "notion", "--sandbox", "imacs-s1"]);
        assert_eq!(
            allow_hosts_args("imacs-s1", &["a.example.com".into(), "b.example.com".into()]),
            ["policy", "allow", "network", "--sandbox", "imacs-s1", "a.example.com,b.example.com"]
        );
    }

    #[test]
    fn host_sandbox_and_permission_flags_stay_on_the_host() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        // The arguments IMACS plans give Codex and Claude tasks.
        for (agent, given, sandboxed) in [
            (
                "codex",
                args(&[
                    "-m",
                    "gpt-6.1-sol",
                    "-c",
                    "model_reasoning_effort=high",
                    "-s",
                    "workspace-write",
                    "-a",
                    "on-request",
                    "-c",
                    r#"sandbox_workspace_write.writable_roots=["/Users/maxfalk/.cargo"]"#,
                ]),
                args(&["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high"]),
            ),
            (
                "codex",
                args(&[
                    "-m",
                    "gpt-6.1-sol",
                    "-c",
                    "model_reasoning_effort=high",
                    "-s",
                    "workspace-write",
                    "-a",
                    "on-request",
                    "-c",
                    r#"sandbox_workspace_write.writable_roots=["/Users/maxfalk/.cargo","/Users/maxfalk/.nuget","/Users/maxfalk/.dotnet"]"#,
                ]),
                args(&["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=high"]),
            ),
            (
                "codex",
                args(&[
                    "-m",
                    "gpt-6.1-sol",
                    "-c",
                    "model_reasoning_effort=low",
                    "-s",
                    "workspace-write",
                    "-a",
                    "on-request",
                ]),
                args(&["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=low"]),
            ),
            ("claude", args(&["--model", "claude-opus-5-5"]), args(&["--model", "claude-opus-5-5"])),
            // Long forms, inline values and the other host-only flags.
            (
                "codex",
                args(&[
                    "--sandbox=read-only",
                    "--ask-for-approval",
                    "untrusted",
                    "--full-auto",
                    "--dangerously-bypass-approvals-and-sandbox",
                    "--add-dir",
                    "/Users/me/.cargo",
                    "--config",
                    "sandbox_mode=workspace-write",
                    "--config=approval_policy=never",
                    "-c",
                    "sandbox_workspace_write.network_access=true",
                    "-c",
                    "sandbox_modes_extra=1",
                    "--model=o4",
                    "-c",
                    "model_reasoning_effort=high",
                ]),
                args(&["-c", "sandbox_modes_extra=1", "--model=o4", "-c", "model_reasoning_effort=high"]),
            ),
            (
                "claude",
                args(&[
                    "--permission-mode",
                    "acceptEdits",
                    "--dangerously-skip-permissions",
                    "--add-dir",
                    "/Users/me/docs",
                    "/Users/me/notes",
                    "--model",
                    "claude-opus-5-5",
                    "--permission-mode=plan",
                    "--allowedTools",
                    "Bash(git:*)",
                ]),
                args(&["--model", "claude-opus-5-5", "--allowedTools", "Bash(git:*)"]),
            ),
            ("shell", args(&["-s", "x"]), args(&["-s", "x"])),
        ] {
            assert_eq!(sandboxed_args(agent, &given), sandboxed, "{agent} {given:?}");
        }
    }

    #[test]
    fn versions_and_diagnosis_parse() {
        assert_eq!(parse_version("sbx version v0.50.1 ddb0268d7124b9cf"), Some((0, 50, 1)));
        assert_eq!(parse_version("sbx version v1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("sbx version dev"), None);
        assert!(parse_version("sbx version v0.49.9 x").unwrap() < MIN_SBX_VERSION);
        assert!(parse_version("sbx version v0.50.1 x").unwrap() >= MIN_SBX_VERSION);
        assert!(parse_version("sbx version v0.51.0 x").unwrap() >= MIN_SBX_VERSION);
        assert_eq!(version_text(MIN_SBX_VERSION), "v0.50.1");
        let diagnosis: Diagnosis = serde_json::from_str(
            r#"{"version":"1.0","checks":[{"name":"Authentication","status":"fail","message":"not signed in","detail":"","hint":"run sbx login"},{"name":"Global network policy","status":"fail","message":"not initialized"}]}"#,
        )
        .unwrap();
        assert!(!diagnosis.signed_in());
        assert!(!diagnosis.policy_initialized());
        assert_eq!(diagnosis.check("authentication").unwrap().hint, "run sbx login");
        let diagnosis: Diagnosis = serde_json::from_str(
            r#"{"checks":[{"name":"Authentication","status":"pass"},{"name":"Global network policy","status":"pass"}]}"#,
        )
        .unwrap();
        assert!(diagnosis.signed_in() && diagnosis.policy_initialized());
        assert!(!Diagnosis::default().signed_in());
    }

    #[test]
    fn orphans_are_imacs_sandboxes_of_this_repository_without_their_worktree() {
        let listed = parse_list(
            r#"[
  {"name":"imacs-live","agent":"claude","status":"running","workspaces":["/repos/app-a","/repos/app/.git/objects"]},
  {"name":"imacs-gone","agent":"codex","status":"stopped","workspaces":["/repos/app-b","/repos/app/.git/objects"]},
  {"name":"imacs-main","agent":"shell","status":"stopped","workspaces":["/repos/app","/repos/app/.git:ro"]},
  {"name":"imacs-other","agent":"claude","status":"stopped","workspaces":["/repos/other-x","/repos/other/.git/objects"]},
  {"name":"poc-1","agent":"claude","status":"stopped","workspaces":["/repos/app-c","/repos/app/.git/objects"]}
]"#,
        )
        .unwrap();
        assert!(parse_list(" ").unwrap().is_empty());
        let worktrees = [PathBuf::from("/repos/app"), PathBuf::from("/repos/app-a"), PathBuf::from("/repos/app-b")];
        let exists = |path: &Path| path != Path::new("/repos/app-b");
        let (attached, orphans) = partition(&listed, &worktrees, Path::new("/repos/app/.git"), exists);
        let names = |list: Vec<&Listed>| list.into_iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(attached), ["imacs-live", "imacs-main"]);
        assert_eq!(names(orphans), ["imacs-gone"], "only IMACS's own sandboxes of this repository");
        assert_eq!(listed[2].workspace(), Some(Path::new("/repos/app")));
    }
}
