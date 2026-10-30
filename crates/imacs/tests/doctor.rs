#[path = "../../../tests/support.rs"]
mod test_support;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::Command;

#[test]
fn setup_from_an_empty_machine_creates_config_and_gives_install_commands_without_secret_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shell = dir.path().join("shell");
    std::fs::write(&shell, "#!/bin/sh\nprintf '__IMACS__%s__IMACS__' \"$PATH\"\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.path().join("config.toml");
    let output = test_support::output(
        Command::new(env!("CARGO_BIN_EXE_imacs"))
            .arg("setup")
            .env_clear()
            .env("HOME", dir.path())
            .env("SHELL", shell)
            .env("PATH", bin)
            .env("IMACS_HOME", dir.path().join("state"))
            .env("IMACS_CONFIG", &config)
            .env("IMACS_HOST", "fixture"),
    );
    assert!(!output.status.success(), "EOF stops setup instead of claiming success");
    assert!(config.exists());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for tool in ["git", "claude", "codex"] {
        assert!(stdout.contains(&format!("✗ sandbox prerequisite: {tool}")), "{stdout}");
    }
    assert!(stdout.contains(if cfg!(target_os = "macos") { "xcode-select --install" } else { "libsecret-tools" }));
    assert!(String::from_utf8(output.stderr).unwrap().contains("rerun `imacs setup` to resume"));
}

#[test]
fn doctor_checks_real_inputs_and_only_requires_git_and_valid_config() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let shell = dir.path().join("shell");
    std::fs::write(&shell, "#!/bin/sh\nprintf '__IMACS__%s__IMACS__' \"$PATH\"\n").unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    let git = bin.join("git");
    let config = dir.path().join("config.toml");
    let socket = dir.path().join("imacs.sock");
    let run = || {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_imacs"))
                .arg("doctor")
                .env_clear()
                .env("HOME", dir.path())
                .env("SHELL", &shell)
                .env("PATH", &bin)
                .env("IMACS_HOME", dir.path().join("state"))
                .env("IMACS_CONFIG", &config)
                .env("IMACS_HOST", "fixture")
                .env("IMACS_SOCKET", &socket),
        )
    };
    std::fs::write(&config, "").unwrap();
    for (probe, expected) in [("echo 'git version fixture'", 0), ("exit 1", 1), ("exit 0", 1)] {
        std::fs::write(&git, format!("#!/bin/sh\n{probe}\n")).unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = run();
        assert_eq!(output.status.code(), Some(expected));
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("✗ claude not found"), "{text}");
        assert!(text.contains("✗ IMACS running"), "{text}");
        assert!(text.contains("✗ global Claude hooks installed"), "{text}");
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
    std::fs::write(&git, "#!/bin/sh\necho 'git version fixture'\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
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
    std::fs::write(&git, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'git version fixture'; else exit 1; fi\n")
        .unwrap();
    let repo_config = dir.path().join(".imacs/config.toml");
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
    assert!(String::from_utf8(output.stdout).unwrap().contains("✓ IMACS running"));
}
