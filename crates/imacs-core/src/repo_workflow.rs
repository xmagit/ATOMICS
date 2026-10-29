//! Trusted project instructions, always read from the main checkout.
#[cfg(feature = "native")]
use std::path::{Path, PathBuf};

/// Maximum characters added to the chief prompt for one project, including file names.
pub const CHIEF_LIMIT: usize = 12_000;

#[derive(Debug, Clone, Default)]
pub struct RepoWorkflow {
    pub instructions: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub fingerprint: u64,
}

impl RepoWorkflow {
    /// Names every included file and bounds the entire per-project section.
    pub fn chief_section(&self, project: &str) -> String {
        if self.instructions.is_empty() {
            return String::new();
        }
        let mut text = format!("\n\nProject instructions from the main checkout of {project}:\n");
        for (file, content) in &self.instructions {
            text.push_str(&format!("\nFrom {file}:\n{content}\n"));
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
        text
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
        workflow.warnings.hash(&mut hash);
        workflow.fingerprint = hash.finish();
        workflow
    }
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
