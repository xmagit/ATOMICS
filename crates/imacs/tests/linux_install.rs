#![cfg(target_os = "linux")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        &target.join("fixture-linux-native/release/imacs"),
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
        let installed = home.join(".local/bin/imacs");
        let next_inode = installed.metadata().unwrap().ino();
        assert_ne!(Some(next_inode), inode, "updates must replace the inode atomically");
        inode = Some(next_inode);
        assert!(home.join("data/applications/imacs.desktop").exists());
        assert!(home.join("data/icons/hicolor/512x512/apps/imacs.png").exists());
        assert!(std::fs::read_to_string(&log).unwrap().contains(&format!("{}:{answer}", installed.display())));
    }
    let output = test_support::output(
        Command::new("sh").arg(installer).arg("--uninstall").env("HOME", &home).env("XDG_DATA_HOME", home.join("data")),
    );
    assert!(output.status.success());
    assert!(!home.join(".local/bin/imacs").exists());
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
    script(&bin.join("shell"), "#!/bin/sh\nprintf '__IMACS__%s__IMACS__' \"$PATH\"\n");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_imacs"), target.join("fixture-linux-native/release/imacs")).unwrap();
    let config = home.join("config.toml");
    let output = test_support::output(
        Command::new("sh")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh"))
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("CARGO_TARGET_DIR", target)
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("SHELL", bin.join("shell"))
            .env("IMACS_HOME", home.join("state"))
            .env("IMACS_CONFIG", &config),
    );
    assert!(!output.status.success(), "EOF must leave setup resumable");
    assert!(config.exists(), "the real setup creates missing configuration");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("IMACS fresh-machine setup"), "{stdout}");
    assert!(stdout.contains("Run the suggested commands in another host terminal"), "{stdout}");
    // IMACS's credential relay reads the Claude subscription with secret-tool.
    assert!(stdout.contains("✗ sandbox prerequisite: secret-tool"), "{stdout}");
    assert!(stdout.contains("sudo apt-get install libsecret-tools gnome-keyring"), "{stdout}");
    assert!(home.join(".local/bin/imacs").exists(), "installation survives interrupted setup");
}
