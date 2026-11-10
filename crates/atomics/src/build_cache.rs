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

pub fn machine_cache() -> PathBuf {
    // Deliberately independent of ATOMICS_HOME: one compiler cache for this user's machine.
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| paths::home().join(".cache"))
        .join("atomics/sccache")
}

pub fn agent_env(cwd: &Path, config: &CacheConfig) -> BTreeMap<String, String> {
    let root = root(cwd);
    let cargo = root.join("Cargo.toml").is_file();
    let user_config = std::env::var_os("CARGO_HOME").map(PathBuf::from).unwrap_or_else(|| paths::home().join(".cargo"));
    let wrapper_set = ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"].iter().any(|key| std::env::var_os(key).is_some())
        || cwd.ancestors().map(|dir| dir.join(".cargo")).chain([user_config]).any(|dir| {
            ["config.toml", "config"]
                .iter()
                .any(|name| std::fs::read_to_string(dir.join(name)).is_ok_and(|text| text.contains("rustc-wrapper")))
        });
    let sccache = (!wrapper_set).then(|| crate::setup::find_program("sccache")).flatten();
    let mut env =
        config.agent_env(cargo, sccache.as_ref().and_then(|p| p.to_str()), &machine_cache().to_string_lossy(), |key| {
            std::env::var(key).ok()
        });
    resolve_env_paths(&paths::atomics_home(), &root, config, &mut env);
    env
}

pub fn resolve_env_paths(home: &Path, root: &Path, config: &CacheConfig, env: &mut BTreeMap<String, String>) {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let shared = stores(home, root, config);
    env.retain(|key, value| {
        if path_variable(key) {
            let path = if relative(value) { root.join(&*value) } else { paths::expand(value) };
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

/// No symlink component is traversed, including links back inside the checkout.
fn inside(root: &Path, relative_path: &str) -> Option<PathBuf> {
    if !relative(relative_path) {
        return None;
    }
    let mut path = root.to_path_buf();
    for part in Path::new(relative_path) {
        path.push(part);
        if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            return None;
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
pub fn link(home: &Path, worktree: &Path, config: &CacheConfig) -> Result<Vec<PathBuf>> {
    if config.shared.is_empty() {
        return Ok(Vec::new());
    }
    let (root, main) = linked_worktree(worktree)?;
    let store = home.join("build-caches/repos").join(paths::runtime_home_hash(&main));
    let mut linked = Vec::new();
    for folder in &config.shared {
        let Some(destination) = inside(&root, folder) else { continue };
        let source = store.join(folder);
        if destination.symlink_metadata().is_ok() {
            continue;
        }
        // The state folder is user-owned. Still refuse altered store components.
        let Some(source) = inside(&store, folder).filter(|p| *p == source) else { continue };
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
pub fn stores(home: &Path, worktree: &Path, config: &CacheConfig) -> Vec<PathBuf> {
    let Ok((root, main)) = linked_worktree(worktree) else { return Vec::new() };
    let store = home.join("build-caches/repos").join(paths::runtime_home_hash(&main));
    config
        .shared
        .iter()
        .filter(|folder| relative(folder))
        .filter_map(|folder| {
            let path = root.join(folder);
            let expected = store.join(folder).canonicalize().ok()?;
            (path.symlink_metadata().ok()?.file_type().is_symlink() && path.canonicalize().ok()? == expected)
                .then_some(expected)
        })
        .collect()
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
        if !git::git(root, &["--literal-pathspecs", "ls-files", "--", &folder]).is_ok_and(|files| files.is_empty()) {
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
        ensure!(
            git::git(&root, &["--literal-pathspecs", "ls-files", "--", &folder])?.is_empty(),
            "output gained tracked files during cleanup"
        );
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
        git::git(&main, &["add", "Cargo.toml"]).unwrap();
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
    fn repository_cache_paths_cannot_escape_a_different_worktree_but_user_overrides_can() {
        let (dir, _main, work) = project();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, work.join(".cache")).unwrap();
        let home = dir.path().join("state");
        let mut config = CacheConfig::default();
        config.repo_paths.insert("SCCACHE_DIR".into());
        let mut env = BTreeMap::from([("SCCACHE_DIR".into(), ".cache/sccache".into())]);
        resolve_env_paths(&home, &work, &config, &mut env);
        assert!(env.is_empty());
        config.repo_paths.clear();
        env.insert("SCCACHE_DIR".into(), ".cache/sccache".into());
        resolve_env_paths(&home, &work, &config, &mut env);
        assert_eq!(env["SCCACHE_DIR"], work.join(".cache/sccache").to_string_lossy());
        std::fs::remove_file(work.join(".cache")).unwrap();
        config.shared.push(".cache/sccache".into());
        config.repo_paths.insert("SCCACHE_DIR".into());
        link(&home, &work, &config).unwrap();
        env.insert("SCCACHE_DIR".into(), ".cache/sccache".into());
        resolve_env_paths(&home, &work, &config, &mut env);
        assert!(env.contains_key("SCCACHE_DIR"), "managed cache links are allowed");
    }

    #[test]
    fn shared_links_are_per_repository_and_never_replace_existing_folders() {
        let (dir, main, one) = project();
        let two = dir.path().join("two");
        git::add_worktree(&main, &two, "two", "main").unwrap();
        let config = CacheConfig { shared: vec![".cache/sccache".into()], ..Default::default() };
        let state = dir.path().join("state");
        link(&state, &one, &config).unwrap();
        link(&state, &two, &config).unwrap();
        let first = one.join(".cache/sccache").canonicalize().unwrap();
        assert_eq!(first, two.join(".cache/sccache").canonicalize().unwrap());
        assert!(first.starts_with(state.join("build-caches/repos")));
        assert!(link(&state, &main, &config).is_err());
        let (_other_dir, _other_main, other) = project();
        link(&state, &other, &config).unwrap();
        assert_ne!(first, other.join(".cache/sccache").canonicalize().unwrap());
        std::fs::remove_file(two.join(".cache/sccache")).unwrap();
        output(&two, ".cache/sccache");
        link(&state, &two, &config).unwrap();
        assert!(two.join(".cache/sccache/artifact").is_file());
        assert!(!two.join(".cache/sccache").symlink_metadata().unwrap().file_type().is_symlink());
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
        link(&dir.path().join("state"), &work, &config).unwrap();
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
