use std::process::Command;

#[test]
fn native_highlighting_uses_only_onig() {
    let output =
        crate::test_support::output(Command::new(env!("CARGO")).current_dir(env!("CARGO_MANIFEST_DIR")).args([
            "tree",
            "--locked",
            "-p",
            "imacs",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
        ]));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let tree = String::from_utf8(output.stdout).unwrap();
    assert!(tree.lines().any(|line| line.starts_with("onig v")), "native highlighting needs onig");
    assert!(!tree.lines().any(|line| line.starts_with("fancy-regex v")), "unexpected fancy-regex");
}
