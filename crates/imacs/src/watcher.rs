//! Watches worktrees so their diffs follow file changes without polling. Where the platform
//! watches folder by folder (inotify), folders whose churn never changes a diff are left out,
//! so a build writing thousands of files into `target/` never reaches IMACS.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use notify::event::{EventKind, ModifyKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher as _, WatcherKind};

/// Folders whose churn (builds, dependencies, git internals) never changes the diff.
const IGNORED_DIRS: &[&str] = &[".git", "node_modules", "target", "bin", "obj", ".idea", ".vs", "dist"];

/// How long a burst of events settles before it is handled.
const SETTLE: Duration = Duration::from_millis(250);

#[derive(Debug, PartialEq)]
pub enum Change {
    /// Files changed in this worktree, named as it was given to [`Watcher::set_roots`].
    Files(PathBuf),
    /// A branch moved: a ref, `HEAD` or `packed-refs` changed in a watched repository.
    Refs,
}

pub struct Watcher {
    shared: Arc<Mutex<Shared>>,
    roots: Vec<PathBuf>,
}

struct Shared {
    watcher: RecommendedWatcher,
    /// Each watched worktree, with its path as the file system reports it.
    roots: Vec<(PathBuf, PathBuf)>,
    /// The folders watched one by one.
    folders: BTreeSet<PathBuf>,
}

impl Watcher {
    pub fn new(mut on_change: impl FnMut(Change) + Send + 'static) -> notify::Result<Watcher> {
        let (tx, events) = channel();
        // Reading a file changes nothing, and git's own reads would trigger the next refresh.
        let watcher = RecommendedWatcher::new(
            move |event: notify::Result<Event>| {
                if !event.as_ref().is_ok_and(|event| event.kind.is_access()) {
                    let _ = tx.send(event);
                }
            },
            notify::Config::default(),
        )?;
        let shared = Arc::new(Mutex::new(Shared { watcher, roots: Vec::new(), folders: BTreeSet::new() }));
        let weak = Arc::downgrade(&shared);
        std::thread::Builder::new()
            .name("imacs-watch".into())
            .spawn(move || settle(events, weak, &mut on_change))
            .map_err(notify::Error::io)?;
        Ok(Watcher { shared, roots: Vec::new() })
    }

    /// The worktrees watched, sorted.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Watches exactly these worktrees. One that cannot be watched yet, such as a worktree
    /// still being created, is left out, so the next call tries it again.
    pub fn set_roots(&mut self, wanted: &[PathBuf]) {
        let mut guard = self.shared.lock().unwrap();
        let shared = &mut *guard;
        let gone: Vec<_> = shared.roots.extract_if(.., |(root, _)| !wanted.contains(root)).collect();
        for (_, real) in gone {
            if !per_folder() {
                let _ = shared.watcher.unwatch(&real);
                continue;
            }
            // A worktree inside another keeps the folders the other one still wants.
            let unwanted: Vec<_> = below(&shared.folders, &real).filter(|folder| !shared.wanted(folder)).collect();
            for folder in unwanted {
                let _ = shared.watcher.unwatch(&folder);
                shared.folders.remove(&folder);
            }
        }
        for root in wanted {
            if shared.roots.iter().any(|(watched, _)| watched == root) {
                continue;
            }
            let Ok(real) = std::fs::canonicalize(root) else { continue };
            shared.roots.push((root.clone(), real.clone()));
            let watching = if per_folder() {
                shared.watch_folder(real)
            } else {
                shared.watcher.watch(&real, RecursiveMode::Recursive).is_ok()
            };
            if !watching {
                shared.roots.pop();
            }
        }
        shared.roots.sort();
        self.roots = shared.roots.iter().map(|(root, _)| root.clone()).collect();
    }
}

/// FSEvents and ReadDirectoryChangesW watch a whole tree through one handle and cannot leave a
/// folder out, so their events are filtered as they arrive. The other backends watch each folder
/// by itself anyway, and IMACS picks the folders.
fn per_folder() -> bool {
    !matches!(RecommendedWatcher::kind(), WatcherKind::Fsevent | WatcherKind::ReadDirectoryChangesWatcher)
}

/// Hands each burst of events over once it settles, until the watcher is dropped.
fn settle(events: Receiver<notify::Result<Event>>, shared: Weak<Mutex<Shared>>, on_change: &mut impl FnMut(Change)) {
    while let Ok(first) = events.recv() {
        let deadline = Instant::now() + SETTLE;
        let mut burst = vec![first];
        while let Some(left) = deadline.checked_duration_since(Instant::now())
            && let Ok(event) = events.recv_timeout(left)
        {
            burst.push(event);
        }
        let Some(shared) = shared.upgrade() else { return };
        shared.lock().unwrap().handle(burst.into_iter().flatten(), on_change);
    }
}

impl Shared {
    fn handle(&mut self, events: impl Iterator<Item = Event>, on_change: &mut impl FnMut(Change)) {
        let (mut changed, mut refs) = (BTreeSet::new(), false);
        let (mut added, mut gone) = (Vec::new(), Vec::new());
        for event in events {
            if event.need_rescan() {
                // Events were lost: look at every worktree again.
                refs = true;
                changed.extend(self.roots.iter().map(|(root, _)| root.clone()));
                added.extend(self.roots.iter().map(|(_, real)| real.clone()));
            }
            for path in event.paths {
                if is_ref(&path) {
                    refs = true;
                } else if let Some(root) = self.root_of(&path) {
                    changed.insert(root.clone());
                }
                match event.kind {
                    EventKind::Create(_) => added.push(path),
                    EventKind::Remove(_) => gone.push(path),
                    EventKind::Modify(ModifyKind::Name(_)) => {
                        added.push(path.clone());
                        gone.push(path);
                    }
                    _ => {}
                }
            }
        }
        if per_folder() {
            // Removals first, so a folder renamed within the burst stays watched by its new name.
            gone.sort();
            gone.dedup();
            for path in gone.iter().filter(|path| !is_dir(path)) {
                for folder in below(&self.folders, path).collect::<Vec<_>>() {
                    // The backend has usually dropped it already.
                    let _ = self.watcher.unwatch(&folder);
                    self.folders.remove(&folder);
                }
            }
            added.sort();
            added.dedup();
            for path in added {
                if is_dir(&path) && self.wanted(&path) {
                    self.watch_folder(path);
                }
            }
        }
        for root in changed {
            on_change(Change::Files(root));
        }
        if refs {
            on_change(Change::Refs);
        }
    }

    /// The innermost worktree holding `path`, unless the path is in one of its ignored folders.
    fn root_of(&self, path: &Path) -> Option<&PathBuf> {
        let (root, relative) = self
            .roots
            .iter()
            .filter_map(|(root, real)| Some((root, path.strip_prefix(real).ok()?)))
            .min_by_key(|(_, relative)| relative.components().count())?;
        (!relative.iter().any(|part| IGNORED_DIRS.iter().any(|dir| part == *dir))).then_some(root)
    }

    /// Whether a worktree holding `folder` wants it watched.
    fn wanted(&self, folder: &Path) -> bool {
        self.roots.iter().any(|(_, real)| folder.strip_prefix(real).is_ok_and(watched_folder))
    }

    /// Watches `folder`, and the wanted folders below it, each by itself. False if `folder`
    /// cannot be watched.
    fn watch_folder(&mut self, folder: PathBuf) -> bool {
        let mut pending = vec![folder.clone()];
        while let Some(next) = pending.pop() {
            if let Err(err) = self.watcher.watch(&next, RecursiveMode::NonRecursive) {
                if matches!(err.kind, notify::ErrorKind::MaxFilesWatch) {
                    eprintln!("IMACS: inotify watch limit reached; raise fs.inotify.max_user_watches");
                    break;
                }
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(&next) {
                pending.extend(
                    entries
                        .flatten()
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .map(|entry| entry.path())
                        .filter(|path| self.wanted(path)),
                );
            }
            self.folders.insert(next);
        }
        self.folders.contains(&folder)
    }
}

/// Whether a folder of a worktree is watched: ignored folders are not, except the parts of
/// `.git` where branches move (its top level, `refs/` and each linked worktree's `HEAD`).
fn watched_folder(relative: &Path) -> bool {
    let mut parts = relative.iter();
    while let Some(part) = parts.next() {
        if part == ".git" {
            return match parts.next() {
                None => true,
                Some(part) if part == "refs" => true,
                Some(part) if part == "worktrees" => parts.count() <= 1,
                Some(_) => false,
            };
        }
        if IGNORED_DIRS.iter().any(|dir| part == *dir) {
            return false;
        }
    }
    true
}

fn is_ref(path: &Path) -> bool {
    path.iter().any(|part| part == ".git")
        && (path.iter().any(|part| part == "refs")
            || path.file_name().is_some_and(|name| name == "HEAD" || name == "packed-refs"))
}

/// A real folder, not a link to one.
fn is_dir(path: &Path) -> bool {
    path.symlink_metadata().is_ok_and(|meta| meta.is_dir())
}

/// `folder` and the watched folders below it.
fn below<'a>(folders: &'a BTreeSet<PathBuf>, folder: &'a Path) -> impl Iterator<Item = PathBuf> + 'a {
    folders.range(folder.to_path_buf()..).take_while(move |watched| watched.starts_with(folder)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scaled_timeout;

    /// The changes reported, waiting up to `first` for one and then until they stop.
    fn settled(changes: &Receiver<Change>, first: Duration) -> Vec<Change> {
        let Ok(change) = changes.recv_timeout(first) else { return Vec::new() };
        let mut seen: Vec<_> =
            std::iter::once(change).chain(std::iter::from_fn(|| changes.recv_timeout(SETTLE * 3).ok())).collect();
        seen.dedup();
        seen
    }

    #[test]
    fn build_output_is_not_watched_but_sources_and_new_folders_are() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().join("worktree");
        for folder in ["src", "target/debug", "node_modules/pkg", "bin", ".git/objects/ab", ".git/refs/heads"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let (tx, changes) = channel();
        let mut watcher = Watcher::new(move |change| drop(tx.send(change))).unwrap();
        watcher.set_roots(std::slice::from_ref(&root));
        assert_eq!(watcher.roots(), std::slice::from_ref(&root));
        let real = std::fs::canonicalize(&root).unwrap();
        let folders = |watcher: &Watcher| -> Vec<String> {
            let shared = watcher.shared.lock().unwrap();
            shared.folders.iter().map(|folder| folder.strip_prefix(&real).unwrap().display().to_string()).collect()
        };
        let initial = ["", ".git", ".git/refs", ".git/refs/heads", "src"];
        if per_folder() {
            assert_eq!(folders(&watcher), initial);
        }
        let (quiet, soon) = (scaled_timeout(SETTLE * 4), scaled_timeout(Duration::from_secs(10)));
        let (none, files): (Vec<Change>, _) = (Vec::new(), vec![Change::Files(root.clone())]);

        std::fs::write(root.join("target/debug/imacs"), "built").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "installed").unwrap();
        assert_eq!(settled(&changes, quiet), none, "build output and dependencies must not be reported");
        std::fs::write(root.join("src/main.rs"), "edited").unwrap();
        assert_eq!(settled(&changes, soon), files);

        std::fs::create_dir_all(root.join("src/new/deep")).unwrap();
        std::fs::create_dir(root.join("src/new/target")).unwrap();
        assert_eq!(settled(&changes, soon), files);
        std::fs::write(root.join("src/new/deep/lib.rs"), "added").unwrap();
        assert_eq!(settled(&changes, soon), files, "folders created later must be watched");
        std::fs::write(root.join("src/new/target/out"), "built").unwrap();
        assert_eq!(settled(&changes, quiet), none);
        if per_folder() {
            let watched = folders(&watcher);
            assert!(watched.iter().any(|folder| folder == "src/new/deep"), "{watched:?}");
            assert!(!watched.iter().any(|folder| folder == "src/new/target"), "{watched:?}");
        }

        std::fs::remove_dir_all(root.join("src/new")).unwrap();
        assert_eq!(settled(&changes, soon), files);
        if per_folder() {
            assert_eq!(folders(&watcher), initial, "removed folders must no longer be watched");
        }
        watcher.set_roots(&[]);
        assert!(watcher.roots().is_empty() && watcher.shared.lock().unwrap().folders.is_empty());
    }

    #[test]
    fn reading_a_ref_is_no_change_and_writing_one_is() {
        let dir = crate::test_support::tempdir().unwrap();
        let root = dir.path().join("worktree");
        std::fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        let head = root.join(".git/HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").unwrap();
        let (tx, changes) = channel();
        let mut watcher = Watcher::new(move |change| drop(tx.send(change))).unwrap();
        watcher.set_roots(std::slice::from_ref(&root));
        for _ in 0..3 {
            std::fs::read_to_string(&head).unwrap();
        }
        let quiet = settled(&changes, scaled_timeout(SETTLE * 4));
        assert!(quiet.is_empty(), "reading a file is not a change: {quiet:?}");
        std::fs::write(&head, "ref: refs/heads/other\n").unwrap();
        assert_eq!(settled(&changes, scaled_timeout(Duration::from_secs(10))), [Change::Refs]);
    }

    #[test]
    fn only_the_parts_of_git_where_branches_move_are_watched() {
        for (folder, watched) in [
            ("src/app", true),
            ("target", false),
            ("crates/app/target/debug", false),
            ("web/node_modules/pkg", false),
            ("Api/bin", false),
            ("Api/obj/Debug", false),
            (".git", true),
            (".git/refs/heads/feature/target", true),
            (".git/worktrees/task", true),
            (".git/worktrees/task/logs", false),
            (".git/objects/ab", false),
            (".git/logs/refs", false),
        ] {
            assert_eq!(watched_folder(Path::new(folder)), watched, "{folder}");
        }
    }
}
