use std::process::Command;

fn dependencies(args: &[&str]) -> String {
    let output = crate::test_support::output(
        Command::new(env!("CARGO"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .args(["tree", "--locked", "-p", "imacs-core", "--edges", "normal,build", "--prefix", "none"])
            .args(args),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn native_dependencies_are_default_but_absent_from_browser_core() {
    let native = dependencies(&[]);
    let browser = dependencies(&["--target", "wasm32-unknown-unknown", "--no-default-features"]);
    for package in ["ureq", "dirs", "base64", "rustls"] {
        let prefix = format!("{package} v");
        assert!(native.lines().any(|line| line.starts_with(&prefix)), "native: {package}");
        assert!(!browser.lines().any(|line| line.starts_with(&prefix)), "browser: {package}");
    }
}

#[test]
fn usage_parsing_keeps_clock_helpers_native() {
    for (args, native) in [(vec![], true), (vec!["--target", "wasm32-unknown-unknown", "--no-default-features"], false)]
    {
        let mut args = args;
        args.extend(["--format", "{p} {f}"]);
        let tree = dependencies(&args);
        let time = tree.lines().find(|line| line.starts_with("time v")).expect("usage parsing needs time");
        let features: Vec<_> = time.split_whitespace().nth(2).unwrap().split(',').collect();
        assert!(features.contains(&"parsing"));
        assert_eq!(features.contains(&"std"), native, "{time}");
        assert!(!features.contains(&"wasm-bindgen"), "usage parsing needs no JS clock");
    }
}
