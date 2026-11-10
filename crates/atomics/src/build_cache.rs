//! Filesystem operations for cache links and disposable worktree output.
mod output;

use std::collections::{BTreeMap, HashSet};
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
            && folder.to_str().is_some_and(str::is_ascii)
            && git::git(&root, &["ls-files", "--", &format!(":(literal,icase){}", folder.to_string_lossy())])
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

fn resolved_location(path: &Path) -> Option<PathBuf> {
    paths::resolved_path(path).ok()
}

fn forbidden_store(home: &Path, path: &Path, domain: Domain) -> bool {
    let Some(path) = resolved_location(path) else { return false };
    let mut forbidden = match domain {
        Domain::Host => vec![home.join("build-caches/sandbox-repos-v2"), home.join("build-caches/sandboxes-v2")],
        Domain::Sandbox => vec![home.join("build-caches/host-repos-v2"), machine_cache()],
    };
    forbidden.extend([home.join("build-caches/repos"), machine_cache().with_file_name("sccache")]);
    forbidden.iter().filter_map(|root| resolved_location(root)).any(|root| path.starts_with(root))
}

fn domain_store_safe(home: &Path, path: &Path, domain: Domain) -> bool {
    resolved_location(path).is_some() && !forbidden_store(home, path, domain)
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
    let mut path = resolved_location(root)?;
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

#[derive(Default)]
pub struct CacheSetup {
    pub skipped: Vec<PathBuf>,
}

impl CacheSetup {
    pub fn filter_env(&self, env: &mut BTreeMap<String, String>) {
        env.retain(|key, value| {
            let skipped = path_variable(key)
                && self.skipped.iter().any(|path| {
                    Path::new(value).starts_with(path)
                        || resolved_location(Path::new(value))
                            .zip(resolved_location(path))
                            .is_some_and(|(value, path)| value.starts_with(path))
                });
            if skipped {
                eprintln!("ATOMICS: ignored cache.env.{key}: cache setup skipped this folder");
            }
            !skipped
        });
        if !env.contains_key("SCCACHE_DIR") {
            for key in ["RUSTC_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER"] {
                if env.get(key).is_some_and(|value| atomics_core::cache::is_sccache(value)) {
                    env.insert(key.into(), String::new());
                }
            }
        }
    }
}

/// Cache setup is optional. Each folder is independent, and forbidden links are
/// removed even when the repository no longer configures them. No cache data is copied.
pub fn link(home: &Path, worktree: &Path, config: &CacheConfig, domain: Domain) -> Result<CacheSetup> {
    let (root, main) = linked_worktree(worktree)?;
    let store = repository_store(home, &main, domain);
    let protected: Vec<_> = registered_worktrees(&main)?
        .into_iter()
        .filter_map(|path| {
            let path = path.canonicalize().unwrap_or(path);
            (path != root && path.starts_with(&root)).then_some(path)
        })
        .collect();
    let mut locations = config.shared.clone();
    locations.extend(atomics_core::cache::REPO_SHARED.iter().map(|path| (*path).to_owned()));
    locations.sort();
    locations.dedup();
    let mut setup = CacheSetup {
        skipped: output::repair_links(&root, &locations, &protected, |target| forbidden_store(home, target, domain))?,
    };
    for folder in &config.shared {
        if !relative(folder) {
            continue;
        }
        let path = root.join(folder);
        if !folder.is_ascii() {
            eprintln!("ATOMICS: cache setup skipped {}: Git cannot verify Unicode case aliases", path.display());
            setup.skipped.push(path);
            continue;
        }
        if setup.skipped.iter().any(|removed| path.starts_with(removed) || removed.starts_with(&path)) {
            continue;
        }
        let result = (|| -> Result<()> {
            let source = store.join(folder);
            if let Ok(meta) = path.symlink_metadata() {
                if meta.file_type().is_symlink() {
                    ensure!(
                        path.canonicalize().ok() == source.canonicalize().ok() && source.canonicalize().is_ok(),
                        "cache link is not the managed store"
                    );
                    ensure!(
                        store_allowed(home, &source, domain),
                        "cache store contains tracked files or belongs to another trust domain"
                    );
                }
                return Ok(());
            }
            let destination = inside(&root, folder).context("unsafe cache destination")?;
            let source = inside(&store, folder).context("unsafe cache store")?;
            ensure!(
                store_allowed(home, &source, domain),
                "cache store contains tracked files or belongs to another trust domain"
            );
            ensure!(
                git::git(&root, &["ls-files", "--", &format!(":(literal,icase){folder}")])?.is_empty(),
                "cache path is tracked"
            );
            std::fs::create_dir_all(&source)?;
            std::fs::create_dir_all(destination.parent().context("cache parent")?)?;
            std::os::unix::fs::symlink(&source, &destination)?;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("ATOMICS: cache setup skipped {}: {error:#}", path.display());
            setup.skipped.push(path);
        }
    }
    Ok(setup)
}

/// Includes already-linked stores, for explicit mounts in Linux sandboxes.
pub fn stores(home: &Path, worktree: &Path, config: &CacheConfig, domain: Domain) -> Vec<PathBuf> {
    let Ok((root, main)) = linked_worktree(worktree) else { return Vec::new() };
    let store = repository_store(home, &main, domain);
    config
        .shared
        .iter()
        .filter(|folder| relative(folder) && folder.is_ascii())
        .filter_map(|folder| {
            let path = root.join(folder);
            let expected = inside(&store, folder)?.canonicalize().ok()?;
            if !safe_store(&expected) || !domain_store_safe(home, &expected, domain) {
                return None;
            }
            if !git::git(&root, &["ls-files", "--", &format!(":(literal,icase){folder}")])
                .is_ok_and(|files| files.is_empty())
            {
                return None;
            }
            (path.symlink_metadata().ok()?.file_type().is_symlink() && path.canonicalize().ok()? == expected)
                .then_some(expected)
        })
        .collect()
}

/// Git must ignore the directory itself using a repository-owned rule. Below an
/// already ignored directory Git never consults nested .gitignore files.
fn ignore_provenance(root: &Path, folder: &str) -> Result<()> {
    // A trailing slash alone is insufficient: target/* also matches target/,
    // although Git still enters target and reads target/.gitignore. Check both
    // spellings to prove the directory itself is excluded from traversal.
    let listed = git::git_input(
        root,
        &["-c", "core.excludesFile=/dev/null", "check-ignore", "-v", "--no-index", "-z", "--stdin"],
        format!("{folder}/\0{folder}\0").as_bytes(),
    )
    .context("output directory is not ignored by a repository rule")?;
    let fields: Vec<_> = listed.split('\0').collect();
    ensure!(
        fields.len() == 9 && fields[8].is_empty() && fields[3] == format!("{folder}/") && fields[7] == folder,
        "output directory itself has no verified ignore rule"
    );
    let exclude = git::git(root, &["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"])?;
    let exclude = Path::new(exclude.strip_suffix('\n').context("missing info/exclude path terminator")?);
    for rule in fields[..8].as_chunks::<4>().0 {
        ensure!(!rule[2].starts_with('!'), "output directory is not ignored");
        let source = Path::new(rule[0]);
        let source = if source.is_absolute() { source.to_path_buf() } else { root.join(source) };
        if source == exclude {
            ensure!(source.symlink_metadata()?.file_type().is_file(), "info/exclude is not a regular file");
            continue;
        }
        let relative = source.strip_prefix(root).context("ignore rule is outside the worktree")?;
        ensure!(
            relative.file_name().is_some_and(|name| name == ".gitignore")
                && root.join(folder).starts_with(source.parent().context("ignore rule has no parent")?),
            "ignore rule is not at or above the output"
        );
        let relative = relative.to_str().context("ignore rule path is not UTF-8")?;
        ensure!(
            inside(root, relative).as_ref() == Some(&source) && source.symlink_metadata()?.file_type().is_file(),
            "ignore rule is not a regular worktree file"
        );
        // Exact index spelling matters: an untracked .gitignore must not borrow
        // the identity of a tracked .GitIgnore on case-sensitive filesystems.
        let tracked = git::git(root, &["--literal-pathspecs", "ls-files", "-z", "--", relative])?;
        ensure!(
            tracked.split('\0').any(|path| path == relative),
            "output is hidden by an untracked ignore file: {relative}"
        );
    }
    Ok(())
}

fn ensure_disposable(root: &Path, folder: &str) -> Result<()> {
    // Git's icase pathspecs do not fold every Unicode case alias. Keep such
    // outputs rather than risk missing a differently cased index entry.
    ensure!(folder.is_ascii(), "Git cannot verify Unicode case aliases");
    let pathspec = format!(":(literal,icase){folder}");
    ensure!(
        git::git(root, &["-c", "core.excludesFile=/dev/null", "ls-files", "--", &pathspec])?.is_empty(),
        "output contains tracked files"
    );
    ensure!(
        git::git(
            root,
            &[
                "-c",
                "core.excludesFile=/dev/null",
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored=no",
                "--",
                &pathspec
            ]
        )?
        .is_empty(),
        "output contains non-ignored work"
    );
    ignore_provenance(root, folder)
}

#[cfg(test)]
fn disposable(root: &Path, folder: &str) -> bool {
    ensure_disposable(root, folder).is_ok()
}

fn registered_worktrees(main: &Path) -> Result<Vec<PathBuf>> {
    // Newlines and quote characters are legal in worktree names. Only NUL
    // separators preserve them without ambiguity; the human-readable listing
    // is not suitable for deciding whether an output can be destroyed.
    let listed = git::git(main, &["worktree", "list", "--porcelain", "-z"])?;
    let mut paths = Vec::new();
    for record in listed.split("\0\0").filter(|record| !record.is_empty()) {
        let first = record.split('\0').next().context("missing worktree record")?;
        let path = PathBuf::from(first.strip_prefix("worktree ").context("unrecognized worktree record")?);
        ensure!(path.is_absolute(), "cannot verify a listed worktree path");
        paths.push(path);
    }
    ensure!(!paths.is_empty(), "no worktrees could be verified");
    Ok(paths)
}

fn no_nested_worktrees(root: &Path, path: &Path) -> Result<()> {
    let main = git::main_checkout(root)?;
    for entry in registered_worktrees(&main)? {
        let overlaps = |nested: &Path| {
            nested.starts_with(path) || (nested != root && nested.starts_with(root) && path.starts_with(nested))
        };
        ensure!(
            !overlaps(&entry) && !entry.canonicalize().is_ok_and(|nested| overlaps(&nested)),
            "output contains a registered worktree: {}",
            entry.display()
        );
    }
    Ok(())
}

fn checked_output(root: &Path, folder: &str) -> Result<output::Output> {
    let path = inside(root, folder).context("output is not an exact, unlinked worktree path")?;
    ensure!(device(root).is_some() && device(&path) == device(root), "output is on another filesystem");
    no_nested_worktrees(root, &path)?;
    let output = output::Output::open(root, folder)?;
    output.inspect()?;
    Ok(output)
}

fn candidate_paths(root: &Path, config: &CacheConfig) -> Vec<PathBuf> {
    let mut folders = config.outputs.clone();
    if root.join("Cargo.toml").is_file() {
        folders.push("target".into());
    }
    let mut found: Vec<_> = folders
        .into_iter()
        .filter(|folder| relative(folder))
        .map(|folder| root.join(folder))
        .filter(|path| path.symlink_metadata().is_ok_and(|meta| meta.is_dir() || meta.is_symlink()))
        .collect();
    found.sort();
    found.dedup();
    found
}

fn outermost(found: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = Vec::new();
    for path in found {
        if !parents.iter().any(|parent| path.starts_with(parent)) {
            parents.push(path);
        }
    }
    parents
}

fn eligible_output(root: &Path, folder: &str, config: &CacheConfig) -> Result<output::Output> {
    ensure!(
        !config.shared.iter().any(|shared| relative(shared)
            && (Path::new(folder).starts_with(shared) || Path::new(shared).starts_with(folder))),
        "output overlaps a shared cache store"
    );
    ensure_disposable(root, folder)?;
    checked_output(root, folder)
}

fn output_paths(root: &Path, config: &CacheConfig) -> Vec<PathBuf> {
    outermost(
        candidate_paths(root, config)
            .into_iter()
            .filter(|path| {
                path.strip_prefix(root)
                    .ok()
                    .and_then(Path::to_str)
                    .is_some_and(|folder| eligible_output(root, folder, config).is_ok())
            })
            .collect(),
    )
}

/// Share file identities across all candidates and worktrees in one inventory.
pub fn inventories(worktrees: &[(PathBuf, CacheConfig)]) -> Vec<BuildOutput> {
    let mut seen = HashSet::new();
    worktrees.iter().flat_map(|(worktree, config)| inventory_with_seen(worktree, config, &mut seen)).collect()
}

fn inventory_with_seen(
    worktree: &Path,
    config: &CacheConfig,
    seen: &mut HashSet<output::Identity>,
) -> Vec<BuildOutput> {
    let Ok((root, _)) = linked_worktree(worktree) else { return Vec::new() };
    outermost(candidate_paths(&root, config))
        .into_iter()
        .map(|path| {
            let folder = path.strip_prefix(&root).unwrap().to_string_lossy();
            let mut reason = eligible_output(&root, &folder, config).err().map(|error| format!("{error:#}"));
            // A refused output stays visible. Measurement uses the same no-follow,
            // repository and mount guards, but requires no write access.
            let measured = output::Output::open(&root, &folder).and_then(|output| output.measure(seen));
            let bytes = match measured {
                Ok(bytes) => bytes,
                Err(error) => {
                    let detail = format!("size unavailable: {error:#}");
                    reason = Some(reason.map_or_else(|| detail.clone(), |reason| format!("{reason}; {detail}")));
                    0
                }
            };
            BuildOutput { bytes, path: path.to_string_lossy().into_owned(), reason }
        })
        .collect()
}

#[cfg(test)]
fn inventory(worktree: &Path, config: &CacheConfig) -> Vec<BuildOutput> {
    inventory_with_seen(worktree, config, &mut HashSet::new())
}

pub fn partial_removal(error: &anyhow::Error) -> Option<String> {
    error.downcast_ref::<output::PartialRemoval>().map(ToString::to_string)
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
        checked_output(&root, &folder)?.remove_if(|| {
            no_nested_worktrees(&root, &path)?;
            ensure_disposable(&root, &folder)?;
            Ok(())
        })?;
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
        assert!(!disposable(&work, "target"));
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
    fn cleanup_preserves_nested_repositories_and_registered_worktrees_in_ignored_output() {
        for kind in ["linked", "registered-without-marker", "registered-with-quoted-path", "independent"] {
            let (_dir, main, work) = project();
            let nested = work.join(if kind == "registered-with-quoted-path" {
                "target/experiment\nquoted"
            } else {
                "target/experiment"
            });
            if kind == "independent" {
                std::fs::create_dir_all(&nested).unwrap();
                git::git(&nested, &["init", "-q"]).unwrap();
            } else {
                git::add_worktree(&main, &nested, "nested", "main").unwrap();
                if kind.starts_with("registered-") {
                    std::fs::remove_file(nested.join(".git")).unwrap();
                }
            }
            std::fs::write(nested.join("new.rs"), "uncommitted source").unwrap();
            std::fs::create_dir(nested.join("out")).unwrap();
            std::fs::write(nested.join("out/new.rs"), "more source").unwrap();
            assert!(disposable(&work, "target"), "outer Git cannot see nested work");
            let config = CacheConfig {
                outputs: vec![nested.join("out").strip_prefix(&work).unwrap().to_string_lossy().into_owned()],
                ..Default::default()
            };
            assert!(inventory(&work, &config).iter().all(|output| output.reason.is_some()));
            assert!(clean(&work, &config).unwrap().is_empty());
            assert!(nested.join("out/new.rs").is_file());
            assert_eq!(std::fs::read_to_string(nested.join("new.rs")).unwrap(), "uncommitted source", "{kind}");
        }
    }

    #[test]
    fn index_case_mismatch_never_makes_tracked_sources_disposable() {
        for (indexed, disk) in [("src", "Src"), ("Äpp", "äpp")] {
            let (_dir, _main, work) = project();
            output(&work, &format!("{indexed}/bin"));
            git::git(&work, &["add", indexed]).unwrap();
            std::fs::rename(work.join(indexed), work.join("temporary-case")).unwrap();
            std::fs::rename(work.join("temporary-case"), work.join(disk)).unwrap();
            git::git(&work, &["config", "core.ignorecase", "true"]).unwrap();
            std::fs::write(work.join(".gitignore"), format!("target/\n{disk}/\n")).unwrap();
            let folder = format!("{disk}/bin");
            assert!(inside(&work, &folder).is_some(), "the output matches the disk exactly");
            assert!(!disposable(&work, &folder));
            assert!(!safe_store(&work.join(&folder)), "tracked stores also require case protection");
            let config = CacheConfig { outputs: vec![folder], ..Default::default() };
            assert!(clean(&work, &config).unwrap().is_empty());
            assert!(work.join(disk).join("bin/artifact").is_file());
        }
    }

    #[test]
    fn global_ignore_rules_and_untracked_ignore_files_cannot_hide_new_work() {
        let (dir, _main, work) = project();
        let global = dir.path().join("global-ignore");
        std::fs::write(&global, "src/bin/\n").unwrap();
        git::git(&work, &["config", "core.excludesFile", global.to_str().unwrap()]).unwrap();
        output(&work, "src/bin");
        let config = CacheConfig { outputs: vec!["src/bin".into()], ..Default::default() };
        assert!(clean(&work, &config).unwrap().is_empty());
        assert!(work.join("src/bin/artifact").is_file());
        // Even ignored ignore-files must not hide new source under an output.
        output(&work, "build/out");
        std::fs::write(work.join("build/.gitignore"), "*\n").unwrap();
        assert!(!disposable(&work, "build/out"));
        let config = CacheConfig { outputs: vec!["build/out".into()], ..Default::default() };
        assert!(clean(&work, &config).unwrap().is_empty());
        assert!(work.join("build/out/artifact").is_file());
    }

    #[test]
    fn nested_ignore_files_below_an_ignored_output_do_not_block_cleanup() {
        let (_dir, _main, work) = project();
        output(&work, "target/debug/build/vendor/out/source");
        std::fs::write(work.join("target/debug/build/vendor/out/source/.gitignore"), "*\n").unwrap();
        std::fs::write(work.join("target/.gitignore"), "*\n").unwrap();
        let outputs = inventory(&work, &CacheConfig::default());
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].reason, None);
        assert!(outputs[0].bytes > 0);
        assert_eq!(clean(&work, &CacheConfig::default()).unwrap().len(), 1);
        assert!(!work.join("target").exists());
    }

    #[test]
    fn ignoring_only_output_contents_does_not_trust_a_nested_untracked_ignore_file() {
        let (_dir, _main, work) = project();
        std::fs::write(work.join(".gitignore"), "target/*\n!target/new.rs\n").unwrap();
        output(&work, "target");
        std::fs::write(work.join("target/.gitignore"), "*\n").unwrap();
        std::fs::write(work.join("target/new.rs"), "untracked source").unwrap();
        assert!(
            git::git(&work, &["status", "--porcelain", "--untracked-files=all", "--ignored=no", "--", "target"])
                .unwrap()
                .is_empty(),
            "status alone cannot see the source"
        );
        assert!(!disposable(&work, "target"));
        assert!(clean(&work, &CacheConfig::default()).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(work.join("target/new.rs")).unwrap(), "untracked source");
        assert!(inventory(&work, &CacheConfig::default())[0].reason.is_some());
    }

    #[test]
    fn inventory_and_warning_explain_an_untracked_ancestor_ignore_rule() {
        let (_dir, _main, work) = project();
        output(&work, "build/out");
        std::fs::write(work.join("build/.gitignore"), "*\n").unwrap();
        let config = CacheConfig { outputs: vec!["build/out".into()], ..Default::default() };
        let outputs = inventory(&work, &config);
        assert_eq!(outputs.len(), 1);
        assert!(outputs[0].bytes > 0, "refusal must not hide allocated bytes");
        assert!(outputs[0].reason.as_ref().unwrap().contains("untracked ignore file"));
        let warning = atomics_core::machine::DiskWarning { largest: outputs, ..Default::default() }.message();
        assert!(warning.contains("build/out") && warning.contains("untracked ignore file"));
        assert!(clean(&work, &config).unwrap().is_empty());
        assert!(work.join("build/out/artifact").is_file());
    }

    #[test]
    fn output_ignore_provenance_accepts_info_exclude_but_not_a_case_alias() {
        let (_dir, main, work) = project();
        std::fs::write(main.join(".git/info/exclude"), "dist/\n").unwrap();
        output(&work, "dist");
        let config = CacheConfig { outputs: vec!["dist".into()], ..Default::default() };
        assert_eq!(clean(&work, &config).unwrap().len(), 1);
        // A differently spelled tracked file is not the untracked ignore file.
        output(&work, "build/out");
        std::fs::write(work.join("build/.GitIgnore"), "tracked source").unwrap();
        git::git(&work, &["add", "build/.GitIgnore"]).unwrap();
        std::fs::write(work.join("build/.gitignore"), "*\n").unwrap();
        assert!(!disposable(&work, "build/out"));
    }

    #[test]
    fn cache_link_repair_visits_only_configured_or_allowlisted_paths() {
        let (dir, _main, work) = project();
        let home = dir.path().join("state");
        let legacy = home.join("build-caches/repos/old/cache");
        output(&legacy, "");
        for folder in ["target/debug/unrelated", "custom/cache", ".cache/pip"] {
            let path = work.join(folder);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&legacy, path).unwrap();
        }
        let first = link(&home, &work, &CacheConfig::default(), Domain::Host).unwrap();
        assert_eq!(first.skipped, [work.canonicalize().unwrap().join(".cache/pip")]);
        assert!(work.join("target/debug/unrelated").symlink_metadata().unwrap().is_symlink());
        assert!(work.join("custom/cache").symlink_metadata().unwrap().is_symlink());
        let config = CacheConfig { shared: vec!["custom/cache".into()], ..Default::default() };
        let second = link(&home, &work, &config, Domain::Host).unwrap();
        assert_eq!(second.skipped, [work.canonicalize().unwrap().join("custom/cache")]);
        assert!(work.join("target/debug/unrelated").symlink_metadata().unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(legacy.join("artifact")).unwrap(), "compiled");
    }

    #[test]
    fn inventory_counts_hard_linked_blocks_once_across_outputs_and_worktrees() {
        use std::os::unix::fs::MetadataExt;
        let (dir, main, one) = project();
        let two = dir.path().join("two");
        git::add_worktree(&main, &two, "two", "main").unwrap();
        for path in [one.join("target"), one.join("custom-output"), two.join("target")] {
            std::fs::create_dir(&path).unwrap();
        }
        let artifact = one.join("target/binary");
        std::fs::write(&artifact, vec![1; 65536]).unwrap();
        for path in [one.join("target/alias"), one.join("custom-output/binary"), two.join("target/binary")] {
            std::fs::hard_link(&artifact, path).unwrap();
        }
        let directory_bytes: u64 = [one.join("target"), one.join("custom-output"), two.join("target")]
            .iter()
            .map(|path| path.metadata().unwrap().blocks() * 512)
            .sum();
        let expected = artifact.metadata().unwrap().blocks() * 512 + directory_bytes;
        let config = CacheConfig { outputs: vec!["custom-output".into()], ..Default::default() };
        let outputs = inventories(&[(one.clone(), config.clone()), (two.clone(), config.clone())]);
        assert_eq!(outputs.len(), 3);
        assert!(outputs.iter().all(|output| output.reason.is_none()));
        assert_eq!(outputs.iter().map(|output| output.bytes).sum::<u64>(), expected);
        // Removing these names must still preserve links outside the outputs.
        std::fs::hard_link(&artifact, dir.path().join("keep")).unwrap();
        clean(&one, &config).unwrap();
        clean(&two, &config).unwrap();
        assert_eq!(std::fs::read(dir.path().join("keep")).unwrap(), vec![1; 65536]);
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
        let switched = link(&home, &guest, &config, Domain::Host).unwrap();
        assert_eq!(switched.skipped.len(), 1);
        assert!(!guest.join(".nuget/packages").exists());
        let mut guest_env =
            BTreeMap::from([("NUGET_PACKAGES".into(), guest.join(".nuget/packages").to_string_lossy().into_owned())]);
        resolve_env_paths(&home, &host, &config, &mut guest_env);
        switched.filter_env(&mut guest_env);
        assert!(
            guest_env.is_empty(),
            "host builds must never read sandbox stores: env={guest_env:?}, skipped={:?}",
            switched.skipped
        );
        assert!(!domain_store_safe(&home, &machine_cache().with_file_name("sccache"), Domain::Host));
        output(&main, "tracked-store");
        git::git(&main, &["add", "tracked-store"]).unwrap();
        assert!(!safe_store(&main.join("tracked-store")));
        let mut env =
            BTreeMap::from([("SCCACHE_DIR".into(), main.join("tracked-store").to_string_lossy().into_owned())]);
        resolve_env_paths(&home, &host, &config, &mut env);
        assert!(env.is_empty(), "tracked cache environment survived: {env:?}");
        let redirected = repository_store(&home, &main.canonicalize().unwrap(), Domain::Sandbox);
        std::fs::remove_dir_all(&redirected).unwrap();
        std::os::unix::fs::symlink(main.join("tracked-store"), &redirected).unwrap();
        assert!(stores(&home, &guest, &config, Domain::Sandbox).is_empty());
    }

    #[test]
    fn cache_setup_skips_a_failed_folder_and_continues_with_independent_folders() {
        let (dir, _main, work) = project();
        let alias = dir.path().join("work-alias");
        std::os::unix::fs::symlink(&work, &alias).unwrap();
        let home = dir.path().join("state");
        let ordinary = dir.path().join("ordinary");
        std::fs::create_dir(&ordinary).unwrap();
        std::fs::create_dir(work.join(".cache")).unwrap();
        std::os::unix::fs::symlink(&ordinary, work.join(".cache/wrong")).unwrap();
        let config = CacheConfig { shared: vec![".cache/wrong".into(), ".cache/pip".into()], ..Default::default() };
        let setup = link(&home, &work, &config, Domain::Host).unwrap();
        assert_eq!(setup.skipped, [work.canonicalize().unwrap().join(".cache/wrong")]);
        assert!(work.join(".cache/pip").symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(work.join(".cache/wrong").canonicalize().unwrap(), ordinary.canonicalize().unwrap());
        let mut env =
            BTreeMap::from([("PIP_CACHE_DIR".into(), alias.join(".cache/wrong").to_string_lossy().into_owned())]);
        setup.filter_env(&mut env);
        assert!(env.is_empty(), "skipped cache environment survived: env={env:?}, skipped={:?}", setup.skipped);
    }

    #[test]
    fn obsolete_forbidden_links_are_removed_without_configuration_or_data_migration() {
        let (dir, main, work) = project();
        let home = dir.path().join("state");
        let legacy = home.join("build-caches/repos/old/cache");
        let guest = home.join("build-caches/sandbox-repos-v2/repo/cache");
        let ordinary = dir.path().join("ordinary");
        for store in [&legacy, &guest, &ordinary] {
            output(store, "");
        }
        for (folder, store) in [(".yarn/cache", &legacy), (".nuget/packages", &guest), (".local/cache", &ordinary)] {
            let path = work.join(folder);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(store, path).unwrap();
        }
        let nested = work.join("target/nested");
        git::add_worktree(&main, &nested, "nested", "main").unwrap();
        std::fs::remove_file(nested.join(".git")).unwrap();
        std::fs::create_dir(nested.join(".yarn")).unwrap();
        std::os::unix::fs::symlink(&legacy, nested.join(".yarn/cache")).unwrap();
        let setup = link(&home, &work, &CacheConfig::default(), Domain::Host).unwrap();
        assert!(
            nested.join(".yarn/cache").symlink_metadata().unwrap().file_type().is_symlink(),
            "a nested worktree must be repaired separately"
        );
        assert_eq!(setup.skipped.len(), 2);
        for folder in [".yarn/cache", ".nuget/packages"] {
            assert!(work.join(folder).symlink_metadata().is_err());
        }
        assert!(work.join(".local/cache").symlink_metadata().unwrap().file_type().is_symlink());
        for store in [&legacy, &guest, &ordinary] {
            assert_eq!(std::fs::read_to_string(store.join("artifact")).unwrap(), "compiled");
        }
        assert!(link(&home, &work, &CacheConfig::default(), Domain::Host).unwrap().skipped.is_empty());
    }

    #[test]
    fn repositories_sandbox_package_stores_are_shared_within_the_documented_domain() {
        let (dir, main, one) = project();
        let two = dir.path().join("two");
        git::add_worktree(&main, &two, "two", "main").unwrap();
        let home = dir.path().join("state");
        let config = CacheConfig { shared: vec![".nuget/packages".into()], ..Default::default() };
        link(&home, &one, &config, Domain::Sandbox).unwrap();
        link(&home, &two, &config, Domain::Sandbox).unwrap();
        assert_eq!(stores(&home, &one, &config, Domain::Sandbox), stores(&home, &two, &config, Domain::Sandbox));
        assert!(stores(&home, &one, &config, Domain::Host).is_empty());
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
        assert_eq!(inventory.iter().filter(|output| output.reason.is_none()).count(), 2);
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
