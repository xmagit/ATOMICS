//! The web crates must build for the browser: `atomics-ui` and `atomics-web` may not pull a desktop-only
//! dependency, on any target. `DESKTOP_ONLY` lists them.

use std::process::Command;

/// Crates that do not build for `wasm32-unknown-unknown`, or that a browser has no use for.
const DESKTOP_ONLY: &[&str] = &[
    "atomics",
    "syntect",
    "onig",
    "onig_sys",
    "two-face",
    "alacritty_terminal",
    "cpal",
    "rfd",
    "notify",
    "ureq",
    "fontique",
    "fontconfig-parser",
    "yeslogic-fontconfig-sys",
    "ring",
    "tempfile",
    "dirs",
    "libc",
    // Warm-folder patterns are matched on the host only.
    "glob",
];

/// Desktop-only crates the host target may still reach, and the only crates allowed to depend on them there.
/// The wasm32 tree may not contain them at all.
const HOST_ONLY_THROUGH: &[(&str, &[&str])] = &[
    // egui's locks (parking_lot), the Noise channel's randomness (getrandom) and ARM CPU detection
    // (cpufeatures) use libc on Unix; the browser tree must still contain no libc.
    ("libc", &["parking_lot_core", "getrandom", "cpufeatures"]),
];

/// The crates that run in the browser.
const WEB_CRATES: &[&str] = &["atomics-ui", "atomics-web"];

fn cargo_tree(krate: &str, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["tree", "--locked", "-p", krate, "--edges", "normal,build"])
        .args(args)
        .output()
        .expect("cargo tree runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("cargo tree prints UTF-8")
}

fn tree(krate: &str, args: &[&str]) -> String {
    cargo_tree(krate, &[&["--prefix", "none", "--format", "{p} {f}"], args].concat())
}

/// The crates in `krate`'s tree that depend on `package` directly.
fn dependents(krate: &str, package: &str, args: &[&str]) -> Vec<String> {
    let inverted = cargo_tree(
        krate,
        &[&["--invert", package, "--depth", "1", "--prefix", "depth", "--format", "{p}"], args].concat(),
    );
    inverted
        .lines()
        .filter_map(|line| line.strip_prefix('1'))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

fn assert_clean(krate: &str, tree: &str, target: &str, args: &[&str]) {
    for package in DESKTOP_ONLY {
        let prefix = format!("{package} v");
        if !tree.lines().any(|line| line.starts_with(&prefix)) {
            continue;
        }
        let allowed = HOST_ONLY_THROUGH.iter().find(|(name, _)| name == package).map(|(_, through)| *through);
        match allowed {
            Some(through) if !args.contains(&"--target") => {
                let from = dependents(krate, package, args);
                assert!(
                    from.iter().all(|dependent| through.contains(&dependent.as_str())),
                    "{target}: {krate} reaches {package} through {from:?}, not only {through:?}"
                );
            }
            _ => panic!("{target}: {krate} pulls in {package}"),
        }
    }
    let core = tree.lines().find(|line| line.starts_with("atomics-core v")).expect("a web crate reads atomics-core");
    let features: Vec<_> = core.split_whitespace().last().unwrap_or("").split(',').collect();
    assert!(!features.contains(&"native"), "{target}: atomics-core must be used without `native`: {core}");
}

#[test]
fn web_crates_pull_no_desktop_only_dependency() {
    let wasm = ["--target", "wasm32-unknown-unknown"];
    for krate in WEB_CRATES {
        assert_clean(krate, &tree(krate, &[]), "host target", &[]);
        assert_clean(krate, &tree(krate, &wasm), "wasm32", &wasm);
    }
}

#[test]
fn the_check_catches_a_desktop_dependency() {
    // Guard the guard: `atomics` itself pulls in several of the listed crates.
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["tree", "--locked", "-p", "atomics", "--edges", "normal,build", "--prefix", "none"])
        .output()
        .expect("cargo tree runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let desktop = String::from_utf8(output.stdout).unwrap();
    for package in ["syntect", "alacritty_terminal", "cpal", "rfd", "notify", "ureq"] {
        assert!(desktop.lines().any(|line| line.starts_with(&format!("{package} v"))), "desktop lacks {package}");
    }
}
