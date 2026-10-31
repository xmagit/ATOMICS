//! How much heavy work the machine runs at once. Cargo compilers share a jobserver;
//! other toolchains get launch budgets, and whole test runs and QA instances wait
//! for a slot. The app owns the pool and the waiting; this is the policy and queue.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotClass {
    Test,
    Qa,
}

impl SlotClass {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "test" => Some(Self::Test),
            "qa" => Some(Self::Qa),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Qa => "qa",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceConfig {
    /// Tokens in the shared Cargo jobserver; unset = cores minus 2, 0 = no pool.
    pub jobs: Option<usize>,
    /// Whole test runs at once (`atomics queue --class test`); unset = one per 6 cores.
    pub test_slots: Option<usize>,
    /// QA instances at once (`atomics queue --class qa`); unset = 1.
    pub qa_slots: Option<usize>,
    /// Niceness agent terminals start at, so ATOMICS and the desktop stay responsive.
    pub agent_nice: i32,
    /// Per-process limits for .NET worktrees (there is no MSBuild jobserver).
    pub dotnet: DotnetConfig,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self { jobs: None, test_slots: None, qa_slots: None, agent_nice: 10, dotnet: DotnetConfig::default() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DotnetConfig {
    pub enabled: bool,
    /// CPUs reported to each .NET process; unset = jobs / test slots / 2, at least one.
    pub processor_count: Option<usize>,
}

impl Default for DotnetConfig {
    fn default() -> Self {
        Self { enabled: true, processor_count: None }
    }
}

/// Toolchain-specific launch policy. New toolchains can add detection and env here
/// without adding project-specific rules to agent startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    Dotnet,
}

impl Toolchain {
    /// Look below the worktree root, including untracked projects. Do not follow
    /// directory links out of the worktree or descend into Git's metadata.
    #[cfg(feature = "native")]
    pub fn detect(root: &std::path::Path) -> Vec<Self> {
        let mut dirs = VecDeque::from([root.to_path_buf()]);
        while let Some(dir) = dirs.pop_front() {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                let name = entry.file_name();
                if kind.is_dir() {
                    if name != ".git" {
                        dirs.push_back(entry.path());
                    }
                } else if kind.is_file()
                    && (name == "global.json"
                        || matches!(entry.path().extension().and_then(|s| s.to_str()), Some("sln" | "slnx" | "csproj")))
                {
                    return vec![Self::Dotnet];
                }
            }
        }
        Vec::new()
    }
}

impl ResourceConfig {
    pub fn env(&self, toolchain: Toolchain, cores: usize) -> Vec<(String, String)> {
        match toolchain {
            Toolchain::Dotnet if self.dotnet.enabled => {
                let processors = self
                    .dotnet
                    .processor_count
                    // Each runnable project has an MSBuild node and a compiler process.
                    .unwrap_or_else(|| self.jobs(cores) / self.slots(SlotClass::Test, cores) / 2)
                    .max(1);
                // -m without a value uses Environment.ProcessorCount. MSBuildMaxCpuCount
                // as an environment property does not set that command-line switch.
                vec![
                    ("DOTNET_PROCESSOR_COUNT".into(), processors.to_string()),
                    ("MSBUILDDISABLENODEREUSE".into(), "1".into()),
                    ("DOTNET_CLI_USE_MSBUILD_SERVER".into(), "0".into()),
                    ("UseSharedCompilation".into(), "false".into()),
                ]
            }
            Toolchain::Dotnet => Vec::new(),
        }
    }

    /// 0 means no shared pool.
    pub fn jobs(&self, cores: usize) -> usize {
        self.jobs.unwrap_or(cores.saturating_sub(2).max(1))
    }

    pub fn slots(&self, class: SlotClass, cores: usize) -> usize {
        match class {
            SlotClass::Test => self.test_slots.unwrap_or(cores / 6),
            SlotClass::Qa => self.qa_slots.unwrap_or(1),
        }
        .max(1)
    }
}

/// One request for a slot: what it runs and the processes it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ticket {
    pub id: u64,
    pub class: SlotClass,
    pub label: String,
    pub ancestors: Vec<u32>,
}

/// First come, first served within each class.
#[derive(Debug, Default)]
pub struct SlotQueue {
    running: Vec<Ticket>,
    waiting: VecDeque<Ticket>,
    next_id: u64,
}

impl SlotQueue {
    pub fn enqueue(&mut self, class: SlotClass, label: String, ancestors: Vec<u32>) -> u64 {
        self.next_id += 1;
        self.waiting.push_back(Ticket { id: self.next_id, class, label, ancestors });
        self.next_id
    }

    /// Moves the oldest waiting tickets of each class into its free slots.
    pub fn grant(&mut self, limit: impl Fn(SlotClass) -> usize) {
        let mut index = 0;
        while index < self.waiting.len() {
            let class = self.waiting[index].class;
            if self.running.iter().filter(|t| t.class == class).count() < limit(class) {
                let ticket = self.waiting.remove(index).expect("index in range");
                self.running.push(ticket);
            } else {
                index += 1;
            }
        }
    }

    pub fn is_running(&self, id: u64) -> bool {
        self.running.iter().any(|t| t.id == id)
    }

    /// 1-based place among the waiting tickets of its class.
    pub fn position(&self, id: u64) -> Option<usize> {
        let class = self.waiting.iter().find(|t| t.id == id)?.class;
        self.waiting.iter().filter(|t| t.class == class).position(|t| t.id == id).map(|p| p + 1)
    }

    pub fn release(&mut self, id: u64) {
        self.running.retain(|t| t.id != id);
        self.waiting.retain(|t| t.id != id);
    }

    pub fn running(&self) -> &[Ticket] {
        &self.running
    }

    pub fn waiting(&self) -> impl Iterator<Item = &Ticket> {
        self.waiting.iter()
    }
}

/// The command inside `atomics queue [--class <c>] -- <command>`, so policies judge what
/// actually runs. Only a bare `atomics` counts: ATOMICS puts its own binary first on agents' PATH,
/// while a path could name any program called atomics.
pub fn queued_command(words: &[String]) -> Option<&[String]> {
    if words.first().map(String::as_str) != Some("atomics") || words.get(1).map(String::as_str) != Some("queue") {
        return None;
    }
    let separator = words.iter().position(|w| w == "--")?;
    let valid = match &words[2..separator] {
        [] => true,
        [option] => option.strip_prefix("--class=").and_then(SlotClass::parse).is_some(),
        [flag, class] => flag == "--class" && SlotClass::parse(class).is_some(),
        _ => false,
    };
    (valid && separator + 1 < words.len()).then(|| &words[separator + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(test: usize, qa: usize) -> impl Fn(SlotClass) -> usize {
        move |class| match class {
            SlotClass::Test => test,
            SlotClass::Qa => qa,
        }
    }

    #[test]
    fn defaults_follow_the_core_count() {
        let config = ResourceConfig::default();
        assert_eq!(config.jobs(18), 16);
        assert_eq!(config.jobs(1), 1);
        assert_eq!(config.slots(SlotClass::Test, 18), 3);
        assert_eq!(config.slots(SlotClass::Test, 4), 1);
        assert_eq!(config.slots(SlotClass::Qa, 18), 1);
        let fixed = ResourceConfig {
            jobs: Some(6),
            test_slots: Some(0),
            qa_slots: Some(2),
            agent_nice: 0,
            ..ResourceConfig::default()
        };
        assert_eq!((fixed.jobs(18), fixed.slots(SlotClass::Test, 18), fixed.slots(SlotClass::Qa, 18)), (6, 1, 2));
        assert_eq!(ResourceConfig { jobs: Some(0), ..ResourceConfig::default() }.jobs(18), 0, "0 turns the pool off");
    }

    #[test]
    fn dotnet_env_follows_resources_and_can_be_disabled() {
        let config = ResourceConfig::default();
        let env: std::collections::HashMap<_, _> = config.env(Toolchain::Dotnet, 18).into_iter().collect();
        assert_eq!(env["DOTNET_PROCESSOR_COUNT"], "2");
        assert_eq!(env["MSBUILDDISABLENODEREUSE"], "1");
        assert_eq!(env["DOTNET_CLI_USE_MSBUILD_SERVER"], "0");
        assert_eq!(env["UseSharedCompilation"], "false");
        assert_eq!(env.len(), 4);
        assert_eq!(config.env(Toolchain::Dotnet, 1)[0].1, "1");
        let mut config = ResourceConfig { jobs: Some(8), test_slots: Some(2), ..ResourceConfig::default() };
        assert_eq!(config.env(Toolchain::Dotnet, 18)[0].1, "2");
        config.jobs = Some(0);
        assert_eq!(config.env(Toolchain::Dotnet, 18)[0].1, "1");
        config.dotnet.processor_count = Some(2);
        assert_eq!(config.env(Toolchain::Dotnet, 18)[0].1, "2");
        config.dotnet.processor_count = Some(0);
        assert_eq!(config.env(Toolchain::Dotnet, 18)[0].1, "1");
        config.dotnet.enabled = false;
        assert!(config.env(Toolchain::Dotnet, 18).is_empty());
    }

    #[cfg(feature = "native")]
    #[test]
    fn dotnet_detection_finds_root_and_nested_markers() {
        for marker in ["Example.sln", "Example.slnx", "Example.csproj", "global.json"] {
            for folder in ["", "src/deep", "bin/tools"] {
                let dir = crate::test_support::tempdir().unwrap();
                let project = dir.path().join(folder);
                std::fs::create_dir_all(&project).unwrap();
                std::fs::write(project.join(marker), "").unwrap();
                assert_eq!(Toolchain::detect(dir.path()), [Toolchain::Dotnet], "{folder}/{marker}");
            }
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn non_dotnet_worktrees_and_git_metadata_have_no_toolchain_env() {
        let dir = crate::test_support::tempdir().unwrap();
        for name in ["Cargo.toml", "package.json", "Something.cs", "global.json.backup"] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/Dependency.csproj"), "").unwrap();
        assert!(Toolchain::detect(dir.path()).is_empty());
        assert!(Toolchain::detect(&dir.path().join("missing")).is_empty());
        #[cfg(unix)]
        {
            let external = crate::test_support::tempdir().unwrap();
            std::fs::write(external.path().join("Outside.sln"), "").unwrap();
            std::os::unix::fs::symlink(external.path(), dir.path().join("linked")).unwrap();
            std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
            assert!(Toolchain::detect(dir.path()).is_empty());
        }
    }

    #[test]
    fn dotnet_config_defaults_and_overrides_round_trip() {
        let old: ResourceConfig = toml::from_str("jobs = 8").unwrap();
        assert!(old.dotnet.enabled);
        assert_eq!(old.dotnet.processor_count, None);
        let config: ResourceConfig = toml::from_str("[dotnet]\nenabled = false\nprocessor_count = 3\n").unwrap();
        let round_trip: ResourceConfig = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert!(!round_trip.dotnet.enabled);
        assert_eq!(round_trip.dotnet.processor_count, Some(3));
    }

    #[test]
    fn slots_are_granted_oldest_first_per_class() {
        let mut queue = SlotQueue::default();
        let a = queue.enqueue(SlotClass::Test, "a".into(), vec![]);
        let b = queue.enqueue(SlotClass::Test, "b".into(), vec![]);
        let q = queue.enqueue(SlotClass::Qa, "q".into(), vec![]);
        let c = queue.enqueue(SlotClass::Test, "c".into(), vec![]);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(a) && queue.is_running(q));
        assert!(!queue.is_running(b) && !queue.is_running(c));
        assert_eq!((queue.position(b), queue.position(c), queue.position(a)), (Some(1), Some(2), None));
        queue.release(a);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(b) && !queue.is_running(c));
        assert_eq!(queue.position(c), Some(1));
    }

    #[test]
    fn a_waiter_that_leaves_frees_its_place() {
        let mut queue = SlotQueue::default();
        let a = queue.enqueue(SlotClass::Test, "a".into(), vec![]);
        let b = queue.enqueue(SlotClass::Test, "b".into(), vec![]);
        let c = queue.enqueue(SlotClass::Test, "c".into(), vec![]);
        queue.grant(limits(1, 1));
        queue.release(b);
        assert_eq!(queue.position(c), Some(1));
        queue.release(a);
        queue.grant(limits(1, 1));
        assert!(queue.is_running(c));
        assert_eq!(queue.waiting().count(), 0);
    }

    #[test]
    fn queued_command_is_the_part_after_the_separator() {
        let words = |text: &str| text.split(' ').map(str::to_string).collect::<Vec<_>>();
        let inner = |text: &str| queued_command(&words(text)).map(|w| w.join(" "));
        assert_eq!(inner("atomics queue -- cargo test"), Some("cargo test".into()));
        assert_eq!(inner("atomics queue --class test -- cargo test -p x"), Some("cargo test -p x".into()));
        assert_eq!(inner("/Apps/ATOMICS.app/Contents/MacOS/atomics queue -- cargo test"), None);
        assert_eq!(inner("./atomics queue -- cargo test"), None);
        assert_eq!(inner("atomics queue --class=qa -- python3 qa.py"), Some("python3 qa.py".into()));
        assert_eq!(inner("atomics queue --class nope -- cargo test"), None);
        assert_eq!(inner("atomics queue --class=nope -- cargo test"), None);
        assert_eq!(inner("atomics queue --"), None);
        assert_eq!(inner("atomics ctl -- cargo test"), None);
        assert_eq!(inner("notatomics queue -- cargo test"), None);
        assert_eq!(inner("cargo test"), None);
    }
}
