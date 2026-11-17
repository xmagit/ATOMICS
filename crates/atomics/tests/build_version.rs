//! Package metadata changes the installed identity without recompiling the Cargo binary.
#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn metadata(root: &Path, binary: &Path, revision: Value) {
    std::fs::write(
        root.join("share/atomics/build.json"),
        json!({"binary":binary, "revision":revision, "commit":"0123456789abcdef0123456789abcdef01234567"}).to_string(),
    )
    .unwrap();
}

fn run(root: &Path, args: &[&str]) -> String {
    let output = test_support::output(
        Command::new(env!("CARGO_BIN_EXE_atomics"))
            .args(args)
            .env("ATOMICS_PACKAGE", root)
            .env("HOME", root)
            .env("ATOMICS_HOME", root.join("state"))
            .env("ATOMICS_CONFIG", root.join("config.toml"))
            .env("ATOMICS_SOCKET", root.join("atomics.sock")),
    );
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn installed_revision_and_hook_path_come_from_the_wrapper_package() {
    let root = test_support::tempdir().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_atomics")).canonicalize().unwrap();
    std::fs::create_dir_all(root.path().join("share/atomics")).unwrap();
    std::fs::create_dir(root.path().join("bin")).unwrap();
    let wrapper = root.path().join("bin/atomics");
    std::os::unix::fs::symlink(&binary, &wrapper).unwrap();
    metadata(root.path(), &binary, json!("  abc1234-dirty\n"));

    let expected = format!("atomics {} (abc1234-dirty)\n", env!("CARGO_PKG_VERSION"));
    for flag in ["--version", "-V"] {
        assert_eq!(run(root.path(), &[flag]), expected);
    }
    let hooks = run(root.path(), &["print-hook-settings", "--agent", "claude"]);
    assert!(hooks.contains(wrapper.to_str().unwrap()), "{hooks}");
    run(root.path(), &["install-hooks"]);
    let installed_hooks = std::fs::read_to_string(root.path().join(".claude/settings.json")).unwrap();
    assert!(installed_hooks.contains(wrapper.to_str().unwrap()), "global hooks also use the stable wrapper");

    // Changing only the metadata changes the reported revision, with the very same binary.
    metadata(root.path(), &binary, json!("def5678"));
    assert_eq!(run(root.path(), &["--version"]), format!("atomics {} (def5678)\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn absent_invalid_or_inherited_metadata_keeps_the_compiled_build_identity() {
    let root = test_support::tempdir().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_atomics")).canonicalize().unwrap();
    std::fs::create_dir_all(root.path().join("share/atomics")).unwrap();
    std::fs::create_dir(root.path().join("bin")).unwrap();
    std::os::unix::fs::symlink(&binary, root.path().join("bin/atomics")).unwrap();
    let fallback = run(root.path(), &["--version"]);
    assert!(fallback.starts_with(&format!("atomics {} (", env!("CARGO_PKG_VERSION"))));

    // An agent or another installation may inherit ATOMICS_PACKAGE from a different binary.
    metadata(root.path(), &root.path().join("other-binary"), json!("wrong-installation"));
    assert_eq!(run(root.path(), &["--version"]), fallback);
    for revision in [Value::Null, json!(" \n")] {
        metadata(root.path(), &binary, revision);
        assert_eq!(run(root.path(), &["--version"]), fallback);
    }
    std::fs::write(root.path().join("share/atomics/build.json"), "not JSON").unwrap();
    assert_eq!(run(root.path(), &["--version"]), fallback);
}

#[test]
fn build_script_records_only_a_clean_full_commit_and_keeps_git_describe_for_display() {
    let dir = test_support::tempdir().unwrap();
    let script = dir.path().join("build-script");
    let output = test_support::output(
        Command::new("rustc")
            .args(["--edition", "2024"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("build.rs"))
            .arg("-o")
            .arg(&script),
    );
    assert!(output.status.success(), "{output:?}");
    let root = dir.path().join("checkout");
    let crate_dir = root.join("crates/atomics");
    let other = root.join("crates/other.rs");
    std::fs::create_dir_all(&crate_dir).unwrap();
    std::fs::create_dir(root.join("assets")).unwrap();
    std::fs::write(&other, "original").unwrap();
    std::fs::write(crate_dir.join("main.rs"), "fn main() {}\n").unwrap();
    let git = |args: &[&str]| {
        let output = test_support::output(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["-c", "user.name=alice", "-c", "user.email=alice@example.com"])
                .args(args),
        );
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "Fixture"]);
    git(&["tag", "v0.1.7"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let build = |cwd: &Path| {
        let output = test_support::output(
            Command::new(&script)
                .current_dir(cwd)
                .env("CARGO_PKG_VERSION", env!("CARGO_PKG_VERSION"))
                .env("TARGET", "aarch64-apple-darwin")
                .env("OUT_DIR", dir.path()),
        );
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let clean = build(&crate_dir);
    assert_eq!(commit.len(), 40);
    assert!(clean.lines().any(|line| line == format!("cargo:rustc-env=ATOMICS_BUILD_COMMIT={commit}")), "{clean}");
    assert!(clean.lines().any(|line| line == "cargo:rustc-env=ATOMICS_BUILD_VERSION=v0.1.7"), "{clean}");
    assert!(
        clean.lines().any(|line| line == format!("cargo:rerun-if-changed={}", other.canonicalize().unwrap().display())),
        "changes in other workspace crates must invalidate the clean commit: {clean}"
    );
    let refuses = |cwd: &Path| {
        let output = build(cwd);
        assert!(output.lines().any(|line| line == "cargo:rustc-env=ATOMICS_BUILD_COMMIT="), "{output}");
    };
    std::fs::write(&other, "unstaged change").unwrap();
    refuses(&crate_dir);
    git(&["add", "."]);
    refuses(&crate_dir);
    std::fs::write(&other, "original").unwrap();
    git(&["add", "."]);
    std::fs::write(root.join("untracked.rs"), "untracked change").unwrap();
    refuses(&crate_dir);
    let unknown = dir.path().join("unknown/crates/atomics");
    std::fs::create_dir_all(&unknown).unwrap();
    refuses(&unknown);
}
