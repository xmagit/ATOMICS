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

/// An install that asks nothing: the program opens a setup screen itself, and a person runs `atomics setup` only if they
/// want it from a terminal.
#[test]
fn install_and_update_ask_nothing_and_replace_the_program_atomically() {
    let dir = test_support::tempdir().unwrap();
    let home = dir.path().join("fresh home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    let ran = dir.path().join("ran");
    // Anything it runs says so. Setup would also ask, and find nobody to answer.
    script(
        &target.join("fixture-linux-native/release/atomics"),
        &format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho \"$@\" >> '{}'\nexit 11\n", ran.display()),
    );
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let install = |args: &[&str]| {
        test_support::output(
            Command::new("sh")
                .arg(&installer)
                .args(args)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", &path)
                .stdin(Stdio::null()),
        )
    };
    let mut inode = None;
    // Builds from before this asked for `--no-setup` by name when they updated.
    for args in [&[][..], &[][..], &["--no-setup"][..]] {
        let output = install(args);
        assert!(output.status.success(), "{output:?}");
        let installed = home.join(".local/bin/atomics");
        let next_inode = installed.metadata().unwrap().ino();
        assert_ne!(Some(next_inode), inode, "updates must replace the inode atomically");
        inode = Some(next_inode);
        assert!(home.join("data/applications/atomics.desktop").exists());
        assert!(home.join("data/icons/hicolor/512x512/apps/atomics.png").exists());
        let said = String::from_utf8(output.stdout).unwrap();
        assert!(said.contains("setup screen"), "it says what comes next: {said}");
        assert!(said.contains("export PATH="), "the exact line for a terminal: {said}");
    }
    assert!(!ran.exists(), "nothing was run: {:?}", std::fs::read_to_string(&ran));
    assert!(home.join(".local/bin/atomics.previous").is_file(), "what it replaced is kept");
    let output = test_support::output(
        Command::new("sh").arg(installer).arg("--uninstall").env("HOME", &home).env("XDG_DATA_HOME", home.join("data")),
    );
    assert!(output.status.success());
    assert!(!home.join(".local/bin/atomics").exists());
    assert!(!home.join(".local/bin/atomics.previous").exists(), "uninstalling takes that too");
}

/// A build that does not run here goes no further, and the stranger with no Rust is told what to do.
#[test]
fn a_build_that_does_not_run_replaces_nothing_and_no_rust_says_what_to_do() {
    let dir = test_support::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let install = |path: &str| {
        test_support::output(
            Command::new("sh")
                .arg(&installer)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", path)
                .stdin(Stdio::null()),
        )
    };
    let built = target.join("fixture-linux-native/release/atomics");
    script(&built, "#!/bin/sh\necho working\n");
    assert!(install(&path).status.success());
    let installed = home.join(".local/bin/atomics");
    script(&built, "#!/bin/sh\necho 'no such library' >&2\nexit 127\n");
    let output = install(&path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no such library"), "{output:?}");
    assert!(std::fs::read_to_string(&installed).unwrap().contains("working"), "the old one stays");
    let names: Vec<_> =
        std::fs::read_dir(installed.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "nothing is left beside it: {names:?}");

    let no_rust = dir.path().join("no-rust");
    test_support::link_tools(&no_rust, &["sh", "uname", "dirname"]);
    let output = install(&no_rust.display().to_string());
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("rustup.rs") && said.contains(".cargo/env") && said.contains("install.sh"), "{said}");
}

/// The script and the program write one menu entry, so that it does not depend on how ATOMICS got here.
#[test]
fn the_script_and_the_program_write_the_same_menu_entry() {
    let dir = test_support::tempdir().unwrap();
    let home = dir.path().join("home with a space");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    script(&bin.join("shell"), "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
    std::fs::copy(env!("CARGO_BIN_EXE_atomics"), target.join("fixture-linux-native/release/atomics")).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let run = |command: &mut Command| {
        let output = test_support::output(
            command
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("SHELL", bin.join("shell"))
                .env("PATH", &path)
                .stdin(Stdio::null()),
        );
        assert!(output.status.success(), "{output:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    run(Command::new("sh").arg(installer));
    let entry = home.join("data/applications/atomics.desktop");
    let icons = [
        home.join("data/icons/hicolor/scalable/apps/atomics.svg"),
        home.join("data/icons/hicolor/512x512/apps/atomics.png"),
    ];
    let from_script = std::fs::read_to_string(&entry).unwrap();
    let icons_from_script = icons.clone().map(|icon| std::fs::read(icon).unwrap());
    std::fs::remove_file(&entry).unwrap();
    for icon in &icons {
        std::fs::remove_file(icon).unwrap();
    }

    let said = run(Command::new(home.join(".local/bin/atomics")).arg("install"));
    assert!(said.contains("Installed ATOMICS") && said.contains("setup screen"), "{said}");
    assert!(said.contains("export PATH="), "the exact line for a terminal: {said}");
    assert_eq!(std::fs::read_to_string(&entry).unwrap(), from_script);
    assert_eq!(icons.map(|icon| std::fs::read(icon).unwrap()), icons_from_script);
    assert!(from_script.contains(&format!("Exec=\"{}/.local/bin/atomics\" %u", home.display())), "{from_script}");
}

/// The Mac's packager, on any machine, with stand-ins for what only a Mac has: a failure on the way leaves the app there.
#[test]
fn the_mac_packager_keeps_the_old_app_until_the_new_one_is_whole_and_keeps_it_after() {
    let dir = test_support::tempdir().unwrap();
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    let dest = dir.path().join("Applications");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-mac/release")).unwrap();
    script(&bin.join("uname"), "#!/bin/sh\necho Darwin\n");
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-mac'\n");
    script(&bin.join("sips"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rsvg-convert"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("codesign"), "#!/bin/sh\nexit 0\n");
    let iconutil = bin.join("iconutil");
    let built = target.join("fixture-mac/release/atomics");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let package = || {
        test_support::output(
            Command::new("sh")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/macos-app.sh"))
                .arg(&dest)
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", &path),
        )
    };
    let program = dest.join("ATOMICS.app/Contents/MacOS/atomics");
    let kept = dest.join(".atomics-previous/Contents/MacOS/atomics");
    let installed = |text: &str| std::fs::read_to_string(&program).unwrap().contains(text);

    script(&iconutil, "#!/bin/sh\nexit 0\n");
    script(&built, "#!/bin/sh\necho one\n");
    assert!(package().status.success());
    assert!(installed("one") && !kept.exists());

    // Each way a build can fail after the app used to be removed.
    script(&built, "#!/bin/sh\necho two\n");
    script(&iconutil, "#!/bin/sh\nexit 1\n");
    assert!(!package().status.success());
    assert!(installed("one"), "the icon failed: the old app stands");
    script(&iconutil, "#!/bin/sh\nexit 0\n");
    script(&built, "#!/bin/sh\nexit 127\n");
    assert!(!package().status.success());
    assert!(installed("one"), "the build does not run: the old app stands");
    assert!(!kept.exists(), "and nothing moved");
    let names: Vec<_> = std::fs::read_dir(&dest).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names.len(), 1, "nothing is left beside it: {names:?}");

    script(&built, "#!/bin/sh\necho two\n");
    assert!(package().status.success());
    assert!(installed("two"));
    assert!(std::fs::read_to_string(&kept).unwrap().contains("one"), "the way back");
}

/// The real `atomics setup`, in a home with a login shell of its own and only the tools a test puts there.
struct SetupHome {
    dir: test_support::TempDir,
}

impl SetupHome {
    /// With a config that says `mode`, and these programs on PATH.
    fn new(mode: &str, programs: &[&str]) -> SetupHome {
        let dir = test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for program in programs {
            script(&bin.join(program), "#!/bin/sh\nexit 0\n");
        }
        script(&bin.join("shell"), "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
        std::fs::write(dir.path().join("config.toml"), format!("[isolation]\nmode = \"{mode}\"\n")).unwrap();
        SetupHome { dir }
    }

    fn run(&self, args: &[&str], answers: &str) -> std::process::Output {
        let mut child = test_support::TestChild::spawn(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(args)
                .env("HOME", self.dir.path())
                .env("PATH", self.dir.path().join("bin"))
                .env("SHELL", self.dir.path().join("bin/shell"))
                .env("ATOMICS_HOME", self.dir.path().join("state"))
                .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", self.dir.path().join("no-host.sock"))
                .stdin(Stdio::piped()),
            test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        child.child.stdin.take().unwrap().write_all(answers.as_bytes()).unwrap();
        child.wait_output().unwrap()
    }
}

#[test]
fn setup_with_agents_on_this_machine_needs_one_agent_and_nothing_of_sandboxes() {
    let home = SetupHome::new("bare", &["git", "claude"]);
    let output = home.run(&["setup"], "");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(said.contains("sandboxes are not needed"), "{said}");
    assert!(said.contains("codex is not installed"), "it says how to get the other: {said}");
    for word in ["secret-tool", "sbx ", "KVM", "Docker"] {
        assert!(!said.contains(word), "{word} is not in use: {said}");
    }
}

#[test]
fn stopping_setup_reads_as_a_stop_and_it_can_still_ask_for_sandboxes() {
    // No agent yet, so it waits; `q` is a person leaving, not a broken install.
    let home = SetupHome::new("bare", &["git"]);
    let output = home.run(&["setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    assert!(said.contains("ATOMICS is installed"), "{said}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("failed"));

    // Sandboxes asked for by name, on a machine whose agents run bare.
    let home = SetupHome::new("bare", &["git", "claude"]);
    let output = home.run(&["sandbox", "setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    // What sandboxes need comes first: on Linux that stops at KVM, and on a Mac it reaches sbx.
    assert!(said.contains(if cfg!(target_os = "linux") { "kvm group" } else { "sbx" }), "{said}");
    assert!(said.contains("ATOMICS is installed"), "{said}");

    // Nobody to answer is not the same as stopping: it says so, and a script can tell.
    let output = SetupHome::new("bare", &["git"]).run(&["setup"], "");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("setup input ended"));
}

/// Claude Code's own installer leaves it in a folder many logins do not list, where the window finds it: so does setup.
#[test]
fn setup_finds_what_an_installer_left_in_the_users_own_programs_folder() {
    let home = SetupHome::new("bare", &["git"]);
    let local = home.dir.path().join(".local/bin");
    std::fs::create_dir_all(&local).unwrap();
    script(&local.join("claude"), "#!/bin/sh\nexit 0\n");
    let output = home.run(&["setup"], "");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(!said.contains("prerequisite: claude"), "{said}");
    assert!(said.contains("sandboxes are not needed"), "{said}");
}

#[test]
fn setup_with_sandboxes_in_use_still_checks_what_they_need() {
    let home = SetupHome::new("sandbox", &["git", "claude"]);
    let output = home.run(&["setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    #[cfg(target_os = "linux")]
    assert!(said.contains("prerequisite: secret-tool"), "{said}");
    assert!(said.contains("ATOMICS fresh-machine setup"), "{said}");
}

#[test]
fn installer_registers_the_atomics_link_scheme() {
    let dir = test_support::tempdir().unwrap();
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
