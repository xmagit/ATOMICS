//! Trusted project instructions, always read from the main checkout.
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

/// Maximum characters added to the chief prompt for one project, including file names.
pub const CHIEF_LIMIT: usize = 12_000;

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
            if file.ends_with("/IMACS.md") {
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
        config
    }

    #[cfg(feature = "native")]
    pub fn read(root: &Path) -> Self {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut workflow = Self::default();
        let mut files = vec![root.join("IMACS.md")];
        let mut topics: Vec<PathBuf> = std::fs::read_dir(root.join(".imacs"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
            .collect();
        topics.sort();
        files.extend(topics);
        let mut hash = DefaultHasher::new();
        for file in files {
            file.hash(&mut hash);
            match std::fs::read_to_string(&file) {
                Ok(text) => {
                    text.hash(&mut hash);
                    workflow.instructions.push((file.display().to_string(), text));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => workflow.warnings.push(format!("{}: {error}", file.display())),
            }
        }
        let mut config =
            crate::selfconfig::Layer::read(crate::selfconfig::Source::Repo, &root.join(".imacs/config.toml"));
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

/// Explicit leaf allowlist. Unknown tables are walked only to report their keys;
/// executable settings never reach Config, launchers or approval policy.
pub fn declarative(table: &toml::Table) -> (toml::Table, Vec<String>) {
    fn visit(table: &toml::Table, prefix: &str, accepted: &mut toml::Table, warnings: &mut Vec<String>) {
        for (key, value) in table {
            let name = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
            let valid = match name.as_str() {
                "base_ref" | "pr_title" | "review_policy" | "tickets.branch" | "workflow.done" => Some(value.is_str()),
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
            } else if matches!(name.as_str(), "tickets" | "worktrees" | "workflow") && value.is_table() {
                let mut inner = toml::Table::new();
                visit(value.as_table().unwrap(), &name, &mut inner, warnings);
                accepted.insert(key.clone(), toml::Value::Table(inner));
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

    #[test]
    fn instructions_reload_by_content_and_bound_each_project() {
        let dir = tempfile::tempdir().unwrap();
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
}
