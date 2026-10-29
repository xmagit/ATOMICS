use std::process::Command;

fn main() {
    // Also rerun after commits and tag changes in a linked worktree.
    for name in ["HEAD", "refs", "packed-refs", "index"] {
        if let Ok(output) = Command::new("git").args(["rev-parse", "--git-path", name]).output()
            && output.status.success()
        {
            println!("cargo:rerun-if-changed={}", String::from_utf8_lossy(&output.stdout).trim());
        }
    }
    let version = Command::new("git")
        .args(["describe", "--always", "--tags", "--dirty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("crate version"));
    println!("cargo:rustc-env=IMACS_BUILD_VERSION={}", version.trim());
}
