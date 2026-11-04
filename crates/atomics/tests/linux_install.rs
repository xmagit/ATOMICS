#![cfg(target_os = "linux")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn script(path: &Path, text: &str) {
    test_support::write_program(path, text);
}

#[test]
fn fresh_install_and_update_run_installed_setup_with_stdin_and_keep_install_on_interruption() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("fresh home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    script(
        &target.join("fixture-linux-native/release/atomics"),
        "#!/bin/sh\n[ \"$1\" = setup ] || exit 10\nread -r answer || exit 11\nprintf '%s:%s\\n' \"$0\" \"$answer\" >> \"$SETUP_LOG\"\n[ \"$answer\" != interrupt ]\n",
    );
    let log = dir.path().join("setup.log");
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let mut inode = None;
    for answer in ["fresh", "update", "interrupt"] {
        let mut child = test_support::TestChild::spawn(
            Command::new("sh")
                .arg(&installer)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", &path)
                .env("SETUP_LOG", &log)
                .stdin(Stdio::piped()),
            test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        writeln!(child.child.stdin.take().unwrap(), "{answer}").unwrap();
        let output = child.wait_output().unwrap();
        assert_eq!(output.status.success(), answer != "interrupt", "{output:?}");
        let installed = home.join(".local/bin/atomics");
        let next_inode = installed.metadata().unwrap().ino();
        assert_ne!(Some(next_inode), inode, "updates must replace the inode atomically");
        inode = Some(next_inode);
        assert!(home.join("data/applications/atomics.desktop").exists());
        assert!(home.join("data/icons/hicolor/512x512/apps/atomics.png").exists());
        assert!(std::fs::read_to_string(&log).unwrap().contains(&format!("{}:{answer}", installed.display())));
    }
    let output = test_support::output(
        Command::new("sh").arg(installer).arg("--uninstall").env("HOME", &home).env("XDG_DATA_HOME", home.join("data")),
    );
    assert!(output.status.success());
    assert!(!home.join(".local/bin/atomics").exists());
    assert_eq!(std::fs::read_to_string(log).unwrap().lines().count(), 3, "uninstall must not run setup");
}

#[test]
fn installer_reaches_real_setup_from_a_fresh_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    script(&bin.join("shell"), "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_atomics"), target.join("fixture-linux-native/release/atomics"))
        .unwrap();
    // PATH holds stubs and the installer's own tools only: what else the host has, such as
    // secret-tool, must not change what setup reports.
    let system = dir.path().join("system");
    std::fs::create_dir_all(&system).unwrap();
    for tool in ["sh", "uname", "dirname", "sed", "mkdir", "mktemp", "install", "mv", "rm", "chmod", "cat"] {
        let found = ["/usr/bin", "/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|path| path.exists())
            .unwrap_or_else(|| panic!("the installer needs {tool}"));
        std::os::unix::fs::symlink(found, system.join(tool)).unwrap();
    }
    let config = home.join("config.toml");
    let output = test_support::output(
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh"))
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("CARGO_TARGET_DIR", target)
            .env("PATH", format!("{}:{}", bin.display(), system.display()))
            .env("SHELL", bin.join("shell"))
            .env("ATOMICS_HOME", home.join("state"))
            .env("ATOMICS_CONFIG", &config),
    );
    assert!(!output.status.success(), "EOF must leave setup resumable");
    assert!(config.exists(), "the real setup creates missing configuration");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ATOMICS fresh-machine setup"), "{stdout}");
    assert!(stdout.contains("Run the suggested commands in another host terminal"), "{stdout}");
    // ATOMICS's credential relay reads the Claude subscription with secret-tool.
    assert!(stdout.contains("✗ sandbox prerequisite: secret-tool"), "{stdout}");
    assert!(stdout.contains("sudo apt-get install libsecret-tools gnome-keyring"), "{stdout}");
    assert!(home.join(".local/bin/atomics").exists(), "installation survives interrupted setup");
}

#[test]
fn installer_registers_the_atomics_link_scheme() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    script(&bin.join("xdg-mime"), "#!/bin/sh\necho \"$@\" >> \"$MIME_LOG\"\n");
    script(&target.join("fixture-linux-native/release/atomics"), "#!/bin/sh\nexit 0\n");
    let log = dir.path().join("mime.log");
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let run = |args: &[&str]| {
        test_support::output(
            Command::new("sh")
                .arg(&installer)
                .args(args)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
                .env("MIME_LOG", &log),
        )
    };
    assert!(run(&[]).status.success());
    let entry = std::fs::read_to_string(home.join("data/applications/atomics.desktop")).unwrap();
    assert!(entry.contains("MimeType=x-scheme-handler/atomics;"), "{entry}");
    let exec = entry.lines().find(|line| line.starts_with("Exec=")).unwrap();
    assert!(exec.ends_with(" %u"), "{exec}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "default atomics.desktop x-scheme-handler/atomics");
    // Uninstalling registers nothing.
    assert!(run(&["--uninstall"]).status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}
