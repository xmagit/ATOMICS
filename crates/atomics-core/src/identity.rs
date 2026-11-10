//! User-owned commit identities and a guard whose diagnostics contain locations only.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// None inherits; an explicit empty list clears the inherited guard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forbidden_words: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentityConfig {
    #[serde(flatten)]
    pub default: Identity,
    /// Main-checkout paths, in the user's config only. Never read from a repository.
    pub projects: BTreeMap<String, Identity>,
}

impl Identity {
    pub fn overlay(&self, other: &Self) -> Self {
        Self {
            name: other.name.clone().or_else(|| self.name.clone()),
            email: other.email.clone().or_else(|| self.email.clone()),
            forbidden_words: other.forbidden_words.clone().or_else(|| self.forbidden_words.clone()),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for (key, value) in [("name", &self.name), ("email", &self.email)] {
            if value
                .as_ref()
                .is_some_and(|v| v.trim().is_empty() || v.chars().any(char::is_control) || v.contains(['<', '>']))
            {
                return Err(format!(
                    "identity.{key} must be nonempty and contain no control characters or angle brackets"
                ));
            }
        }
        if self
            .forbidden_words
            .iter()
            .flatten()
            .any(|word| word.trim().is_empty() || word.chars().any(char::is_control))
        {
            return Err("identity.forbidden_words must contain nonempty words without control characters".into());
        }
        Ok(())
    }

    pub fn guarded(&self) -> bool {
        self.email.is_some() || self.forbidden_words.as_ref().is_some_and(|words| !words.is_empty())
    }

    pub fn has_git_identity(&self) -> bool {
        self.name.is_some() || self.email.is_some()
    }

    pub fn label(&self) -> String {
        format!("{} <{}>", self.name.as_deref().unwrap_or("git name"), self.email.as_deref().unwrap_or("git email"))
    }
}

impl IdentityConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.default.validate()?;
        for identity in self.projects.values() {
            identity.validate()?;
        }
        Ok(())
    }

    /// Resolve each key: project, workspace, main/host, then Git for unset keys.
    pub fn resolve(&self, project: &str, workspace: Option<&Identity>, expand: impl Fn(&str) -> String) -> Identity {
        let base = workspace.map_or_else(|| self.default.clone(), |ws| self.default.overlay(ws));
        self.projects.iter().find(|(path, _)| expand(path) == project).map_or(base.clone(), |(_, id)| base.overlay(id))
    }
}

#[cfg(feature = "native")]
mod native {
    use super::*;
    use crate::{
        diff::{LineKind, parse_unified},
        git,
    };
    use anyhow::{Context, Result, bail, ensure};
    use std::{path::Path, process::Command};

    pub const UNTRACKED_SCAN_LIMIT: u64 = 4 * 1024 * 1024;

    fn include_key(dir: &Path) -> String {
        let mut pattern = String::new();
        for ch in dir.to_string_lossy().chars() {
            if matches!(ch, '\\' | '*' | '?' | '[' | ']') {
                pattern.push('\\');
            }
            pattern.push(ch);
        }
        format!("includeIf.gitdir:{pattern}.path")
    }

    /// A conditional repository-local include selects just this linked worktree. The included
    /// file lives in the common dir's read-only part, so no worktreeConfig extension is needed
    /// and a sandbox cannot inject executable config into host Git. The main identity is untouched.
    pub fn set_worktree_identity(worktree: &Path, identity: &Identity) -> Result<()> {
        identity.validate().map_err(anyhow::Error::msg)?;
        ensure!(identity.has_git_identity(), "an empty identity must be removed, not written");
        let pins = git::resolve_pins(worktree)?;
        let common = pins.common_dir.context("worktree has no common git directory")?;
        let dir = pins.git_dir.context("worktree has no git directory")?;
        ensure!(dir != common, "identity settings are only written for linked worktrees");
        let folder = common.join("atomics-identities");
        std::fs::create_dir_all(&folder)?;
        let file = folder
            .join(format!("{}.config", dir.file_name().context("worktree has no directory name")?.to_string_lossy()));
        let temp = tempfile::NamedTempFile::new_in(&folder)?;
        for (key, value) in [("user.name", &identity.name), ("user.email", &identity.email)] {
            if let Some(value) = value {
                git::git(worktree, &["config", "--file", &temp.path().to_string_lossy(), key, value])?;
            }
        }
        temp.persist(&file)?;
        git::git(worktree, &["config", "--local", "--replace-all", &include_key(&dir), &file.to_string_lossy()])?;
        Ok(())
    }

    /// Unconfigured worktrees get no config entry or identity file. Removing an override
    /// restores Git's own identity, including when a configured identity becomes empty.
    pub fn sync_worktree_identity(worktree: &Path, identity: &Identity) -> Result<()> {
        if identity.has_git_identity() {
            set_worktree_identity(worktree, identity)
        } else if is_managed_worktree(worktree) {
            remove_worktree_identity(worktree, &git::resolve_pins(worktree)?)
        } else {
            Ok(())
        }
    }

    /// Do not leave an identity behind for Git to apply when it reuses an admin directory.
    pub fn remove_worktree_identity(repo: &Path, pins: &git::GitPins) -> Result<()> {
        let (Some(common), Some(dir)) = (&pins.common_dir, &pins.git_dir) else { return Ok(()) };
        if dir == common {
            return Ok(());
        }
        let file = common
            .join("atomics-identities")
            .join(format!("{}.config", dir.file_name().context("worktree has no directory name")?.to_string_lossy()));
        if !file.is_file() {
            return Ok(());
        }
        let key = include_key(dir);
        if git::git(repo, &["config", "--local", "--get-all", &key]).is_ok() {
            git::git(repo, &["config", "--local", "--unset-all", &key])?;
        }
        std::fs::remove_file(&file)?;
        // Other worktrees may still use this folder.
        let _ = std::fs::remove_dir(file.parent().unwrap());
        Ok(())
    }

    pub fn is_managed_worktree(worktree: &Path) -> bool {
        git::resolve_pins(worktree).ok().is_some_and(|pins| match (pins.common_dir, pins.git_dir) {
            (Some(common), Some(dir)) if common != dir => dir.file_name().is_some_and(|name| {
                common.join("atomics-identities").join(format!("{}.config", name.to_string_lossy())).is_file()
            }),
            _ => false,
        })
    }

    /// Apply identity explicitly to both roles, even when the host inherited GIT_*_IDENT
    /// environment overrides. Unset keys retain Git's ordinary behavior.
    pub fn apply_to_command(command: &mut Command, identity: &Identity) {
        for (key, value) in [("NAME", &identity.name), ("EMAIL", &identity.email)] {
            if let Some(value) = value {
                for role in ["AUTHOR", "COMMITTER"] {
                    command.env(format!("GIT_{role}_{key}"), value);
                }
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Hit {
        pub file: String,
        pub line: usize,
        pub reason: &'static str,
    }

    impl std::fmt::Display for Hit {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}:{}: {}", self.file, self.line, self.reason)
        }
    }

    fn text_hits(hits: &mut Vec<Hit>, identity: &Identity, file: &str, line: usize, text: &str) {
        let lower = text.to_lowercase();
        if identity.forbidden_words.iter().flatten().any(|word| lower.contains(&word.to_lowercase())) {
            hits.push(Hit { file: file.into(), line, reason: "forbidden word" });
        }
    }

    fn patch_hits(hits: &mut Vec<Hit>, identity: &Identity, patch: &str) {
        for file in parse_unified(patch) {
            for line in file.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind == LineKind::Added) {
                text_hits(hits, identity, file.path(), line.new_no.unwrap_or(1) as usize, &line.text);
            }
        }
    }

    /// Scan all new commits (without mailmap), their additions, and the live task diff including
    /// every untracked file. No UI diff budget applies. Metadata uses raw Git object line numbers.
    #[cfg(test)]
    pub fn check_task(repo: &Path, branch: &str, base_ref: &str, identity: &Identity) -> Result<Vec<Hit>> {
        check_task_for_integration(repo, branch, base_ref, None, identity)
    }

    #[cfg(test)]
    pub fn check_task_for_integration(
        repo: &Path,
        branch: &str,
        base_ref: &str,
        into: Option<&str>,
        identity: &Identity,
    ) -> Result<Vec<Hit>> {
        check_task_impl(repo, None, true, branch, base_ref, into, identity)
    }

    pub fn check_task_at(
        repo: &Path,
        worktree: &Path,
        branch: &str,
        base_ref: &str,
        into: Option<&str>,
        identity: &Identity,
    ) -> Result<Vec<Hit>> {
        check_task_impl(repo, Some(worktree), false, branch, base_ref, into, identity)
    }

    /// Historical tasks with no remaining worktree are checked without discovering a checkout
    /// from its writable HEAD. Live tasks use `check_task_at` with their recorded path.
    pub fn check_task_history(
        repo: &Path,
        branch: &str,
        base_ref: &str,
        into: Option<&str>,
        identity: &Identity,
    ) -> Result<Vec<Hit>> {
        check_task_impl(repo, None, false, branch, base_ref, into, identity)
    }

    fn check_task_impl(
        repo: &Path,
        worktree: Option<&Path>,
        discover: bool,
        branch: &str,
        base_ref: &str,
        into: Option<&str>,
        identity: &Identity,
    ) -> Result<Vec<Hit>> {
        identity.validate().map_err(anyhow::Error::msg)?;
        if !identity.guarded() {
            return Ok(Vec::new());
        }
        // Git errors may quote diff contents: report only the failed operation.
        let read = |dir: &Path, args: &[&str]| {
            git::git(dir, args).map_err(|_| anyhow::anyhow!("identity guard could not read task Git data"))
        };
        let reference = format!("refs/heads/{branch}");
        let base = git::resolve_base(repo, base_ref)
            .map_err(|_| anyhow::anyhow!("identity guard could not resolve its base"))?;
        // Author identity applies only to work not already accepted into the integration
        // branch or base. Existing commits may predate the configured identity.
        let into = into.map(|name| format!("refs/heads/{name}"));
        let into = into.as_deref().filter(|reference| git::commit_of(repo, reference).ok().flatten().is_some());
        let mut author_args = vec!["rev-list", &reference, "--not", &base];
        if let Some(into) = into {
            author_args.push(into);
        }
        let authors: std::collections::HashSet<String> = if identity.email.is_some() {
            read(repo, &author_args)?.lines().map(str::to_string).collect()
        } else {
            Default::default()
        };
        let base = read(repo, &["merge-base", &base, &reference])?;
        let mut hits = Vec::new();
        text_hits(&mut hits, identity, &format!(".git/{reference}"), 1, branch);
        let commits = read(repo, &["rev-list", &format!("{}..{reference}", base.trim())])?;
        for commit in commits.lines() {
            let raw = read(repo, &["cat-file", "commit", commit])?;
            let file = format!(".git/objects/{commit}");
            let mut message = false;
            for (n, line) in raw.lines().enumerate() {
                if line.is_empty() {
                    message = true;
                }
                if message || line.starts_with("author ") || line.starts_with("committer ") {
                    text_hits(&mut hits, identity, &file, n + 1, line);
                }
                if !message
                    && authors.contains(commit)
                    && let Some(author) = line.strip_prefix("author ")
                    && let Some(expected) = &identity.email
                {
                    let email =
                        author.rsplit_once(" <").and_then(|(_, rest)| rest.split_once('>')).map(|(email, _)| email);
                    if email.is_none_or(|email| !email.eq_ignore_ascii_case(expected)) {
                        hits.push(Hit {
                            file: file.clone(),
                            line: n + 1,
                            reason: "author email differs from active identity",
                        });
                    }
                }
            }
            patch_hits(
                &mut hits,
                identity,
                &read(
                    repo,
                    &[
                        "diff-tree",
                        "--root",
                        "-m",
                        "--no-commit-id",
                        "-p",
                        "--text",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-color",
                        "--unified=0",
                        commit,
                    ],
                )?,
            );
        }
        let entry = match worktree {
            Some(path) => Some(git::recorded_worktree(repo, path, branch)?),
            None if discover => git::list_worktrees(repo)
                .map_err(|_| anyhow::anyhow!("identity guard could not list task worktrees"))?
                .into_iter()
                .find(|e| e.branch.as_deref() == Some(branch) && e.path.is_dir()),
            None => None,
        };
        if let Some(entry) = entry {
            ensure!(entry.trusted, "identity guard cannot read an untrusted task worktree");
            patch_hits(
                &mut hits,
                identity,
                &read(
                    &entry.path,
                    &["diff", "--text", "--no-ext-diff", "--no-textconv", "--no-color", "--unified=0", base.trim()],
                )?,
            );
            let untracked = read(&entry.path, &["ls-files", "--others", "--exclude-standard", "-z"])?;
            for path in untracked.split('\0').filter(|p| !p.is_empty()) {
                let content = match git::read_worktree_file(&entry.path, path, UNTRACKED_SCAN_LIMIT + 1) {
                    Some(git::WorktreeFile::Link(target)) => target,
                    Some(git::WorktreeFile::File { len, bytes })
                        if len > UNTRACKED_SCAN_LIMIT
                            || bytes.as_ref().is_some_and(|bytes| bytes.len() as u64 > UNTRACKED_SCAN_LIMIT) =>
                    {
                        hits.push(Hit {
                            file: path.into(),
                            line: 1,
                            reason: "untracked file exceeds the 4 MiB identity scan limit",
                        });
                        continue;
                    }
                    Some(git::WorktreeFile::File { bytes: Some(bytes), .. }) => {
                        String::from_utf8_lossy(&bytes).into_owned()
                    }
                    _ => bail!("identity guard could not read {path}:1"),
                };
                for (n, line) in content.lines().enumerate() {
                    text_hits(&mut hits, identity, path, n + 1, line);
                }
            }
        }
        hits.sort_by(|a, b| (&a.file, a.line, a.reason).cmp(&(&b.file, b.line, b.reason)));
        hits.dedup();
        Ok(hits)
    }

    pub fn require_clean(hits: &[Hit]) -> Result<()> {
        if !hits.is_empty() {
            bail!(
                "identity guard blocked integration:\n{}",
                hits.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")
            );
        }
        Ok(())
    }

    /// Check text ATOMICS will itself put into a commit before it writes anything.
    pub fn check_pending(identity: &Identity, file: &str, text: &str) -> Result<()> {
        let mut hits = Vec::new();
        for (n, line) in text.lines().enumerate() {
            text_hits(&mut hits, identity, file, n + 1, line);
        }
        require_clean(&hits)
    }
}

#[cfg(feature = "native")]
pub use native::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_precedence_and_explicit_empty_guard() {
        let config: IdentityConfig = toml::from_str(
            r#"
name = "Main Example"
email = "main@example.com"
forbidden_words = ["contoso"]
[projects."~/app"]
name = "Project Example"
email = "project@example.com"
forbidden_words = []
"#,
        )
        .unwrap();
        let ws: Identity = toml::from_str("name = 'Workspace Example'\nemail = 'workspace@example.com'").unwrap();
        let expand = |path: &str| path.replace('~', "/example");
        assert_eq!(config.resolve("/other", None, expand), config.default);
        let workspace = config.resolve("/other", Some(&ws), expand);
        assert_eq!(workspace.email.as_deref(), Some("workspace@example.com"));
        assert_eq!(workspace.name, ws.name);
        assert_eq!(workspace.forbidden_words, config.default.forbidden_words);
        let project = config.resolve("/example/app", Some(&ws), expand);
        assert_eq!(project.email.as_deref(), Some("project@example.com"));
        assert_eq!(project.name.as_deref(), Some("Project Example"));
        assert_eq!(project.forbidden_words, Some(vec![]));
        assert_eq!(IdentityConfig::default().resolve("/other", None, expand), Identity::default());
    }

    #[cfg(feature = "native")]
    fn project() -> (crate::test_support::TempDir, std::path::PathBuf, Identity) {
        let dir = crate::test_support::tempdir().unwrap();
        let repo = dir.path().join("app");
        std::fs::create_dir(&repo).unwrap();
        crate::git::git(&repo, &["init", "-qb", "main"]).unwrap();
        crate::git::git(&repo, &["config", "user.name", "Git Example"]).unwrap();
        crate::git::git(&repo, &["config", "user.email", "git@example.com"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        crate::git::commit_all(&repo, "base").unwrap();
        let identity = Identity {
            name: Some("Context Example".into()),
            email: Some("context@example.com".into()),
            forbidden_words: Some(vec!["contoso".into()]),
        };
        (dir, repo, identity)
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_default_does_not_write_git_config_and_empty_removes_it() {
        use crate::git;
        let (dir, repo, identity) = project();
        let common = git::common_dir(&repo).unwrap();
        let before = std::fs::read(common.join("config")).unwrap();
        let task = dir.path().join("task");
        let into = dir.path().join("integration");
        git::add_worktree(&repo, &task, "task", "main").unwrap();
        sync_worktree_identity(&task, &Identity::default()).unwrap();
        std::fs::write(task.join("work.txt"), "safe\n").unwrap();
        git::integrate(&repo, &task, "task", "finish", "integration", &into, "main").unwrap();
        assert_eq!(std::fs::read(common.join("config")).unwrap(), before);
        assert!(!common.join("atomics-identities").exists());
        sync_worktree_identity(&task, &identity).unwrap();
        assert!(is_managed_worktree(&task));
        sync_worktree_identity(&task, &Identity::default()).unwrap();
        assert!(!is_managed_worktree(&task));
        assert!(!common.join("atomics-identities").exists());
        assert!(!std::fs::read_to_string(common.join("config")).unwrap().contains("includeIf"));
        assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "git@example.com");
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_author_email_ignores_case_and_already_integrated_history() {
        use crate::git;
        let (dir, repo, identity) = project();
        let into = dir.path().join("integration");
        git::add_worktree(&repo, &into, "integration", "main").unwrap();
        std::fs::write(into.join("old.txt"), "old work\n").unwrap();
        git::commit_all(&into, "work before identity was configured").unwrap();
        let task = dir.path().join("task");
        git::add_worktree(&repo, &task, "task", "integration").unwrap();
        assert!(
            check_task(&repo, "task", "main", &identity).unwrap().iter().any(|hit| hit.reason.contains("author email"))
        );
        assert!(check_task_for_integration(&repo, "task", "main", Some("integration"), &identity).unwrap().is_empty());
        std::fs::write(task.join("new.txt"), "new work\n").unwrap();
        let mixed_case = Identity { email: Some("CONTEXT@EXAMPLE.COM".into()), ..identity.clone() };
        git::commit_all_with_identity(&task, "new identity", &mixed_case).unwrap();
        assert!(check_task_for_integration(&repo, "task", "main", Some("integration"), &identity).unwrap().is_empty());
        git::integrate_with_identity(&repo, &task, "task", "finish", "integration", &into, "main", &identity).unwrap();
        std::fs::write(task.join("wrong.txt"), "wrong author\n").unwrap();
        git::commit_all_with_identity(
            &task,
            "new wrong author",
            &Identity { email: Some("other@example.com".into()), ..identity.clone() },
        )
        .unwrap();
        let wrong = git::git(&task, &["rev-parse", "HEAD"]).unwrap();
        let hits = check_task_for_integration(&repo, "task", "main", Some("integration"), &identity).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].file, format!(".git/objects/{}", wrong.trim()));
        assert!(
            git::integrate_with_identity(&repo, &task, "task", "finish", "integration", &into, "main", &identity)
                .is_err()
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_guard_blocks_oversized_untracked_files_with_locations_only() {
        use crate::git;
        let (dir, repo, identity) = project();
        let task = dir.path().join("task");
        git::add_worktree(&repo, &task, "task", "main").unwrap();
        std::fs::File::create(task.join("large.txt")).unwrap().set_len(UNTRACKED_SCAN_LIMIT + 1).unwrap();
        let hits = check_task(&repo, "task", "main", &identity).unwrap();
        assert_eq!(
            hits,
            [Hit { file: "large.txt".into(), line: 1, reason: "untracked file exceeds the 4 MiB identity scan limit" }]
        );
        let error = git::integrate_with_identity(
            &repo,
            &task,
            "task",
            "finish",
            "integration",
            &dir.path().join("integration"),
            "main",
            &identity,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("large.txt:1:"));
        assert!(!git::branch_exists(&repo, "integration"));
        assert!(git::git(&task, &["diff", "--cached"]).unwrap().is_empty());
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_worktrees_and_automatic_commits_keep_the_main_identity() {
        use crate::git;
        let (dir, repo, mut identity) = project();
        let renamed = dir.path().join("app[1]");
        std::fs::rename(&repo, &renamed).unwrap();
        let repo = renamed;
        // Conditional gitdir patterns must treat glob characters in paths literally.
        let task = dir.path().join("task[1]");
        let peer = dir.path().join("peer");
        let into = dir.path().join("integration");
        git::add_worktree(&repo, &task, "task", "main").unwrap();
        git::add_worktree(&repo, &peer, "peer", "main").unwrap();
        identity.name = Some("Context \"Example\" Contributor".into());
        set_worktree_identity(&task, &identity).unwrap();
        assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "context@example.com");
        assert_eq!(git::git(&peer, &["config", "user.email"]).unwrap().trim(), "git@example.com");
        assert_eq!(git::git(&repo, &["config", "user.email"]).unwrap().trim(), "git@example.com");
        assert!(set_worktree_identity(&repo, &identity).is_err());
        git::check_worktree_config(&git::common_dir(&repo).unwrap()).unwrap();
        git::resolve_pins(&task).unwrap();
        std::fs::write(task.join("agent.txt"), "agent work\n").unwrap();
        git::commit_all(&task, "author body line is not a commit header").unwrap();
        assert_eq!(
            git::git(&task, &["log", "-1", "--format=%an|%ae|%ce"]).unwrap().trim(),
            "Context \"Example\" Contributor|context@example.com|context@example.com"
        );
        std::fs::write(task.join("automatic.txt"), "automatic work\n").unwrap();
        let other = Identity { email: Some("other@example.com".into()), ..identity.clone() };
        set_worktree_identity(&task, &other).unwrap();
        git::commit_all_with_identity(&task, "explicit identity", &identity).unwrap();
        assert_eq!(
            git::git(&task, &["log", "-1", "--format=%ae|%ce"]).unwrap().trim(),
            "context@example.com|context@example.com"
        );
        std::fs::write(task.join("automatic.txt"), "automatic work\nmore\n").unwrap();
        let done =
            git::integrate_with_identity(&repo, &task, "task", "finish", "integration", &into, "main", &identity)
                .unwrap();
        assert!(done.committed);
        assert_eq!(done.merge, git::Merge::Merged);
        for path in [&task, &into] {
            assert_eq!(
                git::git(path, &["log", "-1", "--format=%ae|%ce"]).unwrap().trim(),
                "context@example.com|context@example.com"
            );
        }
        sync_worktree_identity(&task, &Identity::default()).unwrap();
        assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "git@example.com");
        assert_eq!(git::git(&repo, &["config", "user.name"]).unwrap().trim(), "Git Example");
        git::remove_worktree(&repo, &task, Some("task"), false).unwrap();
        git::add_worktree(&repo, &task, "fresh", "main").unwrap();
        assert!(!is_managed_worktree(&task));
        assert_eq!(git::git(&task, &["config", "user.email"]).unwrap().trim(), "git@example.com");
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_guard_blocks_before_writing_and_reports_locations_only() {
        use crate::git;
        let (dir, repo, identity) = project();
        let task = dir.path().join("task");
        let into = dir.path().join("integration");
        git::add_worktree(&repo, &task, "task", "main").unwrap();
        set_worktree_identity(&task, &identity).unwrap();
        let pending = git::integrate_with_identity(
            &repo,
            &task,
            "task",
            "CoNtOsO private message",
            "integration",
            &into,
            "main",
            &identity,
        )
        .unwrap_err()
        .to_string();
        assert!(pending.contains(".git/COMMIT_EDITMSG:1: forbidden word"), "{pending}");
        assert!(!pending.contains("private message"));
        std::fs::write(task.join("a.txt"), "one\ntwo\nprivate surrounding CoNtOsO text\n").unwrap();
        std::fs::write(task.join("untracked.txt"), "safe\nCoNtOsO surrounding text\n").unwrap();
        let head = git::git(&task, &["rev-parse", "HEAD"]).unwrap();
        let error =
            git::integrate_with_identity(&repo, &task, "task", "finish", "integration", &into, "main", &identity)
                .unwrap_err()
                .to_string();
        assert!(error.contains("a.txt:3: forbidden word"), "{error}");
        assert!(error.contains("untracked.txt:2: forbidden word"), "{error}");
        assert!(!error.contains("surrounding"));
        assert!(!error.to_lowercase().contains("contoso"));
        assert_eq!(git::git(&task, &["rev-parse", "HEAD"]).unwrap(), head);
        assert!(!git::branch_exists(&repo, "integration"));
        assert!(git::git(&task, &["diff", "--cached"]).unwrap().is_empty());
        // Commit it, then remove the word: additions in earlier commits still count.
        git::commit_all_with_identity(&task, "Safe subject\n\nCoNtOsO private body", &identity).unwrap();
        let commit = git::git(&task, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(task.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::remove_file(task.join("untracked.txt")).unwrap();
        git::commit_all_with_identity(&task, "remove", &identity).unwrap();
        let hits = check_task(&repo, "task", "main", &identity).unwrap();
        assert!(hits.iter().any(|hit| hit.file == "a.txt" && hit.line == 3));
        assert!(hits.iter().any(|hit| hit.file == format!(".git/objects/{}", commit.trim()) && hit.line > 5));
        assert!(
            git::integrate_with_identity(&repo, &task, "task", "finish", "integration", &into, "main", &identity)
                .is_err()
        );
    }

    #[test]
    #[cfg(feature = "native")]
    fn identity_guard_checks_raw_author_committer_and_branch() {
        use crate::git;
        let (dir, repo, identity) = project();
        let task = dir.path().join("task");
        let into = dir.path().join("integration");
        git::add_worktree(&repo, &task, "task-CoNtOsO", "main").unwrap();
        let wrong = Identity {
            name: Some("Contoso Example".into()),
            email: Some("other@example.com".into()),
            ..Default::default()
        };
        std::fs::write(task.join("safe.txt"), "safe\n").unwrap();
        git::commit_all_with_identity(&task, "safe\n\nauthor is prose", &wrong).unwrap();
        // A mailmap must not hide the raw author mismatch.
        std::fs::write(
            task.join(".mailmap"),
            "Context Example <context@example.com> Contoso Example <other@example.com>\n",
        )
        .unwrap();
        let hits = check_task(&repo, "task-CoNtOsO", "main", &identity).unwrap();
        assert!(hits.iter().any(|h| h.file == ".git/refs/heads/task-CoNtOsO" && h.line == 1));
        assert!(hits.iter().any(|h| h.reason == "author email differs from active identity" && h.line == 3));
        assert!(
            hits.iter().any(|h| h.reason == "forbidden word" && h.line == 4 && h.file.starts_with(".git/objects/"))
        );
        let error = git::integrate_with_identity(
            &repo,
            &task,
            "task-CoNtOsO",
            "finish",
            "integration",
            &into,
            "main",
            &identity,
        )
        .unwrap_err()
        .to_string();
        assert!(!error.contains("other@example.com"));
        assert!(!error.contains("Contoso Example"));
        let email_only = Identity { forbidden_words: Some(vec![]), ..identity };
        let hits = check_task(&repo, "task-CoNtOsO", "main", &email_only).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].reason, "author email differs from active identity");
        assert!(
            git::integrate_with_identity(
                &repo,
                &task,
                "task-CoNtOsO",
                "finish",
                "integration",
                &into,
                "main",
                &email_only
            )
            .is_err()
        );
    }
}
