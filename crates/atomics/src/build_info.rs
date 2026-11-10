//! A Nix installation's revision is supplied by its small wrapper, outside the Cargo build.
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(serde::Deserialize)]
struct Metadata {
    binary: PathBuf,
    revision: Option<String>,
}

struct Package {
    executable: PathBuf,
    revision: Option<String>,
}

static PACKAGE: LazyLock<Option<Package>> = LazyLock::new(|| {
    let root = std::env::var_os("ATOMICS_PACKAGE")?;
    let binary = std::env::current_exe().ok()?;
    read_package(Path::new(&root), &binary)
});

fn read_package(root: &Path, binary: &Path) -> Option<Package> {
    let metadata: Metadata =
        serde_json::from_slice(&std::fs::read(root.join("share/atomics/build.json")).ok()?).ok()?;
    // Agents and other installations may inherit the environment. Metadata applies only to this binary.
    if metadata.binary != binary {
        return None;
    }
    let executable = root.join("bin/atomics");
    if !executable.is_file() {
        return None;
    }
    let revision =
        metadata.revision.map(|revision| revision.trim().to_string()).filter(|revision| !revision.is_empty());
    Some(Package { executable, revision })
}

pub fn revision() -> &'static str {
    PACKAGE.as_ref().and_then(|package| package.revision.as_deref()).unwrap_or(env!("ATOMICS_BUILD_VERSION"))
}

pub fn version() -> &'static str {
    static VERSION: LazyLock<String> = LazyLock::new(|| format!("{} ({})", env!("CARGO_PKG_VERSION"), revision()));
    &VERSION
}

/// Hooks and restarts must go back through the wrapper that supplied this package's identity.
pub fn executable() -> Option<&'static Path> {
    PACKAGE.as_ref().map(|package| package.executable.as_path())
}
