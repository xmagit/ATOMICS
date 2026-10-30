//! Build a local sbx snapshot with Linux toolchains and dependency caches.

use super::*;
use imacs_core::{config::Config, repo_workflow::RepoWorkflow, workspace::Workspace};

/// Resolve a project path, or a project's basename in the selected workspace.
pub fn run(project: &str, workspace_name: Option<&str>, agent: &str, dotnet_project: Option<&str>) -> Result<()> {
    let mut config = Config::load(&paths::config_file())?;
    let saved = workspace_name.map(Workspace::load).transpose()?;
    if let Some(overrides) = saved.as_ref().map(Workspace::isolation).transpose()?.flatten() {
        config.isolation = config.isolation.with_overrides(&overrides)?;
    }
    let path = paths::expand(project);
    let path = if path.is_dir() {
        path
    } else {
        saved
            .as_ref()
            .and_then(|w| w.projects.iter().find(|p| p.file_name().is_some_and(|n| n == project)))
            .cloned()
            .with_context(|| format!("project {project:?} is not a directory or a project in --workspace"))?
    };
    // Build the selected checkout, including its tracked edits; policy comes from the main checkout.
    let (cwd, _) = super::workspace(&path)?;
    let root = workspace::project_root(&cwd)?;
    config = RepoWorkflow::read(&root).apply(&config);
    let config = config.isolation.sandbox.with_defaults(&config.resources, std::thread::available_parallelism()?.get());
    let program = crate::setup::resolve_program("sbx");
    build(&program, &cwd, agent, dotnet_project, &config)?;
    println!(
        "Saved {}. New sandboxed {agent} agents in this project use this template.",
        config.template.as_deref().unwrap()
    );
    Ok(())
}

pub(super) fn build(
    program: &str,
    cwd: &Path,
    agent: &str,
    dotnet_project: Option<&str>,
    config: &SandboxConfig,
) -> Result<()> {
    let tag = config
        .template
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .context("set [isolation.sandbox] template in the workspace or .imacs/config.toml before building")?;
    let kind = if cwd.join("Cargo.toml").is_file() { "rust" } else { "dotnet" };
    let project = dotnet_project.unwrap_or("soa-src/Cosmoz3.Shared/Cosmoz3.Contracts/Cosmoz3.Contracts.csproj");
    if kind == "dotnet"
        && (Path::new(project).extension().is_none_or(|ext| ext != "csproj")
            || !Path::new(project).components().all(|part| matches!(part, std::path::Component::Normal(_)))
            || !cwd.join(project).is_file())
    {
        bail!(
            "no Rust workspace or .NET project at {project}; pass --dotnet-project with a repository-relative .csproj"
        );
    }
    let agent = match agent {
        "codex" => "codex",
        "claude" => "claude",
        "shell" => "shell",
        _ => bail!("template agent must be codex, claude or shell"),
    };
    let ready = preflight(program).map_err(anyhow::Error::msg)?;
    if !ready.diagnosis.policy_initialized() {
        sbx(program, &["policy", "init", config.network.name()], false, TIMEOUT)?;
    }
    let name = format!("imacs-template-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let (root, git) = super::workspace(cwd)?;
    // sbx requires a writable primary workspace. Give it an empty disposable folder;
    // the selected checkout and git metadata remain read-only secondary mounts.
    let scratch = tempfile::Builder::new().prefix("imacs-template-").tempdir()?;
    let spec = Spec {
        name: name.clone(),
        agent,
        // The preparation script writes only to the sandbox filesystem.
        workspace: scratch.path().to_path_buf(),
        git: git.into_iter().map(|p| if p.ends_with(":ro") { p } else { p + ":ro" }).collect(),
        warm: Vec::new(),
        read_only: vec![root.clone()],
        memory: config.memory.clone(),
        cpus: config.cpus,
        // Start from the agent image, never a stale snapshot of the template being rebuilt.
        template: None,
    };
    eprintln!("Preparing {tag} in {name} (this may take several minutes)…");
    sbx(program, &isolation::create_args(&spec), false, CREATE_TIMEOUT).inspect_err(|_| {
        let _ = sbx(program, &["rm", "--force", &name], false, TIMEOUT);
    })?;
    let result = (|| -> Result<()> {
        let hosts = config.hosts_for(&real(&workspace::project_root(cwd)?), |p| real(&paths::expand(p)));
        if !hosts.is_empty() {
            sbx(program, &isolation::allow_hosts_args(&name, &hosts), false, TIMEOUT)?;
        }
        let jobs = config.cpus.unwrap_or(2).max(1).to_string();
        let args = vec![
            "exec".into(),
            name.clone(),
            "bash".into(),
            "-lc".into(),
            include_str!("template.sh").into(),
            "imacs-template".into(),
            root.display().to_string(),
            kind.into(),
            project.into(),
            jobs,
        ];
        let output = sbx(program, &args, false, Duration::from_secs(60 * 30))
            .context("warming template toolchains and dependencies")?;
        print!("{output}");
        // Saving a running sandbox prompts to stop it, which fails on our null stdin.
        stop(program, &name).context("stopping the template sandbox before saving")?;
        sbx(program, &["template", "save", &name, tag], false, CREATE_TIMEOUT).context("saving the template")?;
        Ok(())
    })();
    let cleanup = sbx(program, &["rm", "--force", &name], false, TIMEOUT);
    match (result, cleanup) {
        (result, Ok(_)) => result,
        (Ok(()), Err(error)) => Err(error.context(format!("removing temporary template sandbox {name}"))),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("also failed to remove temporary template sandbox {name}: {cleanup:#}")))
        }
    }
}

static NEXT: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_fresh_snapshot_with_private_caches_and_uses_it_for_agents() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake::install(dir.path()).display().to_string();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git::git(&repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
        let config = SandboxConfig {
            template: Some("imacs-cox:local".into()),
            cpus: Some(2),
            memory: Some("4g".into()),
            ..Default::default()
        };
        build(&program, &repo, "codex", None, &config).unwrap();
        let calls = fake::calls(dir.path());
        let create = calls.iter().find(|c| c[0] == "create").unwrap();
        assert!(!create.iter().any(|a| a == "--template"), "a rebuild starts from the agent's image");
        assert!(create.contains(&format!("{}:ro", repo.canonicalize().unwrap().display())));
        let primary = Path::new(&create[create.iter().position(|a| a == "codex").unwrap() + 1]);
        assert!(!primary.to_string_lossy().ends_with(":ro"));
        assert!(!primary.exists(), "the disposable primary workspace is removed");
        let name = &create[2];
        let exec = calls.iter().find(|c| c[0] == "exec").unwrap();
        assert_eq!(&exec[..4], ["exec", name, "bash", "-lc"]);
        assert!(exec[4].contains("cargo check -p imacs-core"));
        assert_eq!(&exec[7..], ["rust", "soa-src/Cosmoz3.Shared/Cosmoz3.Contracts/Cosmoz3.Contracts.csproj", "2"]);
        assert!(calls.contains(&vec!["template".into(), "save".into(), name.clone(), "imacs-cox:local".into()]));
        assert_eq!(calls.last().unwrap(), &["rm", "--force", name]);
        let job = Prepare {
            sbx: program,
            name: "imacs-agent".into(),
            agent: "codex",
            cwd: repo,
            config,
            warm: Vec::new(),
            exe: "/imacs".into(),
            socket: "/imacs.sock".into(),
            briefs: None,
        };
        prepare(&job).unwrap();
        assert!(fake::calls_to(dir.path(), &["create"])[1].windows(2).any(|a| a == ["--template", "imacs-cox:local"]));
    }

    #[test]
    fn failed_warming_or_save_removes_the_builder_and_never_reports_success() {
        for verb in ["create", "exec", "stop", "template"] {
            let dir = tempfile::tempdir().unwrap();
            let program = fake::install(dir.path()).display().to_string();
            git::git(dir.path(), &["init", "-q"]).unwrap();
            std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
            std::fs::write(dir.path().join(format!("fail-{verb}")), "").unwrap();
            let config = SandboxConfig { template: Some("imacs-test:local".into()), ..Default::default() };
            let error = build(&program, dir.path(), "codex", None, &config).unwrap_err();
            assert!(format!("{error:#}").contains(&format!("fake {verb} failure")));
            assert!(list(&program).unwrap().is_empty());
            if verb != "template" {
                assert!(fake::calls_to(dir.path(), &["template"]).is_empty());
            }
        }
    }

    #[test]
    fn a_cleanup_failure_identifies_the_sandbox_and_preserves_the_build_error() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake::install(dir.path()).display().to_string();
        git::git(dir.path(), &["init", "-q"]).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        for verb in ["exec", "rm"] {
            std::fs::write(dir.path().join(format!("fail-{verb}")), "").unwrap();
        }
        let config = SandboxConfig { template: Some("imacs-test:local".into()), ..Default::default() };
        let error = build(&program, dir.path(), "codex", None, &config).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("fake exec failure") && message.contains("fake rm failure"), "{message}");
        let name = &fake::calls_to(dir.path(), &["create"])[0][2];
        assert!(message.contains(name), "tell the user which sandbox needs removing");
    }

    #[test]
    fn dotnet_build_uses_the_selected_project_and_requires_a_template_tag() {
        let dir = tempfile::tempdir().unwrap();
        let program = fake::install(dir.path()).display().to_string();
        git::git(dir.path(), &["init", "-q"]).unwrap();
        std::fs::write(dir.path().join("App.csproj"), "<Project />").unwrap();
        let config = SandboxConfig { template: Some("imacs-dotnet:local".into()), ..Default::default() };
        build(&program, dir.path(), "claude", Some("App.csproj"), &config).unwrap();
        let exec = &fake::calls_to(dir.path(), &["exec"])[0];
        assert_eq!(&exec[7..], ["dotnet", "App.csproj", "2"]);
        assert!(build(&program, dir.path(), "codex", Some("App.csproj"), &SandboxConfig::default()).is_err());
    }

    #[test]
    fn preparation_stages_tracked_edits_and_removes_source_even_when_the_build_fails() {
        use std::os::unix::fs::PermissionsExt;
        for succeeds in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            let home = dir.path().join("home");
            let bin = dir.path().join("bin");
            for path in [&repo, &home, &bin] {
                std::fs::create_dir(path).unwrap();
            }
            git::git(&repo, &["init", "-q"]).unwrap();
            for file in ["tracked", "deleted"] {
                std::fs::write(repo.join(file), "original").unwrap();
            }
            git::git(&repo, &["add", "."]).unwrap();
            std::fs::write(repo.join("tracked"), "edited").unwrap();
            std::fs::remove_file(repo.join("deleted")).unwrap();
            std::fs::write(repo.join("credentials"), "private").unwrap();
            for (name, script) in [
                ("rustup", "#!/bin/sh\nexit 0\n".to_string()),
                (
                    "cargo",
                    format!(
                        "#!/bin/sh\nset -eu\ntest \"$(cat tracked)\" = edited\ntest ! -e deleted\ntest ! -e credentials\npwd > \"$HOME/stage\"\ncp \"$HOME/.cargo/config.toml\" \"$HOME/saved-config\"\nexit {}\n",
                        if succeeds { 0 } else { 17 }
                    ),
                ),
            ] {
                let path = bin.join(name);
                std::fs::write(&path, script).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let output = Command::new("bash")
                .args(["-c", include_str!("template.sh"), "imacs-template"])
                .arg(&repo)
                .args(["rust", "", "2"])
                .env("HOME", &home)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(if succeeds { 0 } else { 17 }), "{output:?}");
            let staged = std::fs::read_to_string(home.join("stage")).unwrap();
            assert!(!Path::new(staged.trim()).parent().unwrap().exists(), "source staging must be removed");
            let config = std::fs::read_to_string(home.join("saved-config")).unwrap();
            assert!(config.contains("/.cache/imacs/target") && config.contains("jobs = 2"));
            assert_eq!(std::fs::read_to_string(repo.join("tracked")).unwrap(), "edited");
        }
    }

    #[test]
    fn dotnet_preparation_runs_the_installer_in_bash_with_the_sdk_pin_or_fallback() {
        use std::os::unix::fs::PermissionsExt;
        for pinned in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path().join("repo");
            let home = dir.path().join("home");
            let bin = dir.path().join("bin");
            for path in [&repo, &home, &bin] {
                std::fs::create_dir(path).unwrap();
            }
            git::git(&repo, &["init", "-q"]).unwrap();
            std::fs::write(repo.join("App.csproj"), "<Project />").unwrap();
            if pinned {
                std::fs::write(repo.join("global.json"), r#"{"sdk":{"version":"8.0.414"}}"#).unwrap();
            }
            git::git(&repo, &["add", "."]).unwrap();
            for (name, script) in [
                (
                    "curl",
                    r##"#!/bin/sh
set -eu
while [ "$1" != -o ]; do shift; done
cat > "$2" <<'INSTALLER'
set -eu
test -n "${BASH_VERSION:?installer requires Bash}"
printf '%s\n' "$@" > "$HOME/install-args"
INSTALLER
"##,
                ),
                ("sudo", "#!/bin/sh\nexit 0\n"),
                (
                    "dotnet",
                    r##"#!/bin/sh
set -eu
test "$DOTNET_ROOT" = "$HOME/.dotnet"
test "$DOTNET_PROCESSOR_COUNT" = 2
test "$MSBUILDDISABLENODEREUSE" = 1
test "$DOTNET_CLI_USE_MSBUILD_SERVER" = 0
test -f App.csproj
printf '%s\n' "$@" > "$HOME/build-args"
"##,
                ),
            ] {
                let path = bin.join(name);
                std::fs::write(&path, script).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let output = Command::new("bash")
                .args(["-c", include_str!("template.sh"), "imacs-template"])
                .arg(&repo)
                .args(["dotnet", "App.csproj", "2"])
                .env("HOME", &home)
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            let args = std::fs::read_to_string(home.join("install-args")).unwrap();
            assert!(args.starts_with(if pinned { "--jsonfile\nglobal.json\n" } else { "--channel\n8.0\n" }));
            assert_eq!(
                std::fs::read_to_string(home.join("build-args")).unwrap(),
                "build\nApp.csproj\n--disable-build-servers\n-m:1\n-p:UseSharedCompilation=false\n"
            );
        }
    }
}

#[cfg(test)]
mod cli_tests {
    use clap::Parser;

    #[test]
    fn parses_the_documented_template_command() {
        let cli = crate::Cli::try_parse_from([
            "imacs",
            "--workspace",
            "backend",
            "sandbox",
            "template",
            "build",
            "cz3backend",
            "--agent",
            "claude",
            "--dotnet-project",
            "App.csproj",
        ])
        .unwrap();
        assert_eq!(cli.workspace.as_deref(), Some("backend"));
        let Some(crate::Command::Sandbox {
            command:
                crate::SandboxCommand::Template {
                    command: crate::TemplateCommand::Build { project, agent, dotnet_project },
                },
        }) = cli.command
        else {
            panic!("template build command")
        };
        assert_eq!(project, "cz3backend");
        assert_eq!(agent, "claude");
        assert_eq!(dotnet_project.as_deref(), Some("App.csproj"));
    }
}
