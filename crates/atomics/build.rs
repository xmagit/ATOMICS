use std::process::Command;

fn main() {
    // Also rerun after commits and tag changes in a linked worktree.
    for name in ["HEAD", "refs", "packed-refs", "index"] {
        if let Ok(output) =
            Command::new("git").env("GIT_NO_REPLACE_OBJECTS", "1").args(["rev-parse", "--git-path", name]).output()
            && output.status.success()
        {
            println!("cargo:rerun-if-changed={}", String::from_utf8_lossy(&output.stdout).trim());
        }
    }
    let version = Command::new("git")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["describe", "--always", "--tags", "--dirty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("crate version"));
    println!("cargo:rustc-env=ATOMICS_BUILD_VERSION={}", version.trim());
    println!("cargo:rustc-env=ATOMICS_TARGET={}", std::env::var("TARGET").expect("cargo's target"));
    release_signing_key();
}

/// Builds in the release trust anchors the checkout has: the key CI signs with and the offline one kept to recover from
/// losing it. With neither the updater refuses to install a download; a release needs both, which its workflow checks.
/// The folder is what is watched, because a missing file would rerun this every build.
fn release_signing_key() {
    println!("cargo:rerun-if-changed=../../assets");
    let keys: Vec<String> = ["release-signing-key.pub", "release-signing-key-offline.pub"]
        .into_iter()
        .filter_map(|name| std::fs::read_to_string(format!("../../assets/{name}")).ok())
        .collect();
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo's output folder")).join("release_keys.rs");
    std::fs::write(out, format!("&{keys:?}")).expect("writing the release keys' source");
}
