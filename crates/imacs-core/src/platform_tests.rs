use std::process::Command;

fn dependencies(package: &str, args: &[&str]) -> String {
    let output = crate::test_support::output(
        Command::new(env!("CARGO"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(["tree", "--locked", "-p", package, "--edges", "normal,build", "--prefix", "none"])
            .args(args),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn native_dependencies_are_default_but_absent_from_browser_core() {
    let native = dependencies("imacs-core", &[]);
    let browser = dependencies("imacs-core", &["--target", "wasm32-unknown-unknown", "--no-default-features"]);
    for package in ["ureq", "dirs", "base64", "rustls"] {
        let prefix = format!("{package} v");
        assert!(native.lines().any(|line| line.starts_with(&prefix)), "native: {package}");
        assert!(!browser.lines().any(|line| line.starts_with(&prefix)), "browser: {package}");
    }
}

#[test]
fn highlighting_uses_only_the_target_regex_engine() {
    for (args, wanted, unwanted) in
        [(vec![], "onig", "fancy-regex"), (vec!["--target", "wasm32-unknown-unknown"], "fancy-regex", "onig")]
    {
        let tree = dependencies("imacs", &args);
        assert!(tree.lines().any(|line| line.starts_with(&format!("{wanted} v"))), "missing {wanted}");
        assert!(!tree.lines().any(|line| line.starts_with(&format!("{unwanted} v"))), "unexpected {unwanted}");
    }
}
