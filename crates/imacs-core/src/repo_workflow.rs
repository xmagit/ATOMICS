//! Trusted project instructions, always read from the main checkout.
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

/// Maximum characters added to the chief prompt for one project, including file names.
pub const CHIEF_LIMIT: usize = 12_000;

/// Maximum bytes read from any repository workflow file. Files reaching this limit are refused.
#[cfg(feature = "native")]
const FILE_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct RepoWorkflow {
    pub instructions: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub fingerprint: u64,
    pub config: crate::selfconfig::Layer,
}

impl Default for RepoWorkflow {
    fn default() -> Self {
        Self {
            instructions: Vec::new(),
            warnings: Vec::new(),
            fingerprint: 0,
            config: crate::selfconfig::Layer::new(crate::selfconfig::Source::Repo, ".imacs/config.toml", None),
        }
    }
}

impl RepoWorkflow {
    pub fn config_text(&self) -> String {
        toml::to_string(&self.config.table).unwrap_or_default()
    }
    /// Names every included file and bounds the entire per-project section.
    pub fn chief_section(&self, project: &str) -> String {
        if self.instructions.is_empty() && self.config.table.is_empty() {
            return String::new();
        }
        let mut text = format!("\n\nProject instructions from the main checkout of {project}:\n");
        for (file, content) in &self.instructions {
            text.push_str(&format!("\nFrom {file}:\n{content}\n"));
        }
        if !self.config.table.is_empty() {
            text.push_str(&format!(
                "\nFrom {} (declarative settings; commands are agent hints):\n{}",
                self.config.path,
                self.config_text()
            ));
        }
        bounded(&text, CHIEF_LIMIT)
    }

    pub fn agent_section(&self) -> String {
        let mut text = String::new();
        for (file, content) in &self.instructions {
            if file.rsplit(['/', '\\']).next() == Some("IMACS.md") {
                text.push_str(&format!("\n\nProject instructions from the main checkout, {file}:\n{content}\n"));
            } else {
                text.push_str(&format!("\nAdditional project workflow topic: {file}\n"));
            }
        }
        if !self.config.table.is_empty() {
            text.push_str(&format!("\n\nDeclarative repository workflow from {} (test/QA commands are hints for agents; IMACS never runs them):\n```toml\n{}\n```\n", self.config.path, self.config_text()));
        }
        text
    }

    /// Apply only sanitized declarative settings. A repository workflow replaces the
    /// workspace workflow as a whole, including defaults for omitted keys.
    pub fn apply(&self, config: &crate::config::Config) -> crate::config::Config {
        let mut config = config.clone();
        if let Some(value) = self.config.table.get("base_ref").and_then(toml::Value::as_str) {
            config.base_ref = value.into();
        }
        if let Some(value) =
            self.config.table.get("tickets").and_then(|table| table.get("branch")).and_then(toml::Value::as_str)
        {
            config.tickets.branch = Some(value.into());
        }
        if let Some(value) =
            self.config.table.get("worktrees").and_then(|table| table.get("warm")).and_then(toml::Value::as_array)
        {
            config.worktrees.warm = value.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
        }
        if let Some(workflow) = self.config.table.get("workflow") {
            config.workflow = workflow.clone().try_into().unwrap_or_default();
        }
        // A repository may require sandbox mode and select its image; host access settings remain user-owned.
        if self.config.table.get("isolation").and_then(|table| table.get("mode")).and_then(toml::Value::as_str)
            == Some("sandbox")
        {
            config.require_sandbox();
        }
        if let Some(template) = self
            .config
            .table
            .get("isolation")
            .and_then(|table| table.get("sandbox"))
            .and_then(|table| table.get("template"))
            .and_then(toml::Value::as_str)
        {
            config.isolation.sandbox.template = Some(template.into());
        }
        config
    }

    #[cfg(feature = "native")]
    pub fn read(root: &Path) -> Self {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut workflow = Self::default();
        let mut hash = DefaultHasher::new();
        let canonical_root = match root.canonicalize() {
            Ok(root) => root,
            Err(error) => {
                workflow.warnings.push(format!("{}: cannot resolve main checkout: {error}", root.display()));
                workflow.warnings.hash(&mut hash);
                workflow.fingerprint = hash.finish();
                return workflow;
            }
        };
        let mut files = vec![root.join("IMACS.md")];
        let topics_dir = root.join(".imacs");
        let topics = canonical_repo_path(&canonical_root, &topics_dir).and_then(|path| {
            path.map(|path| {
                if !path.is_dir() {
                    return Err("not a directory".to_string());
                }
                std::fs::read_dir(path)
                    .and_then(|entries| {
                        entries.map(|entry| entry.map(|entry| topics_dir.join(entry.file_name()))).collect()
                    })
                    .map_err(|error| error.to_string())
            })
            .transpose()
        });
        let topics_allowed = topics.is_ok();
        let mut topics: Vec<PathBuf> = match topics {
            Ok(topics) => topics.unwrap_or_default(),
            Err(error) => {
                workflow.warnings.push(format!("{}: {error}", topics_dir.display()));
                Vec::new()
            }
        };
        topics.retain(|path| path.extension().is_some_and(|ext| ext == "md"));
        topics.sort();
        files.extend(topics);
        for file in files {
            file.hash(&mut hash);
            match read_repo_file(&canonical_root, &file) {
                Ok(Some(text)) => {
                    text.hash(&mut hash);
                    workflow.instructions.push((file.display().to_string(), text));
                }
                Ok(None) => {}
                Err(error) => workflow.warnings.push(format!("{}: {error}", file.display())),
            }
        }
        let config_file = topics_dir.join("config.toml");
        let config_path = crate::paths::display(&config_file);
        let source = crate::selfconfig::Source::Repo;
        let mut config = match if topics_allowed { read_repo_file(&canonical_root, &config_file) } else { Ok(None) } {
            Ok(text) => crate::selfconfig::Layer::new(source, config_path, text),
            Err(error) => crate::selfconfig::Layer::unreadable(source, config_path, error),
        };
        config.text.hash(&mut hash);
        if let Some(error) = &config.error {
            workflow.warnings.push(format!("{}: {error}; repository settings ignored", config.path));
        }
        let (table, warnings) = declarative(&config.table);
        workflow.warnings.extend(warnings.into_iter().map(|warning| format!("{}: {warning}", config.path)));
        config.table = table;
        workflow.config = config;
        workflow.warnings.hash(&mut hash);
        workflow.fingerprint = hash.finish();
        workflow
    }
}

/// Resolve links before inspecting or opening files, or walking the topics directory.
#[cfg(feature = "native")]
fn canonical_repo_path(root: &Path, path: &Path) -> Result<Option<PathBuf>, String> {
    let canonical = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(path).is_err() => {
            return Ok(None);
        }
        Err(error) => return Err(format!("cannot resolve repository workflow path: {error}")),
    };
    if !canonical.starts_with(root) {
        return Err("resolves outside the main checkout; repository workflow refused".into());
    }
    Ok(Some(canonical))
}

#[cfg(feature = "native")]
fn read_repo_file(root: &Path, path: &Path) -> Result<Option<String>, String> {
    use std::io::Read;

    let Some(canonical) = canonical_repo_path(root, path)? else { return Ok(None) };
    let read = || -> std::io::Result<String> {
        if !std::fs::metadata(&canonical)?.is_file() {
            return Err(std::io::Error::other("not a regular file; repository workflow refused"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        // A file replaced by a FIFO or symlink between inspection and open must not block or follow the link.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        }
        let file = options.open(canonical)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other("not a regular file; repository workflow refused"));
        }
        let mut bytes = Vec::new();
        file.take(FILE_LIMIT).read_to_end(&mut bytes)?;
        if bytes.len() as u64 == FILE_LIMIT {
            return Err(std::io::Error::other(format!(
                "reaches the {FILE_LIMIT}-byte read limit; repository workflow refused"
            )));
        }
        String::from_utf8(bytes).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    };
    read().map(Some).map_err(|error| error.to_string())
}

/// Explicit leaf allowlist. Unknown tables are walked only to report their keys;
/// executable settings never reach Config, launchers or approval policy.
pub fn declarative(table: &toml::Table) -> (toml::Table, Vec<String>) {
    fn visit(table: &toml::Table, prefix: &str, accepted: &mut toml::Table, warnings: &mut Vec<String>) {
        for (key, value) in table {
            let name = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            if name == "isolation.mode" && value.as_str() != Some("sandbox") {
                warnings.push(format!(
                    "ignored {name}: a repository can only tighten isolation to \"sandbox\", never loosen it"
                ));
                continue;
            }
            let valid = match name.as_str() {
                "base_ref"
                | "pr_title"
                | "review_policy"
                | "tickets.branch"
                | "workflow.done"
                | "isolation.mode"
                | "isolation.sandbox.template" => Some(value.is_str()),
                "test_commands" | "qa_commands" | "worktrees.warm" | "workflow.stages" => {
                    Some(value.as_array().is_some_and(|items| items.iter().all(toml::Value::is_str)))
                }
                _ => None,
            };
            if let Some(valid) = valid {
                if valid {
                    accepted.insert(key.clone(), value.clone());
                } else {
                    warnings.push(format!("ignored {name}: invalid type for declarative setting"));
                }
            } else if matches!(name.as_str(), "tickets" | "worktrees" | "workflow" | "isolation" | "isolation.sandbox")
                && value.is_table()
            {
                let mut inner = toml::Table::new();
                visit(value.as_table().unwrap(), &name, &mut inner, warnings);
                if !inner.is_empty() || value.as_table().unwrap().is_empty() {
                    accepted.insert(key.clone(), toml::Value::Table(inner));
                }
            } else {
                warnings.push(format!("ignored {name}: repository config allows only declarative workflow settings; commands, ticket servers and approvals are refused"));
            }
        }
    }
    let (mut accepted, mut warnings) = (toml::Table::new(), Vec::new());
    visit(table, "", &mut accepted, &mut warnings);
    if let Some(value) = accepted.get("workflow") {
        let parsed: Result<crate::workflow::Workflow, _> = value.clone().try_into();
        let error = parsed.map_err(|_| "invalid workflow".to_string()).and_then(|workflow| workflow.validate()).err();
        if let Some(error) = error {
            accepted.remove("workflow");
            warnings.push(format!("ignored workflow: {error}"));
        }
    }
    (accepted, warnings)
}

fn bounded(text: &str, limit: usize) -> String {
    const NOTICE: &str = "\n[truncated; read the named project files for the full workflow]\n";
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut text: String = text.chars().take(limit.saturating_sub(NOTICE.chars().count())).collect();
    text.push_str(NOTICE);
    text
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn outside_instruction_and_config_symlinks_are_refused() {
        use std::os::unix::fs::symlink;

        let dir = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".imacs")).unwrap();
        std::fs::write(outside.path().join("instructions"), "outside-secret").unwrap();
        std::fs::write(outside.path().join("config"), "review_policy = 'outside-secret'").unwrap();
        symlink(outside.path().join("instructions"), dir.path().join("IMACS.md")).unwrap();
        symlink(outside.path().join("instructions"), dir.path().join(".imacs/topic.md")).unwrap();
        symlink(outside.path().join("config"), dir.path().join(".imacs/config.toml")).unwrap();

        let workflow = RepoWorkflow::read(dir.path());
        assert!(workflow.instructions.is_empty());
        assert!(workflow.config.text.is_none());
        assert!(workflow.config.table.is_empty());
        assert!(workflow.chief_section("app").is_empty());
        assert!(workflow.agent_section().is_empty());
        for file in ["IMACS.md", ".imacs/topic.md", ".imacs/config.toml"] {
            assert!(
                workflow
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(file) && warning.contains("outside the main checkout")),
                "{:?}",
                workflow.warnings
            );
        }
        assert!(!workflow.warnings.join("\n").contains("outside-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn outside_topics_directory_is_not_walked() {
        let dir = crate::test_support::tempdir().unwrap();
        let outside = crate::test_support::tempdir().unwrap();
        std::fs::write(dir.path().join("IMACS.md"), "inside instructions").unwrap();
        std::fs::write(outside.path().join("topic.md"), "outside-secret").unwrap();
        std::fs::write(outside.path().join("config.toml"), "base_ref = 'outside-secret'").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".imacs")).unwrap();

        let workflow = RepoWorkflow::read(dir.path());
        assert_eq!(workflow.instructions.len(), 1);
        assert!(workflow.config.text.is_none());
        assert!(workflow.config.table.is_empty());
        assert_eq!(workflow.warnings.len(), 1);
        assert!(workflow.warnings[0].contains(".imacs: resolves outside the main checkout"));
        assert!(!workflow.chief_section("app").contains("outside-secret"));
        assert!(!workflow.agent_section().contains("outside-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_within_the_canonical_checkout_are_allowed() {
        use std::os::unix::fs::symlink;

        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().join("checkout");
        std::fs::create_dir_all(root.join("topics")).unwrap();
        std::fs::write(root.join("instructions"), "inside instructions").unwrap();
        std::fs::write(root.join("config"), "base_ref = 'origin/develop'").unwrap();
        symlink("instructions", root.join("IMACS.md")).unwrap();
        symlink("topics", root.join(".imacs")).unwrap();
        symlink("../instructions", root.join("topics/test.md")).unwrap();
        symlink("../config", root.join("topics/config.toml")).unwrap();
        symlink(&root, dir.path().join("linked-checkout")).unwrap();

        let workflow = RepoWorkflow::read(&dir.path().join("linked-checkout"));
        assert!(workflow.warnings.is_empty(), "{:?}", workflow.warnings);
        assert_eq!(workflow.instructions.len(), 2);
        assert!(workflow.instructions.iter().all(|(_, text)| text == "inside instructions"));
        assert!(workflow.instructions[1].0.ends_with(".imacs/test.md"));
        assert_eq!(workflow.config.table["base_ref"].as_str(), Some("origin/develop"));
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_refused_without_blocking() {
        use std::os::unix::{ffi::OsStrExt, net::UnixListener};

        let dir = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".imacs")).unwrap();
        for file in ["IMACS.md", ".imacs/config.toml"] {
            let path = std::ffi::CString::new(dir.path().join(file).as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let _socket = UnixListener::bind(dir.path().join(".imacs/socket.md")).unwrap();
        std::fs::create_dir(dir.path().join(".imacs/directory.md")).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || sender.send(RepoWorkflow::read(dir.path())).unwrap());
        let workflow =
            receiver.recv_timeout(std::time::Duration::from_secs(2)).expect("workflow read must not block on a FIFO");
        reader.join().unwrap();
        assert!(workflow.instructions.is_empty());
        assert!(workflow.config.table.is_empty());
        for file in ["IMACS.md", ".imacs/config.toml", ".imacs/socket.md", ".imacs/directory.md"] {
            assert!(
                workflow
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(file) && warning.contains("not a regular file")),
                "{:?}",
                workflow.warnings
            );
        }
    }

    #[test]
    fn file_reads_are_bounded_and_oversized_files_are_omitted() {
        let dir = crate::test_support::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".imacs")).unwrap();
        std::fs::write(dir.path().join("IMACS.md"), vec![b'x'; FILE_LIMIT as usize - 1]).unwrap();
        let workflow = RepoWorkflow::read(dir.path());
        assert!(workflow.warnings.is_empty());
        assert_eq!(workflow.instructions[0].1.len(), FILE_LIMIT as usize - 1);

        // Invalid UTF-8 beyond the limit must never be read or decoded, including for config.
        let mut oversized = vec![b'x'; FILE_LIMIT as usize];
        oversized.push(0xff);
        for file in ["IMACS.md", ".imacs/topic.md", ".imacs/config.toml"] {
            std::fs::write(dir.path().join(file), &oversized).unwrap();
        }
        let workflow = RepoWorkflow::read(dir.path());
        assert!(workflow.instructions.is_empty());
        assert!(workflow.config.text.is_none());
        assert!(workflow.config.table.is_empty());
        assert!(workflow.chief_section("app").is_empty());
        assert!(workflow.agent_section().is_empty());
        for file in ["IMACS.md", ".imacs/topic.md", ".imacs/config.toml"] {
            assert!(
                workflow
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(file) && warning.contains("65536-byte read limit")),
                "{:?}",
                workflow.warnings
            );
        }
        std::fs::write(dir.path().join("IMACS.md"), vec![b'x'; FILE_LIMIT as usize]).unwrap();
        assert!(RepoWorkflow::read(dir.path()).instructions.is_empty());
    }

    #[test]
    fn instructions_reload_by_content_and_bound_each_project() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".imacs")).unwrap();
        std::fs::write(root.join("IMACS.md"), "Use issue branches.").unwrap();
        std::fs::write(root.join(".imacs/tests.md"), "Run targeted tests.").unwrap();
        let first = RepoWorkflow::read(root);
        assert!(first.chief_section("app").contains(".imacs/tests.md"));
        assert!(first.agent_section().contains("main checkout"));
        assert!(first.agent_section().contains("Use issue branches."));
        std::fs::write(root.join("IMACS.md"), "Use topic branches.").unwrap();
        let second = RepoWorkflow::read(root);
        assert_ne!(first.fingerprint, second.fingerprint);
        assert!(second.agent_section().contains("Use topic branches."));
        std::fs::write(root.join("IMACS.md"), "é".repeat(CHIEF_LIMIT * 2)).unwrap();
        let section = RepoWorkflow::read(root).chief_section("app");
        assert_eq!(section.chars().count(), CHIEF_LIMIT);
        assert!(section.contains("truncated"));
        std::fs::remove_file(root.join(".imacs/tests.md")).unwrap();
        assert_ne!(RepoWorkflow::read(root).fingerprint, second.fingerprint);
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn repo_config_accepts_only_declarative_keys_and_valid_workflows() {
        let text = r##"
base_ref = "origin/develop"
pr_title = "#{key} - {title}"
test_commands = ["cargo test"]
qa_commands = ["imacs --screenshot /tmp/app.png"]
review_policy = "Review before merge."
unknown = "ignored"
agents = [{ name = "evil", command = ["touch", "should-not-exist"] }]
[worktrees]
warm = ["target"]
[tickets]
branch = "issue/{key_number}-{slug}"
server.command = ["evil-server"]
[approvals]
allow = ["*"]
[chief]
command = ["evil-chief"]
[workflow]
done = "qa_passed"
stages = ["accepted", "pr_opened", "qa_passed", "merged"]
"##;
        let (table, warnings) = declarative(&toml::from_str(text).unwrap());
        for key in ["unknown", "agents", "tickets.server", "approvals", "chief"] {
            assert!(warnings.iter().any(|warning| warning.contains(&format!("ignored {key}:"))), "{warnings:?}");
        }
        let mut repo = RepoWorkflow::default();
        repo.config.table = table;
        let mut workspace = crate::config::Config::default();
        workspace.tickets.branch = Some("workspace/{slug}".into());
        let applied = repo.apply(&workspace);
        assert_eq!(applied.base_ref, "origin/develop");
        assert_eq!(applied.worktrees.warm, ["target"]);
        assert_eq!(applied.tickets.branch.as_deref(), Some("issue/{key_number}-{slug}"));
        assert_eq!(applied.workflow.done, "qa_passed");
        assert_eq!(serde_json::to_value(applied.agents).unwrap(), serde_json::to_value(workspace.agents).unwrap());
        assert_eq!(applied.tickets.server, workspace.tickets.server);
        assert!(repo.agent_section().contains("cargo test"));
        assert!(!repo.agent_section().contains("evil"));
        assert_eq!(repo.config.table["pr_title"].as_str(), Some("#{key} - {title}"));
        let (table, warnings) = declarative(&toml::from_str("[workflow]\ndone = 'ci_failed'").unwrap());
        assert!(!table.contains_key("workflow"));
        assert!(warnings.iter().any(|warning| warning.contains("ignored workflow")));
        let (_, warnings) = declarative(&toml::from_str("base_ref = 42\ntest_commands = 'execute me'").unwrap());
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn repo_template_overrides_workspace_without_widening_host_access() {
        let mut user = crate::config::Config::default();
        user.isolation.sandbox.template = Some("workspace:local".into());
        let (table, warnings) = declarative(
            &toml::from_str("[isolation.sandbox]\ntemplate = 'repo:local'\nmounts = ['/']\nssh_agent = true").unwrap(),
        );
        assert_eq!(warnings.len(), 2);
        let mut repo = RepoWorkflow::default();
        repo.config.table = table;
        let applied = repo.apply(&user);
        assert_eq!(applied.isolation.sandbox.template.as_deref(), Some("repo:local"));
        assert!(applied.isolation.sandbox.mounts.is_empty());
        assert!(!applied.isolation.sandbox.ssh_agent);
        let (_, warnings) = declarative(&toml::from_str("[isolation.sandbox]\ntemplate = 42").unwrap());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn repo_config_can_only_tighten_isolation() {
        use crate::isolation::IsolationMode::{Bare, Sandbox};
        let mut user = crate::config::Config::default();
        user.isolation.mode = Bare;
        user.agents[1].isolation = Some(Bare);
        user.agents[2].isolation = Some(Sandbox);
        let applied = |text: &str| {
            let (table, warnings) = declarative(&toml::from_str(text).unwrap());
            let mut repo = RepoWorkflow::default();
            repo.config.table = table;
            (repo.apply(&user), warnings)
        };
        let (config, warnings) = applied("[isolation]\nmode = 'sandbox'");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.isolation.mode, Sandbox);
        for agent in &config.agents {
            for task in [None, Some(Bare), Some(Sandbox)] {
                assert_eq!(config.isolation.mode_for(agent, task), Sandbox, "{} must run sandboxed", agent.name);
            }
        }

        let mut sandboxed = user.clone();
        sandboxed.isolation.mode = Sandbox;
        for text in [
            "[isolation]\nmode = 'bare'",
            "[isolation]\nmode = 'none'",
            "[isolation]\nmode = 1",
            "[isolation.sandbox]\nssh_agent = true\nmounts = ['/']\nallow_hosts = { '.' = ['**'] }",
        ] {
            let (table, warnings) = declarative(&toml::from_str(text).unwrap());
            assert!(!warnings.is_empty(), "{text}");
            assert!(warnings.iter().all(|w| w.starts_with("ignored isolation")), "{warnings:?}");
            let mut repo = RepoWorkflow::default();
            repo.config.table = table;
            for config in [&user, &sandboxed] {
                let applied = repo.apply(config);
                assert_eq!(applied.isolation, config.isolation, "{text} must not change isolation");
                for (applied, agent) in applied.agents.iter().zip(&config.agents) {
                    assert_eq!(applied.isolation, agent.isolation, "{text}");
                }
            }
        }
        let (_, warnings) = declarative(&toml::from_str("[isolation]\nmode = 'bare'").unwrap());
        assert!(warnings[0].contains("only tighten"), "{warnings:?}");
    }
}
