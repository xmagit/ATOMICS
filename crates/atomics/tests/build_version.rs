//! Package metadata changes the installed identity without recompiling the Cargo binary.
#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn metadata(root: &Path, binary: &Path, revision: Value) {
    std::fs::write(root.join("share/atomics/build.json"), json!({"binary":binary, "revision":revision}).to_string())
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
