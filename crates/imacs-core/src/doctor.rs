//! Read-only diagnostics shared by the doctor command and its tests.

use std::path::Path;

use crate::{approvals::ApprovalPolicy, git, plan::Plan};

pub fn codex_checks(supports_hooks: bool) -> [String; 2] {
    [
        check_line(
            supports_hooks,
            "Codex supports IMACS hooks (0.165+ required)",
            "install Codex 0.165 or newer; check with `codex --version`",
        ),
        check_line(
            false,
            "IMACS Codex hook trust not verified (warning)",
            "start Codex in IMACS and open /hooks to review and trust IMACS hooks; this is a one-time step per install, repeated when definitions change; repository hooks keep their own trust review",
        ),
    ]
}

pub fn approvals_check(policy: &ApprovalPolicy) -> String {
    let cargo: Vec<_> = policy
        .allow
        .iter()
        .filter(|entry| {
            shlex::split(entry)
                .and_then(|words| words.into_iter().next())
                .is_some_and(|program| Path::new(&program).file_name().is_some_and(|name| name == "cargo"))
        })
        .map(String::as_str)
        .collect();
    let label = if cargo.is_empty() {
        format!("[approvals] allow list: {} entries, no cargo entries", policy.allow.len())
    } else {
        format!("[approvals] allow list contains cargo entries (warning): {}", cargo.join(", "))
    };
    check_line(
        cargo.is_empty(),
        &label,
        "remove cargo entries from [approvals].allow unless intended; approved commands run outside the sandbox",
    )
}

pub fn plans_check(home: &Path) -> Vec<String> {
    let dir = home.join("plans");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return vec![check_line(true, &format!("no plans yet ({})", dir.display()), "")];
        }
        Err(err) => {
            return vec![check_line(
                false,
                &format!("cannot read plans directory {}: {err}", dir.display()),
                "fix the plans directory's path and read permissions",
            )];
        }
    };
    let mut errors = Vec::new();
    for entry in entries {
        let result = entry.map_err(anyhow::Error::from).and_then(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return Ok(());
            }
            Plan::load(&path).map(|_| ()).map_err(|err| anyhow::anyhow!("{}: {err:#}", path.display()))
        });
        if let Err(err) = result {
            errors.push(check_line(
                false,
                &format!("unreadable plan (warning): {err:#}"),
                "fix this file's read permissions or JSON; restore it from a backup if damaged",
            ));
        }
    }
    errors.sort();
    if errors.is_empty() {
        errors.push(check_line(true, &format!("plan files readable ({})", dir.display()), ""));
    }
    errors
}

pub fn worktree_checks(repo: &Path, base_ref: &str) -> Vec<String> {
    match git::worktree_health(repo, base_ref) {
        Ok(health) => {
            let candidates: Vec<_> = health.iter().filter(|tree| tree.merged && tree.is_safe_to_remove()).collect();
            if candidates.is_empty() {
                return vec![check_line(true, &format!("no merged worktrees to clean up ({})", repo.display()), "")];
            }
            candidates
                .iter()
                .map(|tree| {
                    check_line(
                        false,
                        &format!(
                            "merged worktree can be cleaned up (warning): {} ({})",
                            tree.path.display(),
                            tree.branch.as_deref().unwrap_or("detached")
                        ),
                        "review it in IMACS's Clean up… dialog before removing the worktree",
                    )
                })
                .collect()
        }
        Err(err) => vec![check_line(
            false,
            &format!("cannot check worktrees for {}: {err:#}", repo.display()),
            "check repository access and the configured base_ref",
        )],
    }
}

pub fn check_line(passed: bool, label: &str, hint: &str) -> String {
    let line = if passed { format!("✓ {label}") } else { format!("✗ {label} — {hint}") };
    line.lines().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_version_passes_or_warns_and_trust_is_explicitly_unverified() {
        for supported in [true, false] {
            let [version, trust] = codex_checks(supported);
            assert_eq!(version.starts_with('✓'), supported);
            assert_eq!(version.contains("install Codex 0.165"), !supported);
            assert!(trust.starts_with('✗'));
            assert!(trust.contains("trust not verified"));
            assert!(trust.contains("start Codex in IMACS and open /hooks"));
            assert!(trust.contains("one-time step per install"));
            assert!(trust.contains("repository hooks keep their own trust review"));
            assert_eq!(version.lines().count(), 1);
            assert_eq!(trust.lines().count(), 1);
        }
    }

    #[test]
    fn approval_list_warns_only_for_cargo_executables() {
        assert!(approvals_check(&ApprovalPolicy::default()).starts_with('✓'));
        for entry in ["cargo", "cargo test", " cargo clippy ", "'/tools with spaces/cargo' build"] {
            let line = approvals_check(&ApprovalPolicy { allow: vec![entry.into()] });
            assert!(line.starts_with('✗'), "{entry}");
            assert!(line.contains("outside the sandbox"));
            assert!(line.contains("remove cargo entries"));
            assert_eq!(line.lines().count(), 1);
        }
        for entry in ["cargo-nextest", "echo cargo", "rg cargo"] {
            assert!(approvals_check(&ApprovalPolicy { allow: vec![entry.into()] }).starts_with('✓'));
        }
    }

    #[test]
    fn plan_checks_are_read_only_and_report_bad_json_and_unreadable_files() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        assert!(plans_check(home.path())[0].starts_with('✓'));
        assert!(!home.path().join("plans").exists());
        let dir = home.path().join("plans");
        std::fs::create_dir(&dir).unwrap();
        Plan::default().save(&dir.join("good.json")).unwrap();
        std::fs::write(home.path().join("plan.json"), "legacy plan must not be migrated").unwrap();
        assert!(plans_check(home.path())[0].starts_with('✓'));

        std::fs::write(dir.join("bad.json"), "not JSON\n").unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("unreadable.json")).unwrap();
        Plan::default().save(&dir.join("denied.json")).unwrap();
        std::fs::set_permissions(dir.join("denied.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let lines = plans_check(home.path());
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|line| line.contains("bad.json")));
        assert!(lines.iter().any(|line| line.contains("unreadable.json")));
        assert!(lines.iter().any(|line| line.contains("denied.json")));
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with('✗') && line.contains("read permissions") && line.lines().count() == 1)
        );
        assert!(home.path().join("plan.json").exists());
        assert!(!home.path().join("plan.json.migrated").exists());
        assert_eq!(std::fs::read_to_string(dir.join("bad.json")).unwrap(), "not JSON\n");
    }

    #[test]
    fn inaccessible_plans_directory_warns() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("plans"), "not a directory").unwrap();
        let lines = plans_check(home.path());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains("cannot read plans directory"));
        assert_eq!(lines[0].lines().count(), 1);
    }

    #[test]
    fn only_merged_clean_worktrees_are_reported_for_cleanup() {
        let (dir, repo) = crate::git::tests::project();
        assert!(worktree_checks(&repo, "main")[0].starts_with('✓'));
        let done = dir.path().join("done");
        let dirty = dir.path().join("dirty");
        let unmerged = dir.path().join("unmerged");
        for (path, branch) in [(&done, "done"), (&dirty, "dirty"), (&unmerged, "unmerged")] {
            git::add_worktree(&repo, path, branch, "main").unwrap();
        }
        std::fs::write(dirty.join("wip"), "keep me").unwrap();
        git::git(
            &unmerged,
            &["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "unmerged"],
        )
        .unwrap();
        let lines = worktree_checks(&repo, "main");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains(&done.display().to_string()));
        assert!(lines[0].contains("Clean up…"));
        assert_eq!(lines[0].lines().count(), 1);
        assert!(done.exists() && dirty.exists() && unmerged.exists());
    }

    #[test]
    fn worktree_scan_failure_warns_instead_of_claiming_no_cleanup_needed() {
        let dir = tempfile::tempdir().unwrap();
        let lines = worktree_checks(dir.path(), "main");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains("check repository access"));
        assert_eq!(lines[0].lines().count(), 1);
    }
}
