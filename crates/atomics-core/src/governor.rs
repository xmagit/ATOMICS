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
    /// Available-memory floor in GiB; unset = max(4 GiB, 10% of physical RAM).
    pub memory_floor_gib: Option<u64>,
    /// Additional memory reserved per admitted job, including already-held slots.
    pub memory_reservation_gib: u64,
    /// QA lease length in seconds. Unset or zero disables expiry.
    pub qa_lease_secs: Option<u64>,
    pub disk_warn_gib: u64,
    pub disk_min_gib: u64,
    /// Per-process limits for .NET worktrees (there is no MSBuild jobserver).
    pub dotnet: DotnetConfig,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            jobs: None,
            test_slots: None,
            qa_slots: None,
            agent_nice: 10,
            memory_floor_gib: None,
            memory_reservation_gib: 2,
            qa_lease_secs: None,
            disk_warn_gib: 30,
            disk_min_gib: 10,
            dotnet: DotnetConfig::default(),
        }
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

/// The Rust toolchain a repository pins, by its `rust-toolchain.toml` or `rust-toolchain`, as rustup reads them.
#[cfg(feature = "native")]
pub fn pinned_rust_toolchain(root: &std::path::Path) -> Option<String> {
    let channel = |text: &str| -> Option<String> {
        let parsed = text.parse::<toml::Table>().ok()?;
        Some(parsed.get("toolchain")?.get("channel")?.as_str()?.to_string())
    };
    if let Ok(text) = std::fs::read_to_string(root.join("rust-toolchain.toml")) {
        return channel(&text);
    }
    let text = std::fs::read_to_string(root.join("rust-toolchain")).ok()?;
    // The old form is just the channel's name.
    channel(&text).or_else(|| {
        let name = text.trim();
        (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string())
    })
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

    pub fn memory_floor(&self, total: u64) -> u64 {
        const GIB: u64 = 1024 * 1024 * 1024;
        self.memory_floor_gib.map(|n| n.saturating_mul(GIB)).unwrap_or((4 * GIB).max(total / 10))
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
    pub owner: String,
    pub enqueued_at: u64,
    pub started_at: Option<u64>,
    pub lease_secs: Option<u64>,
    pub lease_deadline: Option<u64>,
    pub pinned: bool,
    pub expired: bool,
}

impl Ticket {
    pub fn holds_slot(&self) -> bool {
        self.started_at.is_some() && !self.pinned && !self.expired
    }
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
        self.enqueue_owned(class, label, ancestors, "host".into(), None, 0)
    }

    pub fn enqueue_owned(
        &mut self,
        class: SlotClass,
        label: String,
        ancestors: Vec<u32>,
        owner: String,
        lease_secs: Option<u64>,
        now: u64,
    ) -> u64 {
        self.next_id += 1;
        self.waiting.push_back(Ticket {
            id: self.next_id,
            class,
            label,
            ancestors,
            owner,
            enqueued_at: now,
            started_at: None,
            lease_secs: lease_secs.filter(|s| *s > 0 && class == SlotClass::Qa),
            lease_deadline: None,
            pinned: false,
            expired: false,
        });
        self.next_id
    }

    #[cfg(test)]
    fn grant(&mut self, limit: impl Fn(SlotClass) -> usize) {
        self.grant_at(
            limit,
            &ResourceConfig::default(),
            Some(crate::machine::Memory { total: u64::MAX, available: u64::MAX }),
            0,
        );
    }

    /// The caller injects one memory reading and a monotonic clock. Each live slot reserves
    /// additional headroom, so a single reading cannot admit a whole batch into the same RAM.
    pub fn grant_at(
        &mut self,
        limit: impl Fn(SlotClass) -> usize,
        config: &ResourceConfig,
        memory: Option<crate::machine::Memory>,
        now: u64,
    ) {
        for ticket in &mut self.running {
            if ticket.lease_deadline.is_some_and(|deadline| now >= deadline) && !ticket.pinned {
                ticket.expired = true;
            }
        }
        let Some(memory) = memory else { return };
        let reservation = config.memory_reservation_gib.saturating_mul(1024 * 1024 * 1024);
        let floor = config.memory_floor(memory.total);
        let held = self.running.iter().filter(|t| t.holds_slot()).count() as u64;
        let mut remaining = memory.available.saturating_sub(held.saturating_mul(reservation));
        let mut index = 0;
        while index < self.waiting.len() {
            let class = self.waiting[index].class;
            if self.running.iter().filter(|t| t.class == class && t.holds_slot()).count() < limit(class)
                && remaining >= floor.saturating_add(reservation)
            {
                let mut ticket = self.waiting.remove(index).expect("index in range");
                ticket.started_at = Some(now);
                ticket.lease_deadline = ticket.lease_secs.map(|seconds| now.saturating_add(seconds));
                self.running.push(ticket);
                remaining = remaining.saturating_sub(reservation);
            } else {
                index += 1;
            }
        }
    }

    pub fn cancel(&mut self, id: u64, owner: &str, chief_override: bool) -> Result<(), &'static str> {
        let ticket = self.running.iter().chain(self.waiting.iter()).find(|t| t.id == id).ok_or("unknown queue job")?;
        if ticket.owner != owner && !chief_override {
            return Err("only the job's owner or the chief can cancel it");
        }
        self.release(id);
        Ok(())
    }

    pub fn renew(&mut self, id: u64, owner: &str, now: u64) -> Result<(), &'static str> {
        let ticket = self.running.iter_mut().find(|t| t.id == id).ok_or("unknown running job")?;
        if ticket.owner != owner {
            return Err("only the job's owner can renew it");
        }
        if ticket.expired || ticket.lease_deadline.is_some_and(|end| now >= end) {
            return Err("QA lease expired; queue a new job to acquire a slot");
        }
        let seconds = ticket.lease_secs.ok_or("this job has no lease")?;
        ticket.lease_deadline = Some(now.saturating_add(seconds));
        Ok(())
    }

    pub fn pin(&mut self, id: u64, owner: &str, chief_override: bool) -> Result<(), &'static str> {
        let ticket = self.running.iter_mut().find(|t| t.id == id).ok_or("unknown running job")?;
        if ticket.owner != owner && !chief_override {
            return Err("only the job's owner or the chief can pin it");
        }
        if ticket.class != SlotClass::Qa {
            return Err("only QA previews can be pinned");
        }
        ticket.pinned = true;
        ticket.lease_deadline = None;
        Ok(())
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

/// Advisory detection in command hooks, including common package-manager prefixes.
/// Quoted shell bodies stay intact; a queued shell covers all commands in that body.
pub fn direct_heavy_command(command: &str) -> bool {
    fn heavy(words: &[String]) -> bool {
        let mut words = words;
        while let Some(first) = words.first() {
            let name = first.rsplit('/').next().unwrap_or(first);
            if first.contains('=') || matches!(name, "env" | "exec" | "command" | "npx" | "pnpm" | "yarn" | "bunx") {
                words = &words[1..];
                while words.first().is_some_and(|w| w.starts_with('-') || matches!(w.as_str(), "exec" | "dlx")) {
                    words = &words[1..];
                }
            } else if name == "rustup" && words.get(1).is_some_and(|w| w == "run") && words.len() > 3 {
                words = &words[3..];
            } else {
                break;
            }
        }
        let Some(first) = words.first() else { return false };
        let name = first.rsplit('/').next().unwrap_or(first);
        if name == "atomics" && words.get(1).is_some_and(|w| w == "queue") && words.iter().any(|w| w == "--") {
            return false;
        }
        if matches!(name, "sh" | "bash" | "zsh") && words.get(1).is_some_and(|w| w == "-c" || w == "-lc") {
            return words.get(2).is_some_and(|script| direct_heavy_command(script));
        }
        if matches!(name, "tsc" | "vitest" | "playwright") {
            return true;
        }
        if name == "cargo" || name == "dotnet" {
            return words
                .iter()
                .skip(1)
                .find(|w| !w.starts_with('+') && !w.starts_with('-'))
                .is_some_and(|w| w == "test");
        }
        false
    }
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0;
    for (index, c) in command.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if matches!(c, ';' | '&' | '|' | '\n') {
            if shlex::split(&command[start..index]).is_some_and(|words| heavy(&words)) {
                return true;
            }
            start = index + c.len_utf8();
        }
    }
    shlex::split(&command[start..]).is_some_and(|words| heavy(&words))
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
    fn direct_heavy_checks_warn_in_command_hooks() {
        for command in [
            "tsc --noEmit",
            "npx vitest run",
            "pnpm exec playwright test",
            "cargo +stable test -p core",
            "dotnet test app.sln",
            "cd web && npx tsc --noEmit",
            "atomics queue -- cargo test; dotnet test",
        ] {
            assert!(direct_heavy_command(command), "{command}");
        }
        for command in [
            "echo tsc",
            "cargo check",
            "dotnet build",
            "atomics queue --class test -- cargo test",
            "atomics queue --class test -- sh -c 'tsc; vitest run'",
            "env FOO=1 atomics queue -- tsc --noEmit",
        ] {
            assert!(!direct_heavy_command(command), "{command}");
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn memory_and_lease_configuration_defaults_and_overrides_round_trip() {
        let default: ResourceConfig = toml::from_str("").unwrap();
        assert_eq!(default.memory_floor_gib, None);
        assert_eq!(default.memory_reservation_gib, 2);
        assert_eq!(default.qa_lease_secs, None);
        let configured: ResourceConfig =
            toml::from_str("memory_floor_gib = 1\nmemory_reservation_gib = 3\nqa_lease_secs = 60").unwrap();
        let configured: ResourceConfig = toml::from_str(&toml::to_string(&configured).unwrap()).unwrap();
        assert_eq!(configured.memory_floor(64 * GIB), GIB);
        assert_eq!(configured.memory_reservation_gib, 3);
        assert_eq!(configured.qa_lease_secs, Some(60));
        let mut queue = SlotQueue::default();
        let id = queue.enqueue_owned(SlotClass::Qa, "preview".into(), vec![], "own".into(), default.qa_lease_secs, 0);
        let memory = Some(crate::machine::Memory { total: 64 * GIB, available: 32 * GIB });
        queue.grant_at(limits(1, 1), &default, memory.clone(), 0);
        queue.grant_at(limits(1, 1), &default, memory, 100_000);
        assert!(queue.running().iter().find(|t| t.id == id).unwrap().holds_slot());
    }

    #[test]
    fn memory_admission_reserves_each_grant_and_waits_for_recovery() {
        let config = ResourceConfig::default();
        let memory = |available| Some(crate::machine::Memory { total: 48 * GIB, available });
        let mut queue = SlotQueue::default();
        let a = queue.enqueue(SlotClass::Test, "a".into(), vec![]);
        let b = queue.enqueue(SlotClass::Test, "b".into(), vec![]);
        queue.grant_at(limits(8, 8), &config, memory(6 * GIB), 0);
        assert!(!queue.is_running(a), "keep the floor plus the next reservation free");
        queue.grant_at(limits(8, 8), &config, memory(7 * GIB), 1);
        assert!(queue.is_running(a));
        assert!(!queue.is_running(b), "a batch cannot spend the same free memory twice");
        queue.grant_at(limits(8, 8), &config, memory(9 * GIB), 2);
        assert!(queue.is_running(b));
        assert_eq!(config.memory_floor(64 * GIB), (64 * GIB) / 10);
        assert_eq!(config.memory_floor(16 * GIB), 4 * GIB);
        queue.release(a);
        queue.release(b);
        let c = queue.enqueue(SlotClass::Test, "c".into(), vec![]);
        queue.grant_at(limits(8, 8), &config, None, 3);
        assert!(!queue.is_running(c), "an unknown reading must not admit heavy work");
    }

    #[test]
    fn cancel_requires_the_owner_or_an_explicit_chief_override() {
        let mut queue = SlotQueue::default();
        let id = queue.enqueue_owned(SlotClass::Test, "check".into(), vec![], "alice".into(), None, 10);
        assert!(queue.cancel(id, "bob", false).is_err());
        assert_eq!(queue.position(id), Some(1));
        assert!(queue.cancel(id, "alice", false).is_ok());
        let id = queue.enqueue_owned(SlotClass::Test, "check".into(), vec![], "alice".into(), None, 10);
        assert!(queue.cancel(id, "chief", true).is_ok());
    }

    #[test]
    fn qa_lease_renewal_expiry_and_pinning_use_the_injected_clock() {
        let mut queue = SlotQueue::default();
        let a = queue.enqueue_owned(SlotClass::Qa, "preview".into(), vec![], "alice".into(), Some(10), 0);
        let b = queue.enqueue(SlotClass::Qa, "next".into(), vec![]);
        let config = ResourceConfig::default();
        let memory = Some(crate::machine::Memory { total: 64 * GIB, available: 32 * GIB });
        queue.grant_at(limits(1, 1), &config, memory.clone(), 20);
        queue.renew(a, "alice", 29).unwrap();
        queue.grant_at(limits(1, 1), &config, memory.clone(), 31);
        assert!(!queue.is_running(b));
        queue.grant_at(limits(1, 1), &config, memory.clone(), 39);
        assert!(queue.is_running(b));
        assert!(queue.running().iter().find(|t| t.id == a).unwrap().expired);
        assert!(queue.renew(a, "alice", 39).is_err());
        assert!(queue.pin(b, "bob", false).is_err());
        queue.pin(b, "chief", true).unwrap();
        let c = queue.enqueue(SlotClass::Qa, "third".into(), vec![]);
        queue.grant_at(limits(1, 1), &config, memory, 40);
        assert!(queue.is_running(c), "a pinned preview holds no slot");
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
