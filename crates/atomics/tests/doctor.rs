#[path = "../../../tests/support.rs"]
mod test_support;

use std::os::unix::net::UnixListener;
use std::process::Command;

#[test]
fn setup_from_an_empty_machine_creates_config_and_gives_install_commands_without_secret_arguments() {
    let dir = test_support::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shell = dir.path().join("shell");
    test_support::write_program(&shell, "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
    // Linux's commands name the package manager this machine has.
    test_support::write_program(bin.join("apt-get"), "#!/bin/sh\nexit 0\n");
    let config = dir.path().join("config.toml");
    let output = test_support::output(
        Command::new(env!("CARGO_BIN_EXE_atomics"))
            .arg("setup")
            .env_clear()
            .env("HOME", dir.path())
            .env("SHELL", shell)
            .env("PATH", bin)
            .env("ATOMICS_HOME", dir.path().join("state"))
            .env("ATOMICS_CONFIG", &config)
            .env("ATOMICS_HOST", "fixture"),
    );
    assert!(!output.status.success(), "EOF stops setup instead of claiming success");
    assert!(config.exists());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for tool in ["git", "claude", "codex"] {
        assert!(stdout.contains(&format!("✗ prerequisite: {tool}")), "{stdout}");
    }
    assert!(stdout.contains(if cfg!(target_os = "macos") {
        "xcode-select --install"
    } else {
        "sudo apt-get install git"
    }));
    // On Linux, ATOMICS's credential relay reads the Claude subscription with secret-tool; macOS has `security`.
    if cfg!(target_os = "macos") {
        assert!(!stdout.contains("secret-tool") && !stdout.contains("libsecret"), "{stdout}");
    } else {
        assert!(stdout.contains("✗ prerequisite: secret-tool"), "{stdout}");
        assert!(stdout.contains("sudo apt-get install libsecret-tools gnome-keyring"), "{stdout}");
    }
    assert!(String::from_utf8(output.stderr).unwrap().contains("rerun `atomics setup` to resume"));
}

#[test]
fn doctor_checks_real_inputs_and_only_requires_git_and_valid_config() {
    let dir = crate::test_support::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shell = dir.path().join("shell");
    test_support::write_program(&shell, "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
    let git = bin.join("git");
    let config = dir.path().join("config.toml");
    let socket = dir.path().join("atomics.sock");
    let run = || {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .arg("doctor")
                .env_clear()
                .env("HOME", dir.path())
                .env("SHELL", &shell)
                .env("PATH", &bin)
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", &config)
                .env("ATOMICS_HOST", "fixture")
                .env("ATOMICS_SOCKET", &socket),
        )
    };
    std::fs::write(&config, "").unwrap();
    for (probe, expected) in [("echo 'git version fixture'", 0), ("exit 1", 1), ("exit 0", 1)] {
        test_support::write_program(&git, format!("#!/bin/sh\n{probe}\n"));
        let output = run();
        assert_eq!(output.status.code(), Some(expected));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("✗ claude not found"), "{text}");
        assert!(text.contains("✗ ATOMICS not running (warning)"), "{text}");
        assert!(text.contains("✗ global Claude hooks not installed (warning)"), "{text}");
        assert!(!text.contains("✗ ATOMICS running") && !text.contains("✗ global Claude hooks installed"), "{text}");
        if expected == 0 {
            assert!(text.contains("✓ git found (git version fixture)"), "{text}");
            assert!(text.contains("✓ config parses"), "{text}");
            assert!(text.contains("Host: fixture"), "{text}");
            // Sandbox mode is the default; a missing sbx is a warning, not a failed doctor.
            assert!(
                text.contains("✗ Docker Sandboxes not ready for sandboxed agents (warning) — Docker Sandboxes (sbx) is not installed"),
                "{text}"
            );
            assert!(text.contains(&format!("Loaded config: {}", config.display())), "{text}");
            assert_eq!(text.matches("Loaded config:").count(), 1, "{text}");
        } else {
            assert!(text.contains("✗ git not found"), "{text}");
        }
    }
    std::fs::remove_file(&git).unwrap();
    assert_eq!(run().status.code(), Some(1), "a missing git executable fails doctor");
    test_support::write_program(&git, "#!/bin/sh\necho 'git version fixture'\n");
    let host_config = dir.path().join("config.d/fixture.toml");
    std::fs::create_dir_all(host_config.parent().unwrap()).unwrap();
    std::fs::write(&host_config, "[font]\nsize = 17.0").unwrap();
    let output = run();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains(&format!("Loaded config: {}", host_config.display())), "{text}");
    assert_eq!(text.matches("Loaded config:").count(), 2, "{text}");
    std::fs::write(&host_config, "[font]\nsize = 'invalid'").unwrap();
    let output = run();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("✗ config failed:"), "{text}");
    assert!(text.contains(&host_config.display().to_string()), "{text}");
    std::fs::remove_file(host_config).unwrap();
    std::fs::write(&config, "[invalid").unwrap();
    let output = run();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stdout).unwrap().contains("✗ config failed:"));
    std::fs::write(&config, "").unwrap();
    test_support::write_program(
        &git,
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'git version fixture'; else exit 1; fi\n",
    );
    let repo_config = dir.path().join(".atomics/config.toml");
    std::fs::create_dir(repo_config.parent().unwrap()).unwrap();
    std::fs::write(
        &repo_config,
        "base_ref = 'main'\n[approvals]\nallow = ['*']\n[tickets.server]\ncommand = ['must-never-run']\n",
    )
    .unwrap();
    std::fs::write(&config, format!("repos = [{:?}]\n", dir.path().display().to_string())).unwrap();
    let output = run();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("repository workflow (warning)"), "{text}");
    assert!(text.contains("ignored approvals"), "{text}");
    assert!(text.contains("ignored tickets.server"), "{text}");
    assert!(!text.contains("must-never-run"));
    let _listener = UnixListener::bind(&socket).unwrap();
    let output = run();
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("✓ ATOMICS running"));
}

#[test]
fn doctor_says_where_path_came_from_and_reads_the_codex_version_with_the_login_shells() {
    let dir = crate::test_support::tempdir().unwrap();
    let started = [dir.path().join("started-a"), dir.path().join("started-b")];
    for folder in &started {
        std::fs::create_dir(folder).unwrap();
    }
    let initial_path = std::env::join_paths(&started).unwrap();
    let tools = dir.path().join("login-tools");
    std::fs::create_dir(&tools).unwrap();
    test_support::write_program(tools.join("git"), "#!/bin/sh\necho 'git version fixture'\n");
    // Codex is a script for an interpreter, as Node runs it, that only the login shell's PATH has.
    test_support::write_program(tools.join("atomics-test-node"), "#!/bin/sh\nexec /bin/sh \"$@\"\n");
    test_support::write_program(
        tools.join("codex"),
        "#!/usr/bin/env atomics-test-node\nprintf 'codex-cli 0.165.0\\n'\n",
    );
    let shell = dir.path().join("shell");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").unwrap();
    let run = |script: &str| {
        test_support::write_program(&shell, format!("#!/bin/sh\n{script}\n"));
        let output = test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .arg("doctor")
                .env_clear()
                .env("HOME", dir.path())
                .env("SHELL", &shell)
                // As a desktop launcher starts it.
                .env("PATH", &initial_path)
                .env("ATOMICS_HOME", dir.path().join("state"))
                .env("ATOMICS_CONFIG", &config)
                .env("ATOMICS_HOST", "fixture")
                .env("ATOMICS_SOCKET", dir.path().join("atomics.sock")),
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let login_path = std::env::join_paths(std::iter::once(&tools).chain(started.iter())).unwrap();
    let text = run(&format!("printf '__ATOMICS__{}__ATOMICS__'", login_path.to_string_lossy()));
    let added =
        format!("✓ PATH of login shell {}: 1 folder added to the PATH ATOMICS was started with", shell.display());
    assert!(text.lines().next().is_some_and(|line| line == added), "{text}");
    assert!(text.contains("✓ git found (git version fixture)"), "{text}");
    assert!(text.contains("✓ Codex supports ATOMICS hooks"), "{text}");
    let text = run("exit 0");
    let unread =
        format!("✗ PATH as ATOMICS was started: login shell {} gave none, it printed no PATH", shell.display());
    assert!(text.contains(&unread) && text.contains("(warning) — make your login shell"), "{text}");
    assert!(!text.contains("Codex supports ATOMICS hooks"), "{text}");
}
