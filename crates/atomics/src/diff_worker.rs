//! Host diff preparation, coalescing and cache. Publishes results to the runtime.
use crate::highlight::{Highlighter, Spans};
use atomics_core::diff::{FileDiff, WorktreeDiff};
use atomics_core::git::WorktreeFile;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};
pub struct Prepared {
    pub diff: WorktreeDiff,
    /// Per file, per hunk, per line.
    pub highlights: Vec<Arc<FileHighlights>>,
    pub longest_line: usize,
    /// Lines added and removed, counted once rather than every frame.
    pub additions: usize,
    pub deletions: usize,
    /// Every file git shows in the worktree, for the file picker.
    pub all_files: Arc<Vec<Arc<str>>>,
}

/// One file's colors, per hunk, per line.
pub type FileHighlights = Vec<Vec<Spans>>;

/// A whole file opened from the file picker.
pub struct FileView {
    pub lines: Vec<String>,
    pub highlights: Vec<Spans>,
    pub longest_line: usize,
    /// Why the content is not shown (binary, too large).
    pub note: Option<String>,
}

pub enum WorkerResult {
    Diff { root: PathBuf, result: Result<Arc<Prepared>, String> },
    File { root: PathBuf, path: String, result: Result<Arc<FileView>, String> },
}

enum Job {
    Diff { root: PathBuf, base_ref: String, untracked_budget: Option<u64>, highlighter: Arc<Highlighter> },
    File { root: PathBuf, path: String, highlighter: Arc<Highlighter> },
}

pub struct DiffWorker {
    tx: Sender<Job>,
    pub rx: Receiver<WorkerResult>,
}

const MAX_VIEW_BYTES: u64 = 4 * 1024 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 20_000;

impl DiffWorker {
    pub fn new(wake: crate::host::Wake) -> DiffWorker {
        let (tx, jobs) = channel::<Job>();
        let (results, rx) = wake.channel();
        std::thread::Builder::new()
            .name("atomics-diff".into())
            .spawn(move || {
                let mut cache = Cache::default();
                while let Ok(first) = jobs.recv() {
                    // Coalesce bursts (an agent saving many files) into one job per worktree and file.
                    let mut diffs: HashMap<PathBuf, Job> = HashMap::new();
                    let mut files: HashMap<(PathBuf, String), Job> = HashMap::new();
                    for job in std::iter::once(first).chain(std::iter::from_fn(|| jobs.try_recv().ok())) {
                        match &job {
                            Job::Diff { root, .. } => diffs.insert(root.clone(), job),
                            Job::File { root, path, .. } => files.insert((root.clone(), path.clone()), job),
                        };
                    }
                    for job in diffs.into_values().chain(files.into_values()) {
                        if results.send(cache.run(job)).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawn diff worker");
        DiffWorker { tx, rx }
    }

    /// Diffs `root`; with `untracked_budget`, untracked content beyond that many bytes is
    /// counted but left out (for diffs nobody is looking at).
    pub fn request(
        &self,
        root: PathBuf,
        base_ref: String,
        untracked_budget: Option<u64>,
        highlighter: Arc<Highlighter>,
    ) {
        let _ = self.tx.send(Job::Diff { root, base_ref, untracked_budget, highlighter });
    }

    pub fn request_file(&self, root: PathBuf, path: String, highlighter: Arc<Highlighter>) {
        let _ = self.tx.send(Job::File { root, path, highlighter });
    }
}

/// A diff that came from elsewhere, such as a pull request's diff from GitHub, prepared like a
/// worktree's: highlighted, measured and counted. It has no worktree, so no file list.
pub fn prepare_diff(diff: WorktreeDiff, highlighter: &Highlighter) -> Arc<Prepared> {
    let highlights = diff.files.iter().map(|file| Arc::new(highlighter.file(file))).collect();
    let longest_line = diff
        .files
        .iter()
        .flat_map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()))
        .map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3)
        .chain(diff.files.iter().map(|f| f.path().len() + 20))
        .max()
        .unwrap_or(0);
    let (additions, deletions) = (diff.additions(), diff.deletions());
    Arc::new(Prepared { diff, highlights, longest_line, additions, deletions, all_files: Arc::default() })
}

/// What the worker keeps between jobs, so a refresh only redoes what changed.
#[derive(Default)]
struct Cache {
    /// The highlighter the cached colors came from; another theme starts over.
    highlighter: Option<Arc<Highlighter>>,
    /// Per worktree, the last diff and the hash of each file in it.
    diffs: HashMap<PathBuf, (Arc<Prepared>, Vec<u64>)>,
    /// Per worktree, its tracked files and the index stamp they were listed at.
    tracked: HashMap<PathBuf, Tracked>,
    /// Every tracked path once, shared by the worktrees of a repository.
    paths: HashSet<Arc<str>>,
    /// The file last opened in the viewer, with the hash of its worktree, path and bytes.
    file: Option<(u64, Arc<FileView>)>,
}

struct Tracked {
    index: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    files: Arc<Vec<Arc<str>>>,
}

impl Cache {
    fn run(&mut self, job: Job) -> WorkerResult {
        match job {
            Job::Diff { root, base_ref, untracked_budget, highlighter } => {
                self.use_highlighter(&highlighter);
                let result =
                    self.prepare(&root, &base_ref, untracked_budget, &highlighter).map_err(|e| format!("{e:#}"));
                WorkerResult::Diff { root, result }
            }
            Job::File { root, path, highlighter } => {
                self.use_highlighter(&highlighter);
                let result = self.load_file(&root, &path, &highlighter).map_err(|e| format!("{e:#}"));
                WorkerResult::File { root, path, result }
            }
        }
    }

    fn use_highlighter(&mut self, highlighter: &Arc<Highlighter>) {
        if self.highlighter.as_ref().is_none_or(|h| !Arc::ptr_eq(h, highlighter)) {
            self.highlighter = Some(highlighter.clone());
            self.diffs.clear();
            self.file = None;
        }
    }

    fn load_file(&mut self, root: &Path, path: &str, highlighter: &Highlighter) -> anyhow::Result<Arc<FileView>> {
        let empty = |note: &str| {
            Arc::new(FileView {
                lines: Vec::new(),
                highlights: Vec::new(),
                longest_line: 0,
                note: Some(note.to_string()),
            })
        };
        // A link is shown as where it points, never opened: in a sandbox's worktree it can point anywhere.
        let bytes = match atomics_core::git::read_worktree_file(root, path, MAX_VIEW_BYTES) {
            Some(WorktreeFile::Link(target)) => return Ok(empty(&format!("Symbolic link to {target}"))),
            Some(WorktreeFile::File { bytes: Some(bytes), .. }) => bytes,
            Some(WorktreeFile::File { bytes: None, .. }) => return Ok(empty("File is too large to show.")),
            None => anyhow::bail!("not a file in the worktree"),
        };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            return Ok(empty("Binary file."));
        }
        let mut hasher = DefaultHasher::new();
        (root, path, &bytes).hash(&mut hasher);
        let hash = hasher.finish();
        if let Some((cached, view)) = &self.file
            && *cached == hash
        {
            return Ok(view.clone());
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        let highlights = if lines.len() <= MAX_HIGHLIGHT_LINES {
            highlighter.lines(path, &lines)
        } else {
            vec![Vec::new(); lines.len()]
        };
        let longest_line = lines.iter().map(|l| l.chars().count() + l.matches('\t').count() * 3).max().unwrap_or(0);
        let view = Arc::new(FileView { lines, highlights, longest_line, note: None });
        self.file = Some((hash, view.clone()));
        Ok(view)
    }

    fn prepare(
        &mut self,
        root: &Path,
        base_ref: &str,
        untracked_budget: Option<u64>,
        highlighter: &Highlighter,
    ) -> anyhow::Result<Arc<Prepared>> {
        let diff = atomics_core::git::diff_against_base(root, base_ref, untracked_budget)?;
        let all_files = self.tracked_files(root);
        let hashes: Vec<u64> = diff.files.iter().map(FileDiff::content_hash).collect();
        // Nothing changed: the same diff, so nothing downstream (the explorer) redoes its work.
        if let Some((last, last_hashes)) = self.diffs.get(root)
            && *last_hashes == hashes
            && (&last.diff.base, &last.diff.base_ref, last.diff.partial) == (&diff.base, &diff.base_ref, diff.partial)
            && Arc::ptr_eq(&last.all_files, &all_files)
        {
            return Ok(last.clone());
        }
        let last = self.diffs.remove(root);
        let previous: HashMap<&str, (u64, &Arc<FileHighlights>)> = last
            .iter()
            .flat_map(|(last, hashes)| {
                last.diff.files.iter().zip(hashes).zip(&last.highlights).map(|((f, h), c)| (f.path(), (*h, c)))
            })
            .collect();
        let highlights = diff
            .files
            .iter()
            .zip(&hashes)
            .map(|(file, hash)| match previous.get(file.path()) {
                Some((cached, colors)) if cached == hash => Arc::clone(colors),
                _ => Arc::new(highlighter.file(file)),
            })
            .collect();
        let longest_line = diff
            .files
            .iter()
            .flat_map(|f| f.hunks.iter().flat_map(|h| h.lines.iter()))
            .map(|l| l.text.chars().count() + l.text.matches('\t').count() * 3)
            .chain(diff.files.iter().map(|f| f.path().len() + 20))
            .max()
            .unwrap_or(0);
        let (additions, deletions) = (diff.additions(), diff.deletions());
        let prepared = Arc::new(Prepared { diff, highlights, longest_line, additions, deletions, all_files });
        self.diffs.insert(root.to_path_buf(), (prepared.clone(), hashes));
        Ok(prepared)
    }

    /// The tracked files of a worktree, listed again only when its index changes.
    fn tracked_files(&mut self, root: &Path) -> Arc<Vec<Arc<str>>> {
        let Cache { tracked, paths, .. } = self;
        let tracked = tracked.entry(root.to_path_buf()).or_insert_with(|| Tracked {
            index: atomics_core::git::index_file(root).ok(),
            stamp: None,
            files: Arc::default(),
        });
        let stamp = tracked
            .index
            .as_ref()
            .and_then(|index| std::fs::metadata(index).ok())
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        if stamp.is_none() || stamp != tracked.stamp {
            let files = atomics_core::git::list_files(root);
            tracked.stamp = stamp.filter(|_| files.is_ok());
            let shared = |path: String| match paths.get(path.as_str()) {
                Some(shared) => shared.clone(),
                None => {
                    let shared: Arc<str> = path.into();
                    paths.insert(shared.clone());
                    shared
                }
            };
            tracked.files = Arc::new(files.unwrap_or_default().into_iter().map(shared).collect());
        }
        tracked.files.clone()
    }
}

/// Host-owned accepted content and scheduling, independent of any diff view.
#[derive(Clone, Default)]
pub struct DiffData {
    pub revision: u64,
    pub prepared: Option<Arc<Prepared>>,
    pub error: Option<String>,
    pub loading: bool,
    pub dirty_since: Option<Instant>,
    pub last_request: Option<Instant>,
    pub comments: Vec<atomics_core::steer::ReviewComment>,
    /// Current native client's file subscription, used for background refresh.
    pub viewing: Option<String>,
    pub file: Option<Arc<FileView>>,
    pub file_error: Option<String>,
}

impl DiffData {
    pub fn edit_comments(
        &mut self,
        before: Vec<atomics_core::steer::ReviewComment>,
        mut after: Vec<atomics_core::steer::ReviewComment>,
    ) {
        // Apply only the client's additions and removals. Match occurrences so
        // equal comments remain distinct, including comments accepted via ctl.
        for comment in before {
            if let Some(index) = after.iter().position(|other| other == &comment) {
                after.remove(index);
            } else if let Some(index) = self.comments.iter().position(|other| other == &comment) {
                self.comments.remove(index);
            }
        }
        self.comments.extend(after);
    }

    pub fn mark_dirty(&mut self) {
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    pub fn due_at(&self, interval: Duration) -> Option<Instant> {
        let quiet = self.dirty_since? + Duration::from_millis(350);
        Some(self.last_request.map_or(quiet, |at| quiet.max(at + interval)))
    }

    pub fn requested(&mut self) {
        self.dirty_since = None;
        self.loading = true;
        self.last_request = Some(Instant::now());
    }
}
