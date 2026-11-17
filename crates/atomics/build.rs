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
    // Watching Git metadata alone misses unstaged edits: Cargo may rebuild Rust sources without
    // rerunning this script, leaving a formerly clean commit attached to a dirty binary.
    if let (Ok(root), Ok(files)) = (
        Command::new("git").args(["rev-parse", "--show-toplevel"]).output(),
        Command::new("git").args(["ls-files", "--full-name", "-z", "--", ":/"]).output(),
    ) && root.status.success()
        && files.status.success()
    {
        let root = String::from_utf8_lossy(&root.stdout);
        for file in files.stdout.split(|byte| *byte == 0).filter(|file| !file.is_empty()) {
            println!("cargo:rerun-if-changed={}/{}", root.trim(), String::from_utf8_lossy(file));
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
    // The release signature covers this full commit separately from the display version. Never
    // identify a dirty checkout as HEAD: that commit does not describe the code being signed.
    let commit = Command::new("git")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["-c", "core.fsmonitor=false", "status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok()
        .filter(|output| output.status.success() && output.stdout.is_empty())
        .and_then(|_| {
            Command::new("git")
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .args(["rev-parse", "--verify", "HEAD^{commit}"])
                .output()
                .ok()
                .filter(|output| output.status.success())
        })
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .unwrap_or_default();
    println!("cargo:rustc-env=ATOMICS_BUILD_COMMIT={}", commit.trim());
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
