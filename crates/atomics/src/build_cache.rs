//! Filesystem operations for cache links and disposable worktree output.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use atomics_core::cache::{CacheConfig, path_variable, relative};
use atomics_core::machine::{BuildOutput, Disk};
use atomics_core::{git, paths};

pub fn root(cwd: &Path) -> PathBuf {
    git::worktree_of(cwd).map(|w| w.root).unwrap_or_else(|| cwd.to_path_buf())
}

pub fn lean_worktree(cwd: &Path) -> bool {
    linked_worktree(&root(cwd)).is_ok()
}

pub fn machine_cache() -> PathBuf {
    // Deliberately independent of ATOMICS_HOME: one compiler cache for this user's machine.
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| paths::home().join(".cache"))
        .join("atomics/sccache-host-v2")
}

pub fn agent_env(cwd: &Path, config: &CacheConfig) -> BTreeMap<String, String> {
    let root = root(cwd);
    let cargo = root.join("Cargo.toml").is_file();
    let user_config = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| paths::home().join(".cargo"));
    let wrapper_set = ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"]
        .iter()
        .any(|key| std::env::var_os(key).is_some() || config.env.contains_key(*key))
        || cwd.ancestors().map(|dir| dir.join(".cargo")).chain([user_config]).any(|dir| {
            ["config.toml", "config"]
                .iter()
                .any(|name| std::fs::read_to_string(dir.join(name)).is_ok_and(|text| text.contains("rustc-wrapper")))
        });
    let sccache = crate::setup::find_program("sccache");
    let cache = machine_cache();
    let program = sccache.as_ref().and_then(|p| p.to_str());
    let mut env = if wrapper_set {
        config.agent_env_without_auto_wrapper(cargo, program, &cache.to_string_lossy(), |key| std::env::var(key).ok())
    } else {
        config.agent_env(cargo, program, &cache.to_string_lossy(), |key| std::env::var(key).ok())
    };
    resolve_env_paths(&paths::atomics_home(), &root, config, &mut env);
    if !env.contains_key("SCCACHE_DIR") {
        for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
            if env.get(key).is_some_and(|value| atomics_core::cache::is_sccache(value)) {
                env.insert(key.into(), String::new());
                eprintln!("ATOMICS: sccache store unavailable; using rustc directly");
            }
        }
    }
    if let Some(store) = env.get("SCCACHE_DIR") {
        // A private socket per effective store avoids the first-server-wins default port.
        let sockets = machine_cache().with_file_name("sccache-host-sockets-v2");
        if safe_store(&sockets)
            && domain_store_safe(&paths::atomics_home(), &sockets, Domain::Host)
            && std::fs::create_dir_all(&sockets).is_ok()
        {
            env.insert(
                "SCCACHE_SERVER_UDS".into(),
                sockets.join(paths::runtime_home_hash(Path::new(store))).to_string_lossy().into_owned(),
            );
        } else {
            env.insert("RUSTC_WRAPPER".into(), String::new());
            env.insert("CARGO_BUILD_RUSTC_WRAPPER".into(), String::new());
            eprintln!("ATOMICS: sccache socket directory unavailable; using rustc directly");
        }
    }
    env
}

pub fn resolve_env_paths(home: &Path, root: &Path, config: &CacheConfig, env: &mut BTreeMap<String, String>) {
    resolve_env_paths_in(home, root, config, env, Domain::Host);
}

pub fn resolve_env_paths_in(
    home: &Path,
    root: &Path,
    config: &CacheConfig,
    env: &mut BTreeMap<String, String>,
    domain: Domain,
) {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let shared = stores(home, root, config, domain);
    env.retain(|key, value| {
        if path_variable(key) {
            let path = if relative(value) { root.join(&*value) } else { paths::expand(value) };
            if !safe_store(&path) || !domain_store_safe(home, &path, domain) {
                eprintln!("ATOMICS: ignored cache.env.{key}: store contains tracked files or cannot be checked");
                return false;
            }
            if config.repo_paths.contains(key) {
                let resolved = path.ancestors().find_map(|parent| parent.canonicalize().ok());
                if !resolved.is_some_and(|parent| {
                    parent.starts_with(&canonical_root) || shared.iter().any(|store| parent.starts_with(store))
                }) {
                    eprintln!("ATOMICS: ignored repository cache.env.{key}: path escapes the worktree");
                    return false;
                }
            }
            *value = path.to_string_lossy().into_owned();
        }
        true
    });
}

/// Resolve the existing ancestor before checking git, including an altered state-directory link.
pub fn safe_store(path: &Path) -> bool {
    let Some(parent) = path.ancestors().find(|parent| parent.exists()) else { return false };
    let Ok(resolved) = parent.canonicalize() else { return false };
    let Ok(tail) = path.strip_prefix(parent) else { return false };
    let destination = resolved.join(tail);
    let cwd = if resolved.is_dir() { resolved } else { resolved.parent().unwrap_or(&resolved).to_path_buf() };
    if let Ok(root) = git::git(&cwd, &["rev-parse", "--show-toplevel"]) {
        let root = PathBuf::from(root.trim());
        let Ok(folder) = destination.strip_prefix(&root) else { return false };
        return !folder.as_os_str().is_empty()
            && git::git(&root, &["--literal-pathspecs", "ls-files", "--", &folder.to_string_lossy()])
                .is_ok_and(|files| files.is_empty());
    }
    // Outside a checkout there cannot be tracked cache contents.
    !cwd.ancestors().any(|parent| parent.join(".git").symlink_metadata().is_ok())
}

#[derive(Clone, Copy)]
pub enum Domain {
    Host,
    Sandbox,
}

fn domain_store_safe(home: &Path, path: &Path, domain: Domain) -> bool {
    let resolve = |path: &Path| -> Option<PathBuf> {
        let ancestor = path.ancestors().find(|parent| parent.exists())?;
        Some(ancestor.canonicalize().ok()?.join(path.strip_prefix(ancestor).ok()?))
    };
    let Some(path) = resolve(path) else { return false };
    let mut forbidden = match domain {
        Domain::Host => vec![home.join("build-caches/sandbox-repos-v2"), home.join("build-caches/sandboxes-v2")],
        Domain::Sandbox => vec![home.join("build-caches/host-repos-v2"), machine_cache()],
    };
    // Neither domain may import the stores shared across the old sandbox boundary.
    forbidden.extend([home.join("build-caches/repos"), machine_cache().with_file_name("sccache")]);
    !forbidden.iter().filter_map(|root| resolve(root)).any(|root| path.starts_with(root))
}

pub fn store_allowed(home: &Path, path: &Path, domain: Domain) -> bool {
    safe_store(path) && domain_store_safe(home, path, domain)
}

fn repository_store(home: &Path, main: &Path, domain: Domain) -> PathBuf {
    let namespace = match domain {
        Domain::Host => "host-repos-v2",
        Domain::Sandbox => "sandbox-repos-v2",
    };
    home.join("build-caches").join(namespace).join(paths::runtime_home_hash(main))
}

/// No symlink component is traversed, including links back inside the checkout.
fn inside(root: &Path, relative_path: &str) -> Option<PathBuf> {
    if !relative(relative_path) {
        return None;
    }
    let mut path = root.to_path_buf();
    for part in Path::new(relative_path) {
        // A literal git pathspec is case-sensitive even on a case-insensitive disk.
        // Missing components are allowed for cache setup; existing names must be exact.
        match std::fs::read_dir(&path) {
            Ok(entries) => {
                let names = entries
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()
                    .ok()?;
                if !names.iter().any(|name| name == part)
                    && (names.iter().any(|name| name.to_string_lossy().eq_ignore_ascii_case(&part.to_string_lossy()))
                        || path.join(part).symlink_metadata().is_ok())
                {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        path.push(part);
        match path.symlink_metadata() {
            Ok(meta) if meta.file_type().is_symlink() => return None,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(path)
}

fn linked_worktree(worktree: &Path) -> Result<(PathBuf, PathBuf)> {
    let root = worktree.canonicalize()?;
    let main = git::main_checkout(&root)?.canonicalize()?;
    ensure!(root != main && !root.starts_with(main.join(".git")), "main checkout is never cleaned or linked");
    ensure!(
        git::list_worktrees(&main)?
            .iter()
            .skip(1)
            .any(|w| w.trusted && w.path.canonicalize().ok().as_ref() == Some(&root)),
        "not a trusted linked worktree"
    );
    Ok((root, main))
}

/// Leave existing folders alone. No migration, copying, or deletion during cache setup.
pub fn link(home: &Path, worktree: &Path, config: &CacheConfig, domain: Domain) -> Result<Vec<PathBuf>> {
    if config.shared.is_empty() {
        return Ok(Vec::new());
    }
    let (root, main) = linked_worktree(worktree)?;
    let store = repository_store(home, &main, domain);
    let mut linked = Vec::new();
    for folder in &config.shared {
        if !relative(folder) {
            continue;
        }
        let source = store.join(folder);
        if let Ok(meta) = root.join(folder).symlink_metadata() {
            if meta.file_type().is_symlink() {
                ensure!(
                    root.join(folder).canonicalize().ok() == source.canonicalize().ok()
                        && source.canonicalize().is_ok(),
                    "cache link belongs to another trust domain: {folder}"
                );
                ensure!(
                    safe_store(&source) && domain_store_safe(home, &source, domain),
                    "cache store contains tracked files or belongs to another trust domain"
                );
            }
            continue;
        }
        let Some(destination) = inside(&root, folder) else { continue };
        // The state folder is user-owned. Still refuse altered store components.
        let Some(source) = inside(&store, folder).filter(|p| *p == source) else { continue };
        ensure!(
            safe_store(&source) && domain_store_safe(home, &source, domain),
            "cache store contains tracked files or belongs to another trust domain"
        );
        ensure!(
            git::git(&root, &["--literal-pathspecs", "ls-files", "--", folder])?.is_empty(),
            "cache path is tracked"
        );
        std::fs::create_dir_all(&source)?;
        std::fs::create_dir_all(destination.parent().context("cache parent")?)?;
        match std::os::unix::fs::symlink(&source, &destination) {
            Ok(()) => linked.push(source),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(linked)
}

/// Includes already-linked stores, for explicit mounts in Linux sandboxes.
pub fn stores(home: &Path, worktree: &Path, config: &CacheConfig, domain: Domain) -> Vec<PathBuf> {
    let Ok((root, main)) = linked_worktree(worktree) else { return Vec::new() };
    let store = repository_store(home, &main, domain);
    config
        .shared
        .iter()
        .filter(|folder| relative(folder))
        .filter_map(|folder| {
            let path = root.join(folder);
            let expected = inside(&store, folder)?.canonicalize().ok()?;
            if !safe_store(&expected) || !domain_store_safe(home, &expected, domain) {
                return None;
            }
            if !git::git(&root, &["--literal-pathspecs", "ls-files", "--", folder]).is_ok_and(|files| files.is_empty())
            {
                return None;
            }
            (path.symlink_metadata().ok()?.file_type().is_symlink() && path.canonicalize().ok()? == expected)
                .then_some(expected)
        })
        .collect()
}

fn disposable(root: &Path, folder: &str) -> bool {
    // Clean tracked sources also produce empty status, so both checks are necessary.
    [
        vec!["--literal-pathspecs", "ls-files", "--", folder],
        vec!["--literal-pathspecs", "status", "--porcelain", "--untracked-files=all", "--ignored=no", "--", folder],
    ]
    .iter()
    .all(|args| git::git(root, args).is_ok_and(|files| files.is_empty()))
}

fn output_paths(root: &Path, config: &CacheConfig) -> Vec<PathBuf> {
    let mut folders = config.outputs.clone();
    if root.join("Cargo.toml").is_file() {
        folders.insert(0, "target".into());
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for folder in folders {
        // A configured shared folder and anything in it belongs to the store, never cleanup.
        if config.shared.iter().any(|shared| {
            relative(shared) && (Path::new(&folder).starts_with(shared) || Path::new(shared).starts_with(&folder))
        }) {
            continue;
        }
        let Some(path) = inside(root, &folder) else { continue };
        if !path.is_dir() || found.iter().any(|parent| path.starts_with(parent)) {
            continue;
        }
        // Refuse a folder with even one tracked file, including sparse and deleted tracked files.
        if !disposable(root, &folder) {
            continue;
        }
        found.retain(|child| !child.starts_with(&path));
        found.push(path);
    }
    found
}

/// Never follows links, so neither accounting nor removal reaches a shared store.
fn bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut pending = vec![path.to_path_buf()];
    let mut size = 0u64;
    while let Some(path) = pending.pop() {
        let Ok(meta) = path.symlink_metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        size = size.saturating_add(meta.blocks().saturating_mul(512));
        if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(&path)
        {
            pending.extend(entries.flatten().map(|e| e.path()));
        }
    }
    size
}

pub fn inventory(worktree: &Path, config: &CacheConfig) -> Vec<BuildOutput> {
    let Ok((root, _)) = linked_worktree(worktree) else { return Vec::new() };
    output_paths(&root, config)
        .into_iter()
        .map(|path| BuildOutput { bytes: bytes(&path), path: path.to_string_lossy().into_owned() })
        .collect()
}

pub fn clean(worktree: &Path, config: &CacheConfig) -> Result<Vec<String>> {
    let (root, _) = linked_worktree(worktree)?;
    let mut notes = Vec::new();
    // Protect an entire worktree with a process in it, even if it has no output file open yet.
    let users = crate::lifecycle::processes_in(&root);
    ensure!(users.is_empty(), "worktree in use: {}", users.join(", "));
    for path in output_paths(&root, config) {
        ensure!(worktree.canonicalize()? == root, "worktree moved during cleanup");
        let folder = path.strip_prefix(&root)?.to_string_lossy();
        ensure!(inside(&root, &folder).as_ref() == Some(&path), "output path changed during cleanup");
        let users = crate::lifecycle::processes_in(&root);
        ensure!(users.is_empty(), "worktree in use: {}", users.join(", "));
        ensure!(disposable(&root, &folder), "output contains tracked or non-ignored work");
        std::fs::remove_dir_all(&path)?;
        notes.push(format!("Removed build output {}", path.display()));
    }
    Ok(notes)
}

/// Stat the actual destination filesystem, walking up for a worktree not created yet.
pub fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    path.ancestors().find_map(|p| std::fs::metadata(p).ok().map(|m| m.dev()))
}

pub fn disk(path: &Path) -> Option<Disk> {
    path.ancestors().find(|p| p.exists()).and_then(|p| crate::machine::disk(p.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn project() -> (crate::test_support::TempDir, PathBuf, PathBuf) {
        let dir = crate::test_support::tempdir().unwrap();
        let main = dir.path().join("repo");
        std::fs::create_dir(&main).unwrap();
        git::git(&main, &["init", "-qb", "main"]).unwrap();
        git::git(
            &main,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        )
        .unwrap();
        std::fs::write(main.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(main.join(".gitignore"), "target/\ncustom-output/\n").unwrap();
        git::git(&main, &["add", "Cargo.toml", ".gitignore"]).unwrap();
        git::git(
            &main,
            &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "workspace"],
        )
        .unwrap();
        let work = dir.path().join("task");
        git::add_worktree(&main, &work, "task", "main").unwrap();
        (dir, main, work)
    }

    fn output(root: &Path, folder: &str) {
        std::fs::create_dir_all(root.join(folder)).unwrap();
        std::fs::write(root.join(folder).join("artifact"), "compiled").unwrap();
    }

    #[test]
    fn cleanup_preserves_non_ignored_new_work_in_default_and_configured_outputs() {
        let (_dir, _main, work) = project();
        std::fs::write(work.join(".gitignore"), "target/*\n!target/new.rs\n").unwrap();
        output(&work, "target");
        std::fs::write(work.join("target/new.rs"), "new source").unwrap();
        output(&work, "dist");
        let config = CacheConfig { outputs: vec!["dist".into()], ..Default::default() };
        assert!(output_paths(&work, &config).is_empty());
        assert!(clean(&work, &config).unwrap().is_empty());
        assert!(work.join("target/new.rs").is_file());
        assert!(work.join("dist/artifact").is_file());
        std::fs::remove_file(work.join("target/new.rs")).unwrap();
        assert!(disposable(&work, "target"));
        std::fs::write(work.join("target/new.rs"), "new source").unwrap();
        assert!(!disposable(&work, "target"));
    }

    #[test]
    fn inside_requires_exact_components_and_refuses_case_only_aliases() {
        let dir = crate::test_support::tempdir().unwrap();
        output(dir.path(), "src/bin");
        assert!(inside(dir.path(), "src/bin").is_some());
        for path in ["Src/bin", "src/Bin", "SRC/new", "src/BIN/new"] {
            assert!(inside(dir.path(), path).is_none(), "{path}");
        }
        assert!(inside(dir.path(), "new/cache").is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cleanup_preserves_tracked_sources_with_case_mismatched_output() {
        let (_dir, _main, work) = project();
        output(&work, "src/bin");
        git::git(&work, &["add", "src"]).unwrap();
        let config = CacheConfig { outputs: vec!["Src/bin".into()], ..Default::default() };
        assert!(clean(&work, &config).unwrap().is_empty());
        assert!(work.join("src/bin/artifact").is_file());
    }

    #[test]
    fn repository_cache_paths_cannot_escape_a_different_worktree_but_user_overrides_can() {
        let (dir, _main, work) = project();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, work.join(".cache")).unwrap();
        let home = dir.path().join("state");
        let mut config = CacheConfig::default();
        config.repo_paths.insert("PIP_CACHE_DIR".into());
        let mut env = BTreeMap::from([("PIP_CACHE_DIR".into(), ".cache/sccache".into())]);
        resolve_env_paths(&home, &work, &config, &mut env);
        assert!(env.is_empty());
        config.repo_paths.clear();
        env.insert("PIP_CACHE_DIR".into(), ".cache/sccache".into());
        resolve_env_paths(&home, &work, &config, &mut env);
        assert_eq!(env["PIP_CACHE_DIR"], work.join(".cache/sccache").to_string_lossy());
        std::fs::remove_file(work.join(".cache")).unwrap();
        config.shared.push(".cache/sccache".into());
        config.repo_paths.insert("PIP_CACHE_DIR".into());
        link(&home, &work, &config, Domain::Host).unwrap();
        env.insert("PIP_CACHE_DIR".into(), ".cache/sccache".into());
        resolve_env_paths(&home, &work, &config, &mut env);
        assert!(env.contains_key("PIP_CACHE_DIR"), "managed cache links are allowed");
    }

    #[test]
    fn shared_links_are_per_repository_and_never_replace_existing_folders() {
        let (dir, main, one) = project();
        let two = dir.path().join("two");
        git::add_worktree(&main, &two, "two", "main").unwrap();
        let config = CacheConfig { shared: vec![".cache/sccache".into()], ..Default::default() };
        let state = dir.path().join("state");
        link(&state, &one, &config, Domain::Host).unwrap();
        link(&state, &two, &config, Domain::Host).unwrap();
        let first = one.join(".cache/sccache").canonicalize().unwrap();
        assert_eq!(first, two.join(".cache/sccache").canonicalize().unwrap());
        assert!(first.starts_with(state.canonicalize().unwrap().join("build-caches/host-repos-v2")));
        assert!(link(&state, &main, &config, Domain::Host).is_err());
        let (_other_dir, _other_main, other) = project();
        link(&state, &other, &config, Domain::Host).unwrap();
        assert_ne!(first, other.join(".cache/sccache").canonicalize().unwrap());
        std::fs::remove_file(two.join(".cache/sccache")).unwrap();
        output(&two, ".cache/sccache");
        link(&state, &two, &config, Domain::Host).unwrap();
        assert!(two.join(".cache/sccache/artifact").is_file());
        assert!(!two.join(".cache/sccache").symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn host_and_sandbox_stores_are_disjoint_and_tracked_stores_are_refused() {
        let (dir, main, host) = project();
        let guest = dir.path().join("guest");
        git::add_worktree(&main, &guest, "guest", "main").unwrap();
        let config = CacheConfig { shared: vec![".nuget/packages".into()], ..Default::default() };
        let home = dir.path().join("state");
        link(&home, &host, &config, Domain::Host).unwrap();
        link(&home, &guest, &config, Domain::Sandbox).unwrap();
        let host_stores = stores(&home, &host, &config, Domain::Host);
        let guest_stores = stores(&home, &guest, &config, Domain::Sandbox);
        assert_eq!((host_stores.len(), guest_stores.len()), (1, 1));
        assert_ne!(host_stores, guest_stores);
        assert!(stores(&home, &guest, &config, Domain::Host).is_empty());
        assert!(link(&home, &guest, &config, Domain::Host).is_err());
        assert!(link(&home, &host, &config, Domain::Sandbox).is_err());
        let mut guest_env =
            BTreeMap::from([("NUGET_PACKAGES".into(), guest.join(".nuget/packages").to_string_lossy().into_owned())]);
        resolve_env_paths(&home, &host, &config, &mut guest_env);
        assert!(guest_env.is_empty(), "host builds must never read sandbox stores");
        assert!(!domain_store_safe(&home, &machine_cache().with_file_name("sccache"), Domain::Host));
        output(&main, "tracked-store");
        git::git(&main, &["add", "tracked-store"]).unwrap();
        assert!(!safe_store(&main.join("tracked-store")));
        let mut env =
            BTreeMap::from([("SCCACHE_DIR".into(), main.join("tracked-store").to_string_lossy().into_owned())]);
        resolve_env_paths(&home, &host, &config, &mut env);
        assert!(env.is_empty());
        let redirected = repository_store(&home, &main.canonicalize().unwrap(), Domain::Sandbox);
        std::fs::remove_dir_all(&redirected).unwrap();
        std::os::unix::fs::symlink(main.join("tracked-store"), &redirected).unwrap();
        assert!(stores(&home, &guest, &config, Domain::Sandbox).is_empty());
    }

    #[test]
    fn cleanup_keeps_main_tracked_files_links_sources_and_shared_stores() {
        let (dir, main, work) = project();
        output(&main, "target");
        output(&work, "target");
        output(&work, "target/nested");
        output(&work, "custom-output");
        output(&work, "src");
        git::git(&work, &["add", "src"]).unwrap();
        output(&work, "keep");
        let outside = dir.path().join("outside");
        output(&outside, "target");
        std::os::unix::fs::symlink(&outside, work.join("linked")).unwrap();
        let config = CacheConfig {
            shared: vec![".cache/sccache".into()],
            outputs: [
                "target/nested",
                "custom-output",
                "src",
                "linked/target",
                "../outside/target",
                ".cache",
                ".cache/sccache",
            ]
            .map(String::from)
            .into(),
            ..Default::default()
        };
        link(&dir.path().join("state"), &work, &config, Domain::Host).unwrap();
        std::fs::write(work.join(".cache/sccache/cached"), "shared").unwrap();
        let inventory = inventory(&work, &config);
        assert_eq!(inventory.len(), 2);
        assert!(clean(&main, &config).is_err());
        clean(&work, &config).unwrap();
        assert!(!work.join("target").exists() && !work.join("custom-output").exists());
        for path in [
            main.join("target/artifact"),
            work.join("src/artifact"),
            work.join("keep/artifact"),
            outside.join("target/artifact"),
            work.join(".cache/sccache/cached"),
        ] {
            assert!(path.is_file(), "{}", path.display());
        }
    }

    #[test]
    fn cleanup_waits_for_a_process_cwd_or_open_file_and_then_reclaims_output() {
        for holds_file in [false, true] {
            let (_dir, _main, work) = project();
            output(&work, "target");
            let mut command = Command::new(crate::test_support::program("sleep"));
            command.arg("60");
            if holds_file {
                command.stdin(std::fs::File::open(work.join("target/artifact")).unwrap());
            } else {
                command.current_dir(&work).stdin(Stdio::null());
            }
            let mut process = command.spawn().unwrap();
            drop(command);
            let kept = clean(&work, &CacheConfig::default());
            process.kill().unwrap();
            process.wait().unwrap();
            assert!(kept.unwrap_err().to_string().contains("in use"));
            assert!(work.join("target/artifact").is_file());
            clean(&work, &CacheConfig::default()).unwrap();
            assert!(!work.join("target").exists());
        }
    }
}
